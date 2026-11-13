//! Encoder benchmark: full-screen 1920x1080 H.264 with openh264 on real desktop
//! pixels, with a window-sized area moving every frame (like a drag).
//!   cargo run --release --example h264bench -- <screenshot.png>

use std::time::Instant;

use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;

fn main() {
    let path = std::env::args().nth(1).expect("screenshot path");
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let mut base = Vec::with_capacity(w * h * 3);
    for px in buf[..w * h * ch].chunks_exact(ch) {
        base.extend_from_slice(&px[..3]);
    }
    let (w, h) = (w & !1, h & !1);
    println!("frame {w}x{h}");

    for (label, threads, complexity) in [("1 thread, low", 1u16, Complexity::Low), ("auto threads, low", 0, Complexity::Low), ("4 threads, medium", 4, Complexity::Medium)] {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .bitrate(BitRate::from_bps(8_000_000))
            .max_frame_rate(FrameRate::from_hz(60.0))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .complexity(complexity)
            .num_threads(threads);
        let mut enc = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
        let mut frame = base.clone();
        let (mut conv_ms, mut enc_ms, mut bytes, mut worst) = (0.0, 0.0, 0usize, 0.0f64);
        let n = 120;
        for i in 0..n {
            // A 600x400 "window" copied from the screenshot, moving 7 px per frame.
            frame.copy_from_slice(&base[..w * h * 3]);
            let (ox, oy) = (100 + (i * 7) % 1100, 200 + (i * 3) % 500);
            for y in 0..400 {
                let src = (300 + y) * w * 3 + 400 * 3;
                let dst = (oy + y) * w * 3 + ox * 3;
                frame.copy_within(0..0, 0);
                let row: Vec<u8> = base[src..src + 600 * 3].to_vec();
                frame[dst..dst + 600 * 3].copy_from_slice(&row);
            }
            let t0 = Instant::now();
            let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&frame[..w * h * 3], (w, h)));
            let t1 = Instant::now();
            let out = enc.encode(&yuv).unwrap().to_vec();
            let t2 = Instant::now();
            if i > 0 {
                conv_ms += (t1 - t0).as_secs_f64() * 1000.0;
                let e = (t2 - t1).as_secs_f64() * 1000.0;
                enc_ms += e;
                worst = worst.max(e);
                bytes += out.len();
            } else {
                println!("  [{label}] keyframe {} KB, {:.1} ms", out.len() / 1024, (t2 - t1).as_secs_f64() * 1000.0);
            }
        }
        let m = (n - 1) as f64;
        println!(
            "  [{label}] RGB->YUV {:.1} ms, encode avg {:.1} ms (worst {:.1}), {:.1} KB/frame = {:.1} Mbit/s at 60 fps",
            conv_ms / m, enc_ms / m, worst, bytes as f64 / m / 1024.0, bytes as f64 / m * 8.0 * 60.0 / 1e6
        );
    }
}
