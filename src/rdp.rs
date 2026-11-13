//! Built-in browser RDP client.
//!
//! The portal itself is the RDP client: it connects to the host, signs in with
//! the stored Windows credentials (NLA), decodes the screen, and streams changed
//! regions to the browser (see transport.rs and tiles.rs). Mouse and keyboard come
//! back over the same socket. The Windows password never leaves the server.
//!
//! Wire format, server -> browser:
//!   binary  a batch of changed tiles, see `tiles.rs`
//!   text    {"t":"size","w":..,"h":..}       session resolution
//!   text    {"t":"cur", ...}                 pointer shape (png/default/hidden)
//!   text    {"t":"closed","m":".."}          session ended
//! Browser -> server: small JSON objects, see `ClientMsg`.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use ironrdp_connector::connection_activation::ConnectionActivationState;
use ironrdp_connector::{
    ClientConnector, Config, ConnectionResult, ConnectorError, ConnectorErrorExt as _,
    ConnectorResult, Credentials, DesktopSize, ServerName,
};
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_core::{impl_as_any, WriteBuf};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::rdp::capability_sets::{MajorPlatformType, RailSupportLevel};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, MovableTokioFramed, NetworkClient};
use serde::Deserialize;
use ironrdp_svc::{SvcProcessor, SvcProcessorMessages};
use serde_json::json;

use crate::tiles::Tiles;
use crate::transport::{Incoming, Transport};
use crate::video::{packet_message, Video, VideoEvent};
use tokio::net::TcpStream;

type TlsFramed = MovableTokioFramed<ironrdp_tls::TlsStream<TcpStream>>;

/// Where to connect and as whom.
pub struct Target {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub width: u16,
    pub height: u16,
    /// Tell the browser why a session failed (admins only).
    pub show_errors: bool,
    /// Whether the session follows the browser window's size.
    pub resizable: bool,
    /// Whether the browser can decode H.264 (WebCodecs), enabling the video path.
    pub video_ok: bool,
    /// Folder shared with the session as a redirected drive (absolute path).
    pub files_dir: Option<std::path::PathBuf>,
    /// Ask Windows for its graphics pipeline (H.264 from the host).
    pub egfx: bool,
    /// Signalled when an administrator disconnects this session.
    pub kick: Option<std::sync::Arc<crate::presence::Kick>>,
}

/// What a session needs besides the connection itself.
struct Extras {
    /// Finished print jobs, saved in the user's folder.
    printed: std::sync::mpsc::Receiver<crate::print::PrintedFile>,
    kick: Option<std::sync::Arc<crate::presence::Kick>>,
    /// The Windows account, for signing it out on request.
    win_username: String,
}

/// Name of the redirected drive inside the session: `\\tsclient\P`.
/// Windows currently shows only the first character of a longer name sent by
/// the RDP library, so a one-letter name keeps the path predictable.
pub const DRIVE_NAME: &str = "P";
/// Soft cap on one batch of screen updates. Small batches keep a slow link
/// responsive: what is not sent yet stays pending and is superseded by newer
/// pixels instead of queueing up behind them.
const BATCH_BYTES: usize = 48 * 1024;
/// How many batches may be on their way to the browser at once. With the
/// unordered direct channel, a lost batch is resent without blocking the others,
/// so a few in flight keeps a high-latency link busy without bloating it.
const MAX_IN_FLIGHT: u32 = 4;

/// Largest clipboard text carried in either direction.
const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;

/// Clamp a browser-reported viewport to something an RDP server accepts.
pub fn sane_size(w: u32, h: u32) -> (u16, u16) {
    let w = w.clamp(800, 3840) as u16;
    let h = h.clamp(600, 2160) as u16;
    (w - w % 4, h - h % 2)
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum ClientMsg {
    /// mouse move
    #[serde(rename = "m")]
    Move { x: i32, y: i32 },
    /// mouse button: b = 0 left, 1 middle, 2 right (DOM numbering)
    #[serde(rename = "b")]
    Button { b: u8, d: bool },
    /// wheel, in notches (positive = away from the user / right)
    #[serde(rename = "w")]
    Wheel { v: i32, h: i32 },
    /// key by DOM `KeyboardEvent.code`
    #[serde(rename = "k")]
    Key { c: String, d: bool },
    /// lock-key state, sent when the browser regains focus
    #[serde(rename = "sync")]
    Sync { caps: bool, num: bool, scroll: bool },
    /// focus lost: release everything that is held down
    #[serde(rename = "rel")]
    ReleaseAll,
    /// the browser window changed size
    #[serde(rename = "resize")]
    Resize { w: u32, h: u32 },
    /// text the user copied on their own computer
    #[serde(rename = "clip")]
    Clip { s: String },
    /// the browser has drawn screen batch `n`
    #[serde(rename = "ack")]
    Ack { n: u32 },
    /// the browser has drawn graphics-pipeline frame `id`
    #[serde(rename = "fa")]
    FrameAck { id: u32 },
    /// the user chose "Sign out of Windows" (closes their apps)
    #[serde(rename = "signout")]
    SignOut,
    /// text typed on an on-screen keyboard, which reports characters, not keys
    #[serde(rename = "txt")]
    Text { s: String },
}

/// What the clipboard channel tells the session loop. The channel calls its
/// backend synchronously while a PDU is processed, so events are queued and
/// handled once processing returns.
enum ClipEvent {
    /// The host wants to know what is on our clipboard.
    RequestFormatList,
    /// Something was copied on the host; `true` if it offers text.
    RemoteCopy(bool),
    /// The host is pasting and asks for our data in this format.
    DataRequest(ClipboardFormatId),
    /// Text fetched from the host's clipboard.
    RemoteText(String),
}

#[derive(Debug)]
struct ClipBackend {
    tx: std::sync::mpsc::Sender<ClipEvent>,
}

impl_as_any!(ClipBackend);

impl CliprdrBackend for ClipBackend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    // Text only: no file streams, no locking.
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let _ = self.tx.send(ClipEvent::RequestFormatList);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let has_text = available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);
        let _ = self.tx.send(ClipEvent::RemoteCopy(has_text));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.tx.send(ClipEvent::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() || response.data().len() > MAX_CLIPBOARD_BYTES {
            return;
        }
        if let Ok(text) = response.to_unicode_string() {
            let _ = self.tx.send(ClipEvent::RemoteText(text));
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// The shared drive: one folder on the portal host, shown inside the session.
#[cfg(windows)]
fn drive_channel(
    dir: &std::path::Path,
    printed: std::sync::mpsc::Sender<crate::print::PrintedFile>,
) -> anyhow::Result<ironrdp_rdpdr::Rdpdr> {
    use ironrdp_rdpdr::{RdpdrBackendFactory as _, RdpdrDrive};
    use ironrdp_rdpdr_native::{RedirectedDrive, WindowsRdpdrBackendFactory, FIRST_NAMED_DRIVE_DEVICE_ID};

    let drive = RedirectedDrive::new(FIRST_NAMED_DRIVE_DEVICE_ID, DRIVE_NAME, dir, false).map_err(|e| anyhow!("drive setup: {e}"))?;
    let product = WindowsRdpdrBackendFactory::new(drive)
        .build_rdpdr_backend()
        .map_err(|e| anyhow!("drive backend: {e}"))?;
    let (backend, drives) = product.into_parts();
    let drives = drives.into_iter().map(RdpdrDrive::into_parts).collect();
    // Printing rides on the same channel: jobs land in the same folder as PDFs.
    let backend = Box::new(crate::print::Backend::new(backend, dir.to_path_buf(), printed));
    Ok(ironrdp_rdpdr::Rdpdr::new(backend, "access-portal".to_owned())
        .with_drives(Some(drives))
        .with_printer_driver(
            crate::print::PRINTER_DEVICE_ID,
            crate::print::PRINTER_NAME.to_owned(),
            crate::print::PRINTER_DRIVER.to_owned(),
        ))
}

/// Encode queued messages of one static channel into a frame for the host.
fn svc_frame<P: SvcProcessor + 'static>(
    stage: &ActiveStage,
    messages: SvcProcessorMessages<P>,
) -> anyhow::Result<Vec<u8>> {
    stage
        .process_svc_processor_messages(messages)
        .map_err(|e| anyhow!("encoding channel data: {e:#}"))
}

/// NTLM needs no outbound network calls; Kerberos would, and is not configured.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ConnectorResult<Vec<u8>> {
        Err(ConnectorError::general("Kerberos network requests are not supported"))
    }
}

fn config(target: &Target) -> Config {
    Config {
        desktop_size: DesktopSize { width: target.width, height: target.height },
        monitor_layout: None,
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        // NLA only: never fall back to the graphical logon screen.
        enable_tls: false,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: target.username.clone(),
            password: target.password.clone(),
        },
        domain: None,
        client_build: 0,
        client_name: "access-portal".to_owned(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: if cfg!(windows) { MajorPlatformType::WINDOWS } else { MajorPlatformType::UNIX },
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        // With Windows' H.264 the full desktop experience costs little: show window
        // contents while dragging, animations, smooth fonts and composition.
        performance_flags: if target.egfx {
            PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION
        } else {
            PerformanceFlags::default()
        },
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        // Pointer shapes are forwarded to the browser, which draws the cursor itself.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: target.egfx,
    }
}

async fn connect(
    target: &Target,
) -> anyhow::Result<(ConnectionResult, TlsFramed, std::sync::mpsc::Receiver<ClipEvent>, Option<crate::gfx::SharedRef>, Extras)> {
    let addr = format!("{}:{}", target.host, target.port);
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("timed out connecting to {addr}"))?
        .with_context(|| format!("connecting to {addr}"))?;
    tcp.set_nodelay(true).ok();
    let client_addr = tcp.local_addr().context("local address")?;

    let mut framed = MovableTokioFramed::new(tcp);
    let (clip_tx, clip_rx) = std::sync::mpsc::channel();
    #[allow(unused_variables)]
    let (print_tx, print_rx) = std::sync::mpsc::channel();
    #[allow(unused_mut)]
    // Display control lets the session follow the browser window's size.
    let mut dvc = DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    let mut gfx_shared = None;
    if target.egfx {
        let (gfx, shared) = crate::gfx::channel();
        dvc = dvc.with_dynamic_channel(gfx);
        gfx_shared = Some(shared);
    }
    let mut connector = ClientConnector::new(config(target), client_addr)
        .with_static_channel(dvc)
        .with_static_channel(CliprdrClient::new(Box::new(ClipBackend { tx: clip_tx })));
    #[cfg(windows)]
    if let Some(dir) = &target.files_dir {
        match drive_channel(dir, print_tx) {
            Ok(rdpdr) => {
                // Windows only starts device redirection when the audio channel
                // is present too, so join it with a backend that plays nothing.
                connector.attach_static_channel(ironrdp_rdpsnd::client::Rdpsnd::new(Box::new(
                    ironrdp_rdpsnd::client::NoopRdpsndBackend,
                )));
                connector.attach_static_channel(rdpdr);
            }
            Err(e) => tracing::warn!("file sharing disabled for {}: {e:#}", target.username),
        }
    }

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| anyhow!("RDP negotiation failed: {e:#}"))?;

    // The host's certificate is self-signed; the channel is bound to it by CredSSP.
    let tcp = framed.into_inner_no_leftover();
    let (tls, cert) = ironrdp_tls::upgrade(tcp, &target.host).await.context("TLS upgrade")?;
    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&cert)
        .ok_or_else(|| anyhow!("host certificate has no public key"))?
        .to_owned();

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = MovableTokioFramed::new(tls);

    let outcome = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ServerName::new(target.host.clone()),
        server_public_key,
        None,
    )
    .await
    .map_err(|e| anyhow!("RDP sign-in failed: {e:#}"))?;

    let result = outcome.into_connected().map_err(|e| anyhow!("{e:#}"))?;
    let extras = Extras { printed: print_rx, kick: target.kick.clone(), win_username: target.username.clone() };
    Ok((result, framed, clip_rx, gfx_shared, extras))
}

/// Run one RDP session for one browser tab. Returns when either side closes.
pub async fn run(target: Target, mut io: Box<dyn Transport>) {
    let outcome = match connect(&target).await {
        Ok((result, framed, clip_rx, gfx, extras)) => {
            session(result, framed, clip_rx, target.resizable, target.video_ok && gfx.is_none(), gfx, extras, io.as_mut()).await
        }
        Err(e) => Err(e),
    };
    let message = match outcome {
        Ok(reason) => reason,
        Err(e) => {
            tracing::warn!("rdp session for {} ended with error: {e:#}", target.username);
            if target.show_errors {
                format!("{e:#}")
            } else {
                "The app could not be opened. Please try again or contact your administrator.".to_owned()
            }
        }
    };
    let _ = io.send_text(json!({"t": "closed", "m": message}).to_string()).await;
    // Let the message leave before the link closes: a data channel drops
    // whatever is still queued when it closes, and the page would then
    // reconnect instead of showing why the session ended.
    tokio::time::sleep(Duration::from_millis(600)).await;
    io.close().await;
}

fn png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 8 + 256);
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header()?;
        writer.write_image_data(data)?;
    }
    Ok(out)
}

/// Graphics-pipeline operations to the browser. Consecutive commands go as one
/// JSON message; pictures and pixels as binary messages between them, so the
/// browser sees everything in Windows' order.
///
/// Binary formats (little endian):
///   H.264 picture: [5][surface u16][flags u8: 1 = draw, 2 = keyframe][rects u16]
///                  [x, y, w, h u16 each rect][Annex B bytes]
///   pixels:        [6][surface u16][x u16][y u16][w u16][h u16][PNG]
async fn send_gfx_ops(io: &mut dyn Transport, ops: Vec<crate::gfx::Op>) -> anyhow::Result<()> {
    use crate::gfx::Op;
    let mut batch: Vec<serde_json::Value> = Vec::new();
    for op in ops {
        let json = match op {
            Op::Reset { width, height } => json!({"o": "reset", "w": width, "h": height}),
            Op::CreateSurface { id, width, height } => json!({"o": "cs", "id": id, "w": width, "h": height}),
            Op::DeleteSurface { id } => json!({"o": "ds", "id": id}),
            Op::MapSurface { id, x, y } => json!({"o": "ms", "id": id, "x": x, "y": y}),
            Op::Fill { surface, rgb, rects } => json!({"o": "fill", "s": surface, "c": rgb, "r": rects}),
            Op::Copy { from, to, rect, points } => json!({"o": "copy", "f": from, "s": to, "r": rect, "p": points}),
            Op::ToCache { surface, slot, rect } => json!({"o": "tc", "s": surface, "slot": slot, "r": rect}),
            Op::FromCache { slot, surface, points } => json!({"o": "fc", "s": surface, "slot": slot, "p": points}),
            Op::Evict { slot } => json!({"o": "ev", "slot": slot}),
            Op::EndFrame { id } => json!({"o": "ef", "id": id}),
            Op::Video { surface, draw, key, rects, data } => {
                if !batch.is_empty() {
                    send_json(io, json!({"t": "g", "ops": std::mem::take(&mut batch)})).await?;
                }
                let mut msg = Vec::with_capacity(6 + rects.len() * 8 + data.len());
                msg.push(5);
                msg.extend_from_slice(&surface.to_le_bytes());
                msg.push(u8::from(draw) | (u8::from(key) << 1));
                msg.extend_from_slice(&(rects.len() as u16).to_le_bytes());
                for (x, y, w, h) in rects {
                    for v in [x, y, w, h] {
                        msg.extend_from_slice(&v.to_le_bytes());
                    }
                }
                msg.extend_from_slice(&data);
                io.send_binary(msg).await?;
                continue;
            }
            Op::Pixels { surface, rect: (x, y, w, h), rgba } => {
                if !batch.is_empty() {
                    send_json(io, json!({"t": "g", "ops": std::mem::take(&mut batch)})).await?;
                }
                if w == 0 || h == 0 {
                    continue;
                }
                let image = png(u32::from(w), u32::from(h), png::ColorType::Rgba, &rgba)?;
                let mut msg = Vec::with_capacity(11 + image.len());
                msg.push(6);
                for v in [surface, x, y, w, h] {
                    msg.extend_from_slice(&v.to_le_bytes());
                }
                msg.extend_from_slice(&image);
                io.send_binary(msg).await?;
                continue;
            }
        };
        batch.push(json);
    }
    if !batch.is_empty() {
        send_json(io, json!({"t": "g", "ops": batch})).await?;
    }
    Ok(())
}

async fn send_json(io: &mut dyn Transport, value: serde_json::Value) -> anyhow::Result<()> {
    io.send_text(value.to_string()).await
}

async fn session(
    result: ConnectionResult,
    mut framed: TlsFramed,
    clip_rx: std::sync::mpsc::Receiver<ClipEvent>,
    resizable: bool,
    video_ok: bool,
    gfx: Option<crate::gfx::SharedRef>,
    extras: Extras,
    io: &mut dyn Transport,
) -> anyhow::Result<String> {
    let kicked = async {
        match &extras.kick {
            Some(k) => k.wait().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(kicked);
    let ConnectionResult {
        io_channel_id,
        user_channel_id,
        message_channel_id,
        share_id,
        static_channels,
        desktop_size,
        enable_server_pointer,
        pointer_software_rendering,
        activation_factory,
        compression_type,
        ..
    } = result;

    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
    let mut stage = ActiveStageBuilder {
        static_channels,
        user_channel_id,
        io_channel_id,
        message_channel_id,
        share_id,
        compression_type,
        enable_server_pointer,
        pointer_software_rendering,
    }
    .build();

    send_json(io, json!({"t": "size", "w": image.width(), "h": image.height()})).await?;

    let mut input = Database::new();
    let mut tiles = Tiles::new(image.width(), image.height(), image.data().len());
    tracing::debug!("session start: video_ok={video_ok}");
    let mut video = Video::new(image.width(), image.height(), video_ok);
    // Graphics pipeline: frames Windows sent so far, acknowledged as the browser draws them.
    let mut gfx_frames_drawn: u32 = 0;
    // Diagnostics: when each frame's end reached the browser, and per-second counts.
    let mut gfx_sent_at: std::collections::HashMap<u32, std::time::Instant> = std::collections::HashMap::new();
    let (mut gfx_in, mut gfx_acked, mut gfx_ack_ms, mut gfx_bytes) = (0u32, 0u32, 0f64, 0usize);
    let mut moves_in = 0u32;
    // Input -> the next frame from Windows (how fast the host reacts, plus the portal).
    let mut gfx_input_at: Option<std::time::Instant> = None;
    let mut gfx_react: Vec<f64> = Vec::new();
    let mut gfx_report_at = std::time::Instant::now();
    // Screen batches sent, and the last one the browser reported drawing.
    let (mut sent_seq, mut acked_seq) = (0u32, 0u32);
    // Something is waiting to be sent. The host draws one frame as a burst of
    // updates; the loop below takes everything the host has already delivered
    // first and sends the moment nothing more is queued. No timer is involved,
    // because short timers are only accurate to about 15 ms on Windows.
    let mut flush_due = false;
    // Timing probes: input sent to host -> host's first redraw -> batch sent.
    let mut input_at: Option<std::time::Instant> = None;
    let mut redraw_at: Option<std::time::Instant> = None;
    let (mut host_ms, mut portal_ms): (Vec<f32>, Vec<f32>) = (Vec::new(), Vec::new());
    let mut last_cursor = String::new();
    // Text copied on the user's own computer, offered to the host on request.
    let mut local_clip: Option<String> = None;
    let text_format = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
    // Housekeeping only (shared-drive replies, and a safety net for pending tiles):
    // screen changes are sent from the `flush_at` branch as they happen.
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A still screen sends nothing; ping the browser so proxies keep the link open.
    const KEEPALIVE: Duration = Duration::from_secs(20);
    let mut last_keepalive = std::time::Instant::now();

    // Send what changed. Busy areas go out as H.264 (the video layer); still
    // areas go out as tiles, skipping whatever the video layer is covering.
    // Tiles are held back while the browser is still busy with earlier batches.
    macro_rules! send_pending {
        () => {{
            for event in video.step(&image) {
                match event {
                    VideoEvent::Start { block, gen, x, y, w, h } => {
                        tracing::debug!("video start block {block} gen {gen} at {x},{y} {w}x{h}");
                        send_json(io, json!({"t": "vstart", "b": block, "g": gen, "x": x, "y": y, "w": w, "h": h})).await?;
                    }
                    VideoEvent::Packet { block, gen, keyframe, seq, data } => {
                        io.send_binary(packet_message(block, gen, keyframe, seq, &data)).await?;
                    }
                    VideoEvent::Stop { block, rect } => {
                        // The browser's video pixels linger there; redraw it crisply.
                        tiles.invalidate(&rect);
                        send_json(io, json!({"t": "vstop", "b": block})).await?;
                    }
                }
            }
            let skip = video.active_rects();
            if sent_seq.wrapping_sub(acked_seq) < MAX_IN_FLIGHT && tiles.has_work() {
                let refine = !tiles.has_dirty();
                if let Some(batch) = tiles.next_batch(
                    image.data(),
                    image.stride(),
                    image.bytes_per_pixel(),
                    sent_seq.wrapping_add(1),
                    BATCH_BYTES,
                    refine,
                    &skip,
                ) {
                    sent_seq = sent_seq.wrapping_add(1);
                    if let Some(t) = redraw_at.take() {
                        portal_ms.push(t.elapsed().as_secs_f32() * 1000.0);
                    }
                    io.send_binary(batch).await?;
                }
            }
            // More to send (budget reached, or tiles to sharpen) and room for it:
            // come straight back. Otherwise wait for an acknowledgement or a change.
            flush_due = tiles.has_work() && sent_seq.wrapping_sub(acked_seq) < MAX_IN_FLIGHT;
        }};
    }

    loop {
        if host_ms.len() >= 20 {
            let median = |v: &mut Vec<f32>| {
                v.sort_by(|a, b| a.total_cmp(b));
                v.get(v.len() / 2).copied().unwrap_or(0.0)
            };
            tracing::debug!(
                "timing: input -> host redraw {:.0} ms, redraw -> sent {:.1} ms (medians of {})",
                median(&mut host_ms),
                median(&mut portal_ms),
                host_ms.len()
            );
            host_ms.clear();
            portal_ms.clear();
        }
        // Print jobs finished since the last turn: offer them in the browser.
        while let Ok(job) = extras.printed.try_recv() {
            send_json(io, json!({"t": "print", "name": job.name, "size": job.size})).await?;
        }
        // Clipboard events queued while the last PDU was processed.
        while let Ok(event) = clip_rx.try_recv() {
            if let ClipEvent::RemoteText(text) = event {
                send_json(io, json!({"t": "clip", "s": text.replace("\r\n", "\n")})).await?;
                continue;
            }
            let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() else {
                continue;
            };
            let messages = match event {
                ClipEvent::RequestFormatList => {
                    clip.initiate_copy(if local_clip.is_some() { &text_format[..] } else { &[] })
                }
                ClipEvent::RemoteCopy(true) => clip.initiate_paste(ClipboardFormatId::CF_UNICODETEXT),
                ClipEvent::DataRequest(format) => {
                    let response = match &local_clip {
                        Some(text) if format == ClipboardFormatId::CF_UNICODETEXT => {
                            FormatDataResponse::new_unicode_string(text)
                        }
                        _ => FormatDataResponse::new_error(),
                    };
                    clip.submit_format_data(response)
                }
                ClipEvent::RemoteCopy(false) | ClipEvent::RemoteText(_) => continue,
            }
            .map_err(|e| anyhow!("clipboard error: {e:#}"))?;
            let frame = svc_frame(&stage, messages)?;
            if !frame.is_empty() {
                framed.write_all(&frame).await.context("writing to the host")?;
            }
        }

        let outputs = tokio::select! {
            biased;
            reason = &mut kicked => {
                return Ok(reason);
            }
            // The browser first: input and frame acknowledgements are tiny, and
            // must never wait behind a stream of screen updates from the host
            // (that adds input lag, and holds back acknowledgements Windows waits for).
            msg = io.recv() => {
                let msg = match msg {
                    Incoming::Text(text) => serde_json::from_str::<ClientMsg>(&text).ok(),
                    Incoming::Closed => {
                        // Tab closed: drop the connection. Windows keeps the session
                        // (and the app) running, disconnected, for the next sign-in.
                        return Ok("browser closed".to_owned());
                    }
                };
                let events = match msg {
                    None => continue,
                    Some(ClientMsg::SignOut) => {
                        let user = extras.win_username.clone();
                        tokio::task::spawn_blocking(move || crate::host::sign_out_blocking(&user)).await.ok();
                        return Ok("You signed out of Windows. Your apps were closed.".to_owned());
                    }
                    Some(ClientMsg::FrameAck { id }) => {
                        if let Some(t) = gfx_sent_at.remove(&id) {
                            gfx_acked += 1;
                            gfx_ack_ms += t.elapsed().as_secs_f64() * 1000.0;
                        }
                        let channel_id = gfx.as_ref().and_then(|g| g.lock().unwrap().channel_id);
                        if let Some(channel_id) = channel_id {
                            gfx_frames_drawn = gfx_frames_drawn.wrapping_add(1);
                            let messages = ironrdp_dvc::encode_dvc_messages(
                                channel_id,
                                vec![crate::gfx::frame_ack(id, gfx_frames_drawn)],
                                ironrdp_svc::ChannelFlags::empty(),
                            )
                            .map_err(|e| anyhow!("graphics acknowledgement: {e}"))?;
                            let frame = svc_frame(&stage, SvcProcessorMessages::<DrdynvcClient>::new(messages))?;
                            framed.write_all(&frame).await.context("writing to the host")?;
                        }
                        continue;
                    }
                    Some(ClientMsg::Ack { n }) => {
                        if n <= sent_seq && n > acked_seq {
                            acked_seq = n;
                        }
                        // Room for another batch: send what has been waiting.
                        if tiles.has_work() {
                            flush_due = true;
                        }
                        continue;
                    }
                    Some(ClientMsg::Resize { w, h }) => {
                        // The host answers with a Deactivation-Reactivation at the new size.
                        let (w, h) = sane_size(w, h);
                        if resizable && (w, h) != (image.width(), image.height()) {
                            if let Some(frame) = stage.encode_resize(u32::from(w), u32::from(h), None, None) {
                                let frame = frame.map_err(|e| anyhow!("RDP resize error: {e:#}"))?;
                                framed.write_all(&frame).await.context("writing to the host")?;
                            }
                        }
                        continue;
                    }
                    Some(ClientMsg::Clip { s }) => {
                        // Windows clipboard text uses CRLF line endings.
                        let text = s.replace("\r\n", "\n").replace('\n', "\r\n");
                        if text.len() <= MAX_CLIPBOARD_BYTES && local_clip.as_deref() != Some(text.as_str()) {
                            local_clip = Some(text);
                            if let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() {
                                if clip.is_ready() {
                                    let messages = clip
                                        .initiate_copy(&text_format)
                                        .map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                                    let frame = svc_frame(&stage, messages)?;
                                    framed.write_all(&frame).await.context("writing to the host")?;
                                }
                            }
                        }
                        continue;
                    }
                    Some(other) => {
                        if matches!(other, ClientMsg::Move { .. }) {
                            moves_in += 1;
                        }
                        gfx_input_at.get_or_insert_with(std::time::Instant::now);
                        input_events(&mut input, other, image.width(), image.height())
                    }
                };
                if events.is_empty() {
                    continue;
                }
                input_at.get_or_insert_with(std::time::Instant::now);
                stage
                    .process_fastpath_input(&mut image, &events)
                    .map_err(|e| anyhow!("RDP input error: {e:#}"))?
            }
            pdu = framed.read_pdu() => {
                let (action, payload) = pdu.context("connection to the host was lost")?;
                // When the frame arrived: Windows measures the link's bandwidth from
                // these, and without them it is told nothing arrived and throttles.
                let received_at = framed.last_read_at();
                stage
                    .process(&mut image, action, &payload, received_at)
                    .map_err(|e| anyhow!("RDP protocol error: {e:#}"))?
            }
            // Reached only when neither the host nor the browser has anything
            // waiting (the branches above are tried first).
            _ = std::future::ready(()), if flush_due => {
                send_pending!();
                continue;
            }
            _ = ticker.tick() => {
                send_pending!();
                if last_keepalive.elapsed() >= KEEPALIVE {
                    io.keepalive().await?;
                    last_keepalive = std::time::Instant::now();
                }
                // File operations on the shared drive finish on worker threads;
                // collect their replies for the host.
                #[cfg(windows)]
                {
                    let deferred = match stage.get_svc_processor_mut::<ironrdp_rdpdr::Rdpdr>() {
                        Some(rdpdr) => rdpdr
                            .poll_deferred_messages()
                            .map_err(|e| anyhow!("shared drive error: {e:#}"))?,
                        None => Vec::new(),
                    };
                    if !deferred.is_empty() {
                        let frame = svc_frame(&stage, SvcProcessorMessages::<ironrdp_rdpdr::Rdpdr>::new(deferred))?;
                        framed.write_all(&frame).await.context("writing to the host")?;
                    }
                }
                continue;
            }
        };

        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed.write_all(&frame).await.context("writing to the host")?;
                }
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    tiles.mark(&rect);
                    video.mark(&rect);
                    flush_due = true;
                    if let Some(t) = input_at.take() {
                        host_ms.push(t.elapsed().as_secs_f32() * 1000.0);
                        redraw_at = Some(std::time::Instant::now());
                    }
                }
                ActiveStageOutput::PointerDefault
                | ActiveStageOutput::PointerHidden
                | ActiveStageOutput::PointerBitmap(_) => {
                    let msg = match &output {
                        ActiveStageOutput::PointerDefault => Some(json!({"t": "cur", "k": "default"})),
                        ActiveStageOutput::PointerHidden => Some(json!({"t": "cur", "k": "none"})),
                        ActiveStageOutput::PointerBitmap(p) if p.width > 0 && p.height > 0 => png(
                            u32::from(p.width),
                            u32::from(p.height),
                            png::ColorType::Rgba,
                            &p.bitmap_data,
                        )
                        .ok()
                        .map(|data| {
                            json!({
                                "t": "cur", "k": "png", "d": STANDARD.encode(data),
                                "x": p.hotspot_x, "y": p.hotspot_y,
                            })
                        }),
                        _ => None,
                    };
                    // The host re-sends the same shape constantly; only forward changes.
                    if let Some(msg) = msg.map(|m| m.to_string()) {
                        if msg != last_cursor {
                            io.send_text(msg.clone()).await?;
                            last_cursor = msg;
                        }
                    }
                }
                ActiveStageOutput::DeactivateAll => {
                    // Deactivation-Reactivation: the server renegotiates capabilities
                    // (and possibly the resolution) mid-session.
                    let mut sequence = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    loop {
                        ironrdp_tokio::single_sequence_step(&mut framed, &mut sequence, &mut buf)
                            .await
                            .map_err(|e| anyhow!("RDP reactivation failed: {e:#}"))?;
                        match sequence.connection_activation_state() {
                            ConnectionActivationState::Finalized {
                                desktop_size,
                                share_id,
                                static_channel_chunk_size,
                                enable_server_pointer,
                                pointer_software_rendering,
                                ..
                            } => {
                                image = DecodedImage::new(
                                    PixelFormat::RgbA32,
                                    desktop_size.width,
                                    desktop_size.height,
                                );
                                stage.reactivate(
                                    io_channel_id,
                                    user_channel_id,
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                    static_channel_chunk_size,
                                );
                                send_json(io, json!({"t": "size", "w": image.width(), "h": image.height()})).await?;
                                // New framebuffer, blank on both sides: start over.
                                tiles = Tiles::new(image.width(), image.height(), image.data().len());
                                video = Video::new(image.width(), image.height(), video_ok);
                                (sent_seq, acked_seq) = (0, 0);
                                break;
                            }
                            ConnectionActivationState::Redirected { .. } => {
                                bail!("the host redirected the session, which is not supported");
                            }
                            _ => {}
                        }
                    }
                }
                ActiveStageOutput::Terminate(reason) => return Ok(reason.description()),
                _ => {}
            }
        }

        // Graphics pipeline: pass what Windows sent on to the browser, in order.
        if let Some(shared) = &gfx {
            let ops = std::mem::take(&mut shared.lock().unwrap().ops);
            if !ops.is_empty() {
                for op in &ops {
                    match op {
                        crate::gfx::Op::EndFrame { id } => {
                            gfx_in += 1;
                            if let Some(t) = gfx_input_at.take() {
                                gfx_react.push(t.elapsed().as_secs_f64() * 1000.0);
                            }
                            gfx_sent_at.insert(*id, std::time::Instant::now());
                        }
                        crate::gfx::Op::Video { data, .. } => gfx_bytes += data.len(),
                        _ => {}
                    }
                }
                send_gfx_ops(io, ops).await?;
            }
            if gfx_report_at.elapsed() >= Duration::from_secs(1) {
                if gfx_in > 0 {
                    gfx_react.sort_by(f64::total_cmp);
                    tracing::debug!(
                        "graphics: {gfx_in} frames from Windows, {gfx_acked} drawn, browser draw+ack {:.1} ms avg, {} KB video, {moves_in} mouse moves in, input -> next frame median {:.0} ms",
                        if gfx_acked > 0 { gfx_ack_ms / f64::from(gfx_acked) } else { 0.0 },
                        gfx_bytes / 1024,
                        gfx_react.get(gfx_react.len() / 2).copied().unwrap_or(0.0)
                    );
                    gfx_react.clear();
                }
                (gfx_in, gfx_acked, gfx_ack_ms, gfx_bytes, moves_in) = (0, 0, 0.0, 0, 0);
                gfx_report_at = std::time::Instant::now();
            }
        }
    }
}

fn input_events(db: &mut Database, msg: ClientMsg, width: u16, height: u16) -> Vec<FastPathInputEvent> {
    let ops: Vec<Operation> = match msg {
        ClientMsg::Move { x, y } => vec![Operation::MouseMove(MousePosition {
            x: x.clamp(0, i32::from(width) - 1) as u16,
            y: y.clamp(0, i32::from(height) - 1) as u16,
        })],
        ClientMsg::Button { b, d } => match MouseButton::from_web_button(b) {
            Some(button) if d => vec![Operation::MouseButtonPressed(button)],
            Some(button) => vec![Operation::MouseButtonReleased(button)],
            None => Vec::new(),
        },
        ClientMsg::Wheel { v, h } => {
            let mut ops = Vec::new();
            if v != 0 {
                ops.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: true,
                    rotation_units: (v.clamp(-10, 10) * 120) as i16,
                }));
            }
            if h != 0 {
                ops.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: false,
                    rotation_units: (h.clamp(-10, 10) * 120) as i16,
                }));
            }
            ops
        }
        ClientMsg::Key { c, d } => match scancode(&c) {
            Some((extended, code)) => {
                let sc = Scancode::from_u8(extended, code);
                vec![if d { Operation::KeyPressed(sc) } else { Operation::KeyReleased(sc) }]
            }
            None => Vec::new(),
        },
        ClientMsg::Text { s } => {
            let shift = Scancode::from_u8(false, 0x2A);
            let mut ops = Vec::new();
            for c in s.chars().take(4096) {
                match char_key(c) {
                    // Characters on a US keyboard are pressed as real keys, which
                    // older applications handle more reliably than Unicode input.
                    Some((code, shifted)) => {
                        let key = Scancode::from_u8(false, code);
                        if shifted {
                            ops.push(Operation::KeyPressed(shift));
                        }
                        ops.push(Operation::KeyPressed(key));
                        ops.push(Operation::KeyReleased(key));
                        if shifted {
                            ops.push(Operation::KeyReleased(shift));
                        }
                    }
                    None if !c.is_control() => {
                        ops.push(Operation::UnicodeKeyPressed(c));
                        ops.push(Operation::UnicodeKeyReleased(c));
                    }
                    None => {}
                }
            }
            ops
        }
        ClientMsg::Sync { caps, num, scroll } => {
            return vec![ironrdp_input::synchronize_event(scroll, num, caps, false)];
        }
        ClientMsg::ReleaseAll => return db.release_all().into_iter().collect(),
        // Handled by the session loop, not as input.
        ClientMsg::Resize { .. } | ClientMsg::Clip { .. } | ClientMsg::Ack { .. } | ClientMsg::FrameAck { .. } | ClientMsg::SignOut => {
            return Vec::new()
        }
    };
    db.apply(ops).into_iter().collect()
}

/// A character -> the US-layout key that types it: `(scancode, needs shift)`.
fn char_key(c: char) -> Option<(u8, bool)> {
    const PLAIN: &str = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./ ";
    const SHIFT: &str = "~!@#$%^&*()_+QWERTYUIOP{}|ASDFGHJKL:\"ZXCVBNM<>? ";
    const CODES: [u8; 48] = [
        0x29, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, // ` 1..0 - =
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x2B, // q..p [ ] \
        0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, // a..l ; '
        0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, // z..m , . /
        0x39, // space
    ];
    match c {
        '\n' | '\r' => return Some((0x1C, false)),
        '\t' => return Some((0x0F, false)),
        _ => {}
    }
    if let Some(i) = PLAIN.chars().position(|p| p == c) {
        return Some((CODES[i], false));
    }
    SHIFT.chars().position(|p| p == c).filter(|i| *i < 47).map(|i| (CODES[i], true))
}

/// DOM `KeyboardEvent.code` -> PC/AT set 1 scancode `(extended, code)`.
fn scancode(code: &str) -> Option<(bool, u8)> {
    let plain = |c: u8| Some((false, c));
    let ext = |c: u8| Some((true, c));
    match code {
        "Escape" => plain(0x01),
        "Digit1" => plain(0x02),
        "Digit2" => plain(0x03),
        "Digit3" => plain(0x04),
        "Digit4" => plain(0x05),
        "Digit5" => plain(0x06),
        "Digit6" => plain(0x07),
        "Digit7" => plain(0x08),
        "Digit8" => plain(0x09),
        "Digit9" => plain(0x0A),
        "Digit0" => plain(0x0B),
        "Minus" => plain(0x0C),
        "Equal" => plain(0x0D),
        "Backspace" => plain(0x0E),
        "Tab" => plain(0x0F),
        "KeyQ" => plain(0x10),
        "KeyW" => plain(0x11),
        "KeyE" => plain(0x12),
        "KeyR" => plain(0x13),
        "KeyT" => plain(0x14),
        "KeyY" => plain(0x15),
        "KeyU" => plain(0x16),
        "KeyI" => plain(0x17),
        "KeyO" => plain(0x18),
        "KeyP" => plain(0x19),
        "BracketLeft" => plain(0x1A),
        "BracketRight" => plain(0x1B),
        "Enter" => plain(0x1C),
        "ControlLeft" => plain(0x1D),
        "KeyA" => plain(0x1E),
        "KeyS" => plain(0x1F),
        "KeyD" => plain(0x20),
        "KeyF" => plain(0x21),
        "KeyG" => plain(0x22),
        "KeyH" => plain(0x23),
        "KeyJ" => plain(0x24),
        "KeyK" => plain(0x25),
        "KeyL" => plain(0x26),
        "Semicolon" => plain(0x27),
        "Quote" => plain(0x28),
        "Backquote" => plain(0x29),
        "ShiftLeft" => plain(0x2A),
        "Backslash" => plain(0x2B),
        "KeyZ" => plain(0x2C),
        "KeyX" => plain(0x2D),
        "KeyC" => plain(0x2E),
        "KeyV" => plain(0x2F),
        "KeyB" => plain(0x30),
        "KeyN" => plain(0x31),
        "KeyM" => plain(0x32),
        "Comma" => plain(0x33),
        "Period" => plain(0x34),
        "Slash" => plain(0x35),
        "ShiftRight" => plain(0x36),
        "NumpadMultiply" => plain(0x37),
        "AltLeft" => plain(0x38),
        "Space" => plain(0x39),
        "CapsLock" => plain(0x3A),
        "F1" => plain(0x3B),
        "F2" => plain(0x3C),
        "F3" => plain(0x3D),
        "F4" => plain(0x3E),
        "F5" => plain(0x3F),
        "F6" => plain(0x40),
        "F7" => plain(0x41),
        "F8" => plain(0x42),
        "F9" => plain(0x43),
        "F10" => plain(0x44),
        "NumLock" => plain(0x45),
        "ScrollLock" => plain(0x46),
        "Numpad7" => plain(0x47),
        "Numpad8" => plain(0x48),
        "Numpad9" => plain(0x49),
        "NumpadSubtract" => plain(0x4A),
        "Numpad4" => plain(0x4B),
        "Numpad5" => plain(0x4C),
        "Numpad6" => plain(0x4D),
        "NumpadAdd" => plain(0x4E),
        "Numpad1" => plain(0x4F),
        "Numpad2" => plain(0x50),
        "Numpad3" => plain(0x51),
        "Numpad0" => plain(0x52),
        "NumpadDecimal" => plain(0x53),
        "IntlBackslash" => plain(0x56),
        "F11" => plain(0x57),
        "F12" => plain(0x58),
        "NumpadEnter" => ext(0x1C),
        "ControlRight" => ext(0x1D),
        "NumpadDivide" => ext(0x35),
        "PrintScreen" => ext(0x37),
        "AltRight" => ext(0x38),
        "Home" => ext(0x47),
        "ArrowUp" => ext(0x48),
        "PageUp" => ext(0x49),
        "ArrowLeft" => ext(0x4B),
        "ArrowRight" => ext(0x4D),
        "End" => ext(0x4F),
        "ArrowDown" => ext(0x50),
        "PageDown" => ext(0x51),
        "Insert" => ext(0x52),
        "Delete" => ext(0x53),
        "MetaLeft" => ext(0x5B),
        "MetaRight" => ext(0x5C),
        "ContextMenu" => ext(0x5D),
        _ => None,
    }
}
