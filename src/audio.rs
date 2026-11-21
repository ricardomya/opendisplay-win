//! System audio to the receiver, routed by screen (local pv 4 extension).
//!
//! The user's rule: media on the primary screen stays on the PC speakers;
//! media whose window sits on the extended (iPad) monitor plays on the iPad.
//!
//! Per-app routing uses the undocumented WinRT activation factory
//! `Windows.Media.Internal.AudioPolicyConfig` (the interface EarTrumpet calls
//! `IAudioPolicyConfigFactory`): vtable slot 25,
//! `SetPersistedDefaultAudioEndpoint(pid, eRender, role, deviceId)`. It takes a
//! **PID whose app has an audio session** and moves that app's output to the
//! chosen endpoint. We point it at a virtual render device on this machine
//! (VB-CABLE, Steam Streaming Speakers, ToDesk/UU virtual audio, ...), capture
//! that device's loopback, and stream 20 ms 48 kHz stereo s16 `ODAU` frames on
//! the existing sender→receiver TCP stream.
//!
//! Unlike muting or zeroing the app's session volume (which this machine taps
//! upstream of process loopback, so the capture goes silent too), the PC
//! speakers genuinely stop receiving the app's audio while the virtual
//! endpoint carries it.
//!
//! Every route is restored when the window leaves the iPad screen, and routes
//! left behind by a crash are cleaned at startup by comparing each app's
//! persisted endpoint with our virtual device.
//!
//! Only receivers that advertise `hello.audio` get the frames (protocol
//! `Hello.audio`), so the App Store receiver, which predates this, is never
//! sent audio it cannot play.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use windows::core::{link, Interface, BOOL, GUID, HSTRING, HRESULT, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{
    CloseHandle, HWND, LPARAM, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

link!("api-ms-win-core-winrt-l1-1-0.dll" "system" fn RoGetActivationFactory(
    classid: HSTRING,
    iid: *const GUID,
    factory: *mut *mut c_void
) -> HRESULT);

/// IAudioPolicyConfigFactory (Windows 11 21H2+).
const POLICY_IID: GUID = GUID::from_u128(0xAB3D4648_E242_459F_B02F_541C70306324);
const MMDEVAPI_PNP: &str = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";

/// Audio frame header: `ODAU` + version/codec byte + 3 reserved zero bytes,
/// then interleaved little-endian s16 PCM (48 kHz stereo).
pub const AUDIO_MAGIC: &[u8; 4] = b"ODAU";
const AUDIO_VERSION: u8 = 1;

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_MS: u32 = 20;
const FRAME_FRAMES: usize = (RATE as usize / 1000) * FRAME_MS as usize; // 960
const FRAME_I16: usize = FRAME_FRAMES * CHANNELS; // 1920

const ROLES: [i32; 3] = [0, 1, 2]; // eConsole, eMultimedia, eCommunications

pub struct AudioSession {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl AudioSession {
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

/// Start routing window audio from `rect` (the iPad monitor) to the writer.
pub fn start(tx: Sender<(Vec<u8>, bool)>, rect: Arc<Mutex<RECT>>) -> AudioSession {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = thread::Builder::new()
        .name("audio".into())
        .spawn(move || run(tx, rect, stop_thread))
        .expect("spawn audio thread");
    AudioSession { stop, handle }
}

fn run(tx: Sender<(Vec<u8>, bool)>, rect: Arc<Mutex<RECT>>, stop: Arc<AtomicBool>) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let device = match find_virtual_device() {
        Some(d) => d,
        None => {
            warn!("audio: no virtual render device found; iPad audio stays off");
            return;
        }
    };
    let virtual_id = endpoint_id(&device);
    let virtual_pnp = pnp_path(&virtual_id);
    info!(
        "audio: virtual endpoint '{}' ({virtual_id})",
        friendly_name(&device)
    );

    // Routes this tool created are recorded so a crash cannot leave an app
    // stuck on the virtual sink: the next start restores exactly those, and
    // never touches a route the user set themselves.
    let mut owned = load_routes();
    unsafe { cleanup_leftovers(&virtual_pnp, &mut owned) };

    let capture_ok = Arc::new(AtomicBool::new(false));
    let routed_count = Arc::new(AtomicUsize::new(0));
    let mut capture_handle: Option<thread::JoinHandle<()>> = None;

    let own_pid = std::process::id();
    let mut routed: HashMap<String, RouteEntry> = HashMap::new();
    let mut last_report = Instant::now() - Duration::from_secs(3600);

    while !stop.load(Ordering::Relaxed) {
        let target = *rect.lock().unwrap();
        let pids = unsafe { pids_on_rect(target, own_pid) };
        let tree = unsafe { descendants(&pids) };

        // Start the endpoint capture once; it keeps running while we route.
        if capture_handle.is_none() && target.right > target.left {
            let tx = tx.clone();
            let stop_c = stop.clone();
            let ok = capture_ok.clone();
            let count = routed_count.clone();
            let vid = virtual_id.clone();
            capture_handle = Some(
                thread::Builder::new()
                    .name("audio-cap".into())
                    .spawn(move || capture_endpoint(vid, tx, stop_c, ok, count))
                    .expect("spawn audio capture"),
            );
        }

        // Sessions give us the PID that actually owns audio (a browser's
        // session lives in a child process, while the window PID has none).
        let sessions = sessions_on_all_endpoints();
        let target_exes: HashSet<String> = sessions
            .iter()
            .filter(|s| tree.contains(&s.pid))
            .map(|s| s.exe.clone())
            .collect();

        if capture_ok.load(Ordering::Relaxed) {
            for s in &sessions {
                if !tree.contains(&s.pid) || s.exe.is_empty() {
                    continue;
                }
                if routed.contains_key(&s.exe) {
                    continue;
                }
                let prior = unsafe { read_persisted(s.pid) };
                if prior.iter().flatten().any(|p| p == &virtual_pnp) {
                    // Already on our sink (a route from an earlier run, or set by
                    // hand). The sink is ours: restore the default output as soon
                    // as the app has no window on the iPad screen, otherwise its
                    // sound would go into a device nothing plays from.
                    routed.insert(s.exe.clone(), RouteEntry { pid: s.pid, prior: [None, None, None] });
                    owned.insert(s.exe.clone());
                    save_routes(&owned);
                    info!("audio: {} is already on the virtual sink; it will be restored when it leaves", s.exe);
                    continue;
                }
                unsafe {
                    for role in ROLES {
                        let _ = set_persisted(s.pid, &virtual_pnp, role);
                    }
                }
                info!("audio: routed {} (pid {}) to the iPad", s.exe, s.pid);
                routed.insert(s.exe.clone(), RouteEntry { pid: s.pid, prior });
                owned.insert(s.exe.clone());
                save_routes(&owned);
            }
        }

        // Restore apps whose window left the iPad screen.
        let gone: Vec<String> = routed
            .keys()
            .filter(|exe| !target_exes.contains(*exe))
            .cloned()
            .collect();
        for exe in gone {
            if let Some(entry) = routed.remove(&exe) {
                match sessions.iter().find(|s| s.exe == exe) {
                    Some(s) => {
                        if unsafe { restore_persisted(s.pid, &entry.prior) } {
                            info!("audio: restored {exe} to the PC output");
                            owned.remove(&exe);
                            save_routes(&owned);
                        } else {
                            debug!("audio: could not restore {exe} yet; will retry");
                            routed.insert(exe, entry);
                        }
                    }
                    None => {
                        // The app is not running (no session). Keep the
                        // route on file; the stale pass restores it when the
                        // app next runs, or routing reuses it if its window
                        // reappears on this screen.
                        debug!("audio: {exe} left the screen and is not running; route kept");
                    }
                }
            }
        }

        // Apps this tool routed in an earlier run that are running now but
        // have no window on the iPad screen (the window moved before this
        // process started): send them back to the default output.
        let stale: Vec<(String, u32)> = owned
            .iter()
            .filter(|e| !target_exes.contains(*e) && !routed.contains_key(*e))
            .filter_map(|e| {
                sessions
                    .iter()
                    .find(|s| &s.exe == e)
                    .map(|s| (e.clone(), s.pid))
            })
            .collect();
        for (exe, pid) in stale {
            if unsafe { restore_persisted(pid, &[None, None, None]) } {
                owned.remove(&exe);
                save_routes(&owned);
                info!("audio: cleared stale route for {exe} (pid {pid})");
            }
        }
        routed_count.store(routed.len(), Ordering::Relaxed);

        if last_report.elapsed() >= Duration::from_secs(30) {
            last_report = Instant::now();
            if !routed.is_empty() {
                info!(
                    "audio: routing {} app(s) from the iPad screen: {}",
                    routed.len(),
                    routed.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }

        thread::sleep(Duration::from_millis(1000));
    }

    // Restore everything we moved, using a current session PID if possible.
    routed_count.store(0, Ordering::Relaxed);
    let sessions = sessions_on_all_endpoints();
    for (exe, entry) in routed.drain() {
        let pid = sessions
            .iter()
            .find(|s| s.exe == exe)
            .map(|s| s.pid)
            .unwrap_or(entry.pid);
        if unsafe { restore_persisted(pid, &entry.prior) } {
            owned.remove(&exe);
        } else {
            warn!("audio: could not restore {exe} on shutdown; it stays queued for next start");
        }
    }
    save_routes(&owned);
    if let Some(h) = capture_handle.take() {
        let _ = h.join();
    }
    debug!("audio: stopped");
}

struct RouteEntry {
    pid: u32,
    prior: [Option<String>; 3],
}

// --- per-app endpoint routing (undocumented factory) -----------------------

fn activation_factory() -> Option<*mut c_void> {
    let mut factory: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        RoGetActivationFactory(
            HSTRING::from("Windows.Media.Internal.AudioPolicyConfig"),
            &POLICY_IID,
            &mut factory,
        )
    };
    if hr.is_err() || factory.is_null() {
        warn!("audio: AudioPolicyConfig factory unavailable: {hr:?}");
        return None;
    }
    Some(factory)
}

unsafe fn factory_vtbl(factory: *mut c_void) -> *const *const c_void {
    unsafe { *(factory as *mut *const *const c_void) }
}

unsafe fn release_factory(factory: *mut c_void) {
    let vtbl = unsafe { factory_vtbl(factory) };
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
    let release: ReleaseFn = unsafe { std::mem::transmute(*vtbl.add(2)) };
    unsafe { release(factory) };
}

unsafe fn set_persisted(pid: u32, device: &str, role: i32) -> HRESULT {
    let Some(factory) = activation_factory() else {
        return HRESULT(0x80004005u32 as i32);
    };
    let vtbl = unsafe { factory_vtbl(factory) };
    type SetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, HSTRING) -> HRESULT;
    let f: SetFn = unsafe { std::mem::transmute(*vtbl.add(25)) };
    let value = if device.is_empty() {
        HSTRING::new()
    } else {
        HSTRING::from(device)
    };
    let hr = unsafe { f(factory, pid, 0 /*eRender*/, role, value) };
    unsafe { release_factory(factory) };
    hr
}

/// Persisted per-app endpoints for the three render roles.
unsafe fn read_persisted(pid: u32) -> [Option<String>; 3] {
    let mut out: [Option<String>; 3] = [None, None, None];
    let Some(factory) = activation_factory() else {
        return out;
    };
    let vtbl = unsafe { factory_vtbl(factory) };
    type GetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut HSTRING) -> HRESULT;
    let f: GetFn = unsafe { std::mem::transmute(*vtbl.add(26)) };
    for (i, role) in ROLES.iter().enumerate() {
        let mut value = HSTRING::new();
        let hr = unsafe { f(factory, pid, 0, *role, &mut value) };
        if hr.is_ok() {
            let s = value.to_string();
            if !s.is_empty() {
                out[i] = Some(s);
            }
        }
    }
    unsafe { release_factory(factory) };
    out
}

/// Put the app's three render roles back. Returns false if any call failed, so
/// the caller keeps the exe on the owned list and can retry.
unsafe fn restore_persisted(pid: u32, prior: &[Option<String>; 3]) -> bool {
    let mut ok = true;
    for (i, role) in ROLES.iter().enumerate() {
        let value = prior[i].as_deref().unwrap_or("");
        let hr = unsafe { set_persisted(pid, value, *role) };
        ok &= hr.is_ok();
    }
    ok
}

unsafe fn cleanup_leftovers(virtual_pnp: &str, owned: &mut HashSet<String>) {
    // Anything still parked on our virtual sink after a crash or a forced
    // restart would play into the void; put it back on the default output.
    // The stream re-routes whatever actually has a window on the iPad screen.
    for s in sessions_on_all_endpoints() {
        if s.pid == 0 || s.exe.is_empty() {
            continue;
        }
        let prior = unsafe { read_persisted(s.pid) };
        if prior.iter().flatten().any(|p| p == virtual_pnp) {
            if unsafe { restore_persisted(s.pid, &[None, None, None]) } {
                info!("audio: cleared a leftover route for {} (pid {})", s.exe, s.pid);
                owned.remove(&s.exe);
            }
        } else {
            // Not actually on our sink (already default): nothing to undo.
            owned.remove(&s.exe);
        }
    }
    save_routes(owned);
}

// --- crash-safe route bookkeeping -----------------------------------------

fn routes_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("OpenDisplay")
            .join("audio_routes.json"),
    )
}

fn load_routes() -> HashSet<String> {
    let Some(path) = routes_path() else {
        return HashSet::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_routes(routes: &HashSet<String>) {
    let Some(path) = routes_path() else {
        return;
    };
    if routes.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let list: Vec<&String> = routes.iter().collect();
    if let Ok(text) = serde_json::to_string(&list) {
        let _ = std::fs::write(&path, text);
    }
}

// --- device discovery -----------------------------------------------------

fn enumerator() -> Option<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() }
}

fn render_devices() -> Vec<IMMDevice> {
    unsafe {
        let Some(en) = enumerator() else {
            return Vec::new();
        };
        let Ok(col) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let n = col.GetCount().unwrap_or(0);
        (0..n).filter_map(|i| col.Item(i).ok()).collect()
    }
}

fn device_id(device: &IMMDevice) -> String {
    unsafe {
        match device.GetId() {
            Ok(pw) => {
                let s = pw.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pw.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

fn pnp_path(id: &str) -> String {
    format!("\\\\?\\SWD#MMDEVAPI#{id}#{MMDEVAPI_PNP}")
}

fn endpoint_id(device: &IMMDevice) -> String {
    device_id(device)
}

fn friendly_name(device: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return String::new();
        };
        let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) else {
            return String::new();
        };
        match PropVariantToStringAlloc(&prop) {
            Ok(pw) => {
                let s = pw.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pw.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

/// Pick a virtual sink to carry iPad-screen audio. `OD_AUDIO_DEVICE` overrides
/// with a substring match.
fn find_virtual_device() -> Option<IMMDevice> {
    let default_id = unsafe {
        enumerator()
            .and_then(|en| en.GetDefaultAudioEndpoint(eRender, eConsole).ok())
            .map(|d| device_id(&d))
    };
    let override_sub = std::env::var("OD_AUDIO_DEVICE").ok();
    // The microphone bridge renders the selected microphone into this device.
    // Using it as the system-audio sink would loop the microphone back into
    // ODAU (and, through the meeting's virtual mic, back to the far end), so
    // never pick it — not even through OD_AUDIO_DEVICE.
    let mic_sub = std::env::var("OD_VIRTUAL_MIC")
        .unwrap_or_else(|_| "CABLE Input".into())
        .to_lowercase();
    let mut best: Option<(i32, IMMDevice)> = None;
    for d in render_devices() {
        let id = device_id(&d);
        if Some(&id) == default_id.as_ref() {
            continue;
        }
        let name = friendly_name(&d).to_lowercase();
        if name.contains(&mic_sub) {
            if override_sub
                .as_ref()
                .map(|s| name.contains(&s.to_lowercase()))
                .unwrap_or(false)
            {
                warn!(
                    "audio: refusing the microphone cable '{}' as the audio sink",
                    friendly_name(&d)
                );
            }
            continue;
        }
        let score = if let Some(sub) = &override_sub {
            if name.contains(&sub.to_lowercase()) {
                1000
            } else {
                -1
            }
        } else if name.contains("streaming speakers") {
            500
        } else if name.contains("virtual audio") {
            300
        } else if name.contains("pico streaming") {
            100
        } else {
            // Never fall back to a cable: every VB-CABLE render endpoint has a
            // capture twin, so it can silently loop audio back into a meeting.
            // OD_AUDIO_DEVICE remains the explicit escape hatch.
            -1
        };
        if score < 0 || name.contains("microphone") || name.contains("fxsound") {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, d));
        }
    }
    best.map(|(_, d)| d)
}

// --- session + process discovery ------------------------------------------

struct SessionInfo {
    pid: u32,
    exe: String,
}

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    }
}

fn sessions_on_all_endpoints() -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for device in render_devices() {
        unsafe {
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            let n = sessions.GetCount().unwrap_or(0);
            for i in 0..n {
                let Ok(ctrl) = sessions.GetSession(i) else {
                    continue;
                };
                let Ok(ctrl2) = ctrl.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = ctrl2.GetProcessId().unwrap_or(0);
                if pid == 0 || !seen.insert(pid) {
                    continue;
                }
                out.push(SessionInfo {
                    pid,
                    exe: process_name(pid),
                });
            }
        }
    }
    out
}

unsafe fn descendants(targets: &HashSet<u32>) -> HashSet<u32> {
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(s) => s,
        Err(_) => return targets.clone(),
    };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            children
                .entry(entry.th32ParentProcessID)
                .or_default()
                .push(entry.th32ProcessID);
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };

    let mut out = targets.clone();
    let mut stack: Vec<u32> = targets.iter().copied().collect();
    while let Some(pid) = stack.pop() {
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                if out.insert(kid) {
                    stack.push(kid);
                }
            }
        }
    }
    out
}

pub(crate) unsafe fn pids_on_rect(rect: RECT, own_pid: u32) -> HashSet<u32> {
    struct Search {
        rect: RECT,
        own_pid: u32,
        pids: HashSet<u32>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return BOOL(1);
            }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            if GetWindowTextLengthW(hwnd) == 0 {
                return BOOL(1);
            }
            let mut wr = RECT::default();
            if GetWindowRect(hwnd, &mut wr).is_err() {
                return BOOL(1);
            }
            let cx = wr.left + (wr.right - wr.left) / 2;
            let cy = wr.top + (wr.bottom - wr.top) / 2;
            if cx < search.rect.left
                || cx >= search.rect.right
                || cy < search.rect.top
                || cy >= search.rect.bottom
            {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 && pid != search.own_pid {
                search.pids.insert(pid);
            }
        }
        BOOL(1)
    }

    let mut search = Search {
        rect,
        own_pid,
        pids: HashSet::new(),
    };
    let _ = unsafe { EnumWindows(Some(callback), LPARAM(&mut search as *mut _ as isize)) };
    search.pids
}

// --- endpoint loopback capture --------------------------------------------

fn capture_endpoint(
    virtual_id: String,
    tx: Sender<(Vec<u8>, bool)>,
    stop: Arc<AtomicBool>,
    ok: Arc<AtomicBool>,
    routed_count: Arc<AtomicUsize>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // IMMDevice is not Send, so the capture thread opens its own handle to
        // the endpoint by id rather than receiving one from the router thread.
        let device: IMMDevice = match enumerator()
            .and_then(|en| en.GetDevice(&HSTRING::from(virtual_id.as_str())).ok())
        {
            Some(d) => d,
            None => {
                warn!("audio: endpoint {virtual_id} disappeared");
                return;
            }
        };
        let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                warn!("audio: activate {virtual_id}: {e}");
                return;
            }
        };
        let mix = match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => {
                warn!("audio: mix format: {e}");
                return;
            }
        };
        let fmt: WAVEFORMATEX = *mix;
        // Auto-reset event; the packet loop drains everything available.
        let event = match CreateEventW(None, false, false, None) {
            Ok(e) => e,
            Err(e) => {
                warn!("audio: event: {e}");
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if let Err(e) = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            200_0000,
            0,
            mix,
            None,
        ) {
            warn!("audio: loopback initialize: {e}");
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        if let Err(e) = client.SetEventHandle(event) {
            warn!("audio: set event: {e}");
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(e) => {
                warn!("audio: capture service: {e}");
                let _ = CloseHandle(event);
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if let Err(e) = client.Start() {
            warn!("audio: start: {e}");
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        ok.store(true, Ordering::Relaxed);
        // WAVEFORMATEX is a packed struct; copy the fields before using them
        // in expressions that take references.
        let rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        info!(
            "audio: capturing '{}' {rate} Hz {channels} ch {bits} bit",
            friendly_name(&device)
        );

        let mut raw: Vec<i16> = Vec::with_capacity(FRAME_I16 * 2);
        let mut pcm: Vec<i16> = Vec::with_capacity(FRAME_I16 * 2);
        let mut resampler = Resampler::new(rate, RATE);
        let mut frames_sent: u64 = 0;
        let mut peak_logged: i32 = -1;

        while !stop.load(Ordering::Relaxed) {
            let wait = WaitForSingleObject(event, 200);
            if wait == WAIT_TIMEOUT {
                continue;
            }
            if wait != WAIT_OBJECT_0 {
                break;
            }
            loop {
                let next = capture.GetNextPacketSize().unwrap_or(0);
                if next == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let bytes = frames as usize * channels * (bits / 8);
                    let samples = std::slice::from_raw_parts(data, bytes);
                    append_pcm(samples, channels, bits, &mut raw);
                    resampler.push(&mut raw, &mut pcm);
                }
                let _ = capture.ReleaseBuffer(frames);

                let routed = routed_count.load(Ordering::Relaxed);
                if routed == 0 && std::env::var_os("OD_AUDIO_ALWAYS").is_none() {
                    // Nobody is bridged; drop pending audio instead of
                    // flooding the link with silence. OD_AUDIO_ALWAYS keeps
                    // it flowing for the local echo self-test.
                    pcm.clear();
                    continue;
                }
                while pcm.len() >= FRAME_I16 {
                    let chunk: Vec<i16> = pcm.drain(..FRAME_I16).collect();
                    let mut payload = Vec::with_capacity(8 + FRAME_I16 * 2);
                    payload.extend_from_slice(AUDIO_MAGIC);
                    payload.push(AUDIO_VERSION);
                    payload.extend_from_slice(&[0, 0, 0]);
                    for s in &chunk {
                        payload.extend_from_slice(&s.to_le_bytes());
                    }
                    frames_sent += 1;
                    if frames_sent % 250 == 0 {
                        let peak = chunk.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
                        if peak != peak_logged {
                            peak_logged = peak;
                            info!("audio: sent {frames_sent} frames, peak {peak}");
                        }
                    }
                    // The writer expects complete wire frames: 4-byte big-endian
                    // length + payload (the frozen-display bug: raw "ODAU" was
                    // read as a ~1.3 GB frame length).
                    let framed = opendisplay_proto::encode_frame(&payload);
                    if tx.send((framed, false)).is_err() {
                        let _ = client.Stop();
                        let _ = CloseHandle(event);
                        CoTaskMemFree(Some(mix as *const _));
                        return;
                    }
                }
            }
        }

        let _ = client.Stop();
        let _ = CloseHandle(event);
        CoTaskMemFree(Some(mix as *const _));
    }
}

/// Append interleaved device samples as stereo s16 (first two channels).
fn append_pcm(samples: &[u8], channels: usize, bits: usize, out: &mut Vec<i16>) {
    if channels == 0 || bits == 0 {
        return;
    }
    let frames = samples.len() / (channels * (bits / 8));
    out.reserve(frames * 2);
    for f in 0..frames {
        let base = f * channels * (bits / 8);
        let mut pair = [0i16; 2];
        for (c, slot) in pair.iter_mut().enumerate().take(channels.min(2)) {
            let off = base + c * (bits / 8);
            *slot = match bits {
                32 => {
                    let v = f32::from_le_bytes([
                        samples[off],
                        samples[off + 1],
                        samples[off + 2],
                        samples[off + 3],
                    ]);
                    (v.clamp(-1.0, 1.0) * 32767.0) as i16
                }
                16 => i16::from_le_bytes([samples[off], samples[off + 1]]),
                _ => 0,
            };
        }
        if channels == 1 {
            pair[1] = pair[0];
        }
        out.extend_from_slice(&pair);
    }
}

/// Stateful linear resampler, stereo interleaved s16.
struct Resampler {
    step: f64,
    pos: f64,
    buf: Vec<i16>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            buf: Vec::new(),
        }
    }

    fn push(&mut self, input: &mut Vec<i16>, out: &mut Vec<i16>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend(input.drain(..));
            return;
        }
        self.buf.append(input);
        let frames = self.buf.len() / 2;
        while self.pos + 1.0 < frames as f64 {
            let i = self.pos.floor() as usize;
            let f = self.pos - i as f64;
            let l = self.buf[i * 2] as f64
                + (self.buf[(i + 1) * 2] as f64 - self.buf[i * 2] as f64) * f;
            let r = self.buf[i * 2 + 1] as f64
                + (self.buf[(i + 1) * 2 + 1] as f64 - self.buf[i * 2 + 1] as f64) * f;
            out.push(l as i16);
            out.push(r as i16);
            self.pos += self.step;
        }
        let drain = (self.pos.floor() as usize).min(frames);
        if drain > 0 {
            self.buf.drain(..drain * 2);
            self.pos -= drain as f64;
        }
    }
}

/// Restore every session on the default render endpoint to unmuted, 100%
/// volume. Diagnostics/tool for undoing experiments that lowered volume.
pub fn restore_all_sessions() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Some(en) = enumerator() else {
            println!("no audio enumerator");
            return;
        };
        let Ok(device) = en.GetDefaultAudioEndpoint(eRender, eConsole) else {
            println!("no render endpoint");
            return;
        };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            println!("no session manager");
            return;
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            println!("no session enumerator");
            return;
        };
        let count = sessions.GetCount().unwrap_or(0);
        let mut fixed = 0;
        for i in 0..count {
            let Ok(ctrl) = sessions.GetSession(i) else {
                continue;
            };
            let Ok(vol) = ctrl.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let mute = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            let level = vol.GetMasterVolume().unwrap_or(1.0);
            if mute || level < 0.999 {
                let _ = vol.SetMute(false, std::ptr::null());
                let _ = vol.SetMasterVolume(1.0, std::ptr::null());
                fixed += 1;
            }
        }
        println!("restored {fixed} session(s) to unmuted, 100% volume");
    }
}
