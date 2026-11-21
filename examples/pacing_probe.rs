//! Runs the same capture -> convert -> encode chain as `pipeline_loop`, but on
//! the primary display with no network, and reports per second:
//! frames the duplicator delivered, access units encoded, how often the
//! frame-pacing gate skipped a fresh frame, and encode / grab-gap latencies.
//! Safe to run while the real app streams the virtual display: it opens its
//! own duplication on a different output.

use std::time::{Duration, Instant};

use opendisplay_win::capture::{Capturer, Grab};
use opendisplay_win::display;
use opendisplay_win::encoder::{self, Converter, Encoder, EncoderConfig};

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn main() -> anyhow::Result<()> {
    opendisplay_win::app::set_dpi_awareness();
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let want = std::env::args().nth(2);
    let all = display::monitors();
    let primary = match want.as_deref() {
        Some("secondary") => all.iter().find(|m| !m.primary).cloned().expect("no secondary monitor"),
        Some(d) => all
            .iter()
            .find(|m| m.device_name.eq_ignore_ascii_case(d))
            .cloned()
            .unwrap_or_else(|| panic!("no monitor {d}")),
        None => all.into_iter().find(|m| m.primary).expect("no primary monitor"),
    };
    let raw = std::env::args().nth(3).as_deref() == Some("raw");
    encoder::mf_startup()?;
    let mut cap = Capturer::new(&primary.device_name)?;
    if raw {
        let start = Instant::now();
        let mut win = Instant::now();
        let (mut n, mut wn) = (0u32, 0u32);
        let mut last: Option<Instant> = None;
        let mut gaps: Vec<f64> = Vec::new();
        let mut all_gaps: Vec<f64> = Vec::new();
        while start.elapsed().as_secs_f64() < secs {
            if let Grab::New = cap.grab(50)? {
                n += 1;
                wn += 1;
                if let Some(l) = last {
                    let g = l.elapsed().as_secs_f64() * 1000.0;
                    gaps.push(g);
                    all_gaps.push(g);
                }
                last = Some(Instant::now());
            }
            if win.elapsed() >= Duration::from_secs(1) {
                println!(
                    "{:5.1}s new={:3}/s gap50={:5.1} gap95={:5.1}",
                    start.elapsed().as_secs_f64(),
                    wn,
                    pct(&mut gaps, 0.5),
                    pct(&mut gaps, 0.95),
                );
                wn = 0;
                gaps.clear();
                win = Instant::now();
            }
        }
        println!(
            "raw: {n} frames in {:.1}s = {:.1} fps, gap50={:.1} gap95={:.1} gap99={:.1}",
            start.elapsed().as_secs_f64(),
            n as f64 / start.elapsed().as_secs_f64(),
            pct(&mut all_gaps, 0.5),
            pct(&mut all_gaps, 0.95),
            pct(&mut all_gaps, 0.99),
        );
        return Ok(());
    }
    let (w, h) = (cap.width, cap.height);
    let mut conv = Converter::new(&cap.gpu, w, h, w, h, 60)?;
    let mut enc = Encoder::new(
        &cap.gpu,
        &EncoderConfig { width: w, height: h, fps: 60, bitrate: 30_000_000, prefer_software: false, low_power: false },
    )?;
    println!("capturing {} {}x{} on {} with {}", primary.device_name, w, h, cap.gpu.adapter, enc.name);
    let interval = Duration::from_secs_f64(1.0 / 60.0);
    let mut next_due = Instant::now();
    let mut dirty = false;
    let mut first = true;
    let (mut grabs, mut encoded, mut early) = (0u32, 0u32, 0u32);
    let (mut enc_ms, mut gap_ms): (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
    let mut last_grab: Option<Instant> = None;
    let mut win = Instant::now();
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        let wait = next_due.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        let mut fresh = false;
        if let Grab::New = cap.grab(wait.as_millis().min(50) as u32)? {
            dirty = true;
            fresh = true;
            grabs += 1;
            if let Some(l) = last_grab {
                gap_ms.push(l.elapsed().as_secs_f64() * 1000.0);
            }
            last_grab = Some(Instant::now());
        }
        if !cap.have_frame || !dirty {
            continue;
        }
        if Instant::now() < next_due {
            if fresh {
                early += 1;
            }
            continue;
        }
        dirty = false;
        next_due = Instant::now().max(next_due) + interval;
        let t0 = Instant::now();
        let nv12 = conv.convert(&cap.frame)?;
        let mut n = 0;
        for _ in enc.encode(&nv12, first)? {
            n += 1;
        }
        first = false;
        enc_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        encoded += n;
        if win.elapsed() >= Duration::from_secs(1) {
            println!(
                "{:5.1}s grabs={:3}/s enc={:3}/s early={:3} | enc50={:5.1} enc95={:5.1} | gap50={:5.1} gap95={:5.1}",
                start.elapsed().as_secs_f64(),
                grabs,
                encoded,
                early,
                pct(&mut enc_ms, 0.5),
                pct(&mut enc_ms, 0.95),
                pct(&mut gap_ms, 0.5),
                pct(&mut gap_ms, 0.95),
            );
            grabs = 0;
            encoded = 0;
            early = 0;
            enc_ms.clear();
            gap_ms.clear();
            win = Instant::now();
        }
    }
    Ok(())
}
