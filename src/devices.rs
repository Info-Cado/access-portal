//! The user's microphone and camera, used by apps in the session.
//!
//! Neither has support in the RDP library, so both protocols are here:
//!
//! - **Microphone** ([MS-RDPEAI], dynamic channel `AUDIO_INPUT`): Windows
//!   offers recording formats, we accept the PCM ones, and when an app in the
//!   session starts recording Windows opens the device. The browser then
//!   captures its microphone, converts it to that format and sends it here; it
//!   goes to Windows in packets of the size Windows asked for.
//! - **Camera** ([MS-RDPECAM], `RDCamera_Device_Enumerator` and one device
//!   channel): we announce one camera, "Portal camera", offering MJPEG. When an
//!   app in the session starts it, the browser captures its camera and sends
//!   JPEG frames; each sample Windows asks for is answered with the latest one.
//!
//! The session loop (`rdp.rs`) owns the browser side: it is told when to start
//! and stop each device (`Shared::events`) and sends data with the channel ids
//! kept here.

use std::sync::{Arc, Mutex};

use ironrdp_core::{impl_as_any, Encode, EncodeResult, WriteCursor};
use ironrdp_dvc::{DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor};
use ironrdp_pdu::PduResult;

pub const MIC_CHANNEL: &str = "AUDIO_INPUT";
pub const CAMERA_ENUMERATOR: &str = "RDCamera_Device_Enumerator";
pub const CAMERA_DEVICE: &str = "RDCamera_Device_0";

/// A message as raw bytes.
pub struct Raw(pub Vec<u8>);

impl Encode for Raw {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.0.len());
        dst.write_slice(&self.0);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "Raw"
    }
    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for Raw {}

fn raw(bytes: Vec<u8>) -> DvcMessage {
    Box::new(Raw(bytes))
}

/// What the session loop needs to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// An app started recording: send PCM in this format.
    MicOn { rate: u32, channels: u16 },
    MicOff,
    /// An app started the camera at this size.
    CameraOn { width: u32, height: u32 },
    CameraOff,
}

#[derive(Default)]
pub struct Shared {
    pub events: Vec<Event>,
    // microphone
    pub mic_channel: Option<u32>,
    mic_formats: Vec<PcmFormat>,
    mic_format: Option<PcmFormat>,
    pub mic_frames_per_packet: u32,
    mic_buffer: Vec<u8>,
    // camera
    pub camera_channel: Option<u32>,
    camera_version: u8,
    /// Windows asked for a frame we didn't have yet.
    pub camera_waiting: bool,
    latest_frame: Option<Vec<u8>>,
}

pub type SharedRef = Arc<Mutex<Shared>>;

#[derive(Debug, Clone, Copy, PartialEq)]
struct PcmFormat {
    channels: u16,
    rate: u32,
    /// The format exactly as Windows described it (sent back unchanged).
    raw_index: usize,
}

// ---------------------------------------------------------------- microphone

const SNDIN_VERSION: u8 = 0x01;
const SNDIN_FORMATS: u8 = 0x02;
const SNDIN_OPEN: u8 = 0x03;
const SNDIN_OPEN_REPLY: u8 = 0x04;
const SNDIN_DATA_INCOMING: u8 = 0x05;
const SNDIN_DATA: u8 = 0x06;
const SNDIN_FORMATCHANGE: u8 = 0x07;

pub struct Microphone {
    shared: SharedRef,
    /// The formats Windows offered, as raw AUDIO_FORMAT bytes.
    offered: Vec<Vec<u8>>,
}

impl_as_any!(Microphone);

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

impl Microphone {
    fn formats_reply(&mut self, payload: &[u8]) -> Vec<DvcMessage> {
        // [id][NumFormats u32][cbSizeFormatsPacket u32][AUDIO_FORMAT...]
        let count = u32_at(payload, 1).unwrap_or(0).min(64) as usize;
        let mut at = 9;
        self.offered.clear();
        let mut accepted = Vec::new();
        for _ in 0..count {
            let (Some(tag), Some(channels), Some(rate), Some(bits), Some(extra)) = (
                u16_at(payload, at),
                u16_at(payload, at + 2),
                u32_at(payload, at + 4),
                u16_at(payload, at + 14),
                u16_at(payload, at + 16),
            ) else {
                break;
            };
            let end = at + 18 + extra as usize;
            let Some(bytes) = payload.get(at..end) else { break };
            // Plain 16-bit PCM, mono or stereo: what the browser produces.
            if tag == 1 && bits == 16 && (1..=2).contains(&channels) && (8000..=48000).contains(&rate) {
                accepted.push((bytes.to_vec(), PcmFormat { channels, rate, raw_index: 0 }));
            }
            at = end;
        }
        // Best first: 44.1/48 kHz mono, then the rest.
        accepted.sort_by_key(|(_, f)| (f.channels != 1, u32::abs_diff(f.rate, 44_100)));
        let mut body = Vec::new();
        let mut list = Vec::new();
        for (i, (bytes, mut f)) in accepted.into_iter().enumerate() {
            f.raw_index = i;
            body.extend_from_slice(&bytes);
            self.offered.push(bytes);
            list.push(f);
        }
        tracing::debug!("microphone formats accepted: {list:?}");
        self.shared.lock().unwrap().mic_formats = list;
        let mut msg = vec![SNDIN_FORMATS];
        msg.extend_from_slice(&(self.offered.len() as u32).to_le_bytes());
        msg.extend_from_slice(&((9 + body.len()) as u32).to_le_bytes());
        msg.extend_from_slice(&body);
        vec![raw(msg)]
    }

    fn select(&self, index: u32) -> Option<PcmFormat> {
        let mut shared = self.shared.lock().unwrap();
        let format = shared.mic_formats.get(index as usize).copied();
        shared.mic_format = format;
        shared.mic_buffer.clear();
        format
    }
}

pub fn microphone(shared: SharedRef) -> Microphone {
    Microphone { shared, offered: Vec::new() }
}

impl DvcProcessor for Microphone {
    fn channel_name(&self) -> &str {
        MIC_CHANNEL
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.shared.lock().unwrap().mic_channel = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let Some(&id) = payload.first() else { return Ok(Vec::new()) };
        Ok(match id {
            SNDIN_VERSION => {
                let mut msg = vec![SNDIN_VERSION];
                msg.extend_from_slice(&2u32.to_le_bytes());
                vec![raw(msg)]
            }
            SNDIN_FORMATS => self.formats_reply(payload),
            SNDIN_OPEN => {
                // [id][FramesPerPacket u32][initialFormat u32][WAVEFORMATEX]
                let frames = u32_at(payload, 1).unwrap_or(0);
                let initial = u32_at(payload, 5).unwrap_or(0);
                let format = self.select(initial);
                tracing::debug!("microphone opened: {format:?}, {frames} frames per packet");
                let mut shared = self.shared.lock().unwrap();
                shared.mic_frames_per_packet = frames.clamp(1, 48_000);
                let result: u32 = if let Some(f) = format {
                    shared.events.push(Event::MicOn { rate: f.rate, channels: f.channels });
                    0
                } else {
                    0x8000_4005 // E_FAIL
                };
                let mut change = vec![SNDIN_FORMATCHANGE];
                change.extend_from_slice(&initial.to_le_bytes());
                let mut reply = vec![SNDIN_OPEN_REPLY];
                reply.extend_from_slice(&result.to_le_bytes());
                vec![raw(change), raw(reply)]
            }
            SNDIN_FORMATCHANGE => {
                let index = u32_at(payload, 1).unwrap_or(0);
                if let Some(f) = self.select(index) {
                    self.shared.lock().unwrap().events.push(Event::MicOn { rate: f.rate, channels: f.channels });
                }
                let mut msg = vec![SNDIN_FORMATCHANGE];
                msg.extend_from_slice(&index.to_le_bytes());
                vec![raw(msg)]
            }
            other => {
                tracing::debug!("microphone: ignored message {other:#x}");
                Vec::new()
            }
        })
    }

    fn close(&mut self, _channel_id: u32) {
        let mut shared = self.shared.lock().unwrap();
        shared.mic_channel = None;
        shared.mic_format = None;
        shared.events.push(Event::MicOff);
    }
}

impl DvcClientProcessor for Microphone {}

/// PCM from the browser: complete packets for Windows (each "incoming data"
/// then the data), in the format it opened the microphone with.
pub fn mic_packets(shared: &SharedRef, pcm: &[u8]) -> Vec<DvcMessage> {
    let mut s = shared.lock().unwrap();
    let Some(format) = s.mic_format else { return Vec::new() };
    if s.mic_channel.is_none() {
        return Vec::new();
    }
    let packet = s.mic_frames_per_packet as usize * format.channels as usize * 2;
    s.mic_buffer.extend_from_slice(pcm);
    // Never fall far behind: drop the oldest audio past a second.
    let limit = format.rate as usize * format.channels as usize * 2;
    if s.mic_buffer.len() > limit {
        let extra = s.mic_buffer.len() - limit;
        s.mic_buffer.drain(..extra);
    }
    let mut out = Vec::new();
    while packet > 0 && s.mic_buffer.len() >= packet {
        let data: Vec<u8> = s.mic_buffer.drain(..packet).collect();
        out.push(raw(vec![SNDIN_DATA_INCOMING]));
        let mut msg = Vec::with_capacity(1 + data.len());
        msg.push(SNDIN_DATA);
        msg.extend_from_slice(&data);
        out.push(raw(msg));
    }
    out
}

// ---------------------------------------------------------------- camera

const CAM_SUCCESS: u8 = 0x01;
const CAM_ERROR: u8 = 0x02;
const CAM_SELECT_VERSION_REQUEST: u8 = 0x03;
const CAM_SELECT_VERSION_RESPONSE: u8 = 0x04;
const CAM_DEVICE_ADDED: u8 = 0x05;
const CAM_ACTIVATE: u8 = 0x07;
const CAM_DEACTIVATE: u8 = 0x08;
const CAM_STREAM_LIST_REQUEST: u8 = 0x09;
const CAM_STREAM_LIST_RESPONSE: u8 = 0x0A;
const CAM_MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
const CAM_MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
const CAM_CURRENT_MEDIA_TYPE_REQUEST: u8 = 0x0D;
const CAM_CURRENT_MEDIA_TYPE_RESPONSE: u8 = 0x0E;
const CAM_START_STREAMS: u8 = 0x0F;
const CAM_STOP_STREAMS: u8 = 0x10;
const CAM_SAMPLE_REQUEST: u8 = 0x11;
const CAM_SAMPLE_RESPONSE: u8 = 0x12;
const CAM_PROPERTY_LIST_REQUEST: u8 = 0x14;
const CAM_PROPERTY_LIST_RESPONSE: u8 = 0x15;

const FORMAT_MJPG: u8 = 0x02;
/// The sizes offered, best first.
const SIZES: [(u32, u32); 2] = [(1280, 720), (640, 480)];

fn media_type(width: u32, height: u32) -> Vec<u8> {
    let mut b = vec![FORMAT_MJPG];
    for v in [width, height, 30, 1, 1, 1] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.push(0x01); // the server must decode it (compressed)
    b
}

pub struct CameraEnumerator {
    shared: SharedRef,
}

impl_as_any!(CameraEnumerator);

pub fn camera_enumerator(shared: SharedRef) -> CameraEnumerator {
    CameraEnumerator { shared }
}

impl DvcProcessor for CameraEnumerator {
    fn channel_name(&self) -> &str {
        CAMERA_ENUMERATOR
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        Ok(vec![raw(vec![2, CAM_SELECT_VERSION_REQUEST])])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        if payload.get(1) != Some(&CAM_SELECT_VERSION_RESPONSE) {
            return Ok(Vec::new());
        }
        let version = payload[0].clamp(1, 2);
        self.shared.lock().unwrap().camera_version = version;
        tracing::debug!("camera redirection version {version}");
        let mut msg = vec![version, CAM_DEVICE_ADDED];
        for u in "Portal camera".encode_utf16().chain(std::iter::once(0)) {
            msg.extend_from_slice(&u.to_le_bytes());
        }
        msg.extend_from_slice(CAMERA_DEVICE.as_bytes());
        msg.push(0);
        Ok(vec![raw(msg)])
    }
}

impl DvcClientProcessor for CameraEnumerator {}

pub struct CameraDevice {
    shared: SharedRef,
}

impl_as_any!(CameraDevice);

pub fn camera_device(shared: SharedRef) -> CameraDevice {
    CameraDevice { shared }
}

impl DvcProcessor for CameraDevice {
    fn channel_name(&self) -> &str {
        CAMERA_DEVICE
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.shared.lock().unwrap().camera_channel = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let Some(&id) = payload.get(1) else { return Ok(Vec::new()) };
        let mut s = self.shared.lock().unwrap();
        let v = if s.camera_version == 0 { 2 } else { s.camera_version };
        let success = || raw(vec![v, CAM_SUCCESS]);
        tracing::debug!("camera request {id:#x}");
        Ok(match id {
            CAM_ACTIVATE => vec![success()],
            CAM_DEACTIVATE | CAM_STOP_STREAMS => {
                s.events.push(Event::CameraOff);
                s.camera_waiting = false;
                s.latest_frame = None;
                vec![success()]
            }
            CAM_STREAM_LIST_REQUEST => {
                // One colour capture stream, selected, shareable.
                let mut msg = vec![v, CAM_STREAM_LIST_RESPONSE];
                msg.extend_from_slice(&1u16.to_le_bytes());
                msg.extend_from_slice(&[0x01, 1, 1]);
                vec![raw(msg)]
            }
            CAM_MEDIA_TYPE_LIST_REQUEST => {
                let mut msg = vec![v, CAM_MEDIA_TYPE_LIST_RESPONSE];
                for (w, h) in SIZES {
                    msg.extend_from_slice(&media_type(w, h));
                }
                vec![raw(msg)]
            }
            CAM_CURRENT_MEDIA_TYPE_REQUEST => {
                let mut msg = vec![v, CAM_CURRENT_MEDIA_TYPE_RESPONSE];
                msg.extend_from_slice(&media_type(SIZES[1].0, SIZES[1].1));
                vec![raw(msg)]
            }
            CAM_START_STREAMS => {
                // [v][id][StreamIndex u8][media type: Format u8, Width u32, Height u32, ...]
                let width = u32_at(payload, 4).filter(|w| (160..=3840).contains(w)).unwrap_or(640);
                let height = u32_at(payload, 8).filter(|h| (120..=2160).contains(h)).unwrap_or(480);
                s.events.push(Event::CameraOn { width, height });
                vec![success()]
            }
            CAM_SAMPLE_REQUEST => match s.latest_frame.take() {
                Some(frame) => vec![sample(v, &frame)],
                None => {
                    s.camera_waiting = true;
                    Vec::new()
                }
            },
            CAM_PROPERTY_LIST_REQUEST => vec![raw(vec![v, CAM_PROPERTY_LIST_RESPONSE])],
            _ => {
                let mut msg = vec![v, CAM_ERROR];
                msg.extend_from_slice(&1u32.to_le_bytes()); // unexpected
                vec![raw(msg)]
            }
        })
    }

    fn close(&mut self, _channel_id: u32) {
        let mut s = self.shared.lock().unwrap();
        s.camera_channel = None;
        s.camera_waiting = false;
        s.events.push(Event::CameraOff);
    }
}

impl DvcClientProcessor for CameraDevice {}

fn sample(version: u8, jpeg: &[u8]) -> DvcMessage {
    let mut msg = Vec::with_capacity(3 + jpeg.len());
    msg.extend_from_slice(&[version, CAM_SAMPLE_RESPONSE, 0]);
    msg.extend_from_slice(jpeg);
    raw(msg)
}

/// A new frame from the browser: the answer to a waiting sample request, or
/// kept for the next one.
pub fn camera_frame(shared: &SharedRef, jpeg: Vec<u8>) -> Vec<DvcMessage> {
    let mut s = shared.lock().unwrap();
    if s.camera_channel.is_none() {
        return Vec::new();
    }
    if s.camera_waiting {
        s.camera_waiting = false;
        let v = if s.camera_version == 0 { 2 } else { s.camera_version };
        return vec![sample(v, &jpeg)];
    }
    s.latest_frame = Some(jpeg);
    Vec::new()
}
