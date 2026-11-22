//! Feed a raw Annex B H.264 file through the real camera pipeline at 30 fps
//! (decode -> shared memory -> virtual camera), for testing without an iPad.
//!
//!   cargo run --example camera_loop -- test720r.h264 [seconds]

use std::time::{Duration, Instant};

/// Split Annex B into access units at AUD (NAL type 9) boundaries.
fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let sc = if data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        if i + sc < data.len() && data[i + sc] & 0x1F == 9 {
            starts.push(i);
        }
        i += sc;
    }
    let mut out = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let e = starts.get(n + 1).copied().unwrap_or(data.len());
        out.push(&data[s..e]);
    }
    out
}

fn is_keyframe(au: &[u8]) -> bool {
    // IDR NAL type 5 after the AUD.
    let mut i = 0;
    while i + 4 < au.len() {
        let sc = if au[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if au[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        if i + sc < au.len() && au[i + sc] & 0x1F == 5 {
            return true;
        }
        i += sc;
    }
    false
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test720r.h264".into());
    let secs: f64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);
    let data = std::fs::read(&path).expect("read h264 file");
    let aus = access_units(&data);
    println!("{} access unit(s) from {path}", aus.len());

    let session = opendisplay_win::camera::start(1280, 720, Box::new(|_enable| {})).expect("camera session");
    let tick = Duration::from_millis(33);
    let start = Instant::now();
    let mut sent = 0usize;
    while start.elapsed().as_secs_f64() < secs {
        let au = aus[sent % aus.len()];
        session.push(au, is_keyframe(au));
        sent += 1;
        std::thread::sleep(tick);
    }
    println!("sent {sent} access unit(s)");
    session.stop();
    println!("stopped");
}
