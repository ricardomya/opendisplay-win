//! Self-test for tablet touch mode: creates a window on the iPad display,
//! injects a real touch with the same API the sender uses, and logs the
//! WM_TOUCH / WM_POINTER messages the window receives.
//!
//!   touch_probe [x y]
//!
//! Exit code 0 and `hit=true` mean Windows delivered the synthetic touch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use opendisplay_proto::control::{TouchContact, Touches};
use opendisplay_win::input::Injector;
use windows::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_INFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
    PostMessageW, PostQuitMessage, RegisterClassW, SetWindowPos, TranslateMessage, HWND_TOPMOST,
    MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_SHOWWINDOW, WM_CLOSE, WM_DESTROY, WM_POINTERDOWN, WM_POINTERUPDATE, WM_POINTERUP,
    WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static HIT: AtomicBool = AtomicBool::new(false);


unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => {
            let id = (w.0 & 0xffff) as u32;
            let mut info = POINTER_INFO::default();
            if unsafe { GetPointerInfo(id, &mut info) }.is_ok() {
                println!(
                    "WM_POINTER msg=0x{msg:x} id={id} at ({},{}) flags=0x{:x}",
                    info.ptPixelLocation.x,
                    info.ptPixelLocation.y,
                    info.pointerFlags.0
                );
                if msg == WM_POINTERDOWN {
                    HIT.store(true, Ordering::Relaxed);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

fn main() {
    let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    println!("SetProcessDpiAwarenessContext: {set:?}");
    let awareness = unsafe {
        GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())
    };
    println!("DPI awareness: {awareness:?}");
    let args: Vec<i32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let x = args.first().copied().unwrap_or(4000);
    let y = args.get(1).copied().unwrap_or(200);
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("TouchProbe");
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
            w!("TouchProbe"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            420,
            300,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap();
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 420, 300, SWP_SHOWWINDOW);
        println!("touch probe window at ({x},{y})");

        let hwnd_raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let (vx, vy, vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            println!("virtual screen ({vx},{vy}) {vw}x{vh}");
            let rect = RECT {
                left: vx,
                top: vy,
                right: vx + vw,
                bottom: vy + vh,
            };
            let mut injector = Injector::new(rect);
            let cx = x + 210;
            let cy = y + 160;
            let nx = (cx - vx) as f64 / (vw - 1) as f64;
            let ny = (cy - vy) as f64 / (vh - 1) as f64;
            let dx = 24.0 / (vw - 1) as f64;
            // Two-finger pinch: both contacts down, move apart, lift.
            let snapshot = |spread: f64| Touches {
                contacts: vec![
                    TouchContact {
                        id: 1,
                        x: nx - spread,
                        y: ny,
                    },
                    TouchContact {
                        id: 2,
                        x: nx + spread,
                        y: ny,
                    },
                ],
                cancelled: false,
            };
            injector.touches(&snapshot(dx));
            std::thread::sleep(Duration::from_millis(150));
            injector.touches(&snapshot(dx * 3.0));
            std::thread::sleep(Duration::from_millis(150));
            injector.touches(&Touches {
                contacts: Vec::new(),
                cancelled: false,
            });
            std::thread::sleep(Duration::from_millis(900));
            println!("hit={}", HIT.load(Ordering::Relaxed));
            let _ = PostMessageW(Some(HWND(hwnd_raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
