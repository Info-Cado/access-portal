//! Copied pictures: Windows' clipboard bitmap (a DIB, `CF_DIB`) to PNG for the
//! browser, and PNG from the browser to a DIB for Windows.

use anyhow::{bail, ensure, Context, Result};

/// Largest picture accepted either way (pixels).
const MAX_PIXELS: u64 = 8192 * 8192;

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(at..at + 2).context("bitmap too short")?.try_into()?))
}
fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(at..at + 4).context("bitmap too short")?.try_into()?))
}

/// A Windows DIB (BITMAPINFOHEADER or V4/V5, 24 or 32 bits) as a PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let header = u32_at(dib, 0)? as usize;
    ensure!(header >= 40 && header <= dib.len(), "unknown bitmap header");
    let width = u32_at(dib, 4)? as i32;
    let height = u32_at(dib, 8)? as i32;
    let bits = u16_at(dib, 14)?;
    let compression = u32_at(dib, 16)?;
    let colors_used = u32_at(dib, 32)?;
    ensure!(width > 0 && height != 0, "empty bitmap");
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    ensure!((w as u64) * (h as u64) <= MAX_PIXELS, "bitmap too large");
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    // Channel masks: after a plain 40-byte header, or inside a V4/V5 header.
    let mut masks = [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0];
    let mut offset = header;
    match compression {
        BI_RGB => {}
        BI_BITFIELDS => {
            for (i, m) in masks.iter_mut().take(3).enumerate() {
                *m = u32_at(dib, 40 + i * 4)?;
            }
            if header >= 56 {
                masks[3] = u32_at(dib, 52)?;
            } else {
                offset += 12;
            }
        }
        other => bail!("compressed bitmaps (type {other}) aren't supported"),
    }
    if bits <= 8 {
        bail!("palette bitmaps aren't supported");
    }
    offset += colors_used as usize * 4;
    let bpp = match bits {
        24 => 3,
        32 => 4,
        other => bail!("{other}-bit bitmaps aren't supported"),
    };
    let stride = (w * bpp + 3) & !3;
    ensure!(dib.len() >= offset + stride * h, "bitmap data too short");
    let channel = |px: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 255;
        }
        let shift = mask.trailing_zeros();
        let max = mask >> shift;
        (((px & mask) >> shift) * 255 / max.max(1)) as u8
    };
    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        // Bottom-up unless the height is negative.
        let src_row = if height > 0 { h - 1 - y } else { y };
        let row = &dib[offset + src_row * stride..];
        for x in 0..w {
            let out = &mut rgba[(y * w + x) * 4..][..4];
            if bpp == 3 {
                let p = &row[x * 3..x * 3 + 3];
                out.copy_from_slice(&[p[2], p[1], p[0], 255]);
            } else {
                let px = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().unwrap());
                let a = if compression == BI_RGB { (px >> 24) as u8 } else { channel(px, masks[3]) };
                any_alpha |= a != 0;
                out.copy_from_slice(&[channel(px, masks[0]), channel(px, masks[1]), channel(px, masks[2]), a]);
            }
        }
    }
    // Most 32-bit clipboard bitmaps leave the fourth byte at zero: that means opaque.
    if bpp == 4 && (!any_alpha || compression == BI_BITFIELDS && masks[3] == 0) {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(&rgba)?;
    }
    Ok(out)
}

/// A PNG as a 32-bit bottom-up DIB (BITMAPINFOHEADER + pixels) for `CF_DIB`.
pub fn png_to_dib(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("reading the picture")?;
    let size = reader.output_buffer_size().context("picture too large")?;
    {
        let info = reader.info();
        ensure!(u64::from(info.width) * u64::from(info.height) <= MAX_PIXELS, "picture too large");
    }
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf).context("decoding the picture")?;
    let (w, h) = (frame.width as usize, frame.height as usize);
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => bail!("unexpected palette picture"),
    };
    let line = frame.line_size;
    let mut dib = Vec::with_capacity(40 + w * h * 4);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
    dib.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
    dib.extend_from_slice(&2835i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let row = &buf[y * line..];
        for x in 0..w {
            let p = &row[x * channels..x * channels + channels];
            let (r, g, b, a) = match channels {
                1 => (p[0], p[0], p[0], 255),
                2 => (p[0], p[0], p[0], p[1]),
                3 => (p[0], p[1], p[2], 255),
                _ => (p[0], p[1], p[2], p[3]),
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // 3x2 picture with distinct pixels.
        let rgba: Vec<u8> = (0..6u8).flat_map(|i| [i * 40, 255 - i * 30, i * 7, 255]).collect();
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 3, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
        let dib = png_to_dib(&png_bytes).unwrap();
        let back = dib_to_png(&dib).unwrap();
        let mut dec = png::Decoder::new(std::io::Cursor::new(&back)).read_info().unwrap();
        let mut out = vec![0; dec.output_buffer_size().unwrap()];
        dec.next_frame(&mut out).unwrap();
        assert_eq!(out, rgba);
    }

    #[test]
    fn rgb24_top_down() {
        // 2x1, 24-bit, top-down (negative height), rows padded to 4 bytes.
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&(-1i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // red, green, padding
        let png_bytes = dib_to_png(&dib).unwrap();
        let mut dec = png::Decoder::new(std::io::Cursor::new(&png_bytes)).read_info().unwrap();
        let mut out = vec![0; dec.output_buffer_size().unwrap()];
        dec.next_frame(&mut out).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }
}
