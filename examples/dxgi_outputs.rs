//! List every DXGI output with its GDI name and whether it is attached to the desktop.

use windows::Win32::Graphics::Dxgi::*;

fn main() {
    opendisplay_win::app::set_dpi_awareness();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("factory");
        for a in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            let Ok(d) = adapter.GetDesc1() else { continue };
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let Ok(desc) = output.GetDesc() else { continue };
                let gdi = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
                println!(
                    "adapter {a} ({name}) output {o}: {gdi} attached={} rect=({}, {})-({}, {})",
                    desc.AttachedToDesktop.as_bool(),
                    desc.DesktopCoordinates.left,
                    desc.DesktopCoordinates.top,
                    desc.DesktopCoordinates.right,
                    desc.DesktopCoordinates.bottom,
                );
            }
        }
    }
}
