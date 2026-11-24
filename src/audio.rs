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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use windows::core::{implement, link, Interface, Ref, Result as WinResult, BOOL, GUID, HSTRING, HRESULT, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    DEVICE_STATE_ACTIVE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantToStringAlloc, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::SetEvent;
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
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
    /// The sink changed in the tray menu: `stop` ended the old run early.
    sink_changed: Arc<AtomicBool>,
    /// The sink preference generation this router was started for.
    epoch: usize,
}

impl AudioSession {
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }

    /// Whether this session ended because another sink was picked: the caller
    /// starts a fresh session instead of dropping audio.
    pub fn sink_changed(&self) -> bool {
        self.sink_changed.load(Ordering::Relaxed)
    }

    /// Whether the picker has moved on since this router started. A router
    /// that gave up early (no usable endpoint) cannot set `sink_changed`
    /// itself, so the session compares this to restart it.
    pub fn epoch(&self) -> usize {
        self.epoch
    }
}

/// The current preference generation (see `AudioSession::epoch`).
pub fn current_sink_epoch() -> usize {
    sink_epoch()
}

/// Start routing window audio from `rect` (the iPad monitor) to the writer.
pub fn start(tx: Sender<(Vec<u8>, bool)>, rect: Arc<Mutex<RECT>>) -> AudioSession {
    let stop = Arc::new(AtomicBool::new(false));
    let sink_changed = Arc::new(AtomicBool::new(false));
    let epoch = sink_epoch();
    let stop_thread = stop.clone();
    let changed_thread = sink_changed.clone();
    let handle = thread::Builder::new()
        .name("audio".into())
        .spawn(move || run(tx, rect, stop_thread, changed_thread))
        .expect("spawn audio thread");
    AudioSession { stop, handle, sink_changed, epoch }
}

fn run(
    tx: Sender<(Vec<u8>, bool)>,
    rect: Arc<Mutex<RECT>>,
    stop: Arc<AtomicBool>,
    sink_changed: Arc<AtomicBool>,
) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Routes this tool created are recorded so a crash cannot leave an app
    // stuck on the virtual sink: the next start restores exactly those, and
    // never touches a route the user set themselves. This runs before the
    // sink is looked up: a route to a device that has since disappeared
    // (ToDesk closed, cable removed) would otherwise leave the app silent
    // with nobody left to undo it.
    let mut owned = load_routes();
    let epoch = sink_epoch();
    let env_sink = std::env::var("OD_AUDIO_DEVICE").ok();
    let mut found = find_virtual_device(env_sink.as_deref());
    if found.is_none() {
        warn!("audio: no virtual render device; restored routed apps, staying off");
        // Sessions may appear long after startup (the app was closed when the
        // cleanup ran); keep putting them back on the default output. A sink
        // picked meanwhile ends this wait so the session can start again, and
        // a sink whose endpoint was merely absent (the monitor carrying the
        // HDMI output slept) is retried until it is back.
        loop {
            if stop.load(Ordering::Relaxed) || sink_epoch() != epoch {
                sink_changed.store(sink_epoch() != epoch, Ordering::Relaxed);
                return;
            }
            unsafe { cleanup_leftovers(None, &mut owned) };
            thread::sleep(Duration::from_secs(2));
            if let Some(d) = find_virtual_device(env_sink.as_deref()) {
                found = Some(d);
                break;
            }
            if !audio_sink_absent(env_sink.as_deref()) {
                // Nothing is wanted (no choice, no default candidate) or only
                // candidates that cannot be used exist: retrying cannot help
                // until the user picks an endpoint, and that bump is what the
                // loop above watches. Stay alive so the pick restarts this
                // session instead of leaving it dead for the whole connection.
                thread::sleep(Duration::from_secs(30));
                warn!("audio: no usable playback endpoint; pick one in the tray menu (iPad sound plays on)");
            }
        }
    }
    let Some(device) = found else { return };
    let virtual_id = endpoint_id(&device);
    let virtual_pnp = pnp_path(&virtual_id);
    let sink_name = friendly_name(&device);
    // A fallback picked because the chosen endpoint was missing (Steam's
    // speakers for an absent monitor output): re-check periodically so the
    // router moves back as soon as the chosen device returns.
    let using_fallback = audio_sink_absent(env_sink.as_deref());
    let mut next_sink_reeval = Instant::now() + Duration::from_secs(60);
    // Remember a deliberate choice (menu or OD_AUDIO_DEVICE), so a launch that
    // lacks the task's environment picks the same endpoint. An automatic pick
    // is not frozen -- and never when the pick was merely the fallback, or the
    // memo would freeze the fallback itself.
    if (sink_preference().is_some() || env_sink.is_some()) && !using_fallback {
        remember_sink(&sink_name);
    }
    unsafe { cleanup_leftovers(Some(&virtual_pnp), &mut owned) };
    info!("audio: virtual endpoint '{}' ({virtual_id})", sink_name);

    // A sink that lives on the microphone's own cable must not be captured
    // while the iPad microphone records: the sibling pin's loopback carries
    // the microphone (measured), so the echo guards below stay armed. An
    // independent sink is safe to capture with the ordinary device loopback;
    // the per-process variant remains behind OD_AUDIO_PROCESS_CAPTURE while
    // its activation path is debugged (it crashed this build once).
    let shared_cable = crate::mic::sink_shares_mic_cable(&sink_name);
    let process_capture =
        !shared_cable && std::env::var_os("OD_AUDIO_PROCESS_CAPTURE").is_some();
    if std::env::var_os("OD_AUDIO_MUTE_SINK").is_some() {
        unsafe { mute_sink(&device) };
    }
    info!(
        "audio: sink '{}' {}",
        sink_name,
        if shared_cable {
            "shares the microphone cable; echo guard armed"
        } else if process_capture {
            "is independent of the microphone cable; capturing per process"
        } else {
            "is independent of the microphone cable; capturing its loopback"
        }
    );

    let capture_ok = Arc::new(AtomicBool::new(false));
    let routed_count = Arc::new(AtomicUsize::new(0));
    let mut capture_handle: Option<thread::JoinHandle<()>> = None;
    let mut next_capture_try = Instant::now();
    let proc_targets: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let mix_queues: MixQueues = Arc::new(Mutex::new(HashMap::new()));
    let mut proc_handle: Option<JoinHandle<()>> = None;

    let own_pid = std::process::id();
    let mut routed: HashMap<String, RouteEntry> = HashMap::new();
    let mut last_guard: HashSet<String> = HashSet::new();
    let mut last_mic_live = false;
    let mut last_report = Instant::now() - Duration::from_secs(3600);
    // Blip absorber: a routed app must have been off the iPad screen (no
    // session judged there and no window there) this long before it is moved
    // back. The window signal does the real work, so this stays short.
    let restore_after = Duration::from_millis(500);
    // Fast polling while a restore is due makes the move happen the moment
    // the wait is over instead of up to a tick later.
    let mut poll_fast = false;

    while !stop.load(Ordering::Relaxed) {
        // Another sink was picked in the tray menu. Give the apps this router
        // moved back to the system default and let the session start over on
        // the new endpoint (its start() has already seen the new preference,
        // this only has to end the old run).
        if sink_epoch() != epoch {
            restore_for_sink_switch(&mut routed, &mut owned);
            sink_changed.store(true, Ordering::Relaxed);
            break;
        }
        // The chosen endpoint was missing at start and a fallback took over:
        // switch back to it when it returns (a monitor's HDMI audio endpoint
        // comes and goes with the monitor).
        if using_fallback && Instant::now() >= next_sink_reeval {
            next_sink_reeval = Instant::now() + Duration::from_secs(60);
            if preferred_sink_present() {
                info!("audio: the chosen playback endpoint is back; switching to it");
                restore_for_sink_switch(&mut routed, &mut owned);
                sink_changed.store(true, Ordering::Relaxed);
                break;
            }
        }
        let target = *rect.lock().unwrap();
        let pids = unsafe { pids_on_rect(target, own_pid) };
        let tree = unsafe { descendants(&pids) };

        // Per-process capture replaces the endpoint loopback on independent
        // sinks: routers below fill `proc_targets` with the routed sessions.
        if process_capture && proc_handle.is_none() {
            proc_handle = Some(
                thread::Builder::new()
                    .name("audio-proc".into())
                    .spawn({
                        let targets = proc_targets.clone();
                        let queues = mix_queues.clone();
                        let tx = tx.clone();
                        let stop = stop.clone();
                        let routed = routed_count.clone();
                        move || process_capture_manager(targets, queues, tx, stop, routed)
                    })
                    .expect("spawn process capture"),
            );
            capture_ok.store(true, Ordering::Relaxed);
        }

        // Start the endpoint capture once; it keeps running while we route.
        // When the capture thread ends (stalled client, write failure) it
        // clears `capture_ok` and is reopened here, rate-limited so a dead
        // endpoint does not spin.
        let capture_free = capture_handle.as_ref().map_or(true, |h| h.is_finished());
        if !process_capture
            && target.right > target.left
            && capture_free
            && Instant::now() >= next_capture_try
        {
            next_capture_try = Instant::now() + Duration::from_secs(5);
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
        // Multi-process apps put the session in a helper whose own window is
        // absent, so walk up to the nearest window-owning ancestor and use
        // that window's screen — the same rule the microphone bridge uses
        // (WeChat's call audio lives in WeChatAppEx children of Weixin.exe).
        let sessions = sessions_on_all_endpoints();
        let parents = unsafe { parent_map() };
        let on_ipad = |pid: u32| -> bool {
            if tree.contains(&pid) {
                return true;
            }
            // Stop at shell hosts: their process tree contains unrelated apps
            // (every UWP app lives under ApplicationFrameHost), so walking
            // through them would route a primary-screen app to the iPad.
            let mut cur = pid;
            for _ in 0..8 {
                let Some(&parent) = parents.get(&cur) else { return false };
                if parent == 0 || parent == cur {
                    return false;
                }
                if pids.contains(&parent) {
                    return true;
                }
                if crate::mic::is_shell_process(&process_name(parent)) {
                    return false;
                }
                cur = parent;
            }
            false
        };
        let mut target_exes: HashSet<String> = sessions
            .iter()
            .filter(|s| on_ipad(s.pid))
            .map(|s| s.exe.clone())
            .collect();
        // Echo guard: an app that records from the iPad microphone keeps its
        // own sound on the PC. Sending it to the iPad would play the far
        // end's voice out of the iPad speaker into the iPad microphone.
        let guard: HashSet<String> = if shared_cable {
            crate::mic::ipad_mic_consumers().into_iter().collect()
        } else {
            HashSet::new()
        };
        for exe in &guard {
            if !last_guard.insert(exe.clone()) {
                continue;
            }
            let restored = sessions
                .iter()
                .find(|s| &s.exe == exe)
                .map(|s| unsafe { restore_persisted(s.pid, &[None, None, None]) })
                .unwrap_or(false);
            if restored {
                info!("audio: {exe} records from the iPad microphone; keeping its sound on the PC");
                if routed.remove(exe).is_some() {
                    owned.remove(exe);
                    save_routes(&owned);
                }
            } else {
                // Retry next tick: the app may not have an audio session yet.
                last_guard.remove(exe);
            }
        }
        last_guard.retain(|e| guard.contains(e));
        for exe in &guard {
            target_exes.remove(exe);
        }
        // While the iPad microphone is live, keep *all* system audio on the
        // PC. The driver's "CABLE In 16ch" loopback also carries the
        // "CABLE Input" microphone signal (two render pins of one VB-CABLE
        // device), so anything routed to the iPad would echo the user's own
        // voice back through the iPad speaker.
        let mic_live = shared_cable && crate::mic::ipad_mic_live();
        if mic_live != last_mic_live {
            last_mic_live = mic_live;
            info!(
                "audio: {}",
                if mic_live {
                    "iPad microphone is recording; keeping system audio on the PC to avoid echo"
                } else {
                    "iPad microphone released; audio routing resumed"
                }
            );
        }
        if mic_live {
            target_exes.clear();
        }
        // Note every routed app that is still on the iPad screen; the restore
        // below waits out the short session gap a device switch causes.
        let now = Instant::now();
        for (exe, entry) in routed.iter_mut() {
            if target_exes.contains(exe) {
                entry.last_seen = now;
            }
        }

        if capture_ok.load(Ordering::Relaxed) {
            for s in &sessions {
                if !on_ipad(s.pid) || s.exe.is_empty() || guard.contains(&s.exe) || mic_live {
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
                    routed.insert(s.exe.clone(), RouteEntry { pid: s.pid, prior: [None, None, None], last_seen: Instant::now() });
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
                routed.insert(s.exe.clone(), RouteEntry { pid: s.pid, prior, last_seen: Instant::now() });
                owned.insert(s.exe.clone());
                save_routes(&owned);
            }
        }

        // Restore an app once *both* signals agree it left the iPad screen:
        // no audio session judged on that screen and no window of it there.
        // Waiting for both absorbs the short session gap a device switch
        // causes, and the window check makes the iPad->PC move land within a
        // tick of the window actually leaving (that move used to take 5+s).
        let window_exes: HashSet<String> =
            pids.iter().map(|p| process_name(*p)).collect();
        let gone: Vec<String> = routed
            .iter()
            .filter(|(exe, entry)| {
                mic_live
                    || guard.contains(*exe)
                    || (!target_exes.contains(*exe)
                        && !window_exes.contains(*exe)
                        && entry.last_seen.elapsed() > restore_after)
            })
            .map(|(exe, _)| exe.clone())
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
                            poll_fast = true;
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
        if process_capture {
            // Per-process capture follows the routed sessions themselves:
            // their audio is what the iPad should hear.
            let want: Vec<u32> = sessions
                .iter()
                .filter(|s| on_ipad(s.pid) && routed.contains_key(&s.exe))
                .map(|s| s.pid)
                .collect();
            let roots = capture_roots(want, &parents);
            *proc_targets.lock().unwrap_or_else(|e| e.into_inner()) = roots;
        }

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

        // A quarter-second tick while an app is routed or an app just left
        // the iPad screen — the iPad->PC move is what the user feels — and a
        // full second only while nothing is routed at all.
        let waiting: bool = !routed.is_empty()
            && routed.iter().any(|(exe, e)| {
                !target_exes.contains(exe) && e.last_seen.elapsed() > restore_after
            });
        let fast = poll_fast || waiting || !routed.is_empty();
        poll_fast = false;
        thread::sleep(if fast {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(1000)
        });
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
    /// Last tick the app still had a session on the iPad screen. Switching an
    /// app to the sink tears its session down for a moment; restoring on that
    /// gap flipped the route back and forth every few seconds.
    last_seen: Instant,
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

unsafe fn cleanup_leftovers(current_pnp: Option<&str>, owned: &mut HashSet<String>) {
    // Anything still parked on a virtual sink after a crash or a forced
    // restart would play into the void; put it back on the default output.
    // Apps this tool routed are known by name even when the sink itself has
    // disappeared (a removed virtual device), so `owned` alone is enough.
    // The stream re-routes whatever actually has a window on the iPad screen.
    for s in sessions_on_all_endpoints() {
        if s.pid == 0 || s.exe.is_empty() {
            continue;
        }
        let prior = unsafe { read_persisted(s.pid) };
        let parked = current_pnp
            .map(|p| prior.iter().flatten().any(|q| q == p))
            .unwrap_or(false);
        if owned.contains(&s.exe) || parked {
            if unsafe { restore_persisted(s.pid, &[None, None, None]) } {
                info!("audio: restored a leftover route for {} (pid {})", s.exe, s.pid);
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

fn capture_devices() -> Vec<IMMDevice> {
    unsafe {
        let Some(en) = enumerator() else {
            return Vec::new();
        };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
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

/// The sink this tool last used, remembered next to the route file so a
/// manual launch without `OD_AUDIO_DEVICE` keeps working (the scheduled
/// task's environment is not always the one the app starts with).
fn sink_memo_path() -> Option<std::path::PathBuf> {
    Some(routes_path()?.with_file_name("audio_sink.txt"))
}

pub fn remembered_sink() -> Option<String> {
    let text = std::fs::read_to_string(sink_memo_path()?).ok()?;
    let name = text.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn remember_sink(name: &str) {
    let Some(path) = sink_memo_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, name.trim());
}

/// The tray menu's "Automatic": a remembered sink would otherwise keep the
/// automatic mode pinned to the endpoint last used.
pub fn forget_sink_memo() {
    if let Some(path) = sink_memo_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether the sink wanted by the menu or `OD_AUDIO_DEVICE` (as opposed to the
/// remembered or automatic one) is present right now.
fn preferred_sink_present() -> bool {
    let exact = sink_preference();
    let Some(wanted) = exact.clone().or_else(|| std::env::var("OD_AUDIO_DEVICE").ok()).filter(|s| !s.trim().is_empty()) else {
        return true;
    };
    let mic_sub = std::env::var("OD_VIRTUAL_MIC")
        .unwrap_or_else(|_| "CABLE Input".into())
        .to_lowercase();
    for d in render_devices() {
        let full = friendly_name(&d);
        let name = full.to_lowercase();
        if name.contains(&mic_sub) {
            continue;
        }
        let hit = if exact.is_some() {
            full.eq_ignore_ascii_case(&wanted)
        } else {
            name.contains(&wanted.to_lowercase())
        };
        if hit {
            return true;
        }
    }
    false
}

/// Hand the apps this router moved back to their own endpoints before another
/// endpoint takes over: the next run routes them onto the new sink again, so
/// they only need to leave the old one. A route that cannot be restored yet
/// stays on file (with a live pid when one exists), like the shutdown path.
fn restore_for_sink_switch(routed: &mut HashMap<String, RouteEntry>, owned: &mut HashSet<String>) {
    let current: HashMap<String, u32> =
        unsafe { sessions_on_all_endpoints() }.into_iter().map(|s| (s.exe, s.pid)).collect();
    let mut leftovers = HashSet::new();
    for (exe, entry) in routed.drain() {
        let pid = current.get(&exe).copied().unwrap_or(entry.pid);
        if unsafe { restore_persisted(pid, &entry.prior) } {
            info!("audio: restored {exe} for the new sink");
        } else {
            debug!("audio: could not restore {exe} for the new sink; kept for the next start");
            leftovers.insert(exe.clone());
        }
    }
    owned.retain(|exe| leftovers.contains(exe));
    save_routes(owned);
}

/// The sink the user picked in the tray menu; `None` = automatic. Held here so
/// the audio thread (and the CLI) uses the choice without any extra plumbing.
static SINK_PREFERENCE: Mutex<Option<String>> = Mutex::new(None);
/// Bumped when the choice changes: the running router compares it every tick
/// and restarts onto the new endpoint (routes it owns are restored first).
static SINK_EPOCH: AtomicUsize = AtomicUsize::new(0);

pub fn set_sink_preference(name: Option<String>) {
    let name = name.filter(|s| !s.trim().is_empty());
    if let Ok(mut g) = SINK_PREFERENCE.lock() {
        if *g != name {
            *g = name;
            SINK_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn sink_epoch() -> usize {
    SINK_EPOCH.load(Ordering::Relaxed)
}

fn sink_preference() -> Option<String> {
    SINK_PREFERENCE.lock().ok().and_then(|g| g.clone())
}

/// Render endpoints the tray menu offers as the iPad's playback sink. The
/// microphone cable, the current default output (audio on the primary screen
/// must stay there by itself) and the endpoints `find_virtual_device` already
/// refuses are left out.
/// `chosen` is the endpoint saved in Settings: the current system default is
/// kept in the list while it is the saved one (otherwise the device the user
/// picked from it would disappear from its own menu).
pub fn render_endpoint_choices(chosen: Option<&str>) -> Vec<String> {
    // Called from the tray menu (the UI thread, which does not use COM yet).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let default_name = unsafe {
        enumerator()
            .and_then(|en| en.GetDefaultAudioEndpoint(eRender, eConsole).ok())
            .map(|d| friendly_name(&d))
    };
    let mic_sub = std::env::var("OD_VIRTUAL_MIC")
        .unwrap_or_else(|_| "CABLE Input".into())
        .to_lowercase();
    let mut out = Vec::new();
    for d in render_devices() {
        let name = friendly_name(&d);
        let lower = name.to_lowercase();
        if name.is_empty()
            || lower.contains(&mic_sub)
            || lower.contains("microphone")
            || lower.contains("fxsound")
        {
            continue;
        }
        let is_default = default_name
            .as_ref()
            .map(|dn| dn.eq_ignore_ascii_case(&name))
            .unwrap_or(false);
        let is_chosen = chosen
            .map(|c| c.eq_ignore_ascii_case(&name))
            .unwrap_or(false);
        if is_default && !is_chosen {
            continue;
        }
        out.push(name);
    }
    out
}

/// A monitor's HDMI/DisplayPort audio output (NVIDIA/AMD/Intel render
/// endpoints show up as "… High Definition Audio", laptops as "… Display
/// Audio"). The screen the iPad mirrors usually has no speakers on it, which
/// makes such an endpoint the natural silent second line for Automatic.
fn is_display_audio(name: &str) -> bool {
    name.contains("high definition audio") || name.contains("display audio")
}

/// Capture endpoints the tray menu offers as the PC microphone that feeds the
/// virtual microphone cable while an app on the primary screen records from
/// it. The cable's own capture twin stays out (it would feed back into itself).
pub fn capture_endpoint_choices() -> Vec<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let cable = std::env::var("OD_VIRTUAL_MIC_CAPTURE")
        .unwrap_or_else(|_| "CABLE Output".into())
        .to_lowercase();
    let mut out = Vec::new();
    for d in capture_devices() {
        let name = friendly_name(&d);
        let lower = name.to_lowercase();
        if name.is_empty() || lower.contains(&cable) {
            continue;
        }
        out.push(name);
    }
    out
}

/// Pick a virtual sink to carry iPad-screen audio. The tray menu's choice
/// (Settings::audio_sink) wins, then `OD_AUDIO_DEVICE`, then the last sink
/// this tool used (so the feature survives a launch that lacks the task's
/// environment). The two variables and the memo are name substrings; a full
/// endpoint name from the menu matches exactly. `audio_sink_absent` tells
/// whether a wanted endpoint is missing from the list right now, so the
/// router keeps retrying rather than staying silent for good.
fn find_virtual_device(override_sub: Option<&str>) -> Option<IMMDevice> {
    find_virtual_device_internal(override_sub).0
}

fn audio_sink_absent(override_sub: Option<&str>) -> bool {
    find_virtual_device_internal(override_sub).1
}

fn find_virtual_device_internal(override_sub: Option<&str>) -> (Option<IMMDevice>, bool) {
    let default_id = unsafe {
        enumerator()
            .and_then(|en| en.GetDefaultAudioEndpoint(eRender, eConsole).ok())
            .map(|d| device_id(&d))
    };
    let preferred = sink_preference();
    // Menu > environment > last used; only a menu choice is an exact name.
    let (wanted, exact) = match preferred {
        Some(p) => (Some(p), true),
        None => match override_sub.map(str::to_string).filter(|s| !s.trim().is_empty()) {
            Some(e) => (Some(e), false),
            None => (remembered_sink(), false),
        },
    };
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
        let full = friendly_name(&d);
        let name = full.to_lowercase();
        if name.contains(&mic_sub) {
            if wanted
                .as_ref()
                .map(|s| name.contains(&s.to_lowercase()))
                .unwrap_or(false)
            {
                warn!(
                    "audio: refusing the microphone cable '{}' as the audio sink",
                    full
                );
            }
            continue;
        }
        // The default output normally stays out of the routing (sound on the
        // primary screen must keep going there anyway) — but an explicitly
        // chosen endpoint is honored even when Windows has since made it the
        // default, or the menu would show a checked choice the router never
        // uses.
        let hit = wanted.as_ref().map(|sub| {
            if exact {
                full.eq_ignore_ascii_case(sub)
            } else {
                name.contains(&sub.to_lowercase())
            }
        }).unwrap_or(false);
        if Some(&id) == default_id.as_ref() && !hit {
            continue;
        }
        let score = if wanted.is_some() {
            if hit {
                1000
            } else if name.contains("streaming speakers") {
                // The chosen endpoint is absent (the monitor that carries the
                // HDMI audio endpoint slept, the app that owned another sink
                // quit). Steam's speakers are software-only and usually
                // present, so routing keeps working instead of going silent;
                // the choice is kept for when the device is back.
                450
            } else {
                -1
            }
        } else if is_display_audio(&name) {
            // Automatic: a monitor's HDMI/DP audio output (NVIDIA/AMD/Intel
            // "High Definition Audio", "Display Audio") is exactly the silent
            // second line the iPad sound travels on: no speakers are wired to
            // it, and it is independent of the microphone cable.
            500
        } else if name.contains("streaming speakers") {
            400
        } else {
            // Any other "virtual audio" endpoint belongs to some other
            // application (ToDesk, UU, Pico streaming, ...): it disappears
            // with that app and may capture the audio into its own session.
            // Dedicated sinks only, or an explicit OD_AUDIO_DEVICE.
            // Never fall back to a cable: every VB-CABLE render endpoint has a
            // capture twin, so it can silently loop audio back into a meeting.
            // OD_AUDIO_DEVICE remains the explicit escape hatch.
            -1
        };
        info!("audio: sink candidate '{name}' score={score}");
        if score < 0 || name.contains("microphone") || name.contains("fxsound") {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, d));
        }
    }
    let seen_wanted = |best: &Option<(i32, IMMDevice)>| {
        // Only a real hit counts as "the chosen endpoint is present"; a
        // fallback (Steam, 450) still leaves the wanted endpoint missing, or
        // the router would never try to move back to it.
        best.as_ref().map(|(s, _)| *s >= 1000).unwrap_or(false)
    };
    let matched = seen_wanted(&best);
    (best.map(|(_, d)| d), wanted.is_some() && !matched)
}

// --- session + process discovery ------------------------------------------

struct SessionInfo {
    pid: u32,
    exe: String,
}

pub(crate) fn process_name(pid: u32) -> String {
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

pub(crate) unsafe fn descendants(targets: &HashSet<u32>) -> HashSet<u32> {
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

/// Visible top-level windows (plus the processes of their child windows, so a
/// UWP app shows up under its own pid rather than ApplicationFrameHost's),
/// each tagged with whether its centre sits on `rect`. A pid can appear more
/// than once when it owns windows on both screens.
pub(crate) unsafe fn window_owners(rect: RECT, own_pid: u32) -> Vec<(u32, bool)> {
    struct Search {
        rect: RECT,
        own_pid: u32,
        out: Vec<(u32, bool)>,
        on: bool,
    }

    unsafe extern "system" fn child_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != 0 && pid != search.own_pid {
            search.out.push((pid, search.on));
        }
        BOOL(1)
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
            let on = !(cx < search.rect.left
                || cx >= search.rect.right
                || cy < search.rect.top
                || cy >= search.rect.bottom);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 && pid != search.own_pid {
                search.out.push((pid, on));
            }
            search.on = on;
            let _ = EnumChildWindows(Some(hwnd), Some(child_cb), lparam);
        }
        BOOL(1)
    }

    let mut search = Search {
        rect,
        own_pid,
        out: Vec::new(),
        on: false,
    };
    let _ = unsafe { EnumWindows(Some(callback), LPARAM(&mut search as *mut _ as isize)) };
    search.out
}

pub(crate) unsafe fn pids_on_rect(rect: RECT, own_pid: u32) -> HashSet<u32> {
    unsafe { window_owners(rect, own_pid) }
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(pid, _)| pid)
        .collect()
}

/// pid -> parent pid for every process.
pub(crate) unsafe fn parent_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return map;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };
    map
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
        // 0 = last report was silence (so the first sound is reported).
        let mut peak_logged: i32 = 0;
        // Peak reports at most once a second (the 250-frame counter alone is
        // useless on quiet endpoints: it never reaches the boundary).
        let mut peak_report = Instant::now();
        // When the backlog was last reported (the warning is rate-limited:
        // the condition repeats every packet while it lasts).
        let mut last_backlog = Instant::now() - Duration::from_secs(60);
        // A loopback client can be invalidated when the endpoint goes idle
        // (HDMI audio powers down while nothing renders to it, or the driver
        // resets the device). WASAPI reports that by never delivering another
        // packet: detect it and let the router reopen the capture instead of
        // staying silent for the rest of the session.
        let mut last_packet = Instant::now();

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
                last_packet = Instant::now();
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
                // Rate check: WASAPI hands out audio at its own pace, so if
                // the receiver is behind the queue grows here forever and the
                // sound (and the video's sync) lags more and more. Frames play
                // at realtime, so a sustained backlog is never wanted; drop
                // what accumulated beyond 250 ms and say so once.
                let backlog_ms = (pcm.len() / CHANNELS) as u64 * 1000 / RATE as u64;
                if backlog_ms > 250 {
                    let now = Instant::now();
                    if now.duration_since(last_backlog) >= Duration::from_secs(10) {
                        last_backlog = now;
                        warn!(
                            "audio: capture backlog {backlog_ms} ms on '{}'; dropping the excess to stay in sync",
                            friendly_name(&device)
                        );
                    }
                    pcm.drain(..pcm.len() - FRAME_I16 * 2);
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
                    if peak_report.elapsed() >= Duration::from_secs(1) {
                        peak_report = Instant::now();
                        let peak = chunk.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
                        // Report silence<->sound transitions (a static line is
                        // normal): "frames" keeps counting while quiet, so a
                        // stream stuck in silence is visible as `peak 0` and
                        // sound coming back is visible too. Logging every
                        // varying peak of music would flood the file.
                        let silent = peak == 0;
                        if silent != (peak_logged == 0) {
                            peak_logged = if silent { 0 } else { peak };
                            info!("audio: sent {frames_sent} frames, peak {peak}");
                        } else if !silent {
                            peak_logged = 1; // remember "was sound", not the value
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
                        ok.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }
            // Loopback keeps delivering silence while anything renders, so a
            // full minute with no packet means the client is dead.
            if last_packet.elapsed() > Duration::from_secs(60) {
                warn!("audio: loopback capture stalled for 60 s; reopening");
                break;
            }
        }

        let _ = client.Stop();
        let _ = CloseHandle(event);
        CoTaskMemFree(Some(mix as *const _));
        // The router only spawns the capture once; clear the flag so the
        // next tick opens a fresh client.
        ok.store(false, Ordering::Relaxed);
    }
}

// --- per-process loopback capture (independent sinks) ----------------------

/// The audio a process tree renders, captured by WASAPI process loopback
/// (Windows 10 2004+). It works with a muted endpoint (capture runs before
/// the endpoint's volume/mute), which is how a monitor's audio output becomes
/// a silent, independent playback line — and it never contains the virtual
/// microphone, which a device loopback on the VB-CABLE does.
type MixQueues = Arc<Mutex<HashMap<u32, Arc<Mutex<VecDeque<i16>>>>>>;

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        unsafe {
            let _ = SetEvent(self.0);
        }
        Ok(())
    }
}

fn activate_process_loopback(pid: u32) -> anyhow::Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let mut prop = PROPVARIANT::default();
        {
            let inner: &mut PROPVARIANT_0_0 = &mut *prop.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous = PROPVARIANT_0_0_0 {
                blob: BLOB {
                    cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                    pBlobData: &mut params as *mut _ as *mut u8,
                },
            };
        }
        let event = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(event).into();
        let operation = ActivateAudioInterfaceAsync(
            &HSTRING::from("VAD\\Process_Loopback"),
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )?;
        if WaitForSingleObject(event, 5000) != WAIT_OBJECT_0 {
            let _ = CloseHandle(event);
            anyhow::bail!("process loopback activation timed out");
        }
        let mut hr = HRESULT(0);
        let mut iface: Option<windows::core::IUnknown> = None;
        operation.GetActivateResult(&mut hr, &mut iface)?;
        let _ = CloseHandle(event);
        if hr.is_err() {
            anyhow::bail!("GetActivateResult: {hr:?}");
        }
        let client: IAudioClient = iface
            .ok_or_else(|| anyhow::anyhow!("no audio client"))?
            .cast()?;
        Ok(client)
    }
}

/// Capture one process tree into `queue` (48 kHz stereo s16) until `stop`.
fn capture_process(
    pid: u32,
    queue: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let client = activate_process_loopback(pid)?;
    // Process loopback requires an explicit format: 48 kHz, 32-bit float,
    // stereo.
    let fmt = WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: 0xFFFE,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: 3,
        SubFormat: GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71), // IEEE float
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            2_000_000,
            0,
            &fmt as *const _ as *const WAVEFORMATEX,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        info!("audio: capturing pid {pid} (process loopback)");
        while !stop.load(Ordering::Relaxed) && !cancelled.load(Ordering::Relaxed) {
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
                if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let n = frames as usize * 2;
                    let samples = std::slice::from_raw_parts(data as *const f32, n);
                    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                    for s in samples {
                        q.push_back((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                    // One second bound: drop the oldest audio on a burst.
                    let cap = RATE as usize * 2;
                    while q.len() > cap {
                        q.pop_front();
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}

/// Keep one capture thread per routed audio-session pid and mix their audio
/// into the ODAU stream the receiver plays.
fn process_capture_manager(
    targets: Arc<Mutex<Vec<u32>>>,
    queues: MixQueues,
    tx: Sender<(Vec<u8>, bool)>,
    stop: Arc<AtomicBool>,
    routed_count: Arc<AtomicUsize>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    {
        let queues = queues.clone();
        let tx = tx.clone();
        let stop = stop.clone();
        let routed_count = routed_count.clone();
        thread::Builder::new()
            .name("audio-mix".into())
            .spawn(move || {
                let mut frames_sent: u64 = 0;
                let mut peak_logged: i32 = -1;
                while !stop.load(Ordering::Relaxed) {
                    if routed_count.load(Ordering::Relaxed) == 0
                        && std::env::var_os("OD_AUDIO_ALWAYS").is_none()
                    {
                        for q in queues.lock().unwrap().values() {
                            q.lock().unwrap().clear();
                        }
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let mut mixed = vec![0i32; FRAME_I16];
                    let mut any = false;
                    for q in queues.lock().unwrap().values() {
                        let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
                        let n = q.len().min(FRAME_I16);
                        if n == 0 {
                            continue;
                        }
                        any = true;
                        for (i, s) in q.drain(..n).enumerate() {
                            mixed[i] += s as i32;
                        }
                    }
                    if !any {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    let chunk: Vec<i16> = mixed
                        .into_iter()
                        .map(|v| v.clamp(-32768, 32767) as i16)
                        .collect();
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
                    let framed = opendisplay_proto::encode_frame(&payload);
                    if tx.send((framed, false)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn audio mixer");
    }
    let mut stops: HashMap<u32, Arc<AtomicBool>> = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        let want = targets.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let gone: Vec<u32> = stops
            .keys()
            .copied()
            .filter(|p| !want.contains(p))
            .collect();
        for pid in gone {
            if let Some(flag) = stops.remove(&pid) {
                flag.store(true, Ordering::Relaxed);
            }
            queues.lock().unwrap().remove(&pid);
        }
        for pid in want {
            if stops.contains_key(&pid) {
                continue;
            }
            let flag = Arc::new(AtomicBool::new(false));
            stops.insert(pid, flag.clone());
            let queue = Arc::new(Mutex::new(VecDeque::<i16>::new()));
            queues.lock().unwrap().insert(pid, queue.clone());
            let stop_session = stop.clone();
            let stop_pid = flag.clone();
            let _ = thread::Builder::new()
                .name(format!("audio-p{pid}"))
                .spawn(move || {
                    if let Err(e) = capture_process(pid, queue, stop_session, stop_pid) {
                        warn!("audio: process capture pid {pid}: {e:#}");
                    }
                });
        }
        thread::sleep(Duration::from_millis(250));
    }
    for (_, flag) in stops.drain() {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Keep only the pids whose ancestors are not captured too: a child's audio
/// is already inside its parent's process tree.
fn capture_roots(mut pids: Vec<u32>, parents: &HashMap<u32, u32>) -> Vec<u32> {
    pids.sort_unstable();
    pids.dedup();
    let set: HashSet<u32> = pids.iter().copied().collect();
    pids.retain(|pid| {
        let mut cur = *pid;
        for _ in 0..12 {
            let Some(&parent) = parents.get(&cur) else { break };
            if parent == cur {
                break;
            }
            if set.contains(&parent) {
                return false;
            }
            cur = parent;
        }
        true
    });
    pids
}

/// Mute the playback sink on this PC. Process loopback captures before the
/// endpoint's mute/volume, so the iPad still gets the audio.
unsafe fn mute_sink(device: &IMMDevice) {
    match device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
        Ok(v) => {
            let _ = v.SetMute(true, std::ptr::null());
            info!("audio: sink muted on this PC (process capture is pre-mute)");
        }
        Err(e) => warn!("audio: muting the sink: {e}"),
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
