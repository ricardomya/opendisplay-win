//! The iPad microphone as a Windows microphone, following the screen.
//!
//! Windows offers no per-app *recording* endpoint routing (the AudioPolicyConfig
//! call rejects the capture flow), so the app-side approach is a virtual
//! microphone cable: the meeting app selects "麦克风 (CABLE Output)" once,
//! and this module decides what that cable carries:
//!
//! * a recording app whose window is on the iPad screen → the iPad microphone
//!   (`ODMI` PCM frames from the receiver);
//! * otherwise → the PC's default microphone, so the same app keeps behaving
//!   like a normal PC microphone when its window is back on the primary screen.
//!
//! VB-CABLE loops render→capture by design. `OD_VIRTUAL_MIC` overrides the
//! render device-name substring (default "CABLE Input").

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, RECT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};


const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// 10 ms of stereo s16.
const CHUNK: usize = (RATE as usize / 100) * CHANNELS;
const MAX_BUFFER: usize = RATE as usize * CHANNELS; // 1 s
const IPAD_FRESH: Duration = Duration::from_millis(800);

/// Manual override set by the user's hotkey: which screen the "active" app
/// was moved to. `None` = automatic (screen detection); `Some(true)` = treat
/// the active app as on the iPad screen; `Some(false)` = on the primary.
static MANUAL_MIC: Mutex<Option<bool>> = Mutex::new(None);
/// When the hotkey last pinned the microphone, so the move it causes itself
/// does not immediately clear the pin.
static MANUAL_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Apps recording from the virtual microphone **while its source is the
/// iPad**. Their own playback must stay on the PC: sending it to the iPad
/// would play the far end's voice back into the iPad's microphone (echo).
static IPAD_MIC_CONSUMERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether the iPad is the live microphone source right now. While it is,
/// the audio router keeps *all* system audio on the PC: this machine's
/// "CABLE In 16ch" loopback also carries the "CABLE Input" mic signal (both
/// are render pins of one VB-CABLE device), so anything sent to the iPad
/// would echo the user's own voice back to it.
static IPAD_MIC_LIVE: AtomicBool = AtomicBool::new(false);

pub fn ipad_mic_live() -> bool {
    IPAD_MIC_LIVE.load(Ordering::Relaxed)
}

/// The device Windows shows in parentheses after an endpoint's name
/// ("CABLE Input (VB-Audio Virtual Cable)" -> "vb-audio virtual cable").
/// Endpoints on the same device share it; two separate cables do not.
fn device_key(friendly: &str) -> String {
    let name = friendly.trim();
    if name.ends_with(')') {
        if let Some(i) = name.rfind('(') {
            return name[i + 1..name.len() - 1].trim().to_lowercase();
        }
    }
    name.to_lowercase()
}

/// Whether the playback sink lives on the same device as the microphone
/// cable. On this VB-CABLE driver a sibling render pin's loopback carries the
/// "CABLE Input" microphone signal (measured), so a shared sink needs the
/// echo guards. Unknown endpoints take the safe path (treated as shared).
pub fn sink_shares_mic_cable(sink_friendly: &str) -> bool {
    let wanted = std::env::var("OD_VIRTUAL_MIC")
        .unwrap_or_else(|_| "CABLE Input".into())
        .to_lowercase();
    unsafe {
        let Some(en) = enumerator() else { return true };
        let Ok(col) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return true;
        };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let name = friendly(&dev).to_lowercase();
            if name.contains(&wanted) {
                return device_key(&name) == device_key(sink_friendly);
            }
        }
    }
    true
}

/// Exe names currently recording from the iPad microphone (empty when the
/// microphone source is the PC or nobody is recording). Read by the audio
/// router to keep those apps' playback off the iPad.
pub fn ipad_mic_consumers() -> Vec<String> {
    IPAD_MIC_CONSUMERS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// The hotkey moved the active window: pin the microphone to that screen.
pub fn set_manual_mic(on_ipad: bool) {
    info!("mic: manual override -> {}", if on_ipad { "iPad" } else { "PC" });
    if let Ok(mut g) = MANUAL_MIC.lock() {
        *g = Some(on_ipad);
    }
    if let Ok(mut t) = MANUAL_AT.lock() {
        *t = Some(Instant::now());
    }
}

/// A window move that was not the hotkey (the user dragged, the app moved
/// itself) clears the override so automatic detection resumes.
pub fn clear_manual_mic() {
    if let Ok(mut g) = MANUAL_MIC.lock() {
        if g.is_some() {
            *g = None;
            info!("mic: manual override cleared");
        }
    }
}

/// Clear a pin left behind by an earlier hotkey once the hotkey's own move is
/// over. Called on every window crossing the screens; without it `Some(true)`
/// from one hotkey press kept the iPad microphone pinned for the whole
/// session even after the user dragged the app back to the primary screen.
pub fn clear_manual_mic_if_stale() {
    let stale = MANUAL_AT
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed() > Duration::from_secs(3))
        .unwrap_or(false);
    if stale {
        clear_manual_mic();
        if let Ok(mut t) = MANUAL_AT.lock() {
            *t = None;
        }
    }
}

pub struct MicSession {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    ipad: Arc<Mutex<VecDeque<i16>>>,
    last_ipad: Arc<Mutex<Option<Instant>>>,
    frames: Arc<AtomicUsize>,
}

impl MicSession {
    /// Feed one 20 ms iPad microphone frame: 48 kHz mono s16, as sent on the
    /// wire. Converts to the internal stereo buffer.
    pub fn push_ipad_mono(&self, pcm: &[i16]) {
        let mut buf = self.ipad.lock().unwrap_or_else(|e| e.into_inner());
        for &s in pcm {
            buf.push_back(s);
            buf.push_back(s);
        }
        while buf.len() > MAX_BUFFER {
            buf.pop_front();
        }
        drop(buf);
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut t) = self.last_ipad.lock() {
            *t = Some(Instant::now());
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MicSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start the virtual-microphone bridge. None when disabled or the cable is
/// missing. `notify` tells the receiver whether it should capture this
/// device's microphone.
pub fn start(
    rect: Arc<Mutex<RECT>>,
    notify: Box<dyn Fn(bool) + Send>,
) -> Option<MicSession> {
    if std::env::var_os("OD_NO_MIC").is_some() {
        info!("mic: disabled by OD_NO_MIC");
        return None;
    }
    let name = std::env::var("OD_VIRTUAL_MIC").unwrap_or_else(|_| "CABLE Input".into());

    let stop = Arc::new(AtomicBool::new(false));
    let ipad = Arc::new(Mutex::new(VecDeque::<i16>::new()));
    let last_ipad = Arc::new(Mutex::new(None::<Instant>));
    let use_ipad = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicUsize::new(0));

    let handle = {
        let stop = stop.clone();
        let ipad = ipad.clone();
        let last_ipad = last_ipad.clone();
        let use_ipad = use_ipad.clone();
        let frames = frames.clone();
        thread::Builder::new()
            .name("mic".into())
            .spawn(move || run(name, rect, stop, ipad, last_ipad, use_ipad, frames, notify))
            .expect("spawn mic thread")
    };

    Some(MicSession {
        stop,
        handle: Some(handle),
        ipad,
        last_ipad,
        frames,
    })
}

fn run(
    render_name: String,
    rect: Arc<Mutex<RECT>>,
    stop: Arc<AtomicBool>,
    ipad: Arc<Mutex<VecDeque<i16>>>,
    last_ipad: Arc<Mutex<Option<Instant>>>,
    use_ipad: Arc<AtomicBool>,
    frames: Arc<AtomicUsize>,
    notify: Box<dyn Fn(bool) + Send>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(render_device) = find_render(&render_name) else {
        warn!("mic: virtual microphone disappeared");
        return;
    };

    // PC microphone → one-slot-per-chunk stream. The capture device is held
    // open only while an app on the primary screen records through the cable,
    // so a meeting on the iPad never keeps the PC microphone (and its LED)
    // alive.
    let pc_enabled = Arc::new(AtomicBool::new(false));
    let (pc_tx, pc_rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(16);
    {
        let stop = stop.clone();
        let enabled = pc_enabled.clone();
        thread::Builder::new()
            .name("mic-pc".into())
            .spawn(move || capture_pc(stop, enabled, pc_tx))
            .ok();
    }

    // Window/session selector: decides who owns the cable and tells the
    // receiver when to capture.
    {
        let stop = stop.clone();
        let rect = rect.clone();
        let use_ipad = use_ipad.clone();
        let last_ipad = last_ipad.clone();
        let frames = frames.clone();
        let pc_enabled = pc_enabled.clone();
        thread::Builder::new()
            .name("mic-sel".into())
            .spawn(move || {
                select_source(stop, rect, use_ipad, last_ipad, frames, pc_enabled, notify)
            })
            .ok();
    }

    render_loop(render_device, stop, ipad, last_ipad, use_ipad, pc_rx);
    debug!("mic: stopped");
}

// --- source selection -----------------------------------------------------

/// Shell hosts whose process tree contains unrelated apps. Expanding from
/// them would make every app on the desktop look like it is on the iPad
/// screen.
pub(crate) fn is_shell_process(name: &str) -> bool {
    matches!(
        name,
        "explorer.exe"
            | "applicationframehost.exe"
            | "shellexperiencehost.exe"
            | "startmenuexperiencehost.exe"
            | "searchhost.exe"
    )
}

/// Follow a recording process up its parent chain until it reaches a process
/// that owns a visible window, and report whether that window is on the iPad
/// screen. Stops (false) at shell hosts, which parent unrelated apps.
fn consumer_on_ipad(
    pid: u32,
    on_ipad: &HashSet<u32>,
    off_ipad: &HashSet<u32>,
    parents: &HashMap<u32, u32>,
    name_of: impl Fn(u32) -> String,
) -> bool {
    let mut cur = pid;
    for _ in 0..8 {
        let name = name_of(cur).to_ascii_lowercase();
        if is_shell_process(&name) {
            return false;
        }
        if on_ipad.contains(&cur) {
            return true;
        }
        if off_ipad.contains(&cur) {
            return false;
        }
        match parents.get(&cur) {
            Some(&p) if p != 0 && p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

fn select_source(
    stop: Arc<AtomicBool>,
    rect: Arc<Mutex<RECT>>,
    use_ipad: Arc<AtomicBool>,
    last_ipad: Arc<Mutex<Option<Instant>>>,
    frames: Arc<AtomicUsize>,
    pc_enabled: Arc<AtomicBool>,
    notify: Box<dyn Fn(bool) + Send>,
) {
    let own_pid = std::process::id();
    let mut last_mode: Option<&'static str> = None;
    let mut last_notified = false;
    let mut last_report = Instant::now() - Duration::from_secs(3600);
    let mut last_target = RECT::default();
    let mut last_on_ipad = usize::MAX;
    let mut last_consumers: HashSet<u32> = HashSet::new();
    // Start silent: a receiver left capturing from an older session must stop.
    notify(false);
    while !stop.load(Ordering::Relaxed) {
        let target = {
            let guard = rect.lock().unwrap_or_else(|e| e.into_inner());
            *guard
        };
        if target != last_target {
            info!(
                "mic: target rect ({}, {})-({}, {})",
                target.left, target.top, target.right, target.bottom
            );
            last_target = target;
        }
        let owners = if target.right > target.left {
            unsafe { crate::audio::window_owners(target, own_pid) }
        } else {
            Vec::new()
        };
        // A pid can own windows on both screens; "on the iPad" wins there,
        // which is what the old exact-pid rule did.
        let mut on_ipad: HashSet<u32> = HashSet::new();
        let mut off_ipad: HashSet<u32> = HashSet::new();
        for (pid, on) in &owners {
            if *on {
                on_ipad.insert(*pid);
            } else {
                off_ipad.insert(*pid);
            }
        }
        if on_ipad.len() != last_on_ipad {
            let mut names: Vec<String> = on_ipad
                .iter()
                .map(|p| format!("{}:{p}", crate::audio::process_name(*p)))
                .collect();
            names.sort();
            info!(
                "mic: windows on the iPad screen: {} ({})",
                on_ipad.len(),
                names.join(", ")
            );
            last_on_ipad = on_ipad.len();
        }
        let parents = unsafe { crate::audio::parent_map() };
        // Which screen an app is on is a property of its visible window, not
        // of the process that happens to hold the capture session: WeChat
        // records from renderer / utility children that own no window, so walk
        // up the parent chain to the nearest window owner and use that
        // window's screen. Matching whole process trees or executable names
        // instead would hand the iPad microphone to a primary-screen call of
        // the same app (or of a shared runtime such as msedgewebview2.exe).
        // Only sessions on the virtual cable matter: an app recording from the
        // real microphone has nothing to do with this bridge.
        let consumers = cable_session_pids();
        let matches_ipad = |pid: u32| {
            consumer_on_ipad(pid, &on_ipad, &off_ipad, &parents, crate::audio::process_name)
        };
        let ipad_recordings = consumers.iter().filter(|p| matches_ipad(**p)).count();
        if consumers != last_consumers {
            let mut lines: Vec<String> = consumers
                .iter()
                .map(|p| {
                    format!(
                        "{}:{p} on_ipad={}",
                        crate::audio::process_name(*p),
                        matches_ipad(*p)
                    )
                })
                .collect();
            lines.sort();
            info!("mic: consumers changed: [{}]", lines.join(", "));
            last_consumers = consumers.clone();
        }
        let ipad_app = ipad_recordings > 0;
        let fresh = last_ipad
            .lock()
            .ok()
            .and_then(|t| *t)
            .map(|t| t.elapsed() < IPAD_FRESH)
            .unwrap_or(false);
        let manual = MANUAL_MIC.lock().ok().and_then(|g| *g);
        let decision = decide_mic(manual, !consumers.is_empty(), ipad_app, fresh);
        let want_ipad = decision.want_ipad;
        use_ipad.store(decision.want_ipad, Ordering::Relaxed);
        pc_enabled.store(decision.want_pc, Ordering::Relaxed);

        // Capture on the iPad only while an app actually consumes the virtual
        // mic (or a hotkey pin asks the iPad to serve a live consumer): no
        // green mic indicator and no battery use while the meeting runs on
        // the primary screen or nobody is recording at all. The edge tracks
        // the decision itself, so going idle *sends* the disable instead of
        // leaving the receiver's engine running.
        let want_capture = decision.want_capture;
        if want_capture != last_notified {
            notify(want_capture);
            info!("mic: iPad capture {}", if want_capture { "on" } else { "off" });
            last_notified = want_capture;
        }
        // The iPad engine stops when its last consumer closes (meeting ends)
        // and gives up reviving after a few failed attempts. If a consumer is
        // back and we want the iPad mic but no frames arrive, re-send the
        // enable flag every few seconds: on iOS it replays `setMicEnabled`
        // and rebuilds the engine. Without this the iPad mic stays dead until
        // the app is reopened.
        // `want_ipad` already demands fresh frames, so gating the re-send on it
        // would never fire in the automatic case that actually breaks: a
        // meeting on the iPad screen ends, the receiver's engine drops to
        // playback, and the next consumer arrives with ipad_frames stale. The
        // rising-edge notify above is the only ask, and if the receiver was
        // mid-teardown it can miss it — keep asking every 2 s while the iPad
        // is supposed to capture and has no microphone behind it. `want_capture`
        // keeps this from waking the receiver when nobody records (or a PC
        // pin released it).
        if want_capture && !fresh && last_report.elapsed() > Duration::from_secs(2) {
            notify(true);
            info!("mic: no iPad frames; re-sent micConfig enable");
            last_report = Instant::now();
        }

        let mode = if consumers.is_empty() {
            "idle"
        } else if want_ipad {
            "iPad"
        } else {
            "PC"
        };
        IPAD_MIC_LIVE.store(mode == "iPad", Ordering::Relaxed);
        if last_mode != Some(mode) {
            info!(
                "mic: source -> {mode} ({} app(s) on the virtual mic, {} on the iPad screen)",
                consumers.len(),
                ipad_recordings
            );
            last_mode = Some(mode);
        }
        // Publish the echo-guard set while the iPad is the microphone: the
        // audio router keeps these apps' own sound on the PC, so the far
        // end's voice never plays out of the iPad speaker next to the iPad
        // microphone (manual speakerphone use included).
        let mut guarded: Vec<String> = if mode == "iPad" {
            consumers
                .iter()
                .filter(|p| matches_ipad(**p))
                .map(|p| crate::audio::process_name(*p))
                .filter(|n| !n.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        guarded.sort();
        guarded.dedup();
        if let Ok(mut g) = IPAD_MIC_CONSUMERS.lock() {
            *g = guarded;
        }
        if last_report.elapsed() > Duration::from_secs(30) && !consumers.is_empty() {
            let age = last_ipad
                .lock()
                .ok()
                .and_then(|t| *t)
                .map(|t| t.elapsed().as_millis());
            info!(
                "mic: debug ipad_frames={} age_ms={:?} consumers={:?}",
                frames.load(Ordering::Relaxed),
                age,
                consumers
            );
            last_report = Instant::now();
        }
        thread::sleep(Duration::from_millis(1000));
    }
    notify(false);
}

/// PIDs with an active capture session on the virtual microphone's capture
/// endpoint (`CABLE Output`) — the apps actually reading this microphone.
fn cable_session_pids() -> HashSet<u32> {
    let wanted = std::env::var("OD_VIRTUAL_MIC_CAPTURE")
        .unwrap_or_else(|_| "CABLE Output".into())
        .to_lowercase();
    let mut out = HashSet::new();
    unsafe {
        let Some(en) = enumerator() else { return out };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(device) = col.Item(i) else { continue };
            if !friendly(&device).to_lowercase().contains(&wanted) {
                continue;
            }
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(ctrl) = sessions.GetSession(j) else { continue };
                let Ok(ctrl2) = ctrl.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let state = ctrl.GetState().map(|s| s.0).unwrap_or(0);
                let pid = ctrl2.GetProcessId().unwrap_or(0);
                if pid != 0 {
                    // Diagnostic: shows which apps hold the virtual microphone.
                    info!("mic: cable session pid={pid} state={state}");
                    if state == 1 {
                        out.insert(pid);
                    }
                }
            }
            return out;
        }
    }
    out
}

// --- PC microphone capture ------------------------------------------------

fn capture_pc(
    stop: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    tx: std::sync::mpsc::SyncSender<Vec<i16>>,
) {
    // Open the PC microphone only while the bridge actually uses it. Closing
    // the WASAPI client releases the device, so its recording LED goes off
    // whenever a meeting on the iPad owns the virtual microphone.
    while !stop.load(Ordering::Relaxed) {
        if !enabled.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        capture_pc_once(&stop, &enabled, &tx);
        thread::sleep(Duration::from_millis(300));
    }
}

/// The PC microphone the user picked in the tray menu; `None` = automatic.
static PC_MIC_PREFERENCE: Mutex<Option<String>> = Mutex::new(None);
/// Bumped when the preference changes, so an open capture reopens with the
/// newly picked device instead of keeping the old one until it is disabled.
static PC_MIC_EPOCH: AtomicUsize = AtomicUsize::new(0);

pub fn set_pc_mic_preference(name: Option<String>) {
    let name = name.filter(|s| !s.trim().is_empty());
    if let Ok(mut g) = PC_MIC_PREFERENCE.lock() {
        if *g != name {
            *g = name;
            PC_MIC_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn pc_mic_preference() -> Option<String> {
    PC_MIC_PREFERENCE.lock().ok().and_then(|g| g.clone())
}

fn pc_mic_memo_path() -> Option<std::path::PathBuf> {
    Some(routes_dir()?.join("pc_mic.txt"))
}

/// Whether the memo was written for a deliberate choice (menu or OD_PC_MIC)
/// rather than the fallback to the system default: "Automatic" must be able
/// to follow a changed Windows default again, so a default pick clears it.
fn pc_mic_memo_marker_path() -> Option<std::path::PathBuf> {
    Some(routes_dir()?.join("pc_mic.choice"))
}

fn routes_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(base).join("OpenDisplay"))
}

fn remembered_pc_mic() -> Option<String> {
    // A memo written for a plain default pick does not count: it would pin
    // the old default after the user changes one in Windows.
    pc_mic_memo_marker_path().filter(|p| p.exists())?;
    let text = std::fs::read_to_string(pc_mic_memo_path()?).ok()?;
    let name = text.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn remember_pc_mic(name: &str) {
    let Some(path) = pc_mic_memo_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, name.trim());
    if let Some(marker) = pc_mic_memo_marker_path() {
        let _ = std::fs::write(marker, "");
    }
}

/// The tray menu's "Automatic": drop the memo as well, or the remembered
/// choice would keep Automatic pinned to it forever.
pub fn forget_pc_mic_memo() {
    if let Some(path) = pc_mic_memo_path() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(marker) = pc_mic_memo_marker_path() {
        let _ = std::fs::remove_file(marker);
    }
}

/// The real microphone the bridge plays into the cable while a consumer on
/// the primary screen records. The tray menu's choice wins, then `OD_PC_MIC`
/// (name substring); otherwise the system default is used unless it is the
/// cable itself, in which case the first real capture device is taken. The
/// cable becomes the system default when an app cannot choose its microphone
/// (WeChat) and still needs routing.
fn pick_pc_microphone(en: &IMMDeviceEnumerator, cable: &str) -> Option<(IMMDevice, bool)> {
    unsafe {
        // A menu choice outranks everything and is never "just the default":
        // even when it currently is the default, its `explicit` flag keeps
        // the memo. OD_PC_MIC and the memo fall through to the default rule
        // below when they are missing — or when the memo merely recorded the
        // default (the memo lives next to a marker written only for real
        // choices; a plain follow-the-default pick does not freeze it).
        if let Some(preferred) = pc_mic_preference() {
            let want = preferred.to_lowercase();
            if !want.is_empty() {
                if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
                    for i in 0..col.GetCount().unwrap_or(0) {
                        let Ok(d) = col.Item(i) else { continue };
                        let name = friendly(&d).to_lowercase();
                        // Never the cable: that would feed the microphone bus
                        // back into itself.
                        if !name.is_empty() && !name.contains(cable) && name.contains(&want) {
                            return Some((d, true));
                        }
                    }
                }
            }
        }
        if let Some(want) = std::env::var("OD_PC_MIC").ok().or_else(remembered_pc_mic) {
            let want = want.to_lowercase();
            if !want.is_empty() {
                if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
                    for i in 0..col.GetCount().unwrap_or(0) {
                        let Ok(d) = col.Item(i) else { continue };
                        let name = friendly(&d).to_lowercase();
                        if !name.is_empty() && !name.contains(cable) && name.contains(&want) {
                            // The environment and the memo are fallbacks: they
                            // do not override sending the menu back to
                            // "Automatic" (the memo is only refreshed for a
                            // real menu choice or OD_PC_MIC).
                            return Some((d, std::env::var_os("OD_PC_MIC").is_some()));
                        }
                    }
                }
            }
        }
        if let Ok(d) = en.GetDefaultAudioEndpoint(eCapture, eConsole) {
            if !friendly(&d).to_lowercase().contains(cable) {
                return Some((d, false));
            }
        }
        let col = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(d) = col.Item(i) else { continue };
            let name = friendly(&d).to_lowercase();
            if !name.is_empty() && !name.contains(cable) {
                return Some((d, false));
            }
        }
        None
    }
}

fn capture_pc_once(
    stop: &AtomicBool,
    enabled: &AtomicBool,
    tx: &std::sync::mpsc::SyncSender<Vec<i16>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Some(en) = enumerator() else { return };
        // Safety: the virtual cable must never be captured as the PC
        // microphone — that would feed the cable back into itself.
        let cable = std::env::var("OD_VIRTUAL_MIC_CAPTURE")
            .unwrap_or_else(|_| "CABLE Output".into())
            .to_lowercase();
        // Read the generation *before* picking: a choice made while the client
        // is being opened must still be seen by the loop below, or the capture
        // stays on the previous device.
        let epoch = PC_MIC_EPOCH.load(Ordering::Relaxed);
        let Some((device, explicit)) = pick_pc_microphone(&en, &cable) else {
            warn!("mic: no real microphone to capture");
            return;
        };
        let mic_name = friendly(&device);
        // Remember a deliberate choice (menu or OD_PC_MIC), so a launch that
        // lacks the task's environment picks the same device. A plain
        // system-default pick clears that memo: the default may change.
        if explicit {
            remember_pc_mic(&mic_name);
        } else {
            forget_pc_mic_memo();
        }
        let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                warn!("mic: activate default microphone: {e}");
                return;
            }
        };
        let mix = match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => {
                warn!("mic: mix format: {e}");
                return;
            }
        };
        let fmt: WAVEFORMATEX = *mix;
        let rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let event = match CreateEventW(None, false, false, None) {
            Ok(e) => e,
            Err(_) => {
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if let Err(e) = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            200_000,
            0,
            mix,
            None,
        ) {
            warn!("mic: initialize capture: {e}");
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        if client.SetEventHandle(event).is_err() {
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(_) => {
                let _ = CloseHandle(event);
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if client.Start().is_err() {
            let _ = CloseHandle(event);
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        info!("mic: PC microphone {mic_name} {rate} Hz {channels} ch {bits} bit");

        let mut converter = Converter::new(rate, channels, bits);
        let mut recheck = Instant::now() + Duration::from_secs(30);
        while !stop.load(Ordering::Relaxed) && enabled.load(Ordering::Relaxed) {
            // A new choice in the tray menu: reopen on the device picked.
            if PC_MIC_EPOCH.load(Ordering::Relaxed) != epoch {
                info!("mic: PC microphone changed; reopening");
                break;
            }
            // We fell back to another device because the chosen one was
            // unplugged: reopen as soon as it is back.
            if !explicit && Instant::now() >= recheck {
                recheck = Instant::now() + Duration::from_secs(30);
                let picked_again = {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    let en = enumerator();
                    en.and_then(|en| pick_pc_microphone(&en, &cable))
                };
                if let Some((again, true)) = picked_again {
                    let _ = again;
                    info!("mic: the chosen microphone is back; reopening");
                    break;
                }
            }
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
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
                    if let Some(chunk) = converter.push(samples) {
                        let _ = tx.try_send(chunk);
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
        CoTaskMemFree(Some(mix as *const _));
        info!("mic: PC microphone released");
    }
}

/// Convert a capture format to 48 kHz stereo s16, yielding ~10 ms chunks.
struct Converter {
    rate: u32,
    channels: usize,
    bits: usize,
    out: Vec<i16>,
    step: f64,
    pos: f64,
    buf: Vec<i16>,
}

impl Converter {
    fn new(rate: u32, channels: usize, bits: usize) -> Self {
        Self {
            rate,
            channels,
            bits,
            out: Vec::new(),
            step: rate as f64 / RATE as f64,
            pos: 0.0,
            buf: Vec::new(),
        }
    }

    fn push(&mut self, samples: &[u8]) -> Option<Vec<i16>> {
        if self.channels == 0 || self.bits == 0 {
            return None;
        }
        let frames = samples.len() / (self.channels * (self.bits / 8));
        for f in 0..frames {
            let base = f * self.channels * (self.bits / 8);
            let mut pair = [0i16; 2];
            for c in 0..2.min(self.channels) {
                let off = base + c * (self.bits / 8);
                pair[c] = match self.bits {
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
            if self.channels == 1 {
                pair[1] = pair[0];
            }
            self.buf.extend_from_slice(&pair);
        }

        if (self.step - 1.0).abs() < 1e-9 {
            self.out.append(&mut self.buf);
        } else {
            let frames = self.buf.len() / 2;
            while self.pos + 1.0 < frames as f64 {
                let i = self.pos.floor() as usize;
                let f = self.pos - i as f64;
                let l = self.buf[i * 2] as f64
                    + (self.buf[(i + 1) * 2] as f64 - self.buf[i * 2] as f64) * f;
                let r = self.buf[i * 2 + 1] as f64
                    + (self.buf[(i + 1) * 2 + 1] as f64 - self.buf[i * 2 + 1] as f64) * f;
                self.out.push(l as i16);
                self.out.push(r as i16);
                self.pos += self.step;
            }
            let drain = (self.pos.floor() as usize).min(frames);
            if drain > 0 {
                self.buf.drain(..drain * 2);
                self.pos -= drain as f64;
            }
        }

        if self.out.len() >= CHUNK {
            Some(self.out.drain(..CHUNK).collect())
        } else {
            None
        }
    }
}

// --- virtual microphone render -------------------------------------------

fn render_loop(
    device: IMMDevice,
    stop: Arc<AtomicBool>,
    ipad: Arc<Mutex<VecDeque<i16>>>,
    last_ipad: Arc<Mutex<Option<Instant>>>,
    use_ipad: Arc<AtomicBool>,
    pc: std::sync::mpsc::Receiver<Vec<i16>>,
) {
    unsafe {
        let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                warn!("mic: activate virtual microphone: {e}");
                return;
            }
        };
        let mix = match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => {
                warn!("mic: render mix format: {e}");
                return;
            }
        };
        let fmt: WAVEFORMATEX = *mix;
        let rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        // Event-driven rendering: sleeping between writes is not real time
        // (a 5 ms sleep is ~15.6 ms), which let the iPad queue grow to the
        // one-second cap and added that much latency. The engine now wakes us
        // once per period instead.
        let event = match CreateEventW(None, false, false, None) {
            Ok(e) => e,
            Err(e) => {
                warn!("mic: render event: {e}");
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if let Err(e) = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            200_000,
            0,
            mix,
            None,
        ) {
            warn!("mic: initialize virtual microphone: {e}");
            CoTaskMemFree(Some(mix as *const _));
            let _ = CloseHandle(event);
            return;
        }
        if let Err(e) = client.SetEventHandle(event) {
            warn!("mic: render event handle: {e}");
            CoTaskMemFree(Some(mix as *const _));
            let _ = CloseHandle(event);
            return;
        }
        let render: IAudioRenderClient = match client.GetService() {
            Ok(r) => r,
            Err(e) => {
                warn!("mic: render service: {e}");
                CoTaskMemFree(Some(mix as *const _));
                let _ = CloseHandle(event);
                return;
            }
        };
        if client.Start().is_err() {
            CoTaskMemFree(Some(mix as *const _));
            let _ = CloseHandle(event);
            return;
        }
        info!("mic: virtual microphone {rate} Hz {channels} ch {bits} bit");

        let buffer_frames = client.GetBufferSize().unwrap_or(4800) as usize;
        let mut pc_buf: VecDeque<i16> = VecDeque::new();
        let mut scratch = vec![0i16; CHUNK];
        let mut resample = Resampler::new(RATE, rate);
        let mut last_qlog = Instant::now();
        let mut underruns: u64 = 0;
        let mut last_under: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Drain the PC source; newest chunk wins if several queued.
            loop {
                match pc.try_recv() {
                    Ok(chunk) => {
                        pc_buf.extend(chunk);
                        while pc_buf.len() > MAX_BUFFER {
                            pc_buf.pop_front();
                        }
                    }
                    Err(_) => break,
                }
            }
            let ipad_ok = use_ipad.load(Ordering::Relaxed)
                && last_ipad
                    .lock()
                    .ok()
                    .and_then(|t| *t)
                    .map(|t| t.elapsed() < IPAD_FRESH)
                    .unwrap_or(false);
            if ipad_ok {
                pc_buf.clear();
            }

            if ipad_ok && last_qlog.elapsed() > Duration::from_secs(10) {
                last_qlog = Instant::now();
                let short = underruns - last_under;
                last_under = underruns;
                let q = ipad.lock().unwrap_or_else(|e| e.into_inner()).len();
                let ms = q / CHANNELS * 1000 / RATE as usize;
                info!("mic: iPad queue {ms} ms, {short} short write(s) in 10 s");
            }
            let padding = client.GetCurrentPadding().unwrap_or(0) as usize;
            let available = buffer_frames.saturating_sub(padding) as u32;
            if available == 0 {
                let _ = WaitForSingleObject(event, 200);
                continue;
            }
            // One 10 ms source chunk at 48 kHz becomes out_cap device frames
            // at the endpoint's rate.
            let out_cap = (((CHUNK / CHANNELS) as u64 * rate as u64) / RATE as u64).max(1) as u32;
            let frames = available.min(out_cap);
            let data = match render.GetBuffer(frames) {
                Ok(d) => d,
                Err(_) => {
                    let _ = WaitForSingleObject(event, 200);
                    continue;
                }
            };
            let src_frames = (((frames as u64 * RATE as u64) / rate as u64).max(1)) as usize;
            let need = (src_frames * CHANNELS).min(scratch.len());
            for v in scratch.iter_mut().take(need) {
                *v = 0;
            }
            let mut have = 0usize;
            if ipad_ok {
                // No jitter buffer: whatever the receiver delivered is played
                // as it comes. A missed block leaves a single 10 ms gap, which
                // is far less noticeable than the added delay a reserve costs.
                //
                // One correction is unavoidable: the iPad's 48 kHz clock and
                // this PC's differ, so the queue grows by a few ms every
                // second (measured 387→536 ms in 76 s on Wi-Fi) and the delay
                // would climb towards the one-second cap. Above 120 ms the
                // loop skips a few samples per block — at most ~1 %, i.e. a
                // barely audible pitch nudge — until the queue is back near
                // normal, then it stops touching the audio entirely.
                let mut buf = ipad.lock().unwrap_or_else(|e| e.into_inner());
                let q_ms = buf.len() / CHANNELS * 1000 / RATE as usize;
                if q_ms > 120 {
                    let skip = (((q_ms - 120) / 40).min(6) as usize) * CHANNELS;
                    for _ in 0..skip.min(buf.len()) {
                        buf.pop_front();
                    }
                }
                while have < need {
                    match buf.pop_front() {
                        Some(s) => {
                            scratch[have] = s;
                            have += 1;
                        }
                        None => break,
                    }
                }
            } else {
                while have < need {
                    match pc_buf.pop_front() {
                        Some(s) => {
                            scratch[have] = s;
                            have += 1;
                        }
                        None => break,
                    }
                }
            }
            if ipad_ok && have < need {
                underruns += 1;
            }
            // Convert the 48 kHz stereo s16 chunk to the device format.
            let src = &scratch[..need];
            write_frames(data, src, frames as usize, channels, bits, rate, &mut resample);
            let _ = render.ReleaseBuffer(frames, 0);
            let _ = WaitForSingleObject(event, 200);
        }

        let _ = client.Stop();
        CoTaskMemFree(Some(mix as *const _));
        let _ = CloseHandle(event);
    }
}

/// Render `frames` device frames (the exact count GetBuffer granted) from a
/// stereo 48 kHz source chunk. The device may run at another rate, so the
/// source is resampled and the result is clamped and zero-filled to the grant:
/// writing frames.len()/2 frames here used to overrun the WASAPI buffer when
/// the endpoint ran above 48 kHz and to release stale samples below it.
fn write_frames(
    data: *mut u8,
    src: &[i16],
    frames: usize,
    channels: usize,
    bits: usize,
    rate: u32,
    resample: &mut Resampler,
) {
    let mut converted = Vec::new();
    if rate == RATE {
        converted.extend_from_slice(src);
    } else {
        resample.process(src, &mut converted);
    }
    let have = (converted.len() / 2).min(frames);
    for f in 0..have {
        for c in 0..channels {
            let v = converted[f * 2 + c.min(1)];
            let idx = f * channels + c;
            unsafe {
                match bits {
                    32 => *((data as *mut f32).add(idx)) = v as f32 / 32768.0,
                    16 => *((data as *mut i16).add(idx)) = v,
                    _ => {}
                }
            }
        }
    }
    for f in have..frames {
        for c in 0..channels {
            let idx = f * channels + c;
            unsafe {
                match bits {
                    32 => *((data as *mut f32).add(idx)) = 0.0,
                    16 => *((data as *mut i16).add(idx)) = 0,
                    _ => {}
                }
            }
        }
    }
}

/// Linear stereo resampler (stateful), same shape as the recorder's.
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

    fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.buf.extend_from_slice(input);
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
        if self.buf.len() > MAX_BUFFER * 2 {
            self.buf.clear();
            self.pos = 0.0;
        }
    }
}

// --- device helpers -------------------------------------------------------

fn enumerator() -> Option<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() }
}

fn find_render(sub: &str) -> Option<IMMDevice> {
    let lower = sub.to_lowercase();
    unsafe {
        let en = enumerator()?;
        let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).ok()?;
        for i in 0..col.GetCount().ok()? {
            let d = col.Item(i).ok()?;
            if friendly(&d).to_lowercase().contains(&lower) {
                return Some(d);
            }
        }
    }
    None
}

fn friendly(device: &IMMDevice) -> String {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::STGM_READ;
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

/// Which microphone this tick uses. Split out so the rules stay testable.
///
/// - A hotkey pin (`Some(true)`) picks the iPad microphone for a **live**
///   consumer only: with nobody recording the iPad microphone must not run
///   (green indicator, battery), even though the pin itself stays set for the
///   next consumer.
/// - Automatic mode uses the iPad only while it is delivering frames; a
///   stalled receiver falls back to the PC microphone.
/// - The iPad is asked to capture only while it serves a consumer: in
///   automatic mode a consumer **on the iPad screen**, with a pin any
///   consumer (the pin decides which side records).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct MicDecision {
    want_ipad: bool,
    want_pc: bool,
    want_capture: bool,
}

fn decide_mic(manual: Option<bool>, has_consumers: bool, ipad_app: bool, fresh: bool) -> MicDecision {
    let want_ipad = match manual {
        Some(pin) => pin && has_consumers,
        None => ipad_app && fresh,
    };
    // While a pinned iPad is not delivering frames the PC microphone would be
    // the only usable source, but the pin means the user asked for the iPad:
    // let the re-send logic revive the receiver instead of flipping sources.
    let want_pc = has_consumers && !want_ipad && !(manual == Some(true) && fresh);
    // A pin to the PC side keeps the iPad released even when the consumer's
    // window sits on the iPad screen.
    let want_capture = want_ipad || (manual.is_none() && ipad_app);
    MicDecision { want_ipad, want_pc, want_capture }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<'a>(pairs: &'a [(u32, &'static str)]) -> impl Fn(u32) -> String + 'a {
        move |p| {
            pairs
                .iter()
                .find(|(id, _)| *id == p)
                .map(|(_, n)| n.to_string())
                .unwrap_or_default()
        }
    }

    fn id_set(ids: &[u32]) -> HashSet<u32> {
        ids.iter().copied().collect()
    }

    fn parent_list(pairs: &[(u32, u32)]) -> HashMap<u32, u32> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn idle_keeps_every_microphone_off() {
        // Nobody records: no source, and the iPad is never asked to capture.
        let d = decide_mic(None, false, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
    }

    #[test]
    fn a_pin_alone_does_not_keep_the_ipad_microphone_busy() {
        // The hotkey pin persists, but with no live consumer the iPad
        // microphone stays released (this was the "mic occupied forever" bug).
        let d = decide_mic(Some(true), false, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
        // Even if frames are still trickling in, nobody is recording.
        let d = decide_mic(Some(true), false, false, true);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
    }

    #[test]
    fn a_pin_serves_a_live_consumer_on_the_primary_screen() {
        // Recording app on the PC screen, user pinned the iPad microphone:
        // the iPad records, the PC microphone stays released.
        let d = decide_mic(Some(true), true, false, true);
        assert_eq!(d, MicDecision { want_ipad: true, want_pc: false, want_capture: true });
    }

    #[test]
    fn automatic_mode_follows_the_ipad_when_fresh() {
        let d = decide_mic(None, true, true, true);
        assert_eq!(d, MicDecision { want_ipad: true, want_pc: false, want_capture: true });
    }

    #[test]
    fn automatic_mode_falls_back_while_the_ipad_is_stalled() {
        // A consumer on the iPad screen whose receiver stopped delivering:
        // keep the meeting audible on the PC microphone, keep asking the iPad.
        let d = decide_mic(None, true, true, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: true });
    }

    #[test]
    fn a_primary_screen_consumer_never_starts_the_ipad_microphone() {
        let d = decide_mic(None, true, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: false });
        // ... even when stale frames make `fresh` true.
        let d = decide_mic(None, true, false, true);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: false });
    }

    #[test]
    fn pinning_the_pc_side_uses_the_pc_microphone() {
        // A PC pin must also keep the iPad microphone released, even with the
        // recording window on the iPad screen.
        let d = decide_mic(Some(false), true, true, true);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: false });
    }

    #[test]
    fn own_window_on_ipad() {
        let (on, off) = (id_set(&[10]), id_set(&[]));
        let parents = parent_list(&[]);
        assert!(consumer_on_ipad(10, &on, &off, &parents, names(&[(10, "app.exe")])));
    }

    #[test]
    fn own_window_on_primary() {
        let (on, off) = (id_set(&[]), id_set(&[10]));
        let parents = parent_list(&[]);
        assert!(!consumer_on_ipad(10, &on, &off, &parents, names(&[(10, "app.exe")])));
    }

    #[test]
    fn helper_follows_parent_to_ipad() {
        // WeChat: renderer 20 -> helper 11 (no window) -> main 10 (window on iPad)
        let (on, off) = (id_set(&[10]), id_set(&[]));
        let parents = parent_list(&[(20, 11), (11, 10)]);
        let name_of = names(&[
            (10, "Weixin.exe"),
            (11, "WeChatAppEx.exe"),
            (20, "WeChatAppEx.exe"),
        ]);
        assert!(consumer_on_ipad(20, &on, &off, &parents, name_of));
    }

    #[test]
    fn helper_with_primary_window_parent_stays_pc() {
        // The mini-program / call window on the primary belongs to the helper.
        let (on, off) = (id_set(&[10]), id_set(&[11]));
        let parents = parent_list(&[(20, 11), (11, 10)]);
        let name_of = names(&[
            (10, "Weixin.exe"),
            (11, "WeChatAppEx.exe"),
            (20, "WeChatAppEx.exe"),
        ]);
        assert!(!consumer_on_ipad(20, &on, &off, &parents, name_of));
    }

    #[test]
    fn shared_runtime_name_does_not_match() {
        // A WebView2 host on the iPad (30) must not make an unrelated
        // msedgewebview2 process (40, whose parent 41 sits on the primary)
        // count as being on the iPad.
        let (on, off) = (id_set(&[30]), id_set(&[41]));
        let parents = parent_list(&[(40, 41)]);
        let name_of = names(&[
            (30, "msedgewebview2.exe"),
            (40, "msedgewebview2.exe"),
            (41, "teams.exe"),
        ]);
        assert!(!consumer_on_ipad(40, &on, &off, &parents, name_of));
    }

    #[test]
    fn shell_parent_stops_the_walk() {
        let (on, off) = (id_set(&[5]), id_set(&[]));
        let parents = parent_list(&[(40, 5)]);
        let name_of = names(&[(5, "explorer.exe"), (40, "app.exe")]);
        assert!(!consumer_on_ipad(40, &on, &off, &parents, name_of));
    }

    #[test]
    fn missing_parent_means_pc() {
        let (on, off) = (id_set(&[10]), id_set(&[]));
        let parents = parent_list(&[]);
        assert!(!consumer_on_ipad(20, &on, &off, &parents, names(&[(10, "app.exe")])));
    }
}
