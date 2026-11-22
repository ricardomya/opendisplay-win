//! Proof for WASAPI process loopback (Windows 10 2004+): captures the audio
//! that one process tree renders, with no virtual cable and therefore no
//! microphone cross-talk.
//!
//!   procloop_probe <pid> [seconds]

use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{GUID, HSTRING, Interface, Ref, Result as WinResult, implement};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, CoInitializeEx, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

// From Win32_Media_Multimedia, which this crate does not enable.
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Done(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
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

fn main() -> windows::core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let pid: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    if pid == 0 {
        println!("usage: procloop_probe <pid> [seconds]");
        return Ok(());
    }
    unsafe {
        let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
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
                    cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                    pBlobData: &mut params as *mut _ as *mut u8,
                },
            };
        }
        let event = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Done(event).into();
        let operation = ActivateAudioInterfaceAsync(
            &HSTRING::from("VAD\\Process_Loopback"),
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )?;
        let wait = WaitForSingleObject(event, 5000);
        if wait != WAIT_OBJECT_0 {
            println!("activation did not complete");
            return Ok(());
        }
        let mut hr = windows::core::HRESULT(0);
        let mut iface: Option<windows::core::IUnknown> = None;
        operation.GetActivateResult(&mut hr, &mut iface)?;
        if hr.is_err() {
            println!("GetActivateResult: {hr:?}");
            return Ok(());
        }
        let client: IAudioClient = iface.expect("audio client").cast()?;

        // Process loopback requires an explicit format: 48 kHz, 32-bit float,
        // stereo (the documented default).
        let fmt = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            2_000_000,
            0,
            &fmt as *const _ as *const WAVEFORMATEX,
            None,
        )?;
        let ready = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(ready)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        println!("process loopback capturing pid {pid} for {secs}s");
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        let mut peak: f32 = 0.0;
        let mut frames_total: u64 = 0;
        while Instant::now() < deadline {
            if WaitForSingleObject(ready, 200) != WAIT_OBJECT_0 {
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
                if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                if frames > 0 && !data.is_null() && (flags & 2) == 0 {
                    let n = frames as usize * 2;
                    let samples = std::slice::from_raw_parts(data as *const f32, n);
                    for s in samples {
                        peak = peak.max(s.abs());
                    }
                    frames_total += frames as u64;
                }
                let _ = capture.ReleaseBuffer(frames);
            }
        }
        let _ = client.Stop();
        println!("frames={frames_total} peak={peak:.6}");
        let _ = CloseHandle(ready);
        let _ = CloseHandle(event);
        let _ = STGM_READ;
    }
    Ok(())
}
