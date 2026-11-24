//! Measures the convert + encode half of the pipeline with synthetic motion:
//! a noise block sliding over a static desktop-sized BGRA texture, fed through
//! the same Converter/Encoder the stream uses. No capture, no network, so it
//! can run while the real app streams.

use std::time::Instant;

use opendisplay_win::capture::create_device;
use opendisplay_win::encoder::{self, Converter, Encoder, EncoderConfig};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn noise(buf: &mut [u8], seed: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        chunk.copy_from_slice(&seed.to_le_bytes());
    }
}

fn main() -> anyhow::Result<()> {
    let (w, h) = (2732u32, 2048u32);
    let frames: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(240);
    encoder::mf_startup()?;
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapter = None;
    for i in 0.. {
        let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
            continue;
        }
        let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
        println!("adapter {i}: {name}");
        if name.contains("Intel") {
            adapter = Some(a);
            break;
        }
        if adapter.is_none() {
            adapter = Some(a);
        }
    }
    let gpu = create_device(&adapter.expect("no hardware adapter"))?;
    println!("device on {}", gpu.adapter);

    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    let tex = tex.expect("texture");
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut full = vec![0u8; (w * h * 4) as usize];
    noise(&mut full, &mut seed);
    // Dim the base image so it resembles desktop content rather than noise.
    for px in full.chunks_mut(4) {
        px[0] /= 3;
        px[1] /= 3;
        px[2] /= 3;
    }
    unsafe { gpu.context.UpdateSubresource(&tex, 0, None, full.as_ptr() as _, w * 4, 0) };

    let mut conv = Converter::new(&gpu, w, h, w, h, 60)?;
    let mut enc = Encoder::new(
        &gpu,
        &EncoderConfig { width: w, height: h, fps: 60, bitrate: 30_000_000, prefer_software: false, low_power: false },
    )?;
    println!("encoder: {}", enc.name);

    let (bw, bh) = (900u32, 700u32);
    let mut block = vec![0u8; (bw * bh * 4) as usize];
    let (mut upd, mut cvt, mut encd): (Vec<f64>, Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new(), Vec::new());
    let mut aus = 0u32;
    let start = Instant::now();
    for i in 0..frames {
        let t0 = Instant::now();
        noise(&mut block, &mut seed);
        let x = (i * 37) % (w - bw);
        let y = (i * 23) % (h - bh);
        let bx = D3D11_BOX { left: x, top: y, front: 0, right: x + bw, bottom: y + bh, back: 1 };
        unsafe { gpu.context.UpdateSubresource(&tex, 0, Some(&bx), block.as_ptr() as _, bw * 4, 0) };
        let t1 = Instant::now();
        let nv12 = conv.convert(&tex)?;
        let t2 = Instant::now();
        for _ in enc.encode(&nv12, i == 0)? {
            aus += 1;
        }
        let t3 = Instant::now();
        upd.push((t1 - t0).as_secs_f64() * 1000.0);
        cvt.push((t2 - t1).as_secs_f64() * 1000.0);
        encd.push((t3 - t2).as_secs_f64() * 1000.0);
    }
    let total = start.elapsed().as_secs_f64();
    let c50 = pct(&mut cvt, 0.5);
    let e50 = pct(&mut encd, 0.5);
    println!(
        "frames={frames} aus={aus} wall={total:.2}s -> {:.1} fps | upd50={:.1} conv50={:.1} conv95={:.1} enc50={:.1} enc95={:.1} | conv+enc p50={:.1}ms -> ceiling {:.0} fps",
        frames as f64 / total,
        pct(&mut upd, 0.5),
        c50,
        pct(&mut cvt, 0.95),
        e50,
        pct(&mut encd, 0.95),
        c50 + e50,
        1000.0 / (c50 + e50).max(0.01),
    );
    Ok(())
}
