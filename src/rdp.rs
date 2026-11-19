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
//!   binary  [7][channels u8][rate u32][16-bit PCM]   sound
//!   binary  [9][channels u8][rate u32][AAC frames]   sound, compressed
//!   binary  [8][PNG]                         a picture copied in the session
//!   text    {"t":"clipfiles","files":[..]}   files copied in the session
//!   text    {"t":"idle","s":..}              disconnecting soon for inactivity
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
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
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

use crate::activity::Actor;
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
    /// Messages for the browser from the app (an administrator's message, ...).
    pub notices: Option<crate::presence::Notices>,
    /// Disconnect after this long without keyboard or mouse input.
    pub idle: Option<Duration>,
    /// The user's allowed hours: the session ends when they are over.
    pub rules: Option<crate::access::AccessRules>,
    /// Who is using the session, for the activity log.
    pub actor: Option<crate::activity::Actor>,
    /// Where files pasted from the browser wait for Windows to read them.
    pub clip_dir: Option<std::path::PathBuf>,
    /// The browser can decode AAC sound (much smaller than PCM).
    pub sound_aac: bool,
    /// The browser is on the local network: send the screen as lossless
    /// pictures, whole, instead of H.264 (see `tiles.rs`).
    pub lan: bool,
}

/// What a session needs besides the connection itself.
struct Extras {
    /// Finished print jobs, saved in the user's folder.
    printed: std::sync::mpsc::Receiver<crate::print::PrintedFile>,
    /// Sound from the session.
    sound: std::sync::mpsc::Receiver<crate::sound::Block>,
    /// The user's microphone and camera.
    devices: crate::devices::SharedRef,
    kick: Option<std::sync::Arc<crate::presence::Kick>>,
    notices: Option<crate::presence::Notices>,
    /// The Windows account, for signing it out on request.
    win_username: String,
    idle: Option<Duration>,
    rules: Option<crate::access::AccessRules>,
    actor: Option<crate::activity::Actor>,
    clip_dir: Option<std::path::PathBuf>,
    files_dir: Option<std::path::PathBuf>,
}

/// Name of the redirected drive inside the session: `\\tsclient\P`.
/// Windows currently shows only the first character of a longer name sent by
/// the RDP library, so a one-letter name keeps the path predictable.
pub const DRIVE_NAME: &str = "P";
/// Soft cap on one batch of screen updates. Small batches keep a slow link
/// responsive: what is not sent yet stays pending and is superseded by newer
/// pixels instead of queueing up behind them.
const BATCH_BYTES: usize = 48 * 1024;
/// On the local network the link is never the bottleneck: send everything
/// that changed at once, so the browser paints the whole change in one go
/// instead of a strip at a time.
const LAN_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Local network: lossless refreshes of photo areas sent as JPEG go in pieces
/// of about this size, one at a time, so a new change never waits long behind
/// one (about 20 ms on Wi-Fi).
const LAN_REFINE_BYTES: usize = 128 * 1024;
/// Local network: photo areas are made lossless once the screen has been still
/// this long (sooner would resend areas that are still moving).
const REFINE_AFTER: Duration = Duration::from_millis(150);
/// Local network: measured link speeds (bytes a second, including the
/// browser's unpacking) at which photo areas go lossless straight away, and
/// below which they go back to JPEG first. Wired gigabit is well above the
/// first; Wi-Fi is usually below the second.
const LOSSLESS_ON_RATE: f64 = 40e6;
const LOSSLESS_OFF_RATE: f64 = 20e6;
/// Batches smaller than this say more about the round trip than the speed.
const RATE_SAMPLE_BYTES: usize = 128 * 1024;
/// How many batches may be on their way to the browser at once. With the
/// unordered direct channel, a lost batch is resent without blocking the others,
/// so a few in flight keeps a high-latency link busy without bloating it.
const MAX_IN_FLIGHT: u32 = 4;

/// Largest clipboard text carried in either direction.
const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;
/// Largest copied picture taken from the host (a 4K screenshot is ~33 MB as a bitmap).
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Copied files are read from the host in pieces of this size.
const FILE_CHUNK: u32 = 256 * 1024;
/// Largest file copied out of the session.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

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
    /// sound on or off for this browser
    #[serde(rename = "snd")]
    Sound { on: bool },
    /// the session's screens: one entry per monitor, left to right
    #[serde(rename = "monitors")]
    Monitors { m: Vec<(u32, u32)> },
    /// measuring the link: answered at once with {"t":"pong","n":n}
    #[serde(rename = "ping")]
    Ping { n: f64 },
    /// a picture copied on the user's computer, uploaded as PNG to the
    /// clipboard folder `b` as `f`
    #[serde(rename = "clipimg")]
    ClipImage { b: String, f: String },
    /// files copied on the user's computer, uploaded to clipboard folder `b`
    #[serde(rename = "clipfiles")]
    ClipFiles { b: String, f: Vec<String> },
    /// save the files copied in the session to the user's folder
    #[serde(rename = "fetch")]
    Fetch,
}

/// What the clipboard channel tells the session loop. The channel calls its
/// backend synchronously while a PDU is processed, so events are queued and
/// handled once processing returns.
enum ClipEvent {
    /// The host wants to know what is on our clipboard.
    RequestFormatList,
    /// Something was copied on the host: text, a picture, files (the format
    /// that lists them).
    RemoteCopy { text: bool, image: bool, files: Option<ClipboardFormatId> },
    /// The host is pasting and asks for our data in this format.
    DataRequest(ClipboardFormatId),
    /// The host's answer to our last paste request; `None` if it failed.
    RemoteData(Option<Vec<u8>>),
    /// The files copied on the host, and the lock that keeps them readable.
    RemoteFiles(Vec<FileDescriptor>, Option<u32>),
    /// The host is pasting our files and wants (part of) one.
    FileRequest(FileContentsRequest),
    /// Part of a host file we asked for; `None` if it failed.
    FileData { stream_id: u32, data: Option<Vec<u8>> },
}

/// What is on the user's own clipboard, offered to the host.
enum LocalClip {
    Text(String),
    /// A picture as a Windows bitmap (CF_DIB).
    Image(Vec<u8>),
    /// Files waiting in the clipboard folder.
    Files(Vec<std::path::PathBuf>),
}

/// What our pending paste requests to the host were for, oldest first.
#[derive(Clone, Copy)]
enum Paste {
    Text,
    Image,
}

/// Copying files out of the session into the user's folder, one at a time.
struct Fetch {
    /// (index in the host's list, file name, size if known)
    queue: std::collections::VecDeque<(i32, String, Option<u64>)>,
    current: Option<FetchFile>,
    data_id: Option<u32>,
    next_stream: u32,
    saved: Vec<String>,
    last_progress: std::time::Instant,
}

struct FetchFile {
    index: i32,
    name: String,
    size: Option<u64>,
    stream_id: u32,
    file: tokio::fs::File,
    part: std::path::PathBuf,
    got: u64,
}

fn utf16_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Windows FILETIME (100 ns since 1601) from a file's modification time.
fn filetime(meta: &std::fs::Metadata) -> Option<u64> {
    let since = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_secs() * 10_000_000 + u64::from(since.subsec_nanos() / 100) + 116_444_736_000_000_000)
}

fn describe_files(paths: &[std::path::PathBuf]) -> Vec<FileDescriptor> {
    paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            let name = p.file_name()?.to_string_lossy().into_owned();
            let mut d = FileDescriptor::new(name)
                .with_attributes(ClipboardFileAttributes::NORMAL)
                .with_file_size(meta.len());
            if let Some(t) = filetime(&meta) {
                d = d.with_last_write_time(t);
            }
            Some(d)
        })
        .collect()
}

/// A plain file name that can't point outside its folder.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != ".." && !name.starts_with('.')
}

/// A file name that is free in `dir`: "report.pdf", then "report (1).pdf", ...
async fn free_name(dir: &std::path::Path, name: &str) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.to_string(), String::new()),
    };
    let mut candidate = name.to_string();
    let mut n = 1;
    while tokio::fs::try_exists(dir.join(&candidate)).await.unwrap_or(false) {
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    candidate
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

    // Text, pictures and files (streamed on request, with locking so a copy
    // stays readable while it is being read).
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let _ = self.tx.send(ClipEvent::RequestFormatList);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let has = |id: ClipboardFormatId| available_formats.iter().any(|f| f.id() == id);
        let files = available_formats
            .iter()
            .find(|f| f.name().is_some_and(|n| n.value() == "FileGroupDescriptorW"))
            .map(|f| f.id());
        let _ = self.tx.send(ClipEvent::RemoteCopy {
            text: has(ClipboardFormatId::CF_UNICODETEXT),
            image: has(ClipboardFormatId::CF_DIB),
            files,
        });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.tx.send(ClipEvent::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let ok = !response.is_error() && response.data().len() <= MAX_CLIPBOARD_IMAGE_BYTES;
        let _ = self.tx.send(ClipEvent::RemoteData(ok.then(|| response.data().to_vec())));
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        let _ = self.tx.send(ClipEvent::RemoteFiles(files.to_vec(), clip_data_id));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let _ = self.tx.send(ClipEvent::FileRequest(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let _ = self.tx.send(ClipEvent::FileData {
            stream_id: response.stream_id(),
            data: (!response.is_error()).then(|| response.data().to_vec()),
        });
    }

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
        // A current client build: Windows only offers newer features (microphone,
        // camera) to clients that report one.
        client_build: 22621,
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
        // A black-screen single-program session is done with the account's shell
        // (see host::set_desktop_mode), not the RDP alternate shell: Windows Home
        // delivers an alternate shell but refuses to launch it ("Access is
        // denied"), the same way it refuses RemoteApp.
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: if cfg!(windows) { MajorPlatformType::WINDOWS } else { MajorPlatformType::UNIX },
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: true,
        enable_audio_capture: true,
        // With Windows' H.264, or on the local network, the full desktop
        // experience costs little: smooth fonts and composition.
        performance_flags: if target.egfx || target.lan {
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

#[allow(clippy::type_complexity)]
async fn connect(
    target: &Target,
) -> anyhow::Result<(
    ConnectionResult,
    TlsFramed,
    std::sync::mpsc::Receiver<ClipEvent>,
    Option<crate::gfx::SharedRef>,
    std::sync::mpsc::Receiver<crate::print::PrintedFile>,
    std::sync::mpsc::Receiver<crate::sound::Block>,
    crate::devices::SharedRef,
)> {
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
    let (sound_tx, sound_rx) = std::sync::mpsc::channel();
    // Display control lets the session follow the browser window's size.
    // Sound: Windows prefers the dynamic audio channels, and sends AAC only on
    // the "lossy" one; both share one negotiation.
    let audio = ironrdp_rdpsnd::client::RdpsndDvcState::new(Box::new(crate::sound::Backend::new(sound_tx.clone(), target.sound_aac)));
    // The user's microphone and camera, for apps in the session.
    let devices = crate::devices::SharedRef::default();
    let mut dvc = DrdynvcClient::new()
        .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
        .with_dynamic_channel(audio.channel())
        .with_dynamic_channel(audio.lossy_channel())
        .with_dynamic_channel(crate::devices::microphone(devices.clone()))
        .with_dynamic_channel(crate::devices::camera_enumerator(devices.clone()))
        .with_dynamic_channel(crate::devices::camera_device(devices.clone()));
    let mut gfx_shared = None;
    if target.egfx {
        let (gfx, shared) = crate::gfx::channel();
        dvc = dvc.with_dynamic_channel(gfx);
        gfx_shared = Some(shared);
    }
    let mut connector = ClientConnector::new(config(target), client_addr)
        .with_static_channel(dvc)
        .with_static_channel(CliprdrClient::new(Box::new(ClipBackend { tx: clip_tx })))
        // The older sound channel: used when the dynamic ones aren't, and Windows
        // only starts device redirection when it is there.
        .with_static_channel(ironrdp_rdpsnd::client::Rdpsnd::new(Box::new(crate::sound::Backend::new(sound_tx, false))));
    #[cfg(windows)]
    if let Some(dir) = &target.files_dir {
        match drive_channel(dir, print_tx) {
            Ok(rdpdr) => connector.attach_static_channel(rdpdr),
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
    Ok((result, framed, clip_rx, gfx_shared, print_rx, sound_rx, devices))
}

/// Run one RDP session for one browser tab. Returns when either side closes,
/// with what the user was told about why it ended, and whether it failed
/// (could not be opened or broke) rather than ended normally.
pub async fn run(mut target: Target, mut io: Box<dyn Transport>) -> (String, Option<String>) {
    if target.lan {
        // No H.264 at all: Windows sends its plain lossless bitmaps and the
        // portal passes each screen change on in one piece.
        target.egfx = false;
        target.video_ok = false;
    }
    tracing::debug!("session for {}: local network {}", target.username, target.lan);
    let connected = connect(&target).await;
    let connected = connected.map(|(result, framed, clip_rx, gfx, printed, sound, devices)| {
        let extras = Extras {
            printed,
            sound,
            devices,
            kick: target.kick.clone(),
            notices: target.notices.take(),
            win_username: target.username.clone(),
            idle: target.idle,
            rules: target.rules.take(),
            actor: target.actor.clone(),
            clip_dir: target.clip_dir.clone(),
            files_dir: target.files_dir.clone(),
        };
        (result, framed, clip_rx, gfx, extras)
    });
    let outcome = match connected {
        Ok((result, framed, clip_rx, gfx, extras)) => {
            session(result, framed, clip_rx, target.resizable, target.video_ok && gfx.is_none(), target.lan, gfx, extras, io.as_mut()).await
        }
        Err(e) => Err(e),
    };
    let mut failure = None;
    let message = match outcome {
        Ok(reason) => reason,
        Err(e) => {
            tracing::warn!("rdp session for {} ended with error: {e:#}", target.username);
            failure = Some(format!("{e:#}"));
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
    (message, failure)
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

/// Answer the host reading (part of) one of the files the user pasted.
async fn file_contents(paths: &[std::path::PathBuf], req: &FileContentsRequest) -> FileContentsResponse<'static> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let Some(path) = usize::try_from(req.index).ok().and_then(|i| paths.get(i)) else {
        return FileContentsResponse::new_error(req.stream_id);
    };
    if req.flags.contains(FileContentsFlags::SIZE) {
        return match tokio::fs::metadata(path).await {
            Ok(meta) => FileContentsResponse::new_size_response(req.stream_id, meta.len()),
            Err(_) => FileContentsResponse::new_error(req.stream_id),
        };
    }
    let read = async {
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(req.position)).await?;
        let want = (req.requested_size as usize).min(8 * 1024 * 1024);
        let mut buf = vec![0u8; want];
        let mut got = 0;
        while got < want {
            let n = file.read(&mut buf[got..]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        std::io::Result::Ok(buf)
    };
    match read.await {
        Ok(data) => FileContentsResponse::new_data_response(req.stream_id, data),
        Err(_) => FileContentsResponse::new_error(req.stream_id),
    }
}

/// Where a file the browser put on the clipboard waits: `<clipboard>/<batch>/<name>`.
fn clip_path(dir: &Option<std::path::PathBuf>, batch: &str, name: &str) -> Option<std::path::PathBuf> {
    let dir = dir.as_ref()?;
    let batch_ok = (8..=64).contains(&batch.len()) && batch.bytes().all(|b| b.is_ascii_hexdigit());
    (batch_ok && plain_name(name)).then(|| dir.join(batch).join(name))
}

/// Remove clipboard batches older than an hour, except `keep`.
fn prune_clipboard(dir: &Option<std::path::PathBuf>, keep: &str) {
    let Some(dir) = dir else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old && entry.file_name() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

enum ClipAction {
    Event(ClipEvent),
    RequestFile(FileContentsRequest),
}

enum FetchStep {
    Ignore,
    Progress { name: String, got: u64, size: u64 },
    Saved { name: String, size: u64 },
    Failed { name: String },
}

impl Fetch {
    /// The next piece to ask the host for, opening the next file when needed.
    async fn next_request(&mut self, dir: &std::path::Path) -> anyhow::Result<Option<FileContentsRequest>> {
        loop {
            if let Some(cur) = &self.current {
                let (flags, position, requested_size) = match cur.size {
                    None => (FileContentsFlags::SIZE, 0, 8),
                    Some(total) => (FileContentsFlags::RANGE, cur.got, (total - cur.got).min(u64::from(FILE_CHUNK)) as u32),
                };
                return Ok(Some(FileContentsRequest {
                    stream_id: cur.stream_id,
                    index: cur.index,
                    flags,
                    position,
                    requested_size,
                    data_id: self.data_id,
                }));
            }
            let Some((index, name, size)) = self.queue.pop_front() else { return Ok(None) };
            self.next_stream = self.next_stream.wrapping_add(1);
            let part = dir.join(format!(".clip-{}-{}.part", std::process::id(), self.next_stream));
            let file = tokio::fs::File::create(&part).await.context("saving a copied file")?;
            self.current = Some(FetchFile { index, name, size, stream_id: self.next_stream, file, part, got: 0 });
            if size == Some(0) {
                // Nothing to read: save it empty straight away.
                let _ = self.finish(dir).await?;
            }
        }
    }

    async fn finish(&mut self, dir: &std::path::Path) -> anyhow::Result<FetchStep> {
        use tokio::io::AsyncWriteExt as _;
        let Some(mut cur) = self.current.take() else { return Ok(FetchStep::Ignore) };
        cur.file.flush().await?;
        drop(cur.file);
        let name = free_name(dir, &cur.name).await;
        tokio::fs::rename(&cur.part, dir.join(&name)).await.context("saving a copied file")?;
        self.saved.push(name.clone());
        Ok(FetchStep::Saved { name, size: cur.got })
    }

    async fn on_data(&mut self, dir: &std::path::Path, stream_id: u32, data: Option<Vec<u8>>) -> anyhow::Result<FetchStep> {
        use tokio::io::AsyncWriteExt as _;
        let Some(cur) = &mut self.current else { return Ok(FetchStep::Ignore) };
        if cur.stream_id != stream_id {
            return Ok(FetchStep::Ignore);
        }
        let Some(data) = data else {
            let cur = self.current.take().unwrap();
            drop(cur.file);
            let _ = tokio::fs::remove_file(&cur.part).await;
            return Ok(FetchStep::Failed { name: cur.name });
        };
        match cur.size {
            None => {
                let size = data.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                if size > MAX_FILE_BYTES {
                    let cur = self.current.take().unwrap();
                    drop(cur.file);
                    let _ = tokio::fs::remove_file(&cur.part).await;
                    return Ok(FetchStep::Failed { name: cur.name });
                }
                cur.size = Some(size);
                if size == 0 {
                    return self.finish(dir).await;
                }
                Ok(FetchStep::Progress { name: cur.name.clone(), got: 0, size })
            }
            Some(size) => {
                cur.file.write_all(&data).await.context("saving a copied file")?;
                cur.got += data.len() as u64;
                if data.is_empty() || cur.got >= size {
                    self.finish(dir).await
                } else {
                    Ok(FetchStep::Progress { name: cur.name.clone(), got: cur.got, size })
                }
            }
        }
    }
}

fn size_text(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n if n < 1024 * 1024 * 1024 => format!("{:.1} MB", n as f64 / 1048576.0),
        n => format!("{:.2} GB", n as f64 / 1073741824.0),
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    result: ConnectionResult,
    mut framed: TlsFramed,
    clip_rx: std::sync::mpsc::Receiver<ClipEvent>,
    resizable: bool,
    video_ok: bool,
    lan: bool,
    gfx: Option<crate::gfx::SharedRef>,
    mut extras: Extras,
    io: &mut dyn Transport,
) -> anyhow::Result<String> {
    let mut notices = extras.notices.take();
    let mut sound_on = true;
    // Clipboard: what the user copied on their computer, what we asked the host
    // for, the host's copied files, and a copy of them in progress.
    let mut local_clip: Option<LocalClip> = None;
    let mut pending_paste: std::collections::VecDeque<Paste> = std::collections::VecDeque::new();
    let mut remote_files: Option<(Vec<FileDescriptor>, Option<u32>)> = None;
    let mut fetch: Option<Fetch> = None;
    // Inactivity and allowed hours.
    let mut last_input = std::time::Instant::now();
    let mut idle_warned = false;
    let mut slow_checks_at = std::time::Instant::now();
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
    let mut tiles = Tiles::new(image.width(), image.height(), image.data().len(), lan);
    // A whole screen change goes as one batch on the local network.
    let batch_bytes = if lan { LAN_BATCH_BYTES } else { BATCH_BYTES };
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
    // Diagnostics for the tile path, reported once a second.
    let mut tstat = TileStats::default();
    // Batches on their way (number, when sent, size), the link speed measured
    // from them, and when the host last drew something.
    let mut in_flight: std::collections::VecDeque<(u32, std::time::Instant, usize)> = Default::default();
    let mut link_rate: Option<f64> = None;
    let mut lossless_link = false;
    let mut last_update = std::time::Instant::now();
    let text_format = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
    let image_format = [ClipboardFormat::new(ClipboardFormatId::CF_DIB)];
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
    // Sharpening JPEG areas is due: nothing new to send, and on the local
    // network the screen has been still for a moment and no batch is on its way.
    macro_rules! refine_due {
        () => {
            !tiles.has_dirty()
                && tiles.has_lossy()
                && (!lan || (last_update.elapsed() >= REFINE_AFTER && sent_seq == acked_seq))
        };
    }
    // Whether a batch can go right now.
    macro_rules! sendable {
        () => {
            sent_seq.wrapping_sub(acked_seq) < MAX_IN_FLIGHT && (tiles.has_dirty() || refine_due!())
        };
    }

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
            let refine = refine_due!();
            if sendable!() {
                let budget = if refine && lan { LAN_REFINE_BYTES } else { batch_bytes };
                if let Some(batch) = tiles.next_batch(
                    image.data(),
                    image.stride(),
                    image.bytes_per_pixel(),
                    sent_seq.wrapping_add(1),
                    budget,
                    refine,
                    &skip,
                ) {
                    sent_seq = sent_seq.wrapping_add(1);
                    tstat.batches += 1;
                    tstat.bytes_out += batch.len();
                    in_flight.push_back((sent_seq, std::time::Instant::now(), batch.len()));
                    if let Some(t) = redraw_at.take() {
                        portal_ms.push(t.elapsed().as_secs_f32() * 1000.0);
                    }
                    io.send_binary(batch).await?;
                }
            }
            // More to send (budget reached, or tiles to sharpen) and room for it:
            // come straight back. Otherwise wait for an acknowledgement, a
            // change, or the ticker (which starts a delayed sharpening pass).
            flush_due = sendable!();
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
        // Sound that arrived since the last turn.
        while let Ok(block) = extras.sound.try_recv() {
            if sound_on {
                let mut msg = Vec::with_capacity(6 + block.data.len());
                msg.push(if block.aac { 9 } else { 7 });
                msg.push(block.channels);
                msg.extend_from_slice(&block.rate.to_le_bytes());
                msg.extend_from_slice(&block.data);
                io.send_binary(msg).await?;
            }
        }
        // Apps in the session started or stopped the microphone or camera.
        let device_events = std::mem::take(&mut extras.devices.lock().unwrap().events);
        for event in device_events {
            use crate::devices::Event;
            let msg = match event {
                Event::MicOn { rate, channels } => json!({"t": "mic", "on": true, "rate": rate, "ch": channels}),
                Event::MicOff => json!({"t": "mic", "on": false}),
                Event::CameraOn { width, height } => json!({"t": "cam", "on": true, "w": width, "h": height}),
                Event::CameraOff => json!({"t": "cam", "on": false}),
            };
            send_json(io, msg).await?;
        }
        // Print jobs finished since the last turn: offer them in the browser.
        while let Ok(job) = extras.printed.try_recv() {
            if let Some(actor) = &extras.actor {
                actor.spawn("print", format!("{} ({})", job.name, size_text(job.size as u64)));
            }
            send_json(io, json!({"t": "print", "name": job.name, "size": job.size})).await?;
        }
        // Clipboard events queued while the last PDU was processed.
        while let Ok(event) = clip_rx.try_recv() {
            tracing::debug!("clipboard: {}", match &event {
                ClipEvent::RequestFormatList => "host asks for our formats".to_string(),
                ClipEvent::RemoteCopy { text, image, files } => format!("host copied: text={text} image={image} files={files:?}"),
                ClipEvent::DataRequest(f) => format!("host pastes our format {f:?}"),
                ClipEvent::RemoteData(d) => format!("host data: {:?} bytes", d.as_ref().map(Vec::len)),
                ClipEvent::RemoteFiles(f, id) => format!("host file list: {} files, lock {id:?}", f.len()),
                ClipEvent::FileRequest(r) => format!("host reads our file {} ({:?} at {} len {})", r.index, r.flags, r.position, r.requested_size),
                ClipEvent::FileData { stream_id, data } => format!("host file data on {stream_id}: {:?} bytes", data.as_ref().map(Vec::len)),
            });
            // Events that only need the browser (or a file read) first.
            let action = match event {
                ClipEvent::RemoteData(data) => {
                    match (pending_paste.pop_front(), data) {
                        (Some(Paste::Text), Some(data)) => {
                            let text = utf16_text(&data);
                            if text.len() <= MAX_CLIPBOARD_BYTES {
                                send_json(io, json!({"t": "clip", "s": text.replace("\r\n", "\n")})).await?;
                            }
                        }
                        (Some(Paste::Image), Some(data)) => match crate::clipimage::dib_to_png(&data) {
                            Ok(png) => {
                                let mut msg = Vec::with_capacity(1 + png.len());
                                msg.push(8);
                                msg.extend_from_slice(&png);
                                io.send_binary(msg).await?;
                            }
                            Err(e) => tracing::debug!("copied picture not passed on: {e:#}"),
                        },
                        _ => {}
                    }
                    continue;
                }
                ClipEvent::RemoteFiles(files, data_id) => {
                    let list: Vec<_> = files
                        .iter()
                        .filter(|f| !f.attributes.is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY)))
                        .take(500)
                        .map(|f| json!({"name": f.name, "size": f.file_size}))
                        .collect();
                    remote_files = Some((files, data_id));
                    if !list.is_empty() {
                        send_json(io, json!({"t": "clipfiles", "files": list})).await?;
                    }
                    continue;
                }
                ClipEvent::FileData { stream_id, data } => {
                    let (Some(f), Some(dir)) = (fetch.as_mut(), extras.files_dir.as_deref()) else { continue };
                    match f.on_data(dir, stream_id, data).await? {
                        FetchStep::Ignore => continue,
                        FetchStep::Progress { name, got, size } => {
                            if f.last_progress.elapsed() >= Duration::from_millis(300) {
                                f.last_progress = std::time::Instant::now();
                                send_json(io, json!({"t": "fetching", "name": name, "got": got, "size": size})).await?;
                            }
                        }
                        FetchStep::Saved { name, size } => {
                            if let Some(actor) = &extras.actor {
                                actor.spawn("clipboard", format!("Copied {name} ({}) out of the session", size_text(size)));
                            }
                        }
                        FetchStep::Failed { name } => {
                            send_json(io, json!({"t": "fetchfail", "name": name})).await?;
                        }
                    }
                    match f.next_request(dir).await? {
                        Some(request) => ClipAction::RequestFile(request),
                        None => {
                            let saved = std::mem::take(&mut f.saved);
                            fetch = None;
                            send_json(io, json!({"t": "fetched", "names": saved})).await?;
                            continue;
                        }
                    }
                }
                ClipEvent::FileRequest(req) => {
                    let response = match &local_clip {
                        Some(LocalClip::Files(paths)) => file_contents(paths, &req).await,
                        _ => FileContentsResponse::new_error(req.stream_id),
                    };
                    let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() else { continue };
                    let messages = clip.submit_file_contents(response).map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                    let frame = svc_frame(&stage, messages)?;
                    if !frame.is_empty() {
                        framed.write_all(&frame).await.context("writing to the host")?;
                    }
                    continue;
                }
                other => ClipAction::Event(other),
            };
            let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() else {
                continue;
            };
            let event = match action {
                // The next piece of a file being copied out of the session.
                ClipAction::RequestFile(request) => {
                    let messages = clip.request_file_contents(request).map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                    let frame = svc_frame(&stage, messages)?;
                    if !frame.is_empty() {
                        framed.write_all(&frame).await.context("writing to the host")?;
                    }
                    continue;
                }
                ClipAction::Event(event) => event,
            };
            let messages = match event {
                ClipEvent::RequestFormatList => match &local_clip {
                    Some(LocalClip::Text(_)) => clip.initiate_copy(&text_format),
                    Some(LocalClip::Image(_)) => clip.initiate_copy(&image_format),
                    Some(LocalClip::Files(paths)) => clip.initiate_file_copy(describe_files(paths)),
                    None => clip.initiate_copy(&[]),
                },
                ClipEvent::RemoteCopy { text, image, files } => {
                    remote_files = None;
                    if text {
                        pending_paste.push_back(Paste::Text);
                        clip.initiate_paste(ClipboardFormatId::CF_UNICODETEXT)
                    } else if image {
                        pending_paste.push_back(Paste::Image);
                        clip.initiate_paste(ClipboardFormatId::CF_DIB)
                    } else if let Some(format) = files {
                        clip.initiate_paste(format)
                    } else {
                        continue;
                    }
                }
                ClipEvent::DataRequest(format) => {
                    let response = match &local_clip {
                        Some(LocalClip::Text(text)) if format == ClipboardFormatId::CF_UNICODETEXT => {
                            FormatDataResponse::new_unicode_string(text)
                        }
                        Some(LocalClip::Image(dib)) if format == ClipboardFormatId::CF_DIB => {
                            FormatDataResponse::new_data(dib.clone())
                        }
                        _ => FormatDataResponse::new_error(),
                    };
                    clip.submit_format_data(response)
                }
                _ => continue,
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
            // A message from the app for this browser.
            note = async {
                match notices.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match note {
                    Some(note) => send_json(io, note).await?,
                    None => notices = None,
                }
                continue;
            }
            // The browser first: input and frame acknowledgements are tiny, and
            // must never wait behind a stream of screen updates from the host
            // (that adds input lag, and holds back acknowledgements Windows waits for).
            msg = io.recv() => {
                let msg = match msg {
                    Incoming::Binary(data) => {
                        // [1][JPEG] a camera frame, [2][PCM] microphone sound.
                        let (messages, channel) = {
                            let devices = &extras.devices;
                            match data.split_first() {
                                Some((1, jpeg)) => (crate::devices::camera_frame(devices, jpeg.to_vec()), devices.lock().unwrap().camera_channel),
                                Some((2, pcm)) => (crate::devices::mic_packets(devices, pcm), devices.lock().unwrap().mic_channel),
                                _ => (Vec::new(), None),
                            }
                        };
                        if let (false, Some(channel_id)) = (messages.is_empty(), channel) {
                            let messages = ironrdp_dvc::encode_dvc_messages(channel_id, messages, ironrdp_svc::ChannelFlags::empty())
                                .map_err(|e| anyhow!("device data: {e}"))?;
                            let frame = svc_frame(&stage, SvcProcessorMessages::<DrdynvcClient>::new(messages))?;
                            framed.write_all(&frame).await.context("writing to the host")?;
                        }
                        continue;
                    }
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
                        // A black-screen program is closed the safe way and never
                        // forced; a desktop is signed out as before.
                        let user = extras.win_username.clone();
                        let outcome = match crate::host::close_safely(&user, "your app", Duration::from_secs(40)).await {
                            crate::host::SafeClose::NotKiosk => {
                                tokio::task::spawn_blocking(move || crate::host::sign_out_blocking(&user)).await.ok();
                                crate::host::SafeClose::Closed
                            }
                            other => other,
                        };
                        if outcome == crate::host::SafeClose::Busy {
                            return Ok("Your app still has a window with unsaved work open, so it was left running. \
                                       Sign in again, save and close that window, then sign out."
                                .to_owned());
                        }
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
                        while let Some(&(seq, at, bytes)) = in_flight.front() {
                            if seq.wrapping_sub(n) as i32 > 0 {
                                break;
                            }
                            in_flight.pop_front();
                            let secs = at.elapsed().as_secs_f64();
                            let ms = secs as f32 * 1000.0;
                            tstat.ack_ms += ms;
                            tstat.acks += 1;
                            tstat.ack_max = tstat.ack_max.max(ms);
                            // Link speed: a big batch's size over the time it took
                            // to arrive and be unpacked (smoothed).
                            if lan && bytes >= RATE_SAMPLE_BYTES && secs > 0.0 {
                                let sample = bytes as f64 / secs;
                                let rate = link_rate.map_or(sample, |r| r * 0.7 + sample * 0.3);
                                link_rate = Some(rate);
                                let fast = if lossless_link { rate >= LOSSLESS_OFF_RATE } else { rate >= LOSSLESS_ON_RATE };
                                if fast != lossless_link {
                                    lossless_link = fast;
                                    tiles.set_lossless_link(fast);
                                    tracing::debug!(
                                        "local link measured at {:.0} Mbit/s: photo areas {}",
                                        rate * 8.0 / 1e6,
                                        if fast { "lossless at once" } else { "as JPEG first" }
                                    );
                                }
                            }
                        }
                        if n <= sent_seq && n > acked_seq {
                            acked_seq = n;
                        }
                        // Room for another batch: send what has been waiting.
                        if sendable!() {
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
                        let same = matches!(&local_clip, Some(LocalClip::Text(t)) if *t == text);
                        if text.len() <= MAX_CLIPBOARD_BYTES && !same {
                            local_clip = Some(LocalClip::Text(text));
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
                    Some(ClientMsg::ClipImage { b, f }) => {
                        tracing::debug!("clipboard: browser pasted a picture {b}/{f}");
                        last_input = std::time::Instant::now();
                        prune_clipboard(&extras.clip_dir, &b);
                        let Some(path) = clip_path(&extras.clip_dir, &b, &f) else { continue };
                        let dib = match tokio::fs::read(&path).await {
                            Ok(png) => crate::clipimage::png_to_dib(&png),
                            Err(e) => Err(e.into()),
                        };
                        match dib {
                            Ok(dib) => {
                                local_clip = Some(LocalClip::Image(dib));
                                if let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() {
                                    if clip.is_ready() {
                                        let messages = clip
                                            .initiate_copy(&image_format)
                                            .map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                                        let frame = svc_frame(&stage, messages)?;
                                        framed.write_all(&frame).await.context("writing to the host")?;
                                    }
                                }
                            }
                            Err(e) => tracing::debug!("pasted picture not usable: {e:#}"),
                        }
                        continue;
                    }
                    Some(ClientMsg::ClipFiles { b, f }) => {
                        tracing::debug!("clipboard: browser pasted files {b}/{f:?}");
                        last_input = std::time::Instant::now();
                        prune_clipboard(&extras.clip_dir, &b);
                        let paths: Vec<_> = f
                            .iter()
                            .take(500)
                            .filter_map(|name| clip_path(&extras.clip_dir, &b, name))
                            .filter(|p| p.is_file())
                            .collect();
                        if paths.is_empty() {
                            continue;
                        }
                        if let Some(actor) = &extras.actor {
                            let names: Vec<_> = paths.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy()).collect();
                            actor.spawn("clipboard", format!("Pasted {} into the session", names.join(", ")));
                        }
                        let files = describe_files(&paths);
                        local_clip = Some(LocalClip::Files(paths));
                        if let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() {
                            if clip.is_ready() {
                                let messages = clip
                                    .initiate_file_copy(files)
                                    .map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                                let frame = svc_frame(&stage, messages)?;
                                framed.write_all(&frame).await.context("writing to the host")?;
                            }
                        }
                        continue;
                    }
                    Some(ClientMsg::Fetch) => {
                        let (Some((files, data_id)), Some(dir), None) = (&remote_files, extras.files_dir.as_deref(), &fetch) else {
                            continue;
                        };
                        let queue = files
                            .iter()
                            .enumerate()
                            .filter(|(_, f)| !f.attributes.is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY)))
                            .take(500)
                            .map(|(i, f)| {
                                let name = crate::web::safe_file_name(&f.name).unwrap_or_else(|| "file".to_owned());
                                (i as i32, name, f.file_size)
                            })
                            .collect();
                        let mut job = Fetch {
                            queue,
                            current: None,
                            data_id: *data_id,
                            next_stream: 0x4150_0000,
                            saved: Vec::new(),
                            last_progress: std::time::Instant::now(),
                        };
                        let request = job.next_request(dir).await?;
                        fetch = Some(job);
                        if let (Some(request), Some(clip)) = (request, stage.get_svc_processor_mut::<CliprdrClient>()) {
                            let messages = clip
                                .request_file_contents(request)
                                .map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                            let frame = svc_frame(&stage, messages)?;
                            framed.write_all(&frame).await.context("writing to the host")?;
                        }
                        continue;
                    }
                    Some(ClientMsg::Sound { on }) => {
                        sound_on = on;
                        continue;
                    }
                    Some(ClientMsg::Monitors { m }) => {
                        // Two (or more) monitors side by side; one goes back to a plain resize.
                        let monitors: Vec<(u32, u32)> = m.into_iter().take(4).map(|(w, h)| {
                            let (w, h) = sane_size(w, h);
                            (u32::from(w), u32::from(h))
                        }).collect();
                        let frame = if monitors.len() <= 1 {
                            let (w, h) = monitors.first().copied().unwrap_or((u32::from(image.width()), u32::from(image.height())));
                            stage.encode_resize(w, h, None, None)
                        } else {
                            match monitor_layout(&monitors) {
                                Ok(layout) => stage.encode_monitor_layout(&layout),
                                Err(e) => {
                                    tracing::warn!("monitor layout: {e:#}");
                                    None
                                }
                            }
                        };
                        if let Some(frame) = frame {
                            let frame = frame.map_err(|e| anyhow!("RDP monitor layout error: {e:#}"))?;
                            framed.write_all(&frame).await.context("writing to the host")?;
                        }
                        continue;
                    }
                    Some(ClientMsg::Ping { n }) => {
                        send_json(io, json!({"t": "pong", "n": n})).await?;
                        continue;
                    }
                    Some(other) => {
                        if matches!(other, ClientMsg::Move { .. }) {
                            moves_in += 1;
                        }
                        if !matches!(other, ClientMsg::Sync { .. } | ClientMsg::ReleaseAll) {
                            last_input = std::time::Instant::now();
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
                tstat.bytes_in += payload.len();
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
                // Nobody at the keyboard: warn a minute ahead, then disconnect.
                if let Some(limit) = extras.idle {
                    let quiet = last_input.elapsed();
                    if quiet >= limit {
                        return Ok(format!(
                            "Disconnected after {} without activity. Your apps are still running.",
                            crate::activity::duration(limit.as_secs() as i64)
                        ));
                    }
                    let warn_at = limit.saturating_sub(Duration::from_secs(60));
                    if !idle_warned && quiet >= warn_at {
                        idle_warned = true;
                        send_json(io, json!({"t": "idle", "s": (limit - quiet).as_secs()})).await?;
                    } else if idle_warned && quiet < warn_at {
                        idle_warned = false;
                    }
                }
                if slow_checks_at.elapsed() >= Duration::from_secs(15) {
                    slow_checks_at = std::time::Instant::now();
                    // The user's allowed hours are over (read fresh: the admin may
                    // have changed them while the session is open).
                    let rules = match &extras.actor {
                        Some(Actor { pool, user_id: Some(id), .. }) => {
                            crate::db::user_by_id(pool, *id).await.ok().flatten().map(|u| u.rules())
                        }
                        _ => extras.rules.clone(),
                    };
                    if let Some(rules) = rules {
                        if let Err(why) = rules.check_time(crate::clock::now()) {
                            return Ok(format!("Your time is up. {why} Your apps are still running."));
                        }
                    }
                    // Release clipboard locks the host no longer needs.
                    if let Some(clip) = stage.get_svc_processor_mut::<CliprdrClient>() {
                        if clip.is_ready() {
                            let messages = clip.drive_timeouts().map_err(|e| anyhow!("clipboard error: {e:#}"))?;
                            let frame = svc_frame(&stage, messages)?;
                            if !frame.is_empty() {
                                framed.write_all(&frame).await.context("writing to the host")?;
                            }
                        }
                    }
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
                    tstat.updates += 1;
                    last_update = std::time::Instant::now();
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
                                tiles = Tiles::new(image.width(), image.height(), image.data().len(), lan);
                                tiles.set_lossless_link(lossless_link);
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

        if tstat.since.elapsed() >= Duration::from_secs(1) {
            if tstat.batches > 0 {
                tracing::debug!(
                    "tiles: {} updates ({} KB) from Windows, {} batches ({} KB) sent, browser ack {:.1} ms avg {:.1} max, link {:.0} Mbit/s",
                    tstat.updates,
                    tstat.bytes_in / 1024,
                    tstat.batches,
                    tstat.bytes_out / 1024,
                    if tstat.acks > 0 { tstat.ack_ms / tstat.acks as f32 } else { 0.0 },
                    tstat.ack_max,
                    link_rate.unwrap_or(0.0) * 8.0 / 1e6
                );
            }
            tstat = TileStats::default();
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

/// Per-second counters for the tile path (diagnostics only).
struct TileStats {
    since: std::time::Instant,
    bytes_in: usize,
    updates: u32,
    batches: u32,
    bytes_out: usize,
    acks: u32,
    ack_ms: f32,
    ack_max: f32,
}

impl Default for TileStats {
    fn default() -> Self {
        Self {
            since: std::time::Instant::now(),
            bytes_in: 0,
            updates: 0,
            batches: 0,
            bytes_out: 0,
            acks: 0,
            ack_ms: 0.0,
            ack_max: 0.0,
        }
    }
}

/// Monitors side by side, the first one primary.
fn monitor_layout(monitors: &[(u32, u32)]) -> anyhow::Result<ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout> {
    use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
    let mut entries = Vec::new();
    let mut left = 0i32;
    for (i, &(w, h)) in monitors.iter().enumerate() {
        let entry = if i == 0 {
            MonitorLayoutEntry::new_primary(w, h).map_err(|e| anyhow!("{e}"))?
        } else {
            MonitorLayoutEntry::new_secondary(w, h).map_err(|e| anyhow!("{e}"))?.with_position(left, 0).map_err(|e| anyhow!("{e}"))?
        };
        entries.push(entry);
        left += w as i32;
    }
    DisplayControlMonitorLayout::new(&entries).map_err(|e| anyhow!("{e}"))
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
        ClientMsg::Resize { .. }
        | ClientMsg::Clip { .. }
        | ClientMsg::Ack { .. }
        | ClientMsg::FrameAck { .. }
        | ClientMsg::SignOut
        | ClientMsg::Sound { .. }
        | ClientMsg::Ping { .. }
        | ClientMsg::Monitors { .. }
        | ClientMsg::ClipImage { .. }
        | ClientMsg::ClipFiles { .. }
        | ClientMsg::Fetch => return Vec::new(),
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
