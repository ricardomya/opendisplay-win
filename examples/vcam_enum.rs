//! Enumerate Media Foundation video capture devices and read a few frames
//! from the OpenDisplay virtual camera (if present).
//!
//!   cargo run --example vcam_enum

use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFStartup, IMFMediaType_Impl,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

unsafe fn attr_string(attrs: &IMFAttributes, key: &GUID) -> String {
    let mut pw = PWSTR::null();
    let mut len = 0u32;
    if unsafe { attrs.GetAllocatedString(key, &mut pw, &mut len) }.is_err() {
        return String::new();
    }
    let s = unsafe { pw.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(pw.0 as *const _)) };
    s
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, 0);

        let mut attrs: Option<IMFAttributes> = None;
        if let Err(e) = unsafe { MFCreateAttributes(&mut attrs, 2) } {
            println!("MFCreateAttributes: {e}");
            return;
        }
        let attrs = attrs.unwrap();
        unsafe {
            let _ = attrs.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            );
        }

        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if let Err(e) = unsafe { MFEnumDeviceSources(&attrs, &mut devices, &mut count) } {
            println!("MFEnumDeviceSources: {e}");
            return;
        }
        let list = unsafe { std::slice::from_raw_parts(devices, count as usize) };
        println!("{count} capture device(s)");
        let mut target: Option<IMFActivate> = None;
        for activate in list.iter().flatten() {
            let name = unsafe { attr_string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME) };
            let link =
                unsafe { attr_string(activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK) };
            println!("- {name} {link}");
            if name.contains("OpenDisplay") {
                target = Some(activate.clone());
            }
        }
        unsafe { CoTaskMemFree(Some(devices as *const _)) };

        let Some(activate) = target else {
            println!("no OpenDisplay camera found");
            return;
        };
        let source: IMFMediaSource = match unsafe { activate.ActivateObject() } {
            Ok(s) => s,
            Err(e) => {
                println!("ActivateObject: {e}");
                return;
            }
        };
        let reader: IMFSourceReader = match unsafe { MFCreateSourceReaderFromMediaSource(&source, None) } {
            Ok(r) => r,
            Err(e) => {
                println!("source reader: {e}");
                return;
            }
        };
        if let Ok(mt) = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0) } {
            let mt: IMFMediaType = mt;
            // Ask for the native type so the reader needs no hardware video
            // processor (which can fail with 0xC00D3704 while a camera app is
            // also streaming).
            let _ = unsafe {
                reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &mt)
            };
            let sub = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
            let size = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
            println!(
                "native type: subtype {:?} {}x{}",
                sub,
                size >> 32,
                size & 0xffff_ffff
            );
        }

        let start = Instant::now();
        let mut samples = 0u32;
        let mut bytes = 0u64;
        let mut stats: Option<(u8, u8, u32)> = None; // byte0, byte1, mean of first 1000

        while start.elapsed() < Duration::from_secs(3) && samples < 60 {
            let mut stream_index = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            if let Err(e) = unsafe {
                reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut stream_index),
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )
            } {
                println!("ReadSample: {e}");
                break;
            }
            if flags & 0x2 != 0 {
                continue; // MF_SOURCE_READERF_STREAMTICK
            }
            if let Some(sample) = sample {
                samples += 1;
                if let Ok(buf) = unsafe { sample.ConvertToContiguousBuffer() } {
                    let len = unsafe { buf.GetCurrentLength() }.unwrap_or(0) as usize;
                    bytes += len as u64;
                    if stats.is_none() && len >= 1000 {
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut max = 0u32;
                        let mut cur = 0u32;
                        if unsafe { buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur)) }.is_ok() {
                            let data = unsafe { std::slice::from_raw_parts(ptr, len) };
                            let mean =
                                data[..1000].iter().map(|&v| v as u32).sum::<u32>() / 1000;
                            stats = Some((data[0], data[1], mean));
                            let _ = unsafe { buf.Unlock() };
                        }
                    }
                }
            }
        }
        println!("read {samples} sample(s), {bytes} bytes");
        if let Some((b0, b1, mean)) = stats {
            println!("first sample: byte0={b0} byte1={b1} mean(first 1000)={mean}");
        }
    }
}
