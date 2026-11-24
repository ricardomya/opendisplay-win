//! Decode a raw Annex B H.264 file (AUD-delimited) with the in-box Media
//! Foundation decoder and report the NV12 output size — the decoder path the
//! camera pipeline will use.
//!
//!   ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30:duration=2 \
//!          -c:v libx264 -x264-params aud=1 -f h264 test.h264
//!   cargo run --example h264_decode -- test.h264

use std::mem::ManuallyDrop;

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, MFT_MESSAGE_COMMAND_DRAIN,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFStartup, MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER, MF_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

const CLSID_CMSH264_DECODER: GUID = GUID::from_u128(0x62CE7E72_4C71_4D20_B15D_452831A87D9D);
const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = 0xC00D_6D72u32 as i32;
const MF_E_TRANSFORM_STREAM_CHANGE: i32 = 0xC00D_6D61u32 as i32;

/// Split Annex B into access units at AUD (NAL type 9) boundaries.
fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let sc = if data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        if i + sc < data.len() && data[i + sc] & 0x1F == 9 {
            starts.push(i);
        }
        i += sc;
    }
    let mut out = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let e = starts.get(n + 1).copied().unwrap_or(data.len());
        out.push(&data[s..e]);
    }
    out
}

fn output_sample(width: u32, height: u32) -> windows::core::Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(width * height * 3 / 2 + 4096)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

fn nv12_type(width: u32, height: u32) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?; // progressive
        Ok(mt)
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "test720.h264".into());
    let data = std::fs::read(&path).expect("read h264 file");
    let aus = access_units(&data);
    println!("{} access unit(s), {} bytes", aus.len(), data.len());

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, 0);
        let transform: IMFTransform =
            CoCreateInstance(&CLSID_CMSH264_DECODER, None, CLSCTX_ALL).expect("decoder MFT");

        if let Ok(attrs) = transform.GetAttributes() {
            let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
        }

        let input = MFCreateMediaType().unwrap();
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).unwrap();
        input
            .SetUINT64(&MF_MT_FRAME_SIZE, (1280u64 << 32) | 720)
            .unwrap();
        input
            .SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)
            .unwrap();
        input.SetUINT32(&MF_MT_INTERLACE_MODE, 2).unwrap();
        transform.SetInputType(0, &input, 0).expect("input type");
        transform
            .SetOutputType(0, &nv12_type(1280, 720).unwrap(), 0)
            .expect("output type");
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .unwrap();

        let mut produced = 0u32;
        let mut first_stats: Option<(u8, u32)> = None;
        for (idx, au) in aus.iter().enumerate() {
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(au.len() as u32).unwrap();
            let mut p: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None).unwrap();
            std::ptr::copy_nonoverlapping(au.as_ptr(), p, au.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(au.len() as u32).unwrap();
            let sample: IMFSample = MFCreateSample().unwrap();
            sample.AddBuffer(&buffer).unwrap();
            sample.SetSampleTime(idx as i64 * 333_333).unwrap();
            sample.SetSampleDuration(333_333).unwrap();
            if let Err(e) = transform.ProcessInput(0, &sample, 0) {
                println!("ProcessInput {idx}: {e}");
                continue;
            }
            loop {
                // This decoder does not provide output samples; hand it one.
                let out_sample = match output_sample(1280, 720) {
                    Ok(s) => s,
                    Err(e) => {
                        println!("output sample: {e}");
                        break;
                    }
                };
                let mut out = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let result = transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
                let s = ManuallyDrop::take(&mut out.pSample);
                let _e = ManuallyDrop::take(&mut out.pEvents);
                match result {
                    Ok(()) => {
                        if let Some(s) = s {
                            produced += 1;
                            if first_stats.is_none() {
                                if let Ok(buf) = s.ConvertToContiguousBuffer() {
                                    let len = buf.GetCurrentLength().unwrap_or(0) as usize;
                                    let mut q: *mut u8 = std::ptr::null_mut();
                                    let mut max = 0u32;
                                    let mut cur = 0u32;
                                    if buf.Lock(&mut q, Some(&mut max), Some(&mut cur)).is_ok() {
                                        let v = std::slice::from_raw_parts(q, len);
                                        let mean = v[..1000.min(len)]
                                            .iter()
                                            .map(|&b| b as u32)
                                            .sum::<u32>()
                                            / 1000.min(len) as u32;
                                        first_stats = Some((v[0], mean));
                                        let _ = buf.Unlock();
                                    }
                                }
                            }
                        }
                    }
                    Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code().0 == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let _ = transform.SetOutputType(0, &nv12_type(1280, 720).unwrap(), 0);
                        continue;
                    }
                    Err(e) => {
                        println!("ProcessOutput: {e}");
                        break;
                    }
                }
            }
        }
        transform
            .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
            .ok();
        loop {
            let Ok(out_sample) = output_sample(1280, 720) else { break };
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(out_sample)),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            match transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status) {
                Ok(()) => {
                    let _ = ManuallyDrop::take(&mut out.pSample);
                    produced += 1;
                }
                Err(_) => break,
            }
        }
        println!("decoded {produced} NV12 frame(s)");
        if let Some((b0, mean)) = first_stats {
            println!("first frame: byte0={b0} mean(first 1000)={mean}");
        }
    }
}
