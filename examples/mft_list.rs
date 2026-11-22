//! List Media Foundation video encoders for H.264 and HEVC.
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

fn list(name: &str, out_sub: &windows::core::GUID) {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: *out_sub,
        };
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let hr = MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_ALL,
            Some(&input),
            Some(&output),
            None,
            &mut arr,
            &mut count,
        );
        println!("{name}: hr={hr:?} count={count}");
        if hr.is_ok() && !arr.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*arr.add(i)).take() {
                    let mut n = String::from("?");
                    let mut p = windows::core::PWSTR::null();
                    let mut len = 0;
                    if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
                        n = p.to_string().unwrap_or(n);
                        CoTaskMemFree(Some(p.0 as _));
                    }
                    println!("  - {n}");
                }
            }
            CoTaskMemFree(Some(arr as _));
        }
    }
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, 0);
        list("H264", &MFVideoFormat_H264);
        list("HEVC", &MFVideoFormat_HEVC);
        let _ = MFShutdown();
    }
}
