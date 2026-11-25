//! The iPad microphone as a Windows microphone, following the screen.
//!
//! This module bridges a virtual microphone cable (`CABLE Input` render ->
//! `CABLE Output` capture) and manages the Windows default capture device while
//! the iPad session is live:
//!
//! * while an iPad session is connected and the virtual cable is healthy, all
//!   three default capture roles (Console, Multimedia, Communications) are
//!   anchored to `CABLE Output` after backing up the user's real default
//!   capture device to `%APPDATA%\OpenDisplay\mic_default_backup.json`;
//! * a recording app whose window is on the iPad screen gets the iPad
//!   microphone (`ODMI` PCM frames from the receiver);
//! * a recording app on the primary screen gets the PC's real microphone
//!   (preferring the tray choice, `OD_PC_MIC`, or the backed-up real Windows
//!   default), so apps keep working normally on both screens;
//! * if sleep/wake, USB replug, or a user switch changes Windows' default
//!   capture device while connected, real changes are adopted into the backup
//!   and `CABLE Output` is re-anchored automatically;
//! * on disconnect, clean exit, cable loss, or startup after a crash, Windows'
//!   default capture roles are restored to the user's real PC microphone so the
//!   PC microphone is never left pointing at an unpowered virtual cable.
//!
//! VB-CABLE loops render->capture by design. `OD_VIRTUAL_MIC` overrides the
//! render device-name substring (default "CABLE Input"), and
//! `OD_VIRTUAL_MIC_CAPTURE` overrides the capture substring (default
//! "CABLE Output").

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use windows::core::{IUnknown, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, ERole, IAudioCaptureClient,
    IAudioClient, IAudioRenderClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
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

/// How fast `select_source` polls active capture sessions (ms). 250 ms picks
/// up short voice messages promptly while keeping CPU negligible.
const SELECT_POLL_MS: u64 = 250;
/// Check default-capture drift & unmute once per second (every 4 select ticks).
const DRIFT_CHECK_EVERY_TICKS: u32 = 4;
/// If the virtual cable stays absent/broken this long, restore the real PC
/// microphone instead of leaving Windows pointed at a dead cable.
const CABLE_LOSS_RESTORE_AFTER: Duration = Duration::from_secs(5);

/// Undocumented Windows COM class & interface used to set default audio
/// endpoints (same API Windows Sound Settings uses).
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

const CAPTURE_ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

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
/// the audio router keeps *all* system audio on the PC when the playback sink
/// shares the VB-CABLE device.
static IPAD_MIC_LIVE: AtomicBool = AtomicBool::new(false);

/// Serializes all default-capture backup / takeover / drift-heal / restore
/// operations so startup recovery, drift checks, and session teardown never
/// race.
static DEFAULT_CAPTURE_LOCK: Mutex<()> = Mutex::new(());
/// Monotonic generation for each `mic::run` instance. A guard only restores on
/// drop when its generation is still current, preventing a slow teardown from
/// clobbering a freshly started reconnect.
static BRIDGE_GENERATION: AtomicUsize = AtomicUsize::new(0);
/// Generation that currently owns Windows' default capture endpoint (`0` =
/// none).
static OWNED_DEFAULT_GEN: AtomicUsize = AtomicUsize::new(0);
/// The user's real default capture device ID saved before taking over Windows'
/// default capture device, so Automatic PC-mic mode uses the real default mic
/// instead of an arbitrary device from enumeration order.
static SAVED_DEFAULT_CAPTURE_ID: Mutex<Option<String>> = Mutex::new(None);

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
    let wanted = virtual_mic_render_name().to_lowercase();
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

fn virtual_mic_render_name() -> String {
    std::env::var("OD_VIRTUAL_MIC").unwrap_or_else(|_| "CABLE Input".into())
}

fn virtual_mic_capture_name() -> String {
    std::env::var("OD_VIRTUAL_MIC_CAPTURE").unwrap_or_else(|_| "CABLE Output".into())
}

fn manage_default_capture_enabled() -> bool {
    std::env::var_os("OD_NO_DEFAULT_MIC").is_none()
}

// --- Windows default capture device management ----------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct CaptureRoles {
    console: Option<String>,
    multimedia: Option<String>,
    communications: Option<String>,
}

impl CaptureRoles {
    fn get(&self, role: usize) -> Option<&str> {
        match role {
            0 => self.console.as_deref(),
            1 => self.multimedia.as_deref(),
            2 => self.communications.as_deref(),
            _ => None,
        }
    }

    fn set(&mut self, role: usize, val: Option<String>) {
        match role {
            0 => self.console = val,
            1 => self.multimedia = val,
            2 => self.communications = val,
            _ => {}
        }
    }

    fn primary_non_cable(&self, cable_id: &str) -> Option<&str> {
        for r in 0..3 {
            if let Some(id) = self.get(r) {
                if !id.is_empty() && !id.eq_ignore_ascii_case(cable_id) {
                    return Some(id);
                }
            }
        }
        None
    }

    fn primary_id(&self) -> Option<&str> {
        for r in 0..3 {
            if let Some(id) = self.get(r) {
                if !id.is_empty() {
                    return Some(id);
                }
            }
        }
        None
    }

    fn any_equals(&self, target: &str) -> bool {
        (0..3).any(|r| {
            self.get(r)
                .map(|id| id.eq_ignore_ascii_case(target))
                .unwrap_or(false)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DriftPlan {
    updated_backup: Option<CaptureRoles>,
    roles_to_reanchor: Vec<i32>,
    adopted_primary_changed: bool,
}

/// Build a clean backup of all three capture roles, guaranteeing that NO role
/// ever stores `cable_id` (so restoring never leaves Windows on the cable).
fn build_backup_from_current(
    current: &CaptureRoles,
    existing: Option<&CaptureRoles>,
    cable_id: &str,
    fallback_real_id: Option<&str>,
) -> CaptureRoles {
    let is_real = |id: &str| !id.is_empty() && !id.eq_ignore_ascii_case(cable_id);
    let fallback_from_current = current.primary_non_cable(cable_id);
    let fallback_from_existing = existing.and_then(|b| b.primary_non_cable(cable_id));
    let global_fallback = fallback_from_current
        .or(fallback_from_existing)
        .or_else(|| fallback_real_id.filter(|id| is_real(id)));

    let mut out = CaptureRoles::default();
    for r in 0..3 {
        let pick = current
            .get(r)
            .filter(|id| is_real(id))
            .or_else(|| existing.and_then(|b| b.get(r)).filter(|id| is_real(id)))
            .or(global_fallback);
        out.set(r, pick.map(str::to_string));
    }
    out
}

/// Inspect the current Windows capture defaults against `cable_id` while the
/// bridge owns them. Any role that drifted away from `cable_id` is queued for
/// re-anchoring; if it drifted to a real active microphone, the backup adopts
/// that microphone so both the PC bridge and disconnect-restore honor it.
fn reconcile_drift(
    current: &CaptureRoles,
    backup: &CaptureRoles,
    cable_id: &str,
    active_ids: &HashSet<String>,
) -> DriftPlan {
    let mut next_backup = backup.clone();
    let mut roles_to_reanchor = Vec::new();
    for r in 0..3 {
        let cur = current.get(r);
        let is_cable = cur
            .map(|id| id.eq_ignore_ascii_case(cable_id))
            .unwrap_or(false);
        if !is_cable {
            roles_to_reanchor.push(r as i32);
            if let Some(id) = cur {
                if !id.is_empty()
                    && !id.eq_ignore_ascii_case(cable_id)
                    && active_ids.contains(id)
                {
                    next_backup.set(r, Some(id.to_string()));
                }
            }
        }
    }
    let adopted_primary_changed = next_backup.primary_id() != backup.primary_id();
    let updated_backup = (next_backup != *backup).then_some(next_backup);
    DriftPlan {
        updated_backup,
        roles_to_reanchor,
        adopted_primary_changed,
    }
}

/// Compute `(role, target_device_id)` restorations for any capture role still
/// pointing at `cable_id`. Never overwrites a role that already points at a
/// real microphone.
fn plan_restore(
    backup: Option<&CaptureRoles>,
    current: &CaptureRoles,
    cable_id: &str,
    active_ids: &HashSet<String>,
    fallback_real_id: Option<&str>,
    startup_recovery: bool,
) -> Vec<(i32, String)> {
    let is_valid =
        |id: &str| !id.is_empty() && !id.eq_ignore_ascii_case(cable_id) && active_ids.contains(id);
    let current_real = (0..3).find_map(|r| current.get(r).filter(|id| is_valid(id)));
    let backup_any_real =
        backup.and_then(|b| (0..3).find_map(|r| b.get(r).filter(|id| is_valid(id))));
    let fallback = fallback_real_id.filter(|id| is_valid(id));

    let mut actions = Vec::new();
    for r in 0..3 {
        let on_cable = current
            .get(r)
            .map(|id| id.eq_ignore_ascii_case(cable_id))
            .unwrap_or(false);
        if !on_cable {
            continue;
        }
        let target = backup
            .and_then(|b| b.get(r))
            .filter(|id| is_valid(id))
            .or_else(|| if startup_recovery { current_real } else { None })
            .or(backup_any_real)
            .or(current_real)
            .or(fallback);
        if let Some(id) = target {
            actions.push((r as i32, id.to_string()));
        }
    }
    actions
}

/// Anti-thrash rate limiter for re-anchoring Windows' default capture device.
/// Normal single drift heals immediately; repeated failures or another tool
/// fighting over the default backs off to 15 s.
#[derive(Debug, Default)]
struct DriftThrottle {
    window_start: Option<Instant>,
    attempts_in_window: u32,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

impl DriftThrottle {
    fn can_attempt(&self, now: Instant) -> bool {
        self.cooldown_until.map(|until| now >= until).unwrap_or(true)
    }

    /// Record one re-anchor attempt. Returns `true` when entering long backoff.
    fn record(&mut self, now: Instant, succeeded: bool) -> bool {
        match self.window_start {
            Some(start) if now.saturating_duration_since(start) <= Duration::from_secs(15) => {
                self.attempts_in_window += 1;
            }
            _ => {
                self.window_start = Some(now);
                self.attempts_in_window = 1;
            }
        }
        if succeeded {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
        }
        if self.consecutive_failures >= 3 || self.attempts_in_window >= 6 {
            self.cooldown_until = Some(now + Duration::from_secs(15));
            true
        } else {
            self.cooldown_until = Some(now + Duration::from_secs(1));
            false
        }
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

fn default_backup_path() -> Option<std::path::PathBuf> {
    Some(routes_dir()?.join("mic_default_backup.json"))
}

fn load_default_backup() -> Option<CaptureRoles> {
    let path = default_backup_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<CaptureRoles>(&text).ok()
}

fn save_default_backup(roles: &CaptureRoles) {
    let Some(path) = default_backup_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string_pretty(roles) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::write(&path, text);
}

fn remove_default_backup() {
    if let Some(path) = default_backup_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn set_saved_default_capture_id(id: Option<String>) {
    if let Ok(mut g) = SAVED_DEFAULT_CAPTURE_ID.lock() {
        *g = id;
    }
}

fn saved_default_capture_id() -> Option<String> {
    SAVED_DEFAULT_CAPTURE_ID.lock().ok().and_then(|g| g.clone())
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

fn read_current_capture_roles(en: &IMMDeviceEnumerator) -> CaptureRoles {
    let mut roles = CaptureRoles::default();
    unsafe {
        for (idx, &role) in CAPTURE_ROLES.iter().enumerate() {
            if let Ok(dev) = en.GetDefaultAudioEndpoint(eCapture, role) {
                let id = device_id(&dev);
                if !id.is_empty() {
                    roles.set(idx, Some(id));
                }
            }
        }
    }
    roles
}

fn active_capture_devices_map(en: &IMMDeviceEnumerator) -> (HashSet<String>, Vec<(String, String)>) {
    let mut ids = HashSet::new();
    let mut list = Vec::new();
    unsafe {
        if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
            for i in 0..col.GetCount().unwrap_or(0) {
                let Ok(d) = col.Item(i) else { continue };
                let id = device_id(&d);
                let name = friendly(&d);
                if !id.is_empty() {
                    ids.insert(id.clone());
                    list.push((id, name));
                }
            }
        }
    }
    (ids, list)
}

fn find_capture_id(en: &IMMDeviceEnumerator, sub: &str) -> Option<(String, String)> {
    let lower = sub.to_lowercase();
    let (_, list) = active_capture_devices_map(en);
    list.into_iter()
        .find(|(_, name)| name.to_lowercase().contains(&lower))
}

fn pick_fallback_real_capture_id(
    list: &[(String, String)],
    cable_sub_lower: &str,
) -> Option<String> {
    if let Some(pref) = pc_mic_preference()
        .or_else(|| std::env::var("OD_PC_MIC").ok())
        .or_else(remembered_pc_mic)
    {
        let want = pref.to_lowercase();
        if !want.is_empty() {
            if let Some((id, _)) = list.iter().find(|(_, name)| {
                let l = name.to_lowercase();
                !l.contains(cable_sub_lower) && l.contains(&want)
            }) {
                return Some(id.clone());
            }
        }
    }
    list.iter()
        .find(|(_, name)| !name.is_empty() && !name.to_lowercase().contains(cable_sub_lower))
        .map(|(id, _)| id.clone())
}

fn friendly_by_id(en: &IMMDeviceEnumerator, id: &str) -> String {
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        match en.GetDevice(PCWSTR(wide.as_ptr())) {
            Ok(d) => friendly(&d),
            Err(_) => id.to_string(),
        }
    }
}

/// Set Windows' default audio endpoint for one role (`0`=Console,
/// `1`=Multimedia, `2`=Communications) through `IPolicyConfig::SetDefaultEndpoint`.
unsafe fn set_default_endpoint(device_id: &str, role: i32) -> Result<(), HRESULT> {
    unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
            .map_err(|e| e.code())?;
        let mut policy: *mut c_void = std::ptr::null_mut();
        let hr = unk.query(&IID_POLICY_CONFIG, &mut policy);
        if hr.is_err() || policy.is_null() {
            return Err(if hr.is_err() {
                hr
            } else {
                HRESULT(0x80004002u32 as i32)
            });
        }
        let vtbl = *(policy as *mut *const *const c_void);
        type SetDefaultFn = unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT;
        type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
        let set_fn: SetDefaultFn = std::mem::transmute(*vtbl.add(13));
        let release_fn: ReleaseFn = std::mem::transmute(*vtbl.add(2));
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        let res = set_fn(policy, PCWSTR(wide.as_ptr()), role);
        release_fn(policy);
        if res.is_ok() {
            Ok(())
        } else {
            Err(res)
        }
    }
}

/// Ensure neither `CABLE Input` nor `CABLE Output` is muted (and that
/// `CABLE Output` capture volume is not zeroed out), so the bridge never
/// silently outputs zeros.
fn ensure_cable_audible(en: &IMMDeviceEnumerator) {
    let render_sub = virtual_mic_render_name().to_lowercase();
    let capture_sub = virtual_mic_capture_name().to_lowercase();
    unsafe {
        if let Ok(col) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
            for i in 0..col.GetCount().unwrap_or(0) {
                let Ok(d) = col.Item(i) else { continue };
                let name = friendly(&d);
                if name.to_lowercase().contains(&render_sub) {
                    if let Ok(vol) = d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
                        if vol.GetMute().map(|m| m.as_bool()).unwrap_or(false) {
                            if vol.SetMute(false, std::ptr::null()).is_ok() {
                                info!("mic: unmuted {name}");
                            }
                        }
                    }
                    break;
                }
            }
        }
        if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
            for i in 0..col.GetCount().unwrap_or(0) {
                let Ok(d) = col.Item(i) else { continue };
                let name = friendly(&d);
                if name.to_lowercase().contains(&capture_sub) {
                    if let Ok(vol) = d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
                        if vol.GetMute().map(|m| m.as_bool()).unwrap_or(false) {
                            if vol.SetMute(false, std::ptr::null()).is_ok() {
                                info!("mic: unmuted {name}");
                            }
                        }
                        if let Ok(sc) = vol.GetMasterVolumeLevelScalar() {
                            if sc < 0.01 {
                                if vol.SetMasterVolumeLevelScalar(1.0, std::ptr::null()).is_ok() {
                                    info!("mic: restored {name} volume from 0% to 100%");
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
}

/// Recover Windows' default capture device at startup if an earlier run
/// crashed (or left any capture role stuck on `CABLE Output` while no bridge
/// is running). Safe to call even when VB-CABLE is not installed.
pub fn recover_default_capture_on_startup() {
    if !manage_default_capture_enabled() {
        return;
    }
    let _lock = DEFAULT_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if OWNED_DEFAULT_GEN.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(en) = enumerator() else { return };
    let cap_sub = virtual_mic_capture_name();
    let cap_lower = cap_sub.to_lowercase();
    let backup = load_default_backup();
    let Some((cable_id, cable_name)) = find_capture_id(&en, &cap_sub) else {
        if backup.is_some() {
            remove_default_backup();
        }
        return;
    };
    let current = read_current_capture_roles(&en);
    if backup.is_none() && !current.any_equals(&cable_id) {
        return;
    }
    let (active_ids, list) = active_capture_devices_map(&en);
    let fallback = pick_fallback_real_capture_id(&list, &cap_lower);
    let actions = plan_restore(
        backup.as_ref(),
        &current,
        &cable_id,
        &active_ids,
        fallback.as_deref(),
        true,
    );
    let mut all_ok = true;
    for (role, target_id) in &actions {
        match unsafe { set_default_endpoint(target_id, *role) } {
            Ok(()) => {
                info!(
                    "mic: startup recovery restored capture role {role} from '{cable_name}' to '{}'",
                    friendly_by_id(&en, target_id)
                );
            }
            Err(hr) => {
                warn!("mic: startup recovery failed for capture role {role}: {hr:?}");
                all_ok = false;
            }
        }
    }
    if all_ok {
        remove_default_backup();
        set_saved_default_capture_id(None);
    }
}

/// Safety-net restore called from `OpenDisplay::quit()` after stopping the
/// controller, in case a shutdown timeout prevented the worker thread from
/// finishing its drop before process exit.
pub fn restore_default_capture_if_owned() {
    if !manage_default_capture_enabled() {
        return;
    }
    let _lock = DEFAULT_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let owned = OWNED_DEFAULT_GEN.swap(0, Ordering::SeqCst);
    if owned == 0 && load_default_backup().is_none() {
        return;
    }
    restore_default_capture_locked();
}

fn restore_default_capture_locked() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(en) = enumerator() else { return };
    let cap_sub = virtual_mic_capture_name();
    let cap_lower = cap_sub.to_lowercase();
    let backup = load_default_backup();
    let Some((cable_id, cable_name)) = find_capture_id(&en, &cap_sub) else {
        remove_default_backup();
        set_saved_default_capture_id(None);
        return;
    };
    let current = read_current_capture_roles(&en);
    let (active_ids, list) = active_capture_devices_map(&en);
    let fallback = pick_fallback_real_capture_id(&list, &cap_lower);
    let actions = plan_restore(
        backup.as_ref(),
        &current,
        &cable_id,
        &active_ids,
        fallback.as_deref(),
        false,
    );
    let mut restored_any = false;
    let mut all_ok = true;
    for (role, target_id) in &actions {
        match unsafe { set_default_endpoint(target_id, *role) } {
            Ok(()) => {
                restored_any = true;
                debug!(
                    "mic: restored capture role {role} from '{cable_name}' to '{}'",
                    friendly_by_id(&en, target_id)
                );
            }
            Err(hr) => {
                warn!("mic: failed to restore capture role {role}: {hr:?}");
                all_ok = false;
            }
        }
    }
    if restored_any {
        if let Some((_, first_target)) = actions.first() {
            info!(
                "mic: restored Windows default microphone to '{}'",
                friendly_by_id(&en, first_target)
            );
        }
    }
    if all_ok {
        remove_default_backup();
    }
    set_saved_default_capture_id(None);
}

/// RAII guard holding Windows' default capture device on `CABLE Output` while
/// a mic bridge is live. Dropping it restores the user's real PC microphone
/// unless a newer bridge generation has already taken ownership.
struct DefaultCaptureGuard {
    generation: usize,
}

impl DefaultCaptureGuard {
    fn acquire(generation: usize) -> Option<Self> {
        if !manage_default_capture_enabled() {
            return None;
        }
        let _lock = DEFAULT_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if BRIDGE_GENERATION.load(Ordering::SeqCst) != generation {
            return None;
        }
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let en = enumerator()?;
        let cap_sub = virtual_mic_capture_name();
        let cap_lower = cap_sub.to_lowercase();
        let (cable_id, cable_name) = find_capture_id(&en, &cap_sub)?;
        ensure_cable_audible(&en);

        let current = read_current_capture_roles(&en);
        let existing = load_default_backup();
        let (_, list) = active_capture_devices_map(&en);
        let fallback = pick_fallback_real_capture_id(&list, &cap_lower);
        let backup =
            build_backup_from_current(&current, existing.as_ref(), &cable_id, fallback.as_deref());
        save_default_backup(&backup);
        set_saved_default_capture_id(backup.primary_id().map(str::to_string));

        let mut set_ok = false;
        let mut changed = false;
        for role in 0..3 {
            let already = current
                .get(role)
                .map(|id| id.eq_ignore_ascii_case(&cable_id))
                .unwrap_or(false);
            if already {
                set_ok = true;
                continue;
            }
            match unsafe { set_default_endpoint(&cable_id, role as i32) } {
                Ok(()) => {
                    set_ok = true;
                    changed = true;
                }
                Err(hr) => {
                    warn!("mic: failed to set default capture role {role} to '{cable_name}': {hr:?}");
                }
            }
        }
        if !set_ok {
            return None;
        }
        OWNED_DEFAULT_GEN.store(generation, Ordering::SeqCst);
        let backup_label = backup
            .primary_id()
            .map(|id| friendly_by_id(&en, id))
            .unwrap_or_else(|| "(none)".into());
        if changed {
            info!(
                "mic: Windows default microphone -> '{cable_name}' (saved '{backup_label}' for restore)"
            );
        } else {
            info!(
                "mic: Windows default microphone already on '{cable_name}' (restore target '{backup_label}')"
            );
        }
        Some(Self { generation })
    }
}

impl Drop for DefaultCaptureGuard {
    fn drop(&mut self) {
        let _lock = DEFAULT_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if OWNED_DEFAULT_GEN.load(Ordering::SeqCst) != self.generation {
            return;
        }
        OWNED_DEFAULT_GEN.store(0, Ordering::SeqCst);
        restore_default_capture_locked();
    }
}

/// Called periodically while the bridge is healthy to heal default-capture
/// drift (sleep/wake, USB replug, MMDevAPI Event 65, or a user switching to
/// another real microphone in Windows Settings).
fn check_and_heal_default_capture(generation: usize, throttle: &mut DriftThrottle) {
    if !manage_default_capture_enabled() {
        return;
    }
    let now = Instant::now();
    if !throttle.can_attempt(now) {
        return;
    }
    let _lock = DEFAULT_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if OWNED_DEFAULT_GEN.load(Ordering::SeqCst) != generation {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(en) = enumerator() else { return };
    let cap_sub = virtual_mic_capture_name();
    let Some((cable_id, cable_name)) = find_capture_id(&en, &cap_sub) else {
        return;
    };
    ensure_cable_audible(&en);

    let current = read_current_capture_roles(&en);
    let backup = load_default_backup().unwrap_or_default();
    let (active_ids, _) = active_capture_devices_map(&en);
    let plan = reconcile_drift(&current, &backup, &cable_id, &active_ids);
    if plan.roles_to_reanchor.is_empty() {
        throttle.reset();
        return;
    }

    if let Some(ref updated) = plan.updated_backup {
        save_default_backup(updated);
        let new_primary = updated.primary_id().map(str::to_string);
        set_saved_default_capture_id(new_primary.clone());
        if plan.adopted_primary_changed
            && pc_mic_preference().is_none()
            && std::env::var_os("OD_PC_MIC").is_none()
        {
            if let Some(ref id) = new_primary {
                info!(
                    "mic: adopted new Windows default microphone '{}' as PC source",
                    friendly_by_id(&en, id)
                );
            }
            PC_MIC_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drifted_desc: Vec<String> = plan
        .roles_to_reanchor
        .iter()
        .map(|&r| {
            let name = current
                .get(r as usize)
                .map(|id| friendly_by_id(&en, id))
                .unwrap_or_else(|| "(none)".into());
            format!("role{r}='{name}'")
        })
        .collect();

    let mut all_ok = true;
    for &role in &plan.roles_to_reanchor {
        if let Err(hr) = unsafe { set_default_endpoint(&cable_id, role) } {
            warn!("mic: failed to re-anchor capture role {role} to '{cable_name}': {hr:?}");
            all_ok = false;
        }
    }
    let backed_off = throttle.record(now, all_ok);
    if all_ok {
        info!(
            "mic: default capture drifted ({}); re-anchored to '{cable_name}'",
            drifted_desc.join(", ")
        );
    }
    if backed_off {
        warn!(
            "mic: repeated default-capture changes/failures ({}); backing off 15 s",
            drifted_desc.join(", ")
        );
    }
}

// --- session lifecycle ----------------------------------------------------

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
            if h.thread().id() != thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for MicSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            if h.thread().id() != thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

/// Start the virtual-microphone bridge. None when disabled via `OD_NO_MIC`.
/// `notify` tells the receiver whether it should capture this device's
/// microphone.
pub fn start(
    rect: Arc<Mutex<RECT>>,
    notify: Box<dyn Fn(bool) + Send>,
) -> Option<MicSession> {
    if std::env::var_os("OD_NO_MIC").is_some() {
        info!("mic: disabled by OD_NO_MIC");
        return None;
    }
    let name = virtual_mic_render_name();

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

fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        thread::sleep(rem.min(Duration::from_millis(100)));
    }
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
    let bridge_gen = BRIDGE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let inner_stop = Arc::new(AtomicBool::new(false));
    let render_healthy = Arc::new(AtomicBool::new(false));

    // PC microphone -> one-slot-per-chunk stream. The capture device is held
    // open only while an app on the primary screen records through the cable,
    // so a meeting on the iPad never keeps the PC microphone (and its LED)
    // alive.
    let pc_enabled = Arc::new(AtomicBool::new(false));
    let (pc_tx, pc_rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(16);
    let pc_handle = {
        let stop = inner_stop.clone();
        let enabled = pc_enabled.clone();
        thread::Builder::new()
            .name("mic-pc".into())
            .spawn(move || capture_pc(stop, enabled, pc_tx))
            .ok()
    };

    // Window/session selector: decides who owns the cable, heals default
    // capture drift, and tells the receiver when to capture.
    let sel_handle = {
        let stop = inner_stop.clone();
        let rect = rect.clone();
        let use_ipad = use_ipad.clone();
        let last_ipad = last_ipad.clone();
        let frames = frames.clone();
        let pc_enabled = pc_enabled.clone();
        let render_healthy = render_healthy.clone();
        thread::Builder::new()
            .name("mic-sel".into())
            .spawn(move || {
                select_source(
                    stop,
                    rect,
                    use_ipad,
                    last_ipad,
                    frames,
                    pc_enabled,
                    render_healthy,
                    bridge_gen,
                    notify,
                )
            })
            .ok()
    };

    let capture_name = virtual_mic_capture_name();
    let mut default_guard: Option<DefaultCaptureGuard> = None;
    let mut missing_since: Option<Instant> = None;
    let mut logged_missing = false;

    // Self-healing render loop: if VB-CABLE temporarily disappears or WASAPI
    // invalidates the render client (sleep/wake, audiosrv restart, Event 65),
    // reopen it automatically instead of leaving the mic bridge permanently
    // dead for the rest of the session.
    while !stop.load(Ordering::Relaxed) {
        let render_device = find_render(&render_name);
        let has_capture = enumerator()
            .and_then(|en| find_capture_id(&en, &capture_name))
            .is_some();

        let Some(render_device) = render_device.filter(|_| has_capture) else {
            render_healthy.store(false, Ordering::Relaxed);
            if !logged_missing {
                warn!(
                    "mic: virtual microphone pair ('{render_name}' / '{capture_name}') unavailable; waiting"
                );
                logged_missing = true;
            }
            let since = *missing_since.get_or_insert_with(Instant::now);
            if default_guard.is_some() && since.elapsed() >= CABLE_LOSS_RESTORE_AFTER {
                warn!("mic: virtual cable absent for >5 s; restoring Windows default microphone until it returns");
                default_guard = None;
            }
            sleep_interruptible(&stop, Duration::from_secs(1));
            continue;
        };

        let Some(active) = open_render_client(&render_device) else {
            render_healthy.store(false, Ordering::Relaxed);
            let since = *missing_since.get_or_insert_with(Instant::now);
            if default_guard.is_some() && since.elapsed() >= CABLE_LOSS_RESTORE_AFTER {
                warn!("mic: virtual cable render failed for >5 s; restoring Windows default microphone until it recovers");
                default_guard = None;
            }
            sleep_interruptible(&stop, Duration::from_secs(1));
            continue;
        };

        logged_missing = false;
        missing_since = None;
        if let Some(en) = enumerator() {
            ensure_cable_audible(&en);
        }
        if default_guard.is_none() {
            default_guard = DefaultCaptureGuard::acquire(bridge_gen);
        }
        render_healthy.store(true, Ordering::Relaxed);

        let exit = pump_render_client(active, &stop, &ipad, &last_ipad, &use_ipad, &pc_rx);
        render_healthy.store(false, Ordering::Relaxed);

        if matches!(exit, RenderExit::DeviceError) && !stop.load(Ordering::Relaxed) {
            warn!("mic: virtual microphone render interrupted; reopening");
            missing_since = Some(Instant::now());
            sleep_interruptible(&stop, Duration::from_millis(500));
        }
    }

    // Deterministic teardown: stop and join sub-threads BEFORE releasing the
    // default-capture guard so `select_source` cannot race a drift heal with
    // restore, and no zombie threads ever outlive `MicSession`.
    render_healthy.store(false, Ordering::Relaxed);
    inner_stop.store(true, Ordering::Relaxed);
    if let Some(h) = sel_handle {
        let _ = h.join();
    }
    if let Some(h) = pc_handle {
        let _ = h.join();
    }
    drop(default_guard);
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

#[allow(clippy::too_many_arguments)]
fn select_source(
    stop: Arc<AtomicBool>,
    rect: Arc<Mutex<RECT>>,
    use_ipad: Arc<AtomicBool>,
    last_ipad: Arc<Mutex<Option<Instant>>>,
    frames: Arc<AtomicUsize>,
    pc_enabled: Arc<AtomicBool>,
    render_healthy: Arc<AtomicBool>,
    bridge_gen: usize,
    notify: Box<dyn Fn(bool) + Send>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let own_pid = std::process::id();
    let mut last_mode: Option<&'static str> = None;
    let mut last_notified = false;
    let mut last_report = Instant::now() - Duration::from_secs(3600);
    let mut last_target = RECT::default();
    let mut last_on_ipad = usize::MAX;
    let mut last_consumers: HashSet<u32> = HashSet::new();
    let mut tick: u32 = 0;
    let mut drift_throttle = DriftThrottle::default();
    let mut warned_bypass: HashSet<(u32, String)> = HashSet::new();
    // Start silent: a receiver left capturing from an older session must stop.
    notify(false);
    while !stop.load(Ordering::Relaxed) {
        if !render_healthy.load(Ordering::Relaxed) {
            use_ipad.store(false, Ordering::Relaxed);
            pc_enabled.store(false, Ordering::Relaxed);
            IPAD_MIC_LIVE.store(false, Ordering::Relaxed);
            if let Ok(mut g) = IPAD_MIC_CONSUMERS.lock() {
                g.clear();
            }
            if last_notified {
                notify(false);
                last_notified = false;
            }
            last_mode = Some("suspended");
            thread::sleep(Duration::from_millis(SELECT_POLL_MS));
            continue;
        }

        if tick % DRIFT_CHECK_EVERY_TICKS == 0 {
            check_and_heal_default_capture(bridge_gen, &mut drift_throttle);
        }
        tick = tick.wrapping_add(1);

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
        let matches_ipad = |pid: u32| {
            consumer_on_ipad(pid, &on_ipad, &off_ipad, &parents, crate::audio::process_name)
        };

        // Only sessions on the virtual cable are routed by this bridge.
        let consumers = cable_session_pids();
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

        // Diagnostic: notice if an app on the iPad screen is recording directly
        // from a physical microphone (bypassing CABLE Output, e.g. hardcoded in
        // the app's own settings). Checked once per second.
        if tick % DRIFT_CHECK_EVERY_TICKS == 1 {
            let mut live_bypass: HashSet<(u32, String)> = HashSet::new();
            for (pid, dev_name) in non_cable_session_pids(own_pid) {
                if matches_ipad(pid) {
                    let key = (pid, dev_name.clone());
                    if warned_bypass.insert(key.clone()) {
                        warn!(
                            "mic: {}:{} on the iPad screen is recording directly from '{}' instead of '{}' (check app mic setting)",
                            crate::audio::process_name(pid),
                            pid,
                            dev_name,
                            virtual_mic_capture_name()
                        );
                    }
                    live_bypass.insert(key);
                }
            }
            warned_bypass.retain(|k| live_bypass.contains(k));
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

        let want_capture = decision.want_capture;
        if want_capture != last_notified {
            notify(want_capture);
            info!("mic: iPad capture {}", if want_capture { "on" } else { "off" });
            last_notified = want_capture;
        }
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
        thread::sleep(Duration::from_millis(SELECT_POLL_MS));
    }
    // Always clear published state on exit so the audio router never sees
    // stale echo-guard flags after a disconnect.
    use_ipad.store(false, Ordering::Relaxed);
    pc_enabled.store(false, Ordering::Relaxed);
    IPAD_MIC_LIVE.store(false, Ordering::Relaxed);
    if let Ok(mut g) = IPAD_MIC_CONSUMERS.lock() {
        g.clear();
    }
    notify(false);
}

/// PIDs with an active capture session on the virtual microphone's capture
/// endpoint (`CABLE Output`) — the apps actually reading this microphone.
fn cable_session_pids() -> HashSet<u32> {
    let wanted = virtual_mic_capture_name().to_lowercase();
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
                    debug!("mic: cable session pid={pid} state={state}");
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

/// Active capture sessions on non-cable microphones (excluding OpenDisplay's
/// own PC-mic capture), used only for diagnostic logging when an iPad-screen
/// app explicitly bypasses `CABLE Output`.
fn non_cable_session_pids(own_pid: u32) -> Vec<(u32, String)> {
    let cable = virtual_mic_capture_name().to_lowercase();
    let mut out = Vec::new();
    unsafe {
        let Some(en) = enumerator() else { return out };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(device) = col.Item(i) else { continue };
            let name = friendly(&device);
            if name.to_lowercase().contains(&cable) {
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
                if state == 1 && pid != 0 && pid != own_pid {
                    out.push((pid, name.clone()));
                }
            }
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
        sleep_interruptible(&stop, Duration::from_millis(300));
    }
}

/// The PC microphone the user picked in the tray menu; `None` = automatic.
static PC_MIC_PREFERENCE: Mutex<Option<String>> = Mutex::new(None);
/// Bumped when the preference (or adopted Windows default) changes, so an open
/// capture reopens on the new device immediately.
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
/// the primary screen records. Priority:
/// 1. tray menu choice (`PC_MIC_PREFERENCE`);
/// 2. `OD_PC_MIC` env var (or explicit memo);
/// 3. live Windows default capture device (if not the cable);
/// 4. backed-up real Windows default capture device ID (saved before this
///    bridge switched Windows' default to `CABLE Output`);
/// 5. first active non-cable capture device.
fn pick_pc_microphone(en: &IMMDeviceEnumerator, cable: &str) -> Option<(IMMDevice, bool)> {
    unsafe {
        if let Some(preferred) = pc_mic_preference() {
            let want = preferred.to_lowercase();
            if !want.is_empty() {
                if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
                    for i in 0..col.GetCount().unwrap_or(0) {
                        let Ok(d) = col.Item(i) else { continue };
                        let name = friendly(&d).to_lowercase();
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
        if let Some(saved_id) = saved_default_capture_id()
            .or_else(|| load_default_backup().and_then(|b| b.primary_id().map(str::to_string)))
        {
            if let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
                for i in 0..col.GetCount().unwrap_or(0) {
                    let Ok(d) = col.Item(i) else { continue };
                    if device_id(&d).eq_ignore_ascii_case(&saved_id)
                        && !friendly(&d).to_lowercase().contains(cable)
                    {
                        return Some((d, false));
                    }
                }
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
        let cable = virtual_mic_capture_name().to_lowercase();
        let epoch = PC_MIC_EPOCH.load(Ordering::Relaxed);
        let Some((device, explicit)) = pick_pc_microphone(&en, &cable) else {
            warn!("mic: no real microphone to capture");
            return;
        };
        let picked_id = device_id(&device);
        let mic_name = friendly(&device);
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
        let mut recheck = Instant::now() + Duration::from_secs(10);
        'outer: while !stop.load(Ordering::Relaxed) && enabled.load(Ordering::Relaxed) {
            if PC_MIC_EPOCH.load(Ordering::Relaxed) != epoch {
                info!("mic: PC microphone changed; reopening");
                break;
            }
            // If we fell back to another mic because the preferred/default mic
            // was unplugged, switch back as soon as it returns.
            if !explicit && Instant::now() >= recheck {
                recheck = Instant::now() + Duration::from_secs(10);
                if let Some((again, _)) = enumerator().and_then(|e| pick_pc_microphone(&e, &cable)) {
                    let again_id = device_id(&again);
                    if !again_id.is_empty() && !again_id.eq_ignore_ascii_case(&picked_id) {
                        info!("mic: preferred PC microphone is back; reopening");
                        break;
                    }
                }
            }
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let next = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("mic: PC microphone invalidated ({e}); reopening");
                        break 'outer;
                    }
                };
                if next == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if let Err(e) = capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) {
                    warn!("mic: PC microphone read failed ({e}); reopening");
                    break 'outer;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let bytes = frames as usize * channels * (bits / 8);
                    let samples = std::slice::from_raw_parts(data, bytes);
                    if let Some(chunk) = converter.push(samples) {
                        let _ = tx.try_send(chunk);
                    }
                }
                if let Err(e) = capture.ReleaseBuffer(frames) {
                    warn!("mic: PC microphone release failed ({e}); reopening");
                    break 'outer;
                }
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
    #[allow(dead_code)]
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

struct ActiveRenderClient {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    mix: *mut WAVEFORMATEX,
    rate: u32,
    channels: usize,
    bits: usize,
    buffer_frames: usize,
}

impl Drop for ActiveRenderClient {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            CoTaskMemFree(Some(self.mix as *const _));
            let _ = CloseHandle(self.event);
        }
    }
}

enum RenderExit {
    Stopped,
    DeviceError,
}

fn open_render_client(device: &IMMDevice) -> Option<ActiveRenderClient> {
    unsafe {
        let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                warn!("mic: activate virtual microphone: {e}");
                return None;
            }
        };
        let mix = match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => {
                warn!("mic: render mix format: {e}");
                return None;
            }
        };
        let fmt: WAVEFORMATEX = *mix;
        let rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let event = match CreateEventW(None, false, false, None) {
            Ok(e) => e,
            Err(e) => {
                warn!("mic: render event: {e}");
                CoTaskMemFree(Some(mix as *const _));
                return None;
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
            return None;
        }
        if let Err(e) = client.SetEventHandle(event) {
            warn!("mic: render event handle: {e}");
            CoTaskMemFree(Some(mix as *const _));
            let _ = CloseHandle(event);
            return None;
        }
        let render: IAudioRenderClient = match client.GetService() {
            Ok(r) => r,
            Err(e) => {
                warn!("mic: render service: {e}");
                CoTaskMemFree(Some(mix as *const _));
                let _ = CloseHandle(event);
                return None;
            }
        };
        if let Err(e) = client.Start() {
            warn!("mic: start virtual microphone: {e}");
            CoTaskMemFree(Some(mix as *const _));
            let _ = CloseHandle(event);
            return None;
        }
        let buffer_frames = client.GetBufferSize().unwrap_or(4800) as usize;
        info!("mic: virtual microphone {rate} Hz {channels} ch {bits} bit");
        Some(ActiveRenderClient {
            client,
            render,
            event,
            mix,
            rate,
            channels,
            bits,
            buffer_frames,
        })
    }
}

fn pump_render_client(
    active: ActiveRenderClient,
    stop: &AtomicBool,
    ipad: &Arc<Mutex<VecDeque<i16>>>,
    last_ipad: &Arc<Mutex<Option<Instant>>>,
    use_ipad: &AtomicBool,
    pc: &std::sync::mpsc::Receiver<Vec<i16>>,
) -> RenderExit {
    let rate = active.rate;
    let channels = active.channels;
    let bits = active.bits;
    let buffer_frames = active.buffer_frames;
    let mut pc_buf: VecDeque<i16> = VecDeque::new();
    let mut scratch = vec![0i16; CHUNK];
    let mut resample = Resampler::new(RATE, rate);
    let mut last_qlog = Instant::now();
    let mut underruns: u64 = 0;
    let mut last_under: u64 = 0;
    let mut consecutive_get_errors: u32 = 0;

    unsafe {
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
            let padding = match active.client.GetCurrentPadding() {
                Ok(p) => p as usize,
                Err(e) => {
                    warn!("mic: GetCurrentPadding failed ({e}); reopening virtual microphone");
                    return RenderExit::DeviceError;
                }
            };
            let available = buffer_frames.saturating_sub(padding) as u32;
            if available == 0 {
                let _ = WaitForSingleObject(active.event, 200);
                continue;
            }
            // One 10 ms source chunk at 48 kHz becomes out_cap device frames
            // at the endpoint's rate.
            let out_cap = (((CHUNK / CHANNELS) as u64 * rate as u64) / RATE as u64).max(1) as u32;
            let frames = available.min(out_cap);
            let data = match active.render.GetBuffer(frames) {
                Ok(d) => {
                    consecutive_get_errors = 0;
                    d
                }
                Err(e) => {
                    consecutive_get_errors += 1;
                    if consecutive_get_errors >= 5 {
                        warn!("mic: GetBuffer failed repeatedly ({e}); reopening virtual microphone");
                        return RenderExit::DeviceError;
                    }
                    let _ = WaitForSingleObject(active.event, 200);
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
                // as it comes. Above 120 ms skip a few samples per block to
                // absorb clock drift without growing latency toward 1 s.
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
            let src = &scratch[..need];
            write_frames(data, src, frames as usize, channels, bits, rate, &mut resample);
            if let Err(e) = active.render.ReleaseBuffer(frames, 0) {
                warn!("mic: ReleaseBuffer failed ({e}); reopening virtual microphone");
                return RenderExit::DeviceError;
            }
            let _ = WaitForSingleObject(active.event, 200);
        }
    }
    RenderExit::Stopped
}

/// Render `frames` device frames (the exact count GetBuffer granted) from a
/// stereo 48 kHz source chunk.
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
    let want_pc = has_consumers && !want_ipad && !(manual == Some(true) && fresh);
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

    fn str_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn parent_list(pairs: &[(u32, u32)]) -> HashMap<u32, u32> {
        pairs.iter().copied().collect()
    }

    fn roles(c: Option<&str>, m: Option<&str>, comm: Option<&str>) -> CaptureRoles {
        CaptureRoles {
            console: c.map(str::to_string),
            multimedia: m.map(str::to_string),
            communications: comm.map(str::to_string),
        }
    }

    #[test]
    fn idle_keeps_every_microphone_off() {
        let d = decide_mic(None, false, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
    }

    #[test]
    fn a_pin_alone_does_not_keep_the_ipad_microphone_busy() {
        let d = decide_mic(Some(true), false, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
        let d = decide_mic(Some(true), false, false, true);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: false, want_capture: false });
    }

    #[test]
    fn a_pin_serves_a_live_consumer_on_the_primary_screen() {
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
        let d = decide_mic(None, true, true, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: true });
    }

    #[test]
    fn a_primary_screen_consumer_never_starts_the_ipad_microphone() {
        let d = decide_mic(None, true, false, false);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: false });
        let d = decide_mic(None, true, false, true);
        assert_eq!(d, MicDecision { want_ipad: false, want_pc: true, want_capture: false });
    }

    #[test]
    fn pinning_the_pc_side_uses_the_pc_microphone() {
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

    // --- default capture backup / drift / restore tests -------------------

    #[test]
    fn backup_never_records_the_cable_id() {
        let cur = roles(Some("usb-mic"), Some("usb-mic"), Some("cable"));
        let b = build_backup_from_current(&cur, None, "cable", Some("fallback"));
        assert_eq!(b, roles(Some("usb-mic"), Some("usb-mic"), Some("usb-mic")));

        let stuck = roles(Some("cable"), Some("cable"), Some("cable"));
        let existing = roles(Some("usb-mic"), Some("usb-mic"), Some("headset"));
        let b2 = build_backup_from_current(&stuck, Some(&existing), "cable", Some("fallback"));
        assert_eq!(b2, existing);

        let b3 = build_backup_from_current(&stuck, None, "cable", Some("fallback"));
        assert_eq!(b3, roles(Some("fallback"), Some("fallback"), Some("fallback")));
    }

    #[test]
    fn drift_from_sleep_reanchors_without_changing_pc_mic() {
        let cur = roles(Some("usb-mic"), Some("usb-mic"), Some("cable"));
        let backup = roles(Some("usb-mic"), Some("usb-mic"), Some("usb-mic"));
        let active = str_set(&["usb-mic", "cable"]);
        let plan = reconcile_drift(&cur, &backup, "cable", &active);
        assert_eq!(plan.roles_to_reanchor, vec![0, 1]);
        assert_eq!(plan.updated_backup, None);
        assert!(!plan.adopted_primary_changed);
    }

    #[test]
    fn user_switching_to_new_real_mic_adopts_it_and_reanchors() {
        let cur = roles(Some("headset"), Some("headset"), Some("cable"));
        let backup = roles(Some("usb-mic"), Some("usb-mic"), Some("usb-mic"));
        let active = str_set(&["usb-mic", "headset", "cable"]);
        let plan = reconcile_drift(&cur, &backup, "cable", &active);
        assert_eq!(plan.roles_to_reanchor, vec![0, 1]);
        assert!(plan.adopted_primary_changed);
        assert_eq!(
            plan.updated_backup,
            Some(roles(Some("headset"), Some("headset"), Some("usb-mic")))
        );
    }

    #[test]
    fn no_drift_is_a_noop() {
        let cur = roles(Some("cable"), Some("cable"), Some("cable"));
        let backup = roles(Some("usb-mic"), Some("usb-mic"), Some("usb-mic"));
        let active = str_set(&["usb-mic", "cable"]);
        let plan = reconcile_drift(&cur, &backup, "cable", &active);
        assert!(plan.roles_to_reanchor.is_empty());
        assert_eq!(plan.updated_backup, None);
        assert!(!plan.adopted_primary_changed);
    }

    #[test]
    fn restore_recovers_only_roles_on_cable_and_handles_unplugged_backup() {
        let cur = roles(Some("cable"), Some("cable"), Some("cable"));
        let backup = roles(Some("usb-mic"), Some("usb-mic"), Some("headset"));
        let active = str_set(&["usb-mic", "cable"]);
        let actions = plan_restore(Some(&backup), &cur, "cable", &active, Some("webcam"), false);
        assert_eq!(
            actions,
            vec![
                (0, "usb-mic".to_string()),
                (1, "usb-mic".to_string()),
                (2, "usb-mic".to_string()),
            ]
        );

        let split = roles(Some("usb-mic"), Some("usb-mic"), Some("cable"));
        let actions2 = plan_restore(None, &split, "cable", &active, Some("webcam"), true);
        assert_eq!(actions2, vec![(2, "usb-mic".to_string())]);
    }

    #[test]
    fn drift_throttle_backs_off_on_repeated_fight_or_failure() {
        let t0 = Instant::now();
        let mut th = DriftThrottle::default();
        assert!(th.can_attempt(t0));
        assert!(!th.record(t0, false));
        assert!(!th.record(t0 + Duration::from_secs(1), false));
        assert!(th.record(t0 + Duration::from_secs(2), false));
        assert!(!th.can_attempt(t0 + Duration::from_secs(10)));
        assert!(th.can_attempt(t0 + Duration::from_secs(17)));

        let mut th2 = DriftThrottle::default();
        for i in 0..5 {
            assert!(!th2.record(t0 + Duration::from_secs(i), true));
        }
        assert!(th2.record(t0 + Duration::from_secs(5), true));
        assert!(!th2.can_attempt(t0 + Duration::from_secs(12)));
        assert!(th2.can_attempt(t0 + Duration::from_secs(21)));
    }
}
