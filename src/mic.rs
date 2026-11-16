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

use std::collections::{HashSet, VecDeque};
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
    // Start silent: a receiver left capturing from an older session must stop.
    notify(false);
    while !stop.load(Ordering::Relaxed) {
        let target = {
            let guard = rect.lock().unwrap_or_else(|e| e.into_inner());
            *guard
        };
        let on_ipad = if target.right > target.left {
            unsafe { crate::audio::pids_on_rect(target, own_pid) }
        } else {
            HashSet::new()
        };
        // Only sessions on the virtual cable matter: an app recording from the
        // real microphone has nothing to do with this bridge.
        let consumers = cable_session_pids();
        let ipad_recordings = consumers.iter().filter(|p| on_ipad.contains(p)).count();
        let ipad_app = ipad_recordings > 0;
        let fresh = last_ipad
            .lock()
            .ok()
            .and_then(|t| *t)
            .map(|t| t.elapsed() < IPAD_FRESH)
            .unwrap_or(false);
        let want_ipad = ipad_app && fresh;
        // If the iPad microphone stalls (iOS interruption), keep the meeting
        // audible with the PC microphone until the iPad recovers.
        let want_pc = !consumers.is_empty() && !want_ipad;
        use_ipad.store(want_ipad, Ordering::Relaxed);
        pc_enabled.store(want_pc, Ordering::Relaxed);

        // Capture on the iPad only while an app **on its screen** actually
        // consumes the virtual mic: no green mic indicator (and no battery
        // use) while the meeting runs on the primary screen or nobody is
        // recording at all.
        if ipad_app != last_notified {
            notify(ipad_app);
            last_notified = ipad_app;
        }

        let mode = if consumers.is_empty() {
            "idle"
        } else if want_ipad {
            "iPad"
        } else {
            "PC"
        };
        if last_mode != Some(mode) {
            info!(
                "mic: source -> {mode} ({} app(s) on the virtual mic, {} on the iPad screen)",
                consumers.len(),
                ipad_recordings
            );
            last_mode = Some(mode);
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
                if ctrl.GetState().map(|s| s.0).unwrap_or(0) != 1 {
                    continue; // Active only
                }
                let pid = ctrl2.GetProcessId().unwrap_or(0);
                if pid != 0 {
                    out.insert(pid);
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

fn capture_pc_once(
    stop: &AtomicBool,
    enabled: &AtomicBool,
    tx: &std::sync::mpsc::SyncSender<Vec<i16>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Some(en) = enumerator() else { return };
        let Ok(device) = en.GetDefaultAudioEndpoint(eCapture, eConsole) else {
            warn!("mic: no default microphone");
            return;
        };
        // Safety: if the system default recording device is the cable itself
        // (the VB-CABLE installer sets it), capturing it would feed the cable
        // back into itself.
        let cable = std::env::var("OD_VIRTUAL_MIC_CAPTURE")
            .unwrap_or_else(|_| "CABLE Output".into())
            .to_lowercase();
        if friendly(&device).to_lowercase().contains(&cable) {
            warn!("mic: default microphone is the virtual cable; not capturing it");
            return;
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
            1_000_000,
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
        info!("mic: PC microphone {rate} Hz {channels} ch {bits} bit");

        let mut converter = Converter::new(rate, channels, bits);
        while !stop.load(Ordering::Relaxed) && enabled.load(Ordering::Relaxed) {
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
        if let Err(e) = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            1_000_000,
            0,
            mix,
            None,
        ) {
            warn!("mic: initialize virtual microphone: {e}");
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        let render: IAudioRenderClient = match client.GetService() {
            Ok(r) => r,
            Err(e) => {
                warn!("mic: render service: {e}");
                CoTaskMemFree(Some(mix as *const _));
                return;
            }
        };
        if client.Start().is_err() {
            CoTaskMemFree(Some(mix as *const _));
            return;
        }
        info!("mic: virtual microphone {rate} Hz {channels} ch {bits} bit");

        let buffer_frames = client.GetBufferSize().unwrap_or(4800) as usize;
        let mut pc_buf: VecDeque<i16> = VecDeque::new();
        let mut scratch = vec![0i16; CHUNK];
        let mut resample = Resampler::new(RATE, rate);

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

            let padding = client.GetCurrentPadding().unwrap_or(0) as usize;
            let available = buffer_frames.saturating_sub(padding) as u32;
            if available == 0 {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let frames = available.min((CHUNK / CHANNELS) as u32);
            let data = match render.GetBuffer(frames) {
                Ok(d) => d,
                Err(_) => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            let need = frames as usize * CHANNELS;
            for v in scratch.iter_mut().take(need) {
                *v = 0;
            }
            let mut have = 0usize;
            if ipad_ok {
                let mut buf = ipad.lock().unwrap_or_else(|e| e.into_inner());
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
            // Convert the 48 kHz stereo s16 chunk to the device format.
            let src = &scratch[..need];
            write_frames(data, src, channels, bits, rate, &mut resample);
            let _ = render.ReleaseBuffer(frames, 0);
            thread::sleep(Duration::from_millis(5));
        }

        let _ = client.Stop();
        CoTaskMemFree(Some(mix as *const _));
    }
}

fn write_frames(
    data: *mut u8,
    src: &[i16],
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
    let frames = converted.len() / 2;
    for f in 0..frames {
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
