//! Read-only timing probe for the display queries the sender uses.
//! Times `monitors()` (EnumDisplayMonitors + GetDpiForMonitor + QueryDisplayConfig),
//! `monitors_basic()` and `monitor_geometry()` once per second and prints the
//! result, so a stall can be attributed while the real app keeps running.

use std::time::{Duration, Instant};

fn main() {
    let mut device: Option<String> = std::env::args().nth(1);
    let start = Instant::now();
    let (mut worst_qdc, mut worst_basic, mut worst_geom) = (0f64, 0f64, 0f64);
    loop {
        let t0 = Instant::now();
        let all = opendisplay_win::display::monitors();
        let qdc = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let basic = opendisplay_win::display::monitors_basic();
        let bms = t1.elapsed().as_secs_f64() * 1000.0;

        if device.is_none() {
            device = basic
                .iter()
                .find(|m| !m.primary)
                .or_else(|| basic.first())
                .map(|m| m.device_name.clone());
        }
        let t2 = Instant::now();
        let g = device
            .as_deref()
            .and_then(opendisplay_win::display::monitor_geometry);
        let gms = t2.elapsed().as_secs_f64() * 1000.0;

        worst_qdc = worst_qdc.max(qdc);
        worst_basic = worst_basic.max(bms);
        worst_geom = worst_geom.max(gms);
        println!(
            "{:8.2}s qdc={:>2} {:8.2}ms | basic={:>2} {:6.2}ms | geom={} {:6.2}ms | worst {:8.1}/{:6.1}/{:6.1}",
            start.elapsed().as_secs_f64(),
            all.len(),
            qdc,
            basic.len(),
            bms,
            g.is_some(),
            gms,
            worst_qdc,
            worst_basic,
            worst_geom,
        );
        std::thread::sleep(Duration::from_millis(1000));
    }
}
