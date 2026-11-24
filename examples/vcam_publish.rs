//! Publish synthetic NV12 frames to the OpenDisplay virtual camera shared
//! memory, so the DLL's shared-memory path can be verified without an iPad.
//!
//!   cargo run --example vcam_publish -- [seconds]

use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};

const MAGIC: u32 = 0x4F44_5643; // 'ODVC'
const VERSION: u32 = 1;
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const STRIDE: u32 = 1280;
const FORMAT_NV12: u32 = 1;
const SLOTS: u32 = 3;
const HEADER: usize = 64;

fn main() {
    unsafe {
        let secs: u64 = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let slot_size = (STRIDE * HEIGHT * 3 / 2) as usize;
        let total = HEADER + SLOTS as usize * slot_size;

        // The camera Frame Server runs as LOCAL SERVICE, which is not in a
        // user object's default DACL; grant it (plus SYSTEM and the owner)
        // access or the media source cannot open this mapping.
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let sddl = HSTRING::from("D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;OW)");
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut sd,
            None,
        ) {
            println!("security descriptor: {e}");
            return;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };

        let mapping: HANDLE = match CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(&sa),
            PAGE_READWRITE,
            0,
            total as u32,
            &HSTRING::from("Global\\OpenDisplayVCam"),
        ) {
            Ok(h) => h,
            Err(e) => {
                println!("CreateFileMapping: {e}");
                let _ = LocalFree(Some(HLOCAL(sd.0)));
                return;
            }
        };
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if mapping.is_invalid() {
            println!("CreateFileMapping returned an invalid handle");
            return;
        }
        let mapped = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, total);
        if mapped.Value.is_null() {
            println!("MapViewOfFile failed");
            let _ = CloseHandle(mapping);
            return;
        }
        let view = mapped.Value as *mut u8;

        // Header (matches vcam/OduFrameSource.h).
        let put32 = |off: usize, v: u32| {
            std::ptr::write_unaligned(view.add(off) as *mut u32, v);
        };
        put32(0, MAGIC);
        put32(4, VERSION);
        put32(8, WIDTH);
        put32(12, HEIGHT);
        put32(16, STRIDE);
        put32(20, FORMAT_NV12);
        put32(24, SLOTS);
        put32(28, slot_size as u32);
        std::ptr::write_unaligned(view.add(32) as *mut u64, 0);
        std::ptr::write_unaligned(view.add(40) as *mut u32, 0);
        put32(44, 0);
        std::ptr::write_unaligned(view.add(48) as *mut u64, 0);

        println!("publishing {WIDTH}x{HEIGHT} NV12 into Global\\OpenDisplayVCam for {secs}s");
        let start = Instant::now();
        let mut seq: u64 = 0;
        while start.elapsed() < Duration::from_secs(secs) {
            let slot = (seq % SLOTS as u64) as usize;
            let frame = view.add(HEADER + slot * slot_size);
            // Y plane: mid-gray, plus a bright bar that walks across.
            let bar_x = ((seq as usize * 24) % WIDTH as usize) as isize;
            for row in 0..HEIGHT as usize {
                let y = frame.add(row * STRIDE as usize);
                std::ptr::write_bytes(y, 110, WIDTH as usize);
                let x0 = bar_x.clamp(0, WIDTH as isize - 1) as usize;
                let x1 = (x0 + 60).min(WIDTH as usize);
                std::ptr::write_bytes(y.add(x0), 235, x1 - x0);
            }
            let uv = frame.add(STRIDE as usize * HEIGHT as usize);
            std::ptr::write_bytes(uv, 128, (STRIDE * HEIGHT / 2) as usize);

            // Publish: point at the new slot, then bump the sequence.
            std::ptr::write_unaligned(view.add(40) as *mut u32, slot as u32);
            std::ptr::write_unaligned(view.add(48) as *mut u64, seq * 333_333);
            (*(view.add(32) as *mut std::sync::atomic::AtomicU64))
                .store(seq + 1, std::sync::atomic::Ordering::Release);

            seq += 1;
            std::thread::sleep(Duration::from_millis(33));
        }
        println!("published {seq} frames");

        let _ = UnmapViewOfFile(mapped);
        let _ = CloseHandle(mapping);
    }
}
