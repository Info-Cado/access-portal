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
//! On the local network tiles are packed the fastest way (PNG with the quickest
//! compression, several tiles at once on separate cores). Photo-like areas,
//! where a lossless picture is several times bigger than a JPEG, go as JPEG
//! first and are made lossless once the screen is still, unless the link has
//! been measured fast enough to carry them lossless straight away.
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
/// Local network: a many-colour area whose fast lossless picture is bigger than
/// a quarter of its raw pixels is photo-like (text and windows pack to well
/// under a tenth), and worth a JPEG first.
const PHOTO_SHARE: usize = 4;
/// Local network: tiles packed on at most this many cores at once.
const MAX_THREADS: usize = 8;

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
    /// Local network: tiles are packed the fastest way (see the module notes).
    lan: bool,
    /// Local network: the link is fast enough for lossless photos too.
    lossless_link: bool,
}

/// One changed area of a tile, waiting to be packed.
struct Job {
    idx: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    rgb: Vec<u8>,
    lossless_only: bool,
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
            lossless_link: false,
        }
    }

    /// Local network: whether the link is fast enough to send photo-like areas
    /// losslessly at once (otherwise they go as JPEG first).
    pub fn set_lossless_link(&mut self, on: bool) {
        self.lossless_link = on;
    }

    /// Some tiles were sent as JPEG and wait for their lossless version.
    pub fn has_lossy(&self) -> bool {
        self.lossy.iter().any(|l| *l)
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

    #[cfg(test)]
    pub fn has_work(&self) -> bool {
        self.has_dirty() || self.has_lossy()
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
        // Local network: areas are collected first and packed together on
        // several cores. Their packed size is not known yet, so the budget is
        // checked against a rough guess.
        let mut jobs: Vec<Job> = Vec::new();
        let mut guess = 0usize;

        let total = self.dirty.len();
        for step in 0..total {
            let idx = (self.cursor + step) % total;
            let is_dirty = self.dirty[idx];
            if !is_dirty && !(refine && self.lossy[idx]) {
                continue;
            }
            if out.len() + guess > budget || usize::from(count) + jobs.len() >= usize::from(u16::MAX) {
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

            if self.lan {
                // Photos pack to about half their raw size losslessly.
                guess += rgb.len() / 2;
                jobs.push(Job { idx, x: bx0, y: by0, w, h, rgb, lossless_only });
                continue;
            }
            let Some((fmt, data)) = encode(&rgb, w, h, !lossless_only) else {
                continue;
            };
            self.note_sent(idx, fmt, lossless_only);
            put_tile(&mut out, bx0, by0, fmt, &data);
            count += 1;
        }

        if !jobs.is_empty() {
            let allow_lossy = !self.lossless_link;
            for (job, packed) in jobs.iter().zip(pack_all(&jobs, allow_lossy)) {
                let Some((fmt, data)) = packed else { continue };
                self.note_sent(job.idx, fmt, job.lossless_only);
                put_tile(&mut out, job.x, job.y, fmt, &data);
                count += 1;
            }
        }

        if count == 0 {
            return None;
        }
        out[5..7].copy_from_slice(&count.to_le_bytes());
        Some(out)
    }

    /// Remember whether the browser now has an exact tile or a JPEG to refine.
    fn note_sent(&mut self, idx: usize, fmt: u8, lossless_only: bool) {
        if lossless_only {
            self.lossy[idx] = false;
        } else if fmt == FMT_JPEG {
            self.lossy[idx] = true;
        }
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

fn put_tile(out: &mut Vec<u8>, x: usize, y: usize, fmt: u8, data: &[u8]) {
    out.extend_from_slice(&(x as u16).to_le_bytes());
    out.extend_from_slice(&(y as u16).to_le_bytes());
    out.push(fmt);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// Local network: pack every job, spread over several cores when there is
/// enough to do. Results are in the jobs' order.
fn pack_all(jobs: &[Job], allow_lossy: bool) -> Vec<Option<(u8, Vec<u8>)>> {
    let pixels: usize = jobs.iter().map(|j| j.w * j.h).sum();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()).min(MAX_THREADS);
    // Starting threads costs more than packing a few small areas.
    let threads = if pixels < 128 * 128 { 1 } else { cores.min(jobs.len()) };
    if threads <= 1 {
        return jobs.iter().map(|j| pack_lan(j, allow_lossy)).collect();
    }
    let mut results: Vec<Option<(u8, Vec<u8>)>> = (0..jobs.len()).map(|_| None).collect();
    // Hand out jobs round robin, so big and small areas mix on every core.
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    jobs.iter()
                        .enumerate()
                        .skip(t)
                        .step_by(threads)
                        .map(|(i, j)| (i, pack_lan(j, allow_lossy)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            if let Ok(done) = handle.join() {
                for (i, packed) in done {
                    results[i] = packed;
                }
            }
        }
    });
    results
}

/// Local network: the fastest lossless PNG, or a JPEG for a photo-like area
/// when lossy is allowed and the JPEG is less than half the size.
fn pack_lan(job: &Job, allow_lossy: bool) -> Option<(u8, Vec<u8>)> {
    let (w, h, rgb) = (job.w, job.h, &job.rgb);
    let exact = png_image(w, h, png::ColorType::Rgb, rgb, None, png::Compression::Fast)?;
    if allow_lossy && !job.lossless_only && w * h >= MIN_LOSSY_PIXELS && exact.len() > rgb.len() / PHOTO_SHARE {
        if let Some(lossy) = jpeg(rgb, w, h) {
            if lossy.len() * 2 < exact.len() {
                return Some((FMT_JPEG, lossy));
            }
        }
    }
    Some((FMT_PNG, exact))
}

/// JPEG without chroma subsampling, so coloured edges (text on photos) stay sharp.
fn jpeg(rgb: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    if w > 65535 || h > 65535 {
        return None;
    }
    let mut out = Vec::with_capacity(w * h / 4);
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    encoder.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).ok()?;
    Some(out)
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
    if allow_lossy && w * h >= MIN_LOSSY_PIXELS {
        if let Some(out) = jpeg(rgb, w, h) {
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

    /// A photo-like screen: smooth shapes with fine grain, as in a picture.
    fn photo_frame(w: usize, h: usize) -> Vec<u8> {
        let mut x = 12345u32;
        let mut frame = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let grain = (x & 7) as f32;
                let (fx, fy) = (col as f32, row as f32);
                let r = 128.0 + 90.0 * (fx / 37.0).sin() * (fy / 53.0).cos() + grain;
                let g = 128.0 + 90.0 * (fy / 29.0).sin() + grain;
                let b = 128.0 + 90.0 * ((fx + fy) / 41.0).cos() + grain;
                frame.extend_from_slice(&[r as u8, g as u8, b as u8, 255]);
            }
        }
        frame
    }

    /// A text-like screen: two colours.
    fn text_frame(w: usize, h: usize) -> Vec<u8> {
        let mut frame = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                let ink = (row % 14 < 9) && ((col * 7 + row * 3) % 11 < 4);
                frame.extend_from_slice(if ink { &[20, 20, 20, 255] } else { &[255, 255, 255, 255] });
            }
        }
        frame
    }

    /// Decode a PNG tile and check it holds exactly the screen's pixels there.
    fn assert_exact(frame: &[u8], width: usize, x: u16, y: u16, data: Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(data)).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgb);
        for row in 0..info.height as usize {
            for col in 0..info.width as usize {
                let src = ((usize::from(y) + row) * width + usize::from(x) + col) * 4;
                let dst = (row * info.width as usize + col) * 3;
                assert_eq!(&buf[dst..dst + 3], &frame[src..src + 3]);
            }
        }
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
        tiles.set_lossless_link(true);
        tiles.mark(&whole(w, h));
        let batch = tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap();
        assert!(!tiles.has_work(), "everything went in the one batch");
        let sent = tiles_of(&batch);
        assert_eq!(sent.len(), 5 * 3, "every 256-pixel tile of 1280x720");
        for (x, y, fmt, data) in sent {
            assert_eq!(fmt, FMT_PNG);
            assert_exact(&frame, usize::from(w), x, y, data);
        }
    }

    #[test]
    fn lan_sends_photos_as_jpeg_then_lossless() {
        let (w, h) = (1024u16, 512u16);
        let frame = photo_frame(w.into(), h.into());
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.mark(&whole(w, h));
        let first = tiles_of(&tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap());
        assert_eq!(first.len(), 4 * 2);
        assert!(first.iter().all(|t| t.2 == FMT_JPEG), "photo areas go as JPEG on a slow link");
        assert!(!tiles.has_dirty() && tiles.has_lossy());
        // Once still: the same areas again, exact this time.
        let second = tiles_of(&tiles.next_batch(&frame, usize::from(w) * 4, 4, 2, 64 * 1024 * 1024, true, &[]).unwrap());
        assert_eq!(second.len(), 4 * 2);
        for (x, y, fmt, data) in second {
            assert_eq!(fmt, FMT_PNG);
            assert_exact(&frame, usize::from(w), x, y, data);
        }
        assert!(!tiles.has_work());
    }

    #[test]
    fn lan_sends_photos_lossless_on_a_fast_link() {
        let (w, h) = (512u16, 512u16);
        let frame = photo_frame(w.into(), h.into());
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.set_lossless_link(true);
        tiles.mark(&whole(w, h));
        let sent = tiles_of(&tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap());
        assert!(sent.iter().all(|t| t.2 == FMT_PNG));
        assert!(!tiles.has_work(), "nothing left to sharpen");
    }

    #[test]
    fn lan_keeps_text_lossless() {
        let (w, h) = (512u16, 512u16);
        let frame = text_frame(w.into(), h.into());
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.mark(&whole(w, h));
        let sent = tiles_of(&tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap());
        assert_eq!(sent.len(), 4);
        for (x, y, fmt, data) in sent {
            assert_eq!(fmt, FMT_PNG);
            assert_exact(&frame, usize::from(w), x, y, data);
        }
        assert!(!tiles.has_lossy());
    }

    #[test]
    fn lan_refresh_respects_a_small_budget() {
        let (w, h) = (1024u16, 1024u16);
        let frame = photo_frame(w.into(), h.into());
        let mut tiles = Tiles::new(w, h, frame.len(), true);
        tiles.mark(&whole(w, h));
        tiles.next_batch(&frame, usize::from(w) * 4, 4, 1, 64 * 1024 * 1024, false, &[]).unwrap();
        // Lossless refresh in pieces of about 256 KB: 16 tiles need several.
        let mut batches = 0;
        while tiles.has_lossy() {
            let b = tiles.next_batch(&frame, usize::from(w) * 4, 4, 2 + batches, 256 * 1024, true, &[]).unwrap();
            assert!(tiles_of(&b).len() < 16);
            batches += 1;
            assert!(batches < 20);
        }
        assert!(batches > 1);
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
