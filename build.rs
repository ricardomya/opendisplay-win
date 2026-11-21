//! Compile assets/opendisplay.rc (icons + version info) into both executables.
//!
//! Upstream builds with llvm-mingw's windres for the gnullvm target. This fork
//! also builds with the native MSVC toolchain, where windres is absent: fall
//! back to the Windows SDK's rc.exe and link its .res directly.
use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("x64").join("rc.exe").exists())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("x64").join("rc.exe"))
}

fn main() {
    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = ver
        .split('.')
        .map(|p| p.split('-').next().unwrap_or("0").to_string());
    let (major, minor, patch) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );

    // Preferred: GNU windres (what upstream's llvm-mingw target uses).
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let obj = format!("{out}/resources.o");
    if let Ok(status) = Command::new(&windres)
        .args(["-O", "coff", "-I", "assets"])
        .args([
            format!("-DVER_MAJOR={major}"),
            format!("-DVER_MINOR={minor}"),
            format!("-DVER_PATCH={patch}"),
            format!("-DVER_STRING=\\\"{ver}\\\""),
        ])
        .args(["assets/opendisplay.rc", "-o", &obj])
        .status()
    {
        assert!(status.success(), "{windres} failed");
        println!("cargo:rustc-link-arg-bins={obj}");
        return;
    }

    // Fallback: MSVC rc.exe from the Windows SDK.
    let rc = find_rc().expect("no windres on PATH and no rc.exe in the Windows SDK");
    let res = format!("{out}/resources.res");
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{res}"))
        .arg("/I")
        .arg("assets")
        .arg(format!("/dVER_MAJOR={major}"))
        .arg(format!("/dVER_MINOR={minor}"))
        .arg(format!("/dVER_PATCH={patch}"))
        .arg(format!("/dVER_STRING=\"{ver}\""))
        .arg("assets/opendisplay.rc")
        .status()
        .expect("running rc.exe");
    assert!(status.success(), "rc.exe failed");
    println!("cargo:rustc-link-arg-bins={res}");
}
