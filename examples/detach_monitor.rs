//! Detach a monitor from the desktop: `detach_monitor <device|secondary>`.

fn main() -> anyhow::Result<()> {
    opendisplay_win::app::set_dpi_awareness();
    let want = std::env::args().nth(1).unwrap_or_else(|| "secondary".into());
    let all = opendisplay_win::display::monitors();
    let name = if want == "secondary" {
        all.iter().find(|m| !m.primary).expect("no secondary monitor").device_name.clone()
    } else {
        want
    };
    opendisplay_win::display::detach(&name)?;
    std::thread::sleep(std::time::Duration::from_millis(800));
    for m in opendisplay_win::display::monitors() {
        println!("{} {}x{} at ({}, {}){}", m.device_name, m.size().0, m.size().1, m.rect.left, m.rect.top, if m.primary { " (primary)" } else { "" });
    }
    Ok(())
}
