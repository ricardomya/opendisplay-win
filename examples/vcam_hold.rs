//! Start the sample virtual camera and keep it alive while another program
//! enumerates/previews it. Ctrl-C (or process kill) ends the session.
//!
//!   cargo run --example vcam_hold -- [seconds]

use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let secs: u64 = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(120);
        let name = HSTRING::from("OpenDisplay Camera");
        let source = HSTRING::from("{7B89B92E-FE71-42D0-8A41-E137D06EA184}");
        let camera = match MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_Session,
            MFVirtualCameraAccess_CurrentUser,
            &name,
            &source,
            None,
        ) {
            Ok(c) => c,
            Err(e) => {
                println!("create failed: {e}");
                return;
            }
        };
        match camera.Start(None) {
            Ok(()) => println!("virtual camera started; holding {secs}s"),
            Err(e) => {
                println!("start failed: {e}");
                let _ = camera.Remove();
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(secs));
        let _ = camera.Stop();
        let _ = camera.Remove();
        println!("stopped");
    }
}
