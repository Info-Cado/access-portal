//! Windows' graphics pipeline (MS-RDPEGFX), passed through to the browser.
//!
//! With the pipeline, Windows sends the screen in its own codecs: full-screen
//! H.264 encoded by the host (on its GPU when it has one) plus a few lossless
//! codecs and copy/fill/cache commands. The portal does not rebuild the screen:
//! it turns each command into an operation the browser applies to its canvas,
//! in order. H.264 goes through untouched and is decoded by the browser's own
//! (hardware) decoder; only the lossless codecs are decoded here and sent as
//! images. Frames are acknowledged to Windows only once the browser has drawn
//! them, so on a slow link Windows sends fewer frames instead of building up a
//! backlog.
//!
//! Windows on this kind of host only offers H.264 as AVC444, which is two
//! ordinary 4:2:0 pictures per frame in one H.264 stream: a main view that is a
//! complete picture of the screen and an auxiliary view with extra colour detail.
//! The browser decodes both (they share one stream) and draws the main view.

use std::sync::{Arc, Mutex};

use ironrdp_core::{decode_cursor, impl_as_any, Decode as _, ReadCursor};
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_egfx::pdu::{
    avc_to_annex_b, is_avc_format, Avc420BitmapStream, Avc444BitmapStream, CapabilitiesAdvertisePdu,
    CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet, Codec1Type, Encoding, FrameAcknowledgePdu, GfxPdu,
    QueueDepth,
};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::rdp6::BitmapStreamDecoder;
use ironrdp_graphics::zgfx;
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_pdu::{decode_err, PduResult};

/// `(x, y, width, height)`.
pub type Rect = (u16, u16, u16, u16);

/// One thing for the browser to do, in order.
pub enum Op {
    /// The output (the browser's canvas) is now this size; all surfaces are gone.
    Reset { width: u32, height: u32 },
    CreateSurface { id: u16, width: u16, height: u16 },
    DeleteSurface { id: u16 },
    /// Show a surface at this position of the output.
    MapSurface { id: u16, x: u32, y: u32 },
    /// Pixels the portal decoded (RGBA), for a rectangle of a surface.
    Pixels { surface: u16, rect: Rect, rgba: Vec<u8> },
    /// One H.264 picture (Annex B) for the surface's decoder. When `draw` is set,
    /// `rects` of the decoded picture are copied to the same place on the surface.
    Video { surface: u16, draw: bool, key: bool, rects: Vec<Rect>, data: Vec<u8> },
    Fill { surface: u16, rgb: [u8; 3], rects: Vec<Rect> },
    /// Copy `rect` of surface `from` to each point on surface `to`.
    Copy { from: u16, to: u16, rect: Rect, points: Vec<(u16, u16)> },
    ToCache { surface: u16, slot: u16, rect: Rect },
    FromCache { slot: u16, surface: u16, points: Vec<(u16, u16)> },
    Evict { slot: u16 },
    /// Everything since the frame began is complete; the browser acknowledges it.
    EndFrame { id: u32 },
}

/// What the channel and the session share.
#[derive(Default)]
pub struct Shared {
    pub ops: Vec<Op>,
    /// The channel's id once Windows opened it (needed to send acknowledgements).
    pub channel_id: Option<u32>,
    /// Windows confirmed the pipeline: the legacy screen path goes quiet.
    pub active: bool,
}

pub type SharedRef = Arc<Mutex<Shared>>;

pub struct Channel {
    shared: SharedRef,
    decompressor: zgfx::Decompressor,
    buffer: Vec<u8>,
    clearcodec: Option<ClearCodecDecoder>,
    planar: BitmapStreamDecoder,
}

impl_as_any!(Channel);

pub fn channel() -> (Channel, SharedRef) {
    let shared = SharedRef::default();
    let channel = Channel {
        shared: shared.clone(),
        decompressor: zgfx::Decompressor::new(),
        buffer: Vec::new(),
        clearcodec: None,
        planar: BitmapStreamDecoder::default(),
    };
    (channel, shared)
}

/// The acknowledgement Windows waits for before sending more frames.
pub fn frame_ack(frame_id: u32, total_frames_decoded: u32) -> DvcMessage {
    Box::new(GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
        queue_depth: QueueDepth::Unavailable,
        frame_id,
        total_frames_decoded,
    }))
}

fn rect(r: &ExclusiveRectangle) -> Rect {
    (r.left, r.top, r.right.saturating_sub(r.left), r.bottom.saturating_sub(r.top))
}

/// Annex B bytes (start codes) for the browser, and whether they start a new
/// sequence the decoder can begin at (an IDR picture or parameter sets).
fn annex_b(data: &[u8]) -> (Vec<u8>, bool) {
    let bytes = if is_avc_format(data) { avc_to_annex_b(data) } else { data.to_vec() };
    let mut key = false;
    let mut i = 0;
    while i + 3 < bytes.len() {
        let start = if bytes[i..].starts_with(&[0, 0, 1]) {
            Some(i + 3)
        } else if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            Some(i + 4)
        } else {
            None
        };
        if let Some(s) = start {
            if let Some(&nal) = bytes.get(s) {
                if matches!(nal & 0x1f, 5 | 7) {
                    key = true;
                    break;
                }
            }
            i = s;
        } else {
            i += 1;
        }
    }
    (bytes, key)
}

impl Channel {
    fn push(&self, op: Op) {
        self.shared.lock().unwrap().ops.push(op);
    }

    fn video(&self, surface: u16, stream: &Avc420BitmapStream<'_>, draw: bool) {
        let (data, key) = annex_b(stream.data);
        let rects = stream.rectangles.iter().map(rect).collect();
        self.push(Op::Video { surface, draw, key, rects, data });
    }

    fn wire_to_surface(&mut self, pdu: ironrdp_egfx::pdu::WireToSurface1Pdu) -> PduResult<()> {
        let dest = rect(&pdu.destination_rectangle);
        let (_, _, w, h) = dest;
        let surface = pdu.surface_id;
        match pdu.codec_id {
            Codec1Type::Avc420 => {
                let mut cursor = ReadCursor::new(&pdu.bitmap_data);
                let stream = Avc420BitmapStream::decode(&mut cursor).map_err(|e| decode_err!(e))?;
                self.video(surface, &stream, true);
            }
            Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                let mut cursor = ReadCursor::new(&pdu.bitmap_data);
                let stream = Avc444BitmapStream::decode(&mut cursor).map_err(|e| decode_err!(e))?;
                // Luma: stream1 is the main view. Chroma: stream1 is the auxiliary
                // view. Both: main view then auxiliary view. All are decoded (they
                // are one H.264 stream); only main views are drawn.
                if stream.encoding == Encoding::CHROMA {
                    self.video(surface, &stream.stream1, false);
                } else {
                    self.video(surface, &stream.stream1, true);
                    if let Some(aux) = &stream.stream2 {
                        self.video(surface, aux, false);
                    }
                }
            }
            Codec1Type::Uncompressed => {
                // 32-bit little endian XRGB: bytes B, G, R, X.
                let rgba = pdu.bitmap_data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                self.push(Op::Pixels { surface, rect: dest, rgba });
            }
            Codec1Type::ClearCodec => {
                let bgra = self
                    .clearcodec
                    .get_or_insert_with(ClearCodecDecoder::new)
                    .decode(&pdu.bitmap_data, w, h)
                    .map_err(|e| ironrdp_pdu::pdu_other_err!("ClearCodec decode", source: e))?;
                let rgba = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                self.push(Op::Pixels { surface, rect: dest, rgba });
            }
            Codec1Type::Planar => {
                let mut rgb = Vec::new();
                self.planar
                    .decode_bitmap_stream_to_rgb24(&pdu.bitmap_data, &mut rgb, usize::from(w), usize::from(h))
                    .map_err(|e| ironrdp_pdu::pdu_other_err!("Planar decode", source: e))?;
                let rgba = rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
                self.push(Op::Pixels { surface, rect: dest, rgba });
            }
            other => {
                // Not used by Windows with the capabilities advertised here.
                tracing::warn!("graphics pipeline: codec {other:?} is not supported; area not drawn");
            }
        }
        Ok(())
    }

    fn handle(&mut self, pdu: GfxPdu) -> PduResult<()> {
        match pdu {
            GfxPdu::CapabilitiesConfirm(confirm) => {
                tracing::debug!("graphics pipeline confirmed: {:?}", confirm.0);
                self.shared.lock().unwrap().active = true;
            }
            GfxPdu::ResetGraphics(p) => self.push(Op::Reset { width: p.width, height: p.height }),
            GfxPdu::CreateSurface(p) => self.push(Op::CreateSurface { id: p.surface_id, width: p.width, height: p.height }),
            GfxPdu::DeleteSurface(p) => self.push(Op::DeleteSurface { id: p.surface_id }),
            GfxPdu::MapSurfaceToOutput(p) => {
                self.push(Op::MapSurface { id: p.surface_id, x: p.output_origin_x, y: p.output_origin_y })
            }
            // Windows maps its desktop surface 1:1 with this message too.
            GfxPdu::MapSurfaceToScaledOutput(p) => {
                self.push(Op::MapSurface { id: p.surface_id, x: p.output_origin_x, y: p.output_origin_y })
            }
            GfxPdu::StartFrame(_) => {}
            GfxPdu::EndFrame(p) => self.push(Op::EndFrame { id: p.frame_id }),
            GfxPdu::WireToSurface1(p) => {
                if let Err(e) = self.wire_to_surface(p) {
                    tracing::warn!("graphics pipeline: dropped an update: {e}");
                }
            }
            GfxPdu::SolidFill(p) => self.push(Op::Fill {
                surface: p.surface_id,
                rgb: [p.fill_pixel.r, p.fill_pixel.g, p.fill_pixel.b],
                rects: p.rectangles.iter().map(rect).collect(),
            }),
            GfxPdu::SurfaceToSurface(p) => self.push(Op::Copy {
                from: p.source_surface_id,
                to: p.destination_surface_id,
                rect: rect(&p.source_rectangle),
                points: p.destination_points.iter().map(|pt| (pt.x, pt.y)).collect(),
            }),
            GfxPdu::SurfaceToCache(p) => {
                self.push(Op::ToCache { surface: p.surface_id, slot: p.cache_slot, rect: rect(&p.source_rectangle) })
            }
            GfxPdu::CacheToSurface(p) => self.push(Op::FromCache {
                slot: p.cache_slot,
                surface: p.surface_id,
                points: p.destination_points.iter().map(|pt| (pt.x, pt.y)).collect(),
            }),
            GfxPdu::EvictCacheEntry(p) => self.push(Op::Evict { slot: p.cache_slot }),
            other => tracing::debug!("graphics pipeline: ignored {}", pdu_name(&other)),
        }
        Ok(())
    }
}

fn pdu_name(pdu: &GfxPdu) -> &'static str {
    match pdu {
        GfxPdu::WireToSurface2(_) => "WireToSurface2",
        GfxPdu::CacheImportReply(_) => "CacheImportReply",
        GfxPdu::MapSurfaceToWindow(_) => "MapSurfaceToWindow",
        GfxPdu::MapSurfaceToScaledWindow(_) => "MapSurfaceToScaledWindow",
        GfxPdu::DeleteEncodingContext(_) => "DeleteEncodingContext",
        _ => "other",
    }
}

impl DvcProcessor for Channel {
    fn channel_name(&self) -> &str {
        ironrdp_egfx::CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.shared.lock().unwrap().channel_id = Some(channel_id);
        // H.264 in thin-client mode; Windows answers with full-screen AVC444.
        let caps = [
            CapabilitySet::V10_7 { flags: CapabilitiesV107Flags::SMALL_CACHE | CapabilitiesV107Flags::AVC_THIN_CLIENT },
            CapabilitySet::V10_6 { flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_THIN_CLIENT },
        ];
        Ok(vec![Box::new(GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&caps)))])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.buffer.clear();
        self.decompressor.decompress(payload, &mut self.buffer).map_err(|e| decode_err!(e))?;
        let buffer = std::mem::take(&mut self.buffer);
        let mut pdus = Vec::new();
        {
            let mut cursor = ReadCursor::new(&buffer);
            while !cursor.is_empty() {
                pdus.push(decode_cursor::<GfxPdu>(&mut cursor).map_err(|e| decode_err!(e))?);
            }
        }
        self.buffer = buffer;
        for pdu in pdus {
            self.handle(pdu)?;
        }
        // Frame acknowledgements are sent by the session once the browser drew them.
        Ok(Vec::new())
    }
}

impl DvcClientProcessor for Channel {}
