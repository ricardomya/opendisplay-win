//! Run the virtual-microphone bridge with no iPad window (PC source only),
//! for testing the cable without a receiver.
//!
//!   cargo run --example mic_loop -- [seconds]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::RECT;

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let session = opendisplay_win::mic::start(Arc::new(Mutex::new(rect))).expect("mic session");
    println!("mic bridge running for {secs}s (PC source)");
    std::thread::sleep(Duration::from_secs(secs));
    session.stop();
    println!("stopped");
}
