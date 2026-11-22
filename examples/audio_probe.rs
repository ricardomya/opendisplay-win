//! Local verification for per-app audio routing + endpoint loopback capture.
//!
//!   audio_probe list
//!   audio_probe route <pid> <device-name-substring>
//!   audio_probe restore <pid>
//!   audio_probe capture <device-name-substring> <seconds>
//!
//! `route` calls the undocumented Windows.Media.Internal.AudioPolicyConfig
//! factory (the one EarTrumpet uses): vtable slot 25,
//! SetPersistedDefaultAudioEndpoint(pid, flow, role, deviceId).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{link, GUID, HSTRING, HRESULT, Interface};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

link!("api-ms-win-core-winrt-l1-1-0.dll" "system" fn RoGetActivationFactory(
    classid: HSTRING,
    iid: *const GUID,
    factory: *mut *mut c_void
) -> HRESULT);

const POLICY_IID: GUID = GUID::from_u128(0xAB3D4648_E242_459F_B02F_541C70306324);

fn endpoint_id(device: &IMMDevice) -> String {
    unsafe {
        let pw = device.GetId().unwrap();
        let s = pw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pw.0 as *const _));
        s
    }
}

fn pnp_path(id: &str) -> String {
    format!("\\\\?\\SWD#MMDEVAPI#{id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

fn friendly(device: &IMMDevice) -> String {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).unwrap();
        let prop = store.GetValue(&PKEY_Device_FriendlyName).unwrap();
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

fn enumerator() -> IMMDeviceEnumerator {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap() }
}

fn devices() -> Vec<IMMDevice> {
    unsafe {
        let col = enumerator().EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap();
        let n = col.GetCount().unwrap();
        (0..n).map(|i| col.Item(i).unwrap()).collect()
    }
}

fn find_device(sub: &str) -> IMMDevice {
    let lower = sub.to_lowercase();
    devices()
        .into_iter()
        .find(|d| friendly(d).to_lowercase().contains(&lower))
        .unwrap_or_else(|| panic!("no render endpoint matching {sub:?}"))
}

fn activation_factory() -> *mut c_void {
    let mut factory: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        RoGetActivationFactory(
            HSTRING::from("Windows.Media.Internal.AudioPolicyConfig"),
            &POLICY_IID,
            &mut factory,
        )
    };
    if hr.is_err() {
        panic!("AudioPolicyConfig factory activation failed: {hr:?}");
    }
    factory
}

unsafe fn set_persisted(pid: u32, device_id: &str, flow: i32, role: i32) -> HRESULT {
    let factory = activation_factory();
    let vtbl = unsafe { *(factory as *mut *const *const c_void) };
    type SetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, HSTRING) -> HRESULT;
    let f: SetFn = unsafe { std::mem::transmute(*vtbl.add(25)) };
    let value = if device_id.is_empty() {
        HSTRING::new()
    } else {
        HSTRING::from(device_id)
    };
    let hr = unsafe { f(factory, pid, flow, role, value) };
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
    let release: ReleaseFn = unsafe { std::mem::transmute(*vtbl.add(2)) };
    unsafe { release(factory) };
    hr
}

fn route(pid: u32, sub: &str) {
    let device = find_device(sub);
    let id = pnp_path(&endpoint_id(&device));
    println!("routing pid {pid} -> {} ({id})", friendly(&device));
    unsafe {
        for (flow, role) in [(0, 0), (0, 1), (0, 2)] {
            let hr = set_persisted(pid, &id, flow, role);
            println!("  flow {flow} role {role}: {hr:?}");
        }
    }
}

fn restore(pid: u32) {
    unsafe {
        for (flow, role) in [(0, 0), (0, 1), (0, 2)] {
            let hr = set_persisted(pid, "", flow, role);
            println!("  cleared flow {flow} role {role}: {hr:?}");
        }
    }
}

fn get_persisted(pid: u32) {
    unsafe {
        let factory = activation_factory();
        let vtbl = unsafe { *(factory as *mut *const *const c_void) };
        type GetFn = unsafe extern "system" fn(
            *mut c_void,
            u32,
            i32,
            i32,
            *mut HSTRING,
        ) -> HRESULT;
        let f: GetFn = unsafe { std::mem::transmute(*vtbl.add(26)) };
        for (flow, role) in [(0, 0), (0, 1), (0, 2)] {
            let mut value = HSTRING::new();
            let hr = unsafe { f(factory, pid, flow, role, &mut value) };
            println!("  flow {flow} role {role}: {hr:?} {:?}", value.to_string());
        }
        type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
        let release: ReleaseFn = unsafe { std::mem::transmute(*vtbl.add(2)) };
        unsafe { release(factory) };
    }
}

fn sessions_all() {
    for d in devices() {
        unsafe {
            let Ok(mgr) = d.Activate::<windows::Win32::Media::Audio::IAudioSessionManager2>(
                CLSCTX_ALL,
                None,
            ) else {
                continue;
            };
            let Ok(en) = mgr.GetSessionEnumerator() else { continue };
            for i in 0..en.GetCount().unwrap_or(0) {
                let Ok(c) = en.GetSession(i) else { continue };
                let Ok(c2) = c.cast::<windows::Win32::Media::Audio::IAudioSessionControl2>() else {
                    continue;
                };
                let pid = c2.GetProcessId().unwrap_or(0);
                println!("{}\t{pid}\t{}", friendly(&d), process_name(pid));
            }
        }
    }
}

fn capture(sub: &str, secs: f64) -> f32 {
    unsafe {
        let device = find_device(sub);
        println!("capturing {}", friendly(&device));
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix,
                None,
            )
            .unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        client.SetEventHandle(event).unwrap();
        let cap: IAudioCaptureClient = client.GetService().unwrap();
        client.Start().unwrap();

        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let mut peak = 0.0f32;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs_f64(secs) {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let n = cap.GetNextPacketSize().unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let count = frames as usize * channels;
                    if bits == 32 {
                        let s = std::slice::from_raw_parts(data as *const f32, count);
                        for &v in s {
                            let a = v.abs();
                            if a.is_finite() && a > peak {
                                peak = a;
                            }
                        }
                    } else if bits == 16 {
                        let s = std::slice::from_raw_parts(data as *const i16, count);
                        for &v in s {
                            let a = (v as f32 / 32768.0).abs();
                            if a > peak {
                                peak = a;
                            }
                        }
                    }
                }
                let _ = cap.ReleaseBuffer(frames);
            }
        }
        client.Stop().unwrap();
        CoTaskMemFree(Some(mix as *const _));
        let _ = CloseHandle(event);
        peak
    }
}


fn default_device() -> IMMDevice {
    unsafe { enumerator().GetDefaultAudioEndpoint(eRender, eConsole).unwrap() }
}

fn process_name(pid: u32) -> String {
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return String::from("?"),
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::from("?");
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    }
}

fn set_endpoint_volume(sub: &str, level: f32) {
    unsafe {
        let dev = find_device(sub);
        let v: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume =
            dev.Activate(CLSCTX_ALL, None).unwrap();
        v.SetMasterVolumeLevelScalar(level, std::ptr::null()).unwrap();
        println!(
            "{}: master={:.3} mute={:?}",
            friendly(&dev),
            v.GetMasterVolumeLevelScalar().unwrap_or(-1.0),
            v.GetMute().map(|b| b.as_bool())
        );
    }
}

fn endpoint_volume(sub: Option<&str>) {
    unsafe {
        let dev = sub.map(find_device).unwrap_or_else(default_device);
        let v: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume =
            dev.Activate(CLSCTX_ALL, None).unwrap();
        println!(
            "{}: master={:.3} mute={:?}",
            friendly(&dev),
            v.GetMasterVolumeLevelScalar().unwrap_or(-1.0),
            v.GetMute().map(|b| b.as_bool())
        );
    }
}

/// Sustained 440 Hz tone in shared mode (a single buffer plays only ~20 ms).
fn tone_shared(device_sub: &str, secs: f64) {
    use windows::Win32::Media::Audio::IAudioRenderClient;
    unsafe {
        let dev = find_device(device_sub);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        let (ch, bits, rate) = (
            fmt.nChannels as usize,
            fmt.wBitsPerSample as usize,
            fmt.nSamplesPerSec as f64,
        );
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                200_000,
                0,
                mix,
                None,
            )
            .unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        client.SetEventHandle(event).unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let buffer = client.GetBufferSize().unwrap();
        client.Start().unwrap();
        println!("tone -> {} for {secs}s", friendly(&dev));
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        let mut phase = 0.0f64;
        while Instant::now() < deadline {
            let _ = WaitForSingleObject(event, 200);
            let padding = client.GetCurrentPadding().unwrap_or(0);
            let avail = buffer.saturating_sub(padding);
            if avail == 0 {
                continue;
            }
            let Ok(data) = render.GetBuffer(avail) else { break };
            for i in 0..avail as usize {
                let v = (phase * 2.0 * std::f64::consts::PI).sin() * 0.25;
                phase = (phase + 440.0 / rate) % 1.0;
                for c in 0..ch {
                    let idx = i * ch + c;
                    match bits {
                        32 => *((data as *mut f32).add(idx)) = v as f32,
                        16 => *((data as *mut i16).add(idx)) = (v * 32767.0) as i16,
                        _ => {}
                    }
                }
            }
            let _ = render.ReleaseBuffer(avail, 0);
        }
        let _ = client.Stop();
        CoTaskMemFree(Some(mix as *const _));
    }
}

/// Play a 440 Hz tone into a render endpoint (used to test virtual cables).
fn play(device_sub: &str, secs: f64) {
    use windows::Win32::Media::Audio::IAudioRenderClient;
    unsafe {
        let dev = find_device(device_sub);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
            .unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let frames = client.GetBufferSize().unwrap();
        let data = render.GetBuffer(frames).unwrap();
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let rate = fmt.nSamplesPerSec as f64;
        for i in 0..frames as usize {
            let t = i as f64 / rate;
            let v = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.3) as f32;
            for c in 0..channels {
                let idx = i * channels + c;
                match bits {
                    32 => *((data as *mut f32).add(idx)) = v,
                    16 => *((data as *mut i16).add(idx)) = (v * 32767.0) as i16,
                    _ => {}
                }
            }
        }
        render.ReleaseBuffer(frames, 0).unwrap();
        client.Start().unwrap();
        std::thread::sleep(Duration::from_secs_f64(secs));
        client.Stop().unwrap();
        CoTaskMemFree(Some(mix as *const _));
    }
}

/// Sustained 440 Hz tone into `device_sub` in **exclusive** mode. Shared-mode
/// rendering to one VB-CABLE pin shows up in the loopback of its sibling pin
/// on this driver; exclusive streams bypass the shared mixer, so this tests
/// whether the microphone can be kept out of the loopback the sender uses.
fn tone_excl(device_sub: &str, secs: f64) -> Result<(), String> {
    use windows::Win32::Media::Audio::{IAudioRenderClient, AUDCLNT_SHAREMODE_EXCLUSIVE};
    unsafe {
        let dev = find_device(device_sub);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let mix = client.GetMixFormat().map_err(|e| e.to_string())?;
        let fmt: WAVEFORMATEX = *mix;
        // WAVEFORMATEX is packed; copy fields out before formatting them.
        let (fmt_ch, fmt_bits, fmt_rate) =
            (fmt.nChannels, fmt.wBitsPerSample, fmt.nSamplesPerSec);
        let (mut default_period, mut min_period) = (0i64, 0i64);
        let _ = client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period));
        let period = if default_period > 0 { default_period } else { 100_000 };
        let mut inited = client
            .Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                period,
                mix,
                None,
            )
            .is_ok();
        let pcm = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: fmt.nChannels,
            nSamplesPerSec: fmt.nSamplesPerSec,
            nAvgBytesPerSec: fmt.nSamplesPerSec * fmt.nChannels as u32 * 2,
            nBlockAlign: fmt.nChannels * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut used_pcm = false;
        let mut channels = fmt.nChannels as usize;
        let mut rate = fmt.nSamplesPerSec as f64;
        let mut bits = fmt.wBitsPerSample as usize;
        if !inited {
            inited = client
                .Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    period,
                    period,
                    &pcm,
                    None,
                )
                .is_ok();
            if inited {
                used_pcm = true;
                channels = pcm.nChannels as usize;
                rate = pcm.nSamplesPerSec as f64;
                bits = 16;
            }
        }
        let result = if !inited {
            Err(format!(
                "exclusive Initialize failed (mix {fmt_ch} ch {fmt_bits} bit {fmt_rate} Hz)"
            ))
        } else {
            let event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
            client.SetEventHandle(event).map_err(|e| e.to_string())?;
            let render: IAudioRenderClient = client.GetService().map_err(|e| e.to_string())?;
            let buffer = client.GetBufferSize().map_err(|e| e.to_string())?;
            client.Start().map_err(|e| e.to_string())?;
            println!(
                "exclusive tone -> {} ({}{} ch {} Hz)",
                friendly(&dev),
                if used_pcm { "pcm16, " } else { "mix, " },
                channels,
                rate
            );
            let deadline = Instant::now() + Duration::from_secs_f64(secs);
            while Instant::now() < deadline {
                let _ = WaitForSingleObject(event, 200);
                let padding = client.GetCurrentPadding().unwrap_or(0);
                let avail = buffer.saturating_sub(padding);
                if avail == 0 {
                    continue;
                }
                let Ok(data) = render.GetBuffer(avail) else { break };
                for i in 0..avail as usize {
                    let t = i as f64 / rate;
                    let v = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.25;
                    for c in 0..channels {
                        let idx = i * channels + c;
                        match bits {
                            32 => *((data as *mut f32).add(idx)) = v as f32,
                            16 => *((data as *mut i16).add(idx)) = (v * 32767.0) as i16,
                            _ => {}
                        }
                    }
                }
                let _ = render.ReleaseBuffer(avail, 0);
            }
            let _ = client.Stop();
            Ok(())
        };
        CoTaskMemFree(Some(mix as *const _));
        result
    }
}

/// Play a 440 Hz tone in alternating 2 s windows into `render_sub` while
/// recording `capture_sub`; report the 440 Hz amplitude in 0.5 s bins.
///
/// The sender loops the virtual endpoint into `ODAU`, so with the iPad as the
/// microphone source this measures the complete echo path: virtual endpoint ->
/// iPad speaker -> room -> iPad mic -> `ODMI` -> CABLE Input -> CABLE Output.
/// A working echo canceller keeps `tone_on` near `tone_off` (ratio ~1).
fn echo_probe(render_sub: &str, capture_sub: &str, secs: f64) {
    let stop = Arc::new(AtomicBool::new(false));
    let r_sub = render_sub.to_string();
    let c_sub = capture_sub.to_string();
    let stop_r = stop.clone();
    let renderer = std::thread::spawn(move || tone_loop(&r_sub, secs, stop_r));
    let (samples, rate) = capture_timeline(&c_sub, secs);
    stop.store(true, Ordering::Relaxed);
    renderer.join().ok();

    let bin_len = (rate * 0.5).round() as usize;
    let mut on = Vec::new();
    let mut off = Vec::new();
    for (i, chunk) in samples.chunks(bin_len).enumerate() {
        if chunk.len() < bin_len / 2 {
            break;
        }
        let t = (i as f64 + 0.5) * 0.5;
        let tone = goertzel_amp(chunk, rate, 440.0);
        let rms = (chunk.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>()
            / chunk.len() as f64)
            .sqrt();
        println!("t={t:5.2} tone={tone:.6} rms={rms:.6}");
        if (t >= 1.75 && t < 2.75) || (t >= 5.75 && t < 6.75) {
            on.push(tone);
        } else if (t >= 3.75 && t < 4.75) || (t >= 7.75 && t < 8.75) {
            off.push(tone);
        }
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let on = mean(&on);
    let off = mean(&off);
    println!(
        "tone_on={on:.6} tone_off={off:.6} ratio={:.2}",
        if off > 1e-9 { on / off } else { f64::INFINITY }
    );
}

fn tone_loop(device_sub: &str, secs: f64, stop: Arc<AtomicBool>) {
    use windows::Win32::Media::Audio::IAudioRenderClient;
    unsafe {
        let dev = find_device(device_sub);
        println!("tone -> {}", friendly(&dev));
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
            .unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let frames = client.GetBufferSize().unwrap() as usize;
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let rate = fmt.nSamplesPerSec as f64;
        client.Start().unwrap();
        let start = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let t = start.elapsed().as_secs_f64();
            if t >= secs {
                break;
            }
            let padding = client.GetCurrentPadding().unwrap_or(0) as usize;
            let avail = frames.saturating_sub(padding);
            if avail == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let n = avail.min(480);
            let data = render.GetBuffer(n as u32).unwrap();
            for i in 0..n {
                let ts = t + i as f64 / rate;
                // 1 s lead-in, then 2 s tone / 2 s silence.
                let window = (ts / 2.0) as u64;
                let active = ts >= 1.0 && window % 2 == 0;
                let v = if active {
                    ((2.0 * std::f64::consts::PI * 440.0 * ts).sin() * 0.25) as f32
                } else {
                    0.0
                };
                for c in 0..channels {
                    let idx = i * channels + c;
                    match bits {
                        32 => *((data as *mut f32).add(idx)) = v,
                        16 => *((data as *mut i16).add(idx)) = (v * 32767.0) as i16,
                        _ => {}
                    }
                }
            }
            render.ReleaseBuffer(n as u32, 0).unwrap();
        }
        client.Stop().unwrap();
        CoTaskMemFree(Some(mix as *const _));
    }
}

fn capture_timeline(device_sub: &str, secs: f64) -> (Vec<f32>, f64) {
    let dev = find_capture_device(device_sub);
    unsafe {
        println!("capture <- {}", friendly(&dev));
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix,
                None,
            )
            .unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        client.SetEventHandle(event).unwrap();
        let cap: IAudioCaptureClient = client.GetService().unwrap();
        client.Start().unwrap();
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let rate = fmt.nSamplesPerSec as f64;
        let mut out: Vec<f32> = Vec::new();
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < secs {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let n = cap.GetNextPacketSize().unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if cap
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    for f in 0..frames as usize {
                        let idx = f * channels;
                        let v = match bits {
                            32 => *((data as *const f32).add(idx)),
                            16 => *((data as *const i16).add(idx)) as f32 / 32768.0,
                            _ => 0.0,
                        };
                        out.push(v);
                    }
                }
                let _ = cap.ReleaseBuffer(frames);
            }
        }
        client.Stop().unwrap();
        CoTaskMemFree(Some(mix as *const _));
        let _ = CloseHandle(event);
        (out, rate)
    }
}

fn goertzel_amp(samples: &[f32], rate: f64, freq: f64) -> f64 {
    let n = samples.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    if power <= 0.0 {
        0.0
    } else {
        2.0 * power.sqrt() / n
    }
}

fn find_capture_device(sub: &str) -> IMMDevice {
    let lower = sub.to_lowercase();
    let col = unsafe {
        enumerator()
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .unwrap()
    };
    for i in 0..unsafe { col.GetCount() }.unwrap() {
        let d = unsafe { col.Item(i) }.unwrap();
        if friendly(&d).to_lowercase().contains(&lower) {
            return d;
        }
    }
    panic!("no capture endpoint matching {sub:?}")
}

/// Peak level of a capture endpoint's live input (not loopback).
fn capture_input(device_sub: &str, secs: f64) -> f32 {
    capture_device(find_capture_device(device_sub), secs)
}

fn capture_default(secs: f64) -> f32 {
    let dev = unsafe { enumerator().GetDefaultAudioEndpoint(eCapture, eConsole).unwrap() };
    capture_device(dev, secs)
}

fn capture_device(dev: IMMDevice, secs: f64) -> f32 {
    use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let fmt: WAVEFORMATEX = *mix;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix,
                None,
            )
            .unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        client.SetEventHandle(event).unwrap();
        let cap: IAudioCaptureClient = client.GetService().unwrap();
        client.Start().unwrap();
        let channels = fmt.nChannels as usize;
        let bits = fmt.wBitsPerSample as usize;
        let mut peak = 0.0f32;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs_f64(secs) {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let n = cap.GetNextPacketSize().unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let count = frames as usize * channels;
                    match bits {
                        32 => {
                            let s = std::slice::from_raw_parts(data as *const f32, count);
                            for &v in s {
                                let a = v.abs();
                                if a.is_finite() && a > peak {
                                    peak = a;
                                }
                            }
                        }
                        16 => {
                            let s = std::slice::from_raw_parts(data as *const i16, count);
                            for &v in s {
                                let a = (v as f32 / 32768.0).abs();
                                if a > peak {
                                    peak = a;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let _ = cap.ReleaseBuffer(frames);
            }
        }
        client.Stop().unwrap();
        CoTaskMemFree(Some(mix as *const _));
        let _ = CloseHandle(event);
        peak
    }
}

/// Route one app's **recording** default to a capture endpoint (flow 1).
fn route_input(pid: u32, sub: &str) {
    let dev = find_capture_device(sub);
    let id = pnp_path(&endpoint_id(&dev));
    println!("routing pid {pid} input -> {} ({id})", friendly(&dev));
    unsafe {
        for role in [0, 1] {
            let hr = set_persisted(pid, &id, 1, role);
            println!("  flow 1 role {role}: {hr:?}");
        }
    }
}

fn restore_input(pid: u32) {
    unsafe {
        for role in [0, 1] {
            let hr = set_persisted(pid, "", 1, role);
            println!("  cleared flow 1 role {role}: {hr:?}");
        }
    }
}

fn capture_devices() -> Vec<IMMDevice> {
    unsafe {
        let col = enumerator().EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).unwrap();
        (0..col.GetCount().unwrap()).map(|i| col.Item(i).unwrap()).collect()
    }
}

/// Which capture endpoint every process is recording from right now.
fn mic_sessions() {
    unsafe {
        let default_id = enumerator()
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()
            .map(|d| endpoint_id(&d));
        for d in capture_devices() {
            let id = endpoint_id(&d);
            let is_default = Some(&id) == default_id.as_ref();
            println!("[{}] {}", if is_default { "default" } else { "capture" }, friendly(&d));
            let Ok(mgr) = d.Activate::<windows::Win32::Media::Audio::IAudioSessionManager2>(
                CLSCTX_ALL,
                None,
            ) else {
                continue;
            };
            let Ok(en) = mgr.GetSessionEnumerator() else { continue };
            for i in 0..en.GetCount().unwrap_or(0) {
                let Ok(c) = en.GetSession(i) else { continue };
                let pid = c
                    .cast::<windows::Win32::Media::Audio::IAudioSessionControl2>()
                    .map(|c2| c2.GetProcessId().unwrap_or(0))
                    .unwrap_or(0);
                let state = c.GetState().map(|s| s.0).unwrap_or(-1);
                println!("    pid {pid} state {state} {}", process_name(pid));
            }
        }
    }
}

fn sessions() {
    unsafe {
        let dev = default_device();
        println!("default: {} ({})", friendly(&dev), endpoint_id(&dev));
        let mgr: windows::Win32::Media::Audio::IAudioSessionManager2 =
            dev.Activate(CLSCTX_ALL, None).unwrap();
        let en = mgr.GetSessionEnumerator().unwrap();
        let n = en.GetCount().unwrap();
        for i in 0..n {
            let c = en.GetSession(i).unwrap();
            let c2: windows::Win32::Media::Audio::IAudioSessionControl2 = c.cast().unwrap();
            let pid = c2.GetProcessId().unwrap_or(0);
            let state = c.GetState().map(|s| s.0).unwrap_or(-1);
            let vol = c
                .cast::<windows::Win32::Media::Audio::ISimpleAudioVolume>()
                .ok();
            let muted = vol
                .as_ref()
                .and_then(|v| v.GetMute().ok())
                .map(|b| b.as_bool())
                .unwrap_or(false);
            let level = vol
                .as_ref()
                .and_then(|v| v.GetMasterVolume().ok())
                .unwrap_or(-1.0);
            println!(
                "{pid}\tstate={state}\tmute={muted}\tvol={level:.3}\t{}",
                process_name(pid)
            );
        }
    }
}

fn main() {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("default") => {
            let d = default_device();
            println!("{} ({})", friendly(&d), endpoint_id(&d));
        }
        Some("sessions") => sessions(),
        Some("mic-sessions") => mic_sessions(),
        Some("pair") => {
            let r = args[2].clone();
            let c = args[3].clone();
            let secs: f64 = args[4].parse().unwrap();
            let t = std::thread::spawn(move || play(&r, secs));
            let peak = capture_input(&c, secs);
            t.join().ok();
            println!("peak {peak:.6}");
        }
        Some("set-default") => set_default(&args[2], &args[3]),
        Some("capture-input") => {
            let peak = capture_input(&args[2], args[3].parse().unwrap());
            println!("peak {peak:.6}");
        }
        Some("echo") => {
            echo_probe(&args[2], &args[3], args[4].parse().unwrap());
        }
        Some("tone") => {
            tone_shared(&args[2], args[3].parse().unwrap());
        }
        Some("set-mute") => {
            unsafe {
                let dev = find_device(&args[2]);
                let v: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume =
                    dev.Activate(CLSCTX_ALL, None).unwrap();
                let on = args[3] != "0";
                v.SetMute(on, std::ptr::null()).unwrap();
                println!(
                    "{}: mute={:?}",
                    friendly(&dev),
                    v.GetMute().map(|b| b.as_bool())
                );
            }
        }
        Some("exclpair") => {
            // Exclusive-mode tone into one render endpoint, shared-mode
            // loopback capture of another: does exclusive audio bypass the
            // cross-pin mixing that a shared-mode render shows?
            let r = args[2].clone();
            let c = args[3].clone();
            let secs: f64 = args[4].parse().unwrap();
            let t = std::thread::spawn(move || match tone_excl(&r, secs) {
                Ok(()) => {}
                Err(e) => eprintln!("exclusive tone failed: {e}"),
            });
            let peak = capture(&c, secs);
            t.join().ok();
            println!("peak {peak:.6}");
        }
        Some("exclcap") => {
            // Exclusive-mode tone into a render endpoint, capture endpoint
            // read: the microphone must still reach apps this way.
            let r = args[2].clone();
            let c = args[3].clone();
            let secs: f64 = args[4].parse().unwrap();
            let t = std::thread::spawn(move || match tone_excl(&r, secs) {
                Ok(()) => {}
                Err(e) => eprintln!("exclusive tone failed: {e}"),
            });
            let peak = capture_input(&c, secs);
            t.join().ok();
            println!("peak {peak:.6}");
        }
        Some("looppair") => {
            // Tone into one render endpoint, loopback-capture another render
            // endpoint: detects cross-talk between two render pins of the same
            // virtual cable.
            let r = args[2].clone();
            let c = args[3].clone();
            let secs: f64 = args[4].parse().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_r = stop.clone();
            let t = std::thread::spawn(move || tone_loop(&r, secs, stop_r));
            let peak = capture(&c, secs);
            stop.store(true, Ordering::Relaxed);
            t.join().ok();
            println!("peak {peak:.6}");
        }
        Some("capture-default") => {
            let peak = capture_default(args[2].parse().unwrap());
            println!("peak {peak:.6}");
        }
        Some("route-in") => route_input(args[2].parse().unwrap(), &args[3]),
        Some("restore-in") => restore_input(args[2].parse().unwrap()),
        Some("list") => {
            for (i, d) in devices().into_iter().enumerate() {
                println!("{i}\t{}\t{}", friendly(&d), endpoint_id(&d));
            }
        }
        Some("route") => route(args[2].parse().unwrap(), &args[3]),
        Some("restore") => restore(args[2].parse().unwrap()),
        Some("get") => get_persisted(args[2].parse().unwrap()),
        Some("sessions-all") => sessions_all(),
        Some("endpoint-volume") => endpoint_volume(args.get(2).map(String::as_str)),
        Some("set-volume") => {
            set_endpoint_volume(&args[2], args[3].parse().unwrap());
        }
        Some("capture") => {
            let name = if args[2] == "default" {
                friendly(&default_device())
            } else {
                args[2].clone()
            };
            let peak = capture(&name, args[3].parse().unwrap());
            println!("peak {peak:.6}");
        }
        _ => println!("usage: audio_probe list | route <pid> <substr> | restore <pid> | capture <substr> <secs>"),
    }
}

/// Set the system default endpoint with the undocumented IPolicyConfig COM
/// object (slot 13 = SetDefaultEndpoint), for all three roles.
fn set_default(kind: &str, sub: &str) {
    const POLICY_CLSID: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    let device = if kind == "capture" {
        find_capture_device(sub)
    } else {
        find_device(sub)
    };
    let id = endpoint_id(&device);
    let wide = HSTRING::from(id.as_str());
    unsafe {
        let factory: windows::core::IUnknown =
            CoCreateInstance(&POLICY_CLSID, None, CLSCTX_ALL).unwrap();
        let raw = factory.as_raw();
        let vtbl = *(raw as *mut *const *const std::ffi::c_void);
        type SetDefaultFn =
            unsafe extern "system" fn(*mut std::ffi::c_void, windows::core::PCWSTR, i32) -> HRESULT;
        let f: SetDefaultFn = std::mem::transmute(*vtbl.add(13));
        println!("default {kind} -> {}", friendly(&device));
        for role in 0..3 {
            let hr = f(raw, windows::core::PCWSTR(wide.as_ptr()), role);
            println!("  role {role}: {hr:?}");
        }
    }
}
