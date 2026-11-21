//! Local end-to-end check of the screen-routed audio pipeline, no iPad needed.
//!
//!   cargo run --example audio_e2e -- <left> <top> <right> <bottom> <seconds>
//!
//! Play something in a window whose centre is inside that rect (the rect the
//! sender would use for the iPad monitor). The check:
//!   1. the tone is audible on the default (PC) endpoint,
//!   2. `audio::start` produces `ODAU` frames whose peak matches it,
//!   3. while routed, the default endpoint goes quiet (PC speakers silent),
//!   4. after `AudioSession::stop` the route is restored and the tone is back.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{Interface, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, RECT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};

fn enumerator() -> IMMDeviceEnumerator {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap() }
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

fn device_by_name(sub: &str) -> Option<IMMDevice> {
    unsafe {
        let default = enumerator().GetDefaultAudioEndpoint(eRender, eConsole).ok();
        if sub == "default" {
            return default;
        }
        let lower = sub.to_lowercase();
        let col = enumerator()
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .ok()?;
        for i in 0..col.GetCount().ok()? {
            if let Ok(d) = col.Item(i) {
                if friendly(&d).to_lowercase().contains(&lower) {
                    return Some(d);
                }
            }
        }
        None
    }
}

/// Peak level on an endpoint's loopback for `secs`, 0.0..=1.0.
fn endpoint_peak(sub: &str, secs: f64) -> f32 {
    unsafe {
        let Some(device) = device_by_name(sub) else {
            println!("endpoint_peak: no device {sub:?}");
            return -1.0;
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
        let mix = client.GetMixFormat().unwrap();
        let channels = (*mix).nChannels as usize;
        let bits = (*mix).wBitsPerSample as usize;
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
                if cap
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
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

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::from("?");
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
            return String::from("?");
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    }
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let args: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: f64| -> f64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d) };
    let rect = RECT {
        left: num(1, 0.0) as i32,
        top: num(2, 0.0) as i32,
        right: num(3, 1280.0) as i32,
        bottom: num(4, 720.0) as i32,
    };
    let secs = num(5, 10.0);
    println!("rect {rect:?}, {secs}s");

    let pre = endpoint_peak("default", 1.5);
    println!("pre-default-peak {pre:.6}");

    let (tx, rx): (mpsc::Sender<(Vec<u8>, bool)>, Receiver<(Vec<u8>, bool)>) = mpsc::channel();
    let frames = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicI32::new(0));
    let collector = {
        let frames = frames.clone();
        let peak = peak.clone();
        thread::spawn(move || {
            while let Ok((bytes, _)) = rx.recv() {
                parse_frames(&bytes, &frames, &peak);
            }
        })
    };

    let session = opendisplay_win::audio::start(tx, Arc::new(Mutex::new(rect)));
    println!("collecting for {secs}s (4s warm-up) ...");
    thread::sleep(Duration::from_secs_f64(4.0_f64.min(secs)));

    let stop_peak = Arc::new(AtomicBool::new(false));
    let during = {
        let stop_peak = stop_peak.clone();
        thread::spawn(move || {
            let mut peak = 0.0f32;
            while !stop_peak.load(Ordering::Relaxed) {
                let p = endpoint_peak("default", 1.0);
                if p > peak {
                    peak = p;
                }
            }
            peak
        })
    };
    thread::sleep(Duration::from_secs_f64((secs - 4.0).max(0.0)));
    stop_peak.store(true, Ordering::Relaxed);
    let during_peak = during.join().unwrap();
    println!("odau-frames {}", frames.load(Ordering::Relaxed));
    println!("captured-peak {:.6}", peak.load(Ordering::Relaxed) as f32 / 32767.0);
    println!("during-default-peak {during_peak:.6}");

    session.stop();
    let after = endpoint_peak("default", 1.5);
    println!("after-default-peak {after:.6}");

    drop(collector);

    // Levels are relative: the PC's master volume is user state (here 14%),
    // so absolute thresholds would test the volume slider, not the routing.
    let captured = peak.load(Ordering::Relaxed) as f32 / 32767.0;
    let mut pass = true;
    if pre < 0.001 {
        println!("FAIL: nothing was playing on the default endpoint before the test");
        pass = false;
    }
    if frames.load(Ordering::Relaxed) < 50 {
        println!("FAIL: too few ODAU frames were produced");
        pass = false;
    }
    if captured < 0.01 {
        println!("FAIL: captured audio is silent");
        pass = false;
    }
    if during_peak > pre * 0.25 {
        println!("FAIL: the PC speakers were not silent while routed");
        pass = false;
    }
    if after < pre * 0.5 {
        println!("FAIL: the route was not restored to the PC speakers");
        pass = false;
    }
    println!("{}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}

/// Minimal wire-frame parse so the collector sees only audio payloads.
fn parse_frames(bytes: &[u8], frames: &AtomicU64, peak: &AtomicI32) {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        i += 4;
        if i + len > bytes.len() {
            break;
        }
        let payload = &bytes[i..i + len];
        i += len;
        if payload.len() > 8 && &payload[..4] == b"ODAU" {
            frames.fetch_add(1, Ordering::Relaxed);
            let mut local = 0i32;
            for pair in payload[8..].chunks_exact(2) {
                let v = i16::from_le_bytes([pair[0], pair[1]]) as i32;
                let a = v.abs();
                if a > local {
                    local = a;
                }
            }
            if local > peak.load(Ordering::Relaxed) {
                peak.store(local, Ordering::Relaxed);
            }
        }
    }
}
