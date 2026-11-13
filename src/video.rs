//! Motion hybrid: still areas of the screen are sent as crisp tiles (see
//! `tiles.rs`), but an area that keeps changing — a scrolling report, a dragged
//! window — switches to an H.264 stream, which is far cheaper for continuous
//! motion than resending images. When the motion stops, a final crisp tile is
//! sent and the area goes back to tiles, so text stays sharp and an idle screen
//! still costs nothing.
//!
//! The screen is a grid of blocks. Each block tracks how busy it is; a busy one
//! becomes a "video" block with its own H.264 encoder, and a quiet one falls
//! back to tiles. Encoding happens at most `~30` times a second.
//!
//! Wire format of one video packet (little endian), sent as a binary message:
//!   [3][block:u8][gen:u8][flags:u8][seq:u32][len:u32][H.264 Annex-B bytes]
//!   flags bit 0 = keyframe. seq orders a block's frames (the direct channel is
//!   unordered, so the browser reassembles order from it). gen rises each time a
//!   block (re)starts video, so a stale packet from a previous run is ignored.
//! Start/stop of a block are JSON text messages (`vstart` / `vstop`).

use std::time::{Duration, Instant};

use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_session::image::DecodedImage;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;

/// Target block size; the last column/row may be smaller. A multiple of the tile
/// size (256) so video blocks align to the tile grid and the two never straddle.
const BLOCK_W: usize = 512;
const BLOCK_H: usize = 512;
/// Activity needed before a block switches to video, and the ceiling it climbs to.
/// A single change adds `STEP_UP`; a quiet cycle subtracts one. So a block needs
/// sustained motion (a few busy cycles) to become video, and several quiet cycles
/// to leave it — which stops it flapping on and off.
const ENTER: u8 = 9;
const STEP_UP: u8 = 3;
const CAP: u8 = 15;
/// Per-block H.264 bitrate ceiling.
const BITRATE_BPS: u32 = 2_000_000;
/// Fastest video cadence (~33 fps).
const MIN_INTERVAL: Duration = Duration::from_millis(30);

/// What the session loop should do with the browser after a `step`.
pub enum VideoEvent {
    /// A block became video: create a decoder for it at this position.
    Start { block: u8, gen: u8, x: u16, y: u16, w: u16, h: u16 },
    /// One encoded frame for a block.
    Packet { block: u8, gen: u8, keyframe: bool, seq: u32, data: Vec<u8> },
    /// A block went quiet: tear its decoder down. The caller then sends a crisp
    /// tile for the area so it is lossless again.
    Stop { block: u8, rect: InclusiveRectangle },
}

struct Block {
    x: usize,
    y: usize,
    w: usize, // even, may be 0 at a ragged edge (then never video)
    h: usize, // even
    activity: u8,
    dirty: bool,
    video: bool,
    seq: u32,
    gen: u8,
    encoder: Option<Encoder>,
}

impl Block {
    fn rect(&self) -> InclusiveRectangle {
        InclusiveRectangle {
            left: self.x as u16,
            top: self.y as u16,
            right: (self.x + self.w).saturating_sub(1) as u16,
            bottom: (self.y + self.h).saturating_sub(1) as u16,
        }
    }
}

pub struct Video {
    enabled: bool,
    blocks: Vec<Block>,
    last_encode: Instant,
}

impl Video {
    pub fn new(width: u16, height: u16, enabled: bool) -> Self {
        let (width, height) = (usize::from(width), usize::from(height));
        let cols = width.div_ceil(BLOCK_W).max(1);
        let rows = height.div_ceil(BLOCK_H).max(1);
        let mut blocks = Vec::with_capacity(cols * rows);
        for by in 0..rows {
            for bx in 0..cols {
                let x = bx * BLOCK_W;
                let y = by * BLOCK_H;
                // Clamp to the framebuffer and to even dimensions (YUV 4:2:0).
                let w = (width.min(x + BLOCK_W) - x) & !1;
                let h = (height.min(y + BLOCK_H) - y) & !1;
                blocks.push(Block { x, y, w, h, activity: 0, dirty: false, video: false, seq: 0, gen: 0, encoder: None });
            }
        }
        Self { enabled, blocks, last_encode: Instant::now() - MIN_INTERVAL }
    }

    /// Note that the host drew somewhere inside `rect`.
    pub fn mark(&mut self, rect: &InclusiveRectangle) {
        if !self.enabled {
            return;
        }
        for b in &mut self.blocks {
            let overlaps = usize::from(rect.left) < b.x + b.w
                && usize::from(rect.right) >= b.x
                && usize::from(rect.top) < b.y + b.h
                && usize::from(rect.bottom) >= b.y
                && b.w > 0;
            if overlaps {
                b.dirty = true;
            }
        }
    }

    /// Areas currently served as video, so the tile layer can skip them.
    pub fn active_rects(&self) -> Vec<InclusiveRectangle> {
        self.blocks.iter().filter(|b| b.video).map(Block::rect).collect()
    }

    /// Update motion state and produce encoder output. Runs at most every
    /// `MIN_INTERVAL`; between runs, `mark` keeps accumulating.
    pub fn step(&mut self, image: &DecodedImage) -> Vec<VideoEvent> {
        if !self.enabled || self.last_encode.elapsed() < MIN_INTERVAL {
            return Vec::new();
        }
        self.last_encode = Instant::now();

        let stride = image.stride();
        let bpp = image.bytes_per_pixel();
        let data = image.data();
        let mut events = Vec::new();

        for (i, b) in self.blocks.iter_mut().enumerate() {
            let block = i as u8;
            b.activity = if b.dirty {
                (b.activity + STEP_UP).min(CAP)
            } else {
                b.activity.saturating_sub(1)
            };

            if !b.video {
                // Quiet, or a block too small/ragged to encode: stay on tiles.
                if b.activity >= ENTER && b.w >= 16 && b.h >= 16 {
                    match make_encoder() {
                        Some(mut enc) => {
                            enc.force_intra_frame();
                            b.encoder = Some(enc);
                            b.video = true;
                            b.seq = 0;
                            b.gen = b.gen.wrapping_add(1);
                            let gen = b.gen;
                            events.push(VideoEvent::Start {
                                block,
                                gen,
                                x: b.x as u16,
                                y: b.y as u16,
                                w: b.w as u16,
                                h: b.h as u16,
                            });
                            if let Some(data) = encode_block(b, data, stride, bpp) {
                                events.push(VideoEvent::Packet { block, gen, keyframe: true, seq: 0, data });
                                b.seq = 1;
                            }
                        }
                        None => b.activity = 0, // encoder failed; stay on tiles
                    }
                }
            } else if b.activity == 0 {
                b.encoder = None;
                b.video = false;
                events.push(VideoEvent::Stop { block, rect: b.rect() });
            } else if b.dirty {
                // Encode a frame only when the block actually changed, so a video
                // block that briefly stops moving sends nothing.
                if let Some(data) = encode_block(b, data, stride, bpp) {
                    let seq = b.seq;
                    b.seq = b.seq.wrapping_add(1);
                    events.push(VideoEvent::Packet { block, gen: b.gen, keyframe: false, seq, data });
                }
            }
            b.dirty = false;
        }
        events
    }
}

fn make_encoder() -> Option<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(BITRATE_BPS))
        .max_frame_rate(FrameRate::from_hz(33.0))
        .rate_control_mode(RateControlMode::Bitrate)
        // Required for the bitrate cap to take effect; drops a frame under pressure.
        .skip_frames(true);
    Encoder::with_api_config(OpenH264API::from_source(), config).ok()
}

/// Copy a block's pixels out of the RGBA framebuffer and encode one H.264 frame.
fn encode_block(b: &mut Block, frame: &[u8], stride: usize, bpp: usize) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity(b.w * b.h * 3);
    for row in 0..b.h {
        let start = (b.y + row) * stride + b.x * bpp;
        for px in frame[start..start + b.w * bpp].chunks_exact(bpp) {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (b.w, b.h)));
    let encoder = b.encoder.as_mut()?;
    let bitstream = encoder.encode(&yuv).ok()?;
    let out = bitstream.to_vec();
    (!out.is_empty()).then_some(out)
}

/// Build one video packet's binary message.
pub fn packet_message(block: u8, gen: u8, keyframe: bool, seq: u32, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(data.len() + 12);
    msg.push(3);
    msg.push(block);
    msg.push(gen);
    msg.push(if keyframe { 1 } else { 0 });
    msg.extend_from_slice(&seq.to_le_bytes());
    msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
    msg.extend_from_slice(data);
    msg
}
