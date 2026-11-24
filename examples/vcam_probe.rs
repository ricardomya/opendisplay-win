//! Does this Windows build support the Media Foundation virtual camera API?
//!
//!   cargo run --example vcam_probe
//!
//! Prints whether MFVirtualCameraType_SoftwareCameraSource is supported and
//! what MFCreateVirtualCamera returns for a placeholder source id. No camera
//! is left behind: the object is only created, never started, and removed.

use windows::core::HSTRING;
use windows::Win32::Media::KernelStreaming::KSCATEGORY_VIDEO_CAMERA;
use windows::Win32::Media::MediaFoundation::{
    MFCreateVirtualCamera, MFIsVirtualCameraTypeSupported, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match MFIsVirtualCameraTypeSupported(MFVirtualCameraType_SoftwareCameraSource) {
            Ok(supported) => println!("software camera source supported: {}", supported.as_bool()),
            Err(e) => println!("MFIsVirtualCameraTypeSupported failed: {e}"),
        }

        let name = HSTRING::from("OpenDisplay Camera Probe");
        // Placeholder CLSID: creation may succeed even when the source is not
        // registered; Start is what needs the real media source.
        let source = HSTRING::from("{7B89B92E-FE71-42D0-8A41-E137D06EA184}");
        let categories = [KSCATEGORY_VIDEO_CAMERA];
        match MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_Session,
            MFVirtualCameraAccess_CurrentUser,
            &name,
            &source,
            Some(&categories),
        ) {
            Ok(camera) => {
                println!("MFCreateVirtualCamera: ok ({camera:?})");
                match camera.Start(None) {
                    Ok(()) => {
                        println!("Start: ok");
                        let _ = camera.Stop();
                    }
                    Err(e) => println!("Start: {e}"),
                }
                let _ = camera.Remove();
            }
            Err(e) => println!("MFCreateVirtualCamera: {e}"),
        }
    }
}
