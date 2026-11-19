//! Turns framebuffer changes into small, cheap-to-send updates.
//!
//! The screen is divided into a grid of tiles. The session marks tiles the host
//! touched; when it is time to send, each marked tile is compared with a copy of
//! what the browser already has, and only the part that really differs is
//! encoded:
//!
//! * few colours (text, UI)  -> palette PNG, lossless and tiny
//! * many colours, large     -> JPEG now, then a lossless PNG once things go quiet
//! * many colours, small     -> RGB PNG
//!
//! On the local network every tile is a lossless PNG packed the fastest way
//! (about 12 times quicker than the above for a slightly bigger picture).
//!
//! Batches are capped in size and the caller limits how many are in flight, so
//! on a slow link pending changes pile up as *flags* rather than as queued
//! bytes, and the newest pixels are what gets sent next.
//!
//! Wire format of one batch (little endian):
//!   [2][seq:u32][count:u16] then per tile [x:u16][y:u16][fmt:u8][len:u32][bytes]
//!   fmt 0 = PNG, 1 = JPEG

use std::collections::HashMap;

use ironrdp_pdu::geometry::InclusiveRectangle;

const TILE: usize = 256;
const FMT_PNG: u8 = 0;
const FMT_JPEG: u8 = 1;
/// Many-colour regions smaller than this are not worth a lossy pass.
const MIN_LOSSY_PIXELS: usize = 64 * 64;
const JPEG_QUALITY: u8 = 90;

pub struct Tiles {
    width: usize,
    height: usize,
    cols: usize,
    /// What the browser currently shows, in the framebuffer's own layout.
    shadow: Vec<u8>,
    /// Tiles the host has drawn into since they were last examined.
    dirty: Vec<bool>,
    /// Tiles last sent with lossy compression, awaiting a lossless pass.
    lossy: Vec<bool>,
    /// Tiles that must be resent as a whole, losslessly — used when a video block
    /// goes quiet and its area needs a crisp repaint (the shadow there is stale).
    force: Vec<bool>,
    /// Where the next batch starts, so every tile gets its turn.
    cursor: usize,
    /// Local network: every tile is a plain lossless PNG, packed the fastest
    /// way. Bytes are cheap there; the time to pack them is not.
    lan: bool,
}

impl Tiles {
    pub fn new(width: u16, height: u16, framebuffer_len: usize, lan: bool) -> Self {
        let (width, height) = (usize::from(width), usize::from(height));
        let cols = width.div_ceil(TILE).max(1);
        let rows = height.div_ceil(TILE).max(1);
        Self {
            width,
            height,
            cols,
            shadow: vec![0; framebuffer_len],
            dirty: vec![false; cols * rows],
            lossy: vec![false; cols * rows],
            force: vec![false; cols * rows],
            cursor: 0,
            lan,
        }
    }

    /// Force a crisp, whole-tile repaint of everything in `rect`. Used when a
    /// video block stops: the browser has video pixels there, not what the tile
    /// shadow believes, so the area must be redrawn in full.
    pub fn invalidate(&mut self, rect: &InclusiveRectangle) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let x0 = usize::from(rect.left).min(self.width - 1) / TILE;
        let x1 = usize::from(rect.right).min(self.width - 1) / TILE;
        let y0 = usize::from(rect.top).min(self.height - 1) / TILE;
        let y1 = usize::from(rect.bottom).min(self.height - 1) / TILE;
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                let idx = ty * self.cols + tx;
                self.dirty[idx] = true;
                self.force[idx] = true;
            }
        }
    }

    /// Note that the host drew somewhere inside `rect`.
    pub fn mark(&mut self, rect: &InclusiveRectangle) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let x0 = usize::from(rect.left).min(self.width - 1) / TILE;
        let x1 = usize::from(rect.right).min(self.width - 1) / TILE;
        let y0 = usize::from(rect.top).min(self.height - 1) / TILE;
        let y1 = usize::from(rect.bottom).min(self.height - 1) / TILE;
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                self.dirty[ty * self.cols + tx] = true;
            }
        }
    }

    pub fn has_dirty(&self) -> bool {
        self.dirty.iter().any(|d| *d)
    }

    pub fn has_work(&self) -> bool {
        self.has_dirty() || self.lossy.iter().any(|l| *l)
    }

    /// Encode pending changes into one batch of at most roughly `budget` bytes.
    ///
    /// With `refine`, tiles previously sent lossy are re-sent losslessly too.
    /// Returns `None` when nothing needed sending.
    pub fn next_batch(
        &mut self,
        frame: &[u8],
        stride: usize,
        bpp: usize,
        seq: u32,
        budget: usize,
        refine: bool,
        skip: &[InclusiveRectangle],
    ) -> Option<Vec<u8>> {
        if frame.len() != self.shadow.len() || bpp < 3 {
            return None;
        }
        let mut out = Vec::with_capacity(4096);
        out.push(2);
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        let mut count: u16 = 0;

        let total = self.dirty.len();
        for step in 0..total {
            let idx = (self.cursor + step) % total;
            let is_dirty = self.dirty[idx];
            if !is_dirty && !(refine && self.lossy[idx]) {
                continue;
            }
            if out.len() > budget || count == u16::MAX {
                self.cursor = idx;
                break;
            }

            let x0 = (idx % self.cols) * TILE;
            let y0 = (idx / self.cols) * TILE;
            let x1 = (x0 + TILE).min(self.width);
            let y1 = (y0 + TILE).min(self.height);

            // A tile inside a video block is covered by the video stream; drop its
            // update (video blocks are tile-aligned, so a tile is wholly in or out).
            if skip.iter().any(|r| {
                x0 >= usize::from(r.left)
                    && x0 <= usize::from(r.right)
                    && y0 >= usize::from(r.top)
                    && y0 <= usize::from(r.bottom)
            }) {
                self.dirty[idx] = false;
                self.lossy[idx] = false;
                continue;
            }

            let (bx0, by0, bx1, by1, lossless_only) = if is_dirty && !self.force[idx] {
                self.dirty[idx] = false;
                match self.changed_box(frame, stride, bpp, x0, y0, x1, y1) {
                    Some((a, b, c, d)) => (a, b, c, d, false),
                    None => continue,
                }
            } else {
                // A forced repaint, or a lossy tile being refined: send the whole
                // tile losslessly.
                self.dirty[idx] = false;
                self.force[idx] = false;
                (x0, y0, x1, y1, true)
            };

            let (w, h) = (bx1 - bx0, by1 - by0);
            let mut rgb = Vec::with_capacity(w * h * 3);
            for y in by0..by1 {
                let row = y * stride + bx0 * bpp;
                let src = &frame[row..row + w * bpp];
                self.shadow[row..row + w * bpp].copy_from_slice(src);
                for px in src.chunks_exact(bpp) {
                    rgb.extend_from_slice(&px[..3]);
                }
            }

            let encoded = if self.lan {
                png_image(w, h, png::ColorType::Rgb, &rgb, None, png::Compression::Fast).map(|d| (FMT_PNG, d))
            } else {
                encode(&rgb, w, h, !lossless_only)
            };
            let Some((fmt, data)) = encoded else {
                continue;
            };
            if lossless_only {
                self.lossy[idx] = false;
            } else if fmt == FMT_JPEG {
                self.lossy[idx] = true;
            }

            out.extend_from_slice(&(bx0 as u16).to_le_bytes());
            out.extend_from_slice(&(by0 as u16).to_le_bytes());
            out.push(fmt);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
            count += 1;
        }

        if count == 0 {
            return None;
        }
        out[5..7].copy_from_slice(&count.to_le_bytes());
        Some(out)
    }

    /// Smallest box inside the tile that differs from what the browser has.
    #[allow(clippy::too_many_arguments)]
    fn changed_box(
        &self,
        frame: &[u8],
        stride: usize,
        bpp: usize,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        let (mut top, mut bottom) = (usize::MAX, 0);
        let (mut left, mut right) = (usize::MAX, 0);
        for y in y0..y1 {
            let start = y * stride + x0 * bpp;
            let end = y * stride + x1 * bpp;
            let (new, old) = (&frame[start..end], &self.shadow[start..end]);
            if new == old {
                continue;
            }
            top = top.min(y);
            bottom = y + 1;
            let first = new
                .chunks_exact(bpp)
                .zip(old.chunks_exact(bpp))
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let last = new
                .chunks_exact(bpp)
                .zip(old.chunks_exact(bpp))
                .rposition(|(a, b)| a != b)
                .unwrap_or(x1 - x0 - 1);
            left = left.min(x0 + first);
            right = right.max(x0 + last + 1);
        }
        (top != usize::MAX).then_some((left, top, right, bottom))
    }
}

/// Pick the cheapest faithful encoding for one RGB region.
fn encode(rgb: &[u8], w: usize, h: usize, allow_lossy: bool) -> Option<(u8, Vec<u8>)> {
    // Up to 256 distinct colours: a palette image is lossless and compresses far
    // better than full colour. This is the common case for text and UI.
    let mut palette: Vec<u8> = Vec::with_capacity(256 * 3);
    let mut lookup: HashMap<[u8; 3], u8> = HashMap::with_capacity(512);
    let mut indices: Vec<u8> = Vec::with_capacity(w * h);
    let mut last: Option<([u8; 3], u8)> = None;
    let mut fits = true;
    for px in rgb.chunks_exact(3) {
        let colour = [px[0], px[1], px[2]];
        if let Some((c, i)) = last {
            if c == colour {
                indices.push(i);
                continue;
            }
        }
        let index = match lookup.get(&colour) {
            Some(i) => *i,
            None => {
                if lookup.len() == 256 {
                    fits = false;
                    break;
                }
                let i = lookup.len() as u8;
                lookup.insert(colour, i);
                palette.extend_from_slice(&colour);
                i
            }
        };
        last = Some((colour, index));
        indices.push(index);
    }

    if fits {
        return png_image(w, h, png::ColorType::Indexed, &indices, Some(palette), png::Compression::Balanced).map(|d| (FMT_PNG, d));
    }
    if allow_lossy && w * h >= MIN_LOSSY_PIXELS && w <= 65535 && h <= 65535 {
        let mut out = Vec::with_capacity(w * h / 4);
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
        // No chroma subsampling: coloured edges (text on photos) stay sharp.
        encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
        if encoder.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).is_ok() {
            // A lossy tile is followed later by a lossless one, so it only pays
            // off when it is much smaller. Otherwise send the lossless tile now.
            let lossless = png_image(w, h, png::ColorType::Rgb, rgb, None, png::Compression::Balanced);
            return match lossless {
                Some(exact) if exact.len() <= out.len() * 2 => Some((FMT_PNG, exact)),
                _ => Some((FMT_JPEG, out)),
            };
        }
    }
    png_image(w, h, png::ColorType::Rgb, rgb, None, png::Compression::Balanced).map(|d| (FMT_PNG, d))
}

fn png_image(
    w: usize,
    h: usize,
    colour: png::ColorType,
    data: &[u8],
    palette: Option<Vec<u8>>,
    compression: png::Compression,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 8 + 128);
    {
        let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
        encoder.set_color(colour);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(compression);
        if let Some(palette) = palette {
            encoder.set_palette(palette);
        }
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(data).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen with many colours (no palette fits), so the normal path would
    /// pick JPEG for a big change.
    fn noisy_frame(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        let mut frame = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            frame.extend_from_slice(&[x as u8, (x >> 8) as u8, (x >> 16) as u8, 255]);
        }
        frame
    }

    fn whole(w: u16, h: u16) -> InclusiveRectangle {
        InclusiveRectangle { left: 0, top: 0, right: w - 1, bottom: h - 1 }
    }

    /// Unpack a batch into (x, y, fmt, bytes) per tile.
    fn tiles_of(batch: &[u8]) -> Vec<(u16, u16, u8, Vec<u8>)> {
        assert_eq!(batch[0], 2);
        let count = u16::from_le_bytes([batch[5], batch[6]]) as usize;
        let mut off = 7;
        let mut out = Vec::new();
        for _ in 0..count {
            let x = u16::from_le_bytes([batch[off], batch[off + 1]]);
            let y = u16::from_le_bytes([batch[off + 2], batch[off + 3]]);
            let fmt = batch[off + 4];
            let len = u32::from_le_bytes(batch[off + 5..off + 9].try_into().unwrap()) as usize;
            out.push((x, y, fmt, batch[off + 9..off + 9 + len].to_vec()));
            off += 9 + len;
        }
        assert_eq!(off, batch.len());
        out
    }

    #[test]
    fn lan_sends_a_whole_screen_in_one_lossless_batch() {
        let (w, h) = (1280u16, 720u16);
        let frame = noisy_frame(w.into(), h.into(), 7);
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.mark(&whole(w, h));
        let batch = tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap();
        assert!(!tiles.has_work(), "everything went in the one batch");
        let sent = tiles_of(&batch);
        assert_eq!(sent.len(), 5 * 3, "every 256-pixel tile of 1280x720");
        for (x, y, fmt, data) in sent {
            assert_eq!(fmt, FMT_PNG);
            // The PNG holds exactly the screen's pixels.
            let mut reader = png::Decoder::new(std::io::Cursor::new(data)).read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut buf).unwrap();
            assert_eq!(info.color_type, png::ColorType::Rgb);
            for row in 0..info.height as usize {
                for col in 0..info.width as usize {
                    let src = ((usize::from(y) + row) * usize::from(w) + usize::from(x) + col) * 4;
                    let dst = (row * info.width as usize + col) * 3;
                    assert_eq!(&buf[dst..dst + 3], &frame[src..src + 3]);
                }
            }
        }
    }

    #[test]
    fn internet_path_still_caps_batches() {
        let (w, h) = (1280u16, 720u16);
        let frame = noisy_frame(w.into(), h.into(), 9);
        let mut tiles = Tiles::new(w, h, frame.len(), false);
        tiles.mark(&whole(w, h));
        let batch = tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 48 * 1024, false, &[]).unwrap();
        assert!(tiles.has_work(), "a small budget leaves the rest for later batches");
        assert!(tiles_of(&batch).len() < 15);
    }

    #[test]
    fn lan_sends_only_what_changed() {
        let (w, h) = (1024u16, 512u16);
        let mut frame = noisy_frame(w.into(), h.into(), 3);
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.mark(&whole(w, h));
        tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap();
        // Change a 10x10 square inside one tile.
        for row in 300..310usize {
            for col in 600..610usize {
                frame[(row * usize::from(w) + col) * 4] ^= 0xff;
            }
        }
        tiles.mark(&InclusiveRectangle { left: 600, top: 300, right: 609, bottom: 309 });
        let batch = tiles.next_batch(&frame, usize::from(w) * 4, 4, 2, 64 * 1024 * 1024, false, &[]).unwrap();
        let sent = tiles_of(&batch);
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].0, sent[0].1), (600, 300));
    }
}
