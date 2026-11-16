//! Test helper: a window placed on the iPad display that holds an active
//! **virtual-cable** capture session, so the virtual-mic source selector
//! should switch to the iPad microphone while it is there. Capturing CABLE
//! Output makes it behave like a meeting app using the virtual microphone.
//!
//!   cargo run --example mic_window_test -- [x y] [capture-substring]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, MSG, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static STOP: AtomicBool = AtomicBool::new(false);

fn find_capture(sub: &str) -> Option<IMMDevice> {
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let col = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(device) = col.Item(i) else { continue };
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else { continue };
            let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) else { continue };
            let name = match PropVariantToStringAlloc(&prop) {
                Ok(pw) => {
                    let s = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const _));
                    s
                }
                Err(_) => continue,
            };
            if name.to_lowercase().contains(&sub.to_lowercase()) {
                println!("mic_window_test: capturing {name}");
                return Some(device);
            }
        }
    }
    None
}

fn capture_loop(stop: Arc<AtomicBool>, name: String) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Some(device) = find_capture(&name) else {
            println!("mic_window_test: no capture endpoint matching {name:?}");
            return;
        };
        let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else { return };
        let Ok(mix) = client.GetMixFormat() else { return };
        let event = CreateEventW(None, false, false, None).unwrap();
        if client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix,
                None,
            )
            .is_err()
        {
            return;
        }
        let _ = client.SetEventHandle(event);
        let Ok(cap) = client.GetService::<IAudioCaptureClient>() else { return };
        if client.Start().is_err() {
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 200) != windows::Win32::Foundation::WAIT_OBJECT_0 {
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
                let _ = cap.ReleaseBuffer(frames);
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
        CoTaskMemFree(Some(mix as *const _));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_DESTROY {
        STOP.store(true, Ordering::Relaxed);
        windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn main() {
    opendisplay_win::app::set_dpi_awareness();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let x: i32 = raw.first().and_then(|a| a.parse().ok()).unwrap_or(3900);
    let y: i32 = raw.get(1).and_then(|a| a.parse().ok()).unwrap_or(100);
    let capture = raw
        .get(2)
        .cloned()
        .unwrap_or_else(|| "CABLE Output".into());
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("MicWindowTest");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("MicWindowTest"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            480,
            320,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap();
        let _ = hwnd;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        std::thread::spawn(move || capture_loop(stop_thread, capture));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
