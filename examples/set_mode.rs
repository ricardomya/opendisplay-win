//! Set a monitor's mode from the command line: `set_mode <device|secondary> <w> <h> <hz>`.

fn main() -> anyhow::Result<()> {
    let dev = std::env::args().nth(1).expect("device name or 'secondary'");
    let w: u32 = std::env::args().nth(2).expect("width").parse()?;
    let h: u32 = std::env::args().nth(3).expect("height").parse()?;
    let hz: u32 = std::env::args().nth(4).expect("hz").parse()?;
    let all = opendisplay_win::display::monitors();
    let name = if dev == "secondary" {
        all.iter().find(|m| !m.primary).expect("no secondary").device_name.clone()
    } else {
        dev
    };
    let pos = match (std::env::args().nth(5), std::env::args().nth(6)) {
        (Some(x), Some(y)) => Some((x.parse()?, y.parse()?)),
        _ => None,
    };
    opendisplay_win::display::set_mode(&name, w, h, hz, pos)?;
    std::thread::sleep(std::time::Duration::from_millis(700));
    for m in opendisplay_win::display::monitors() {
        println!("{} {}x{} at ({}, {}) dpi={}", m.device_name, m.size().0, m.size().1, m.rect.left, m.rect.top, m.dpi);
    }
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            println!("current: {}x{} @ {} Hz", dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency);
        }
    }
    Ok(())
}
