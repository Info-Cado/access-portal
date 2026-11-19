//! How a session talks to the browser.
//!
//! Two carriers speak the same messages (JSON text and binary tile batches):
//!
//! * **WebRTC data channel** — a direct, peer-to-peer UDP path between the
//!   browser and this machine. Tried first.
//! * **WebSocket** — through whatever serves the portal's web pages. The fallback
//!   when a direct path cannot be established.
//!
//! Data channel messages have a size limit, so on that carrier every message is
//! cut into chunks: one flag byte (`bit 0` = last chunk, `bit 1` = text) followed
//! by up to `CHUNK` bytes. The channel is ordered and reliable, so chunks are
//! simply concatenated on arrival.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use bytes::BytesMut;
use tokio::sync::{mpsc, oneshot};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer,
    RTCPeerConnectionState, RTCSessionDescription, Registry,
};
use webrtc::peer_connection::StatsSelector;
use webrtc::runtime::{default_runtime, Runtime};

const CHUNK: usize = 15_000;
const FLAG_LAST: u8 = 1;
const FLAG_TEXT: u8 = 2;

pub enum Incoming {
    Text(String),
    /// Microphone sound and camera frames (first byte says which).
    Binary(Vec<u8>),
    Closed,
}

#[async_trait]
pub trait Transport: Send {
    /// Next message from the browser. Cancel-safe.
    async fn recv(&mut self) -> Incoming;
    async fn send_text(&mut self, text: String) -> anyhow::Result<()>;
    async fn send_binary(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
    /// Keep an otherwise quiet link open. Proxies in front of the portal
    /// (Cloudflare closes idle WebSockets after about 100 seconds) need this.
    async fn keepalive(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn close(&mut self);
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

pub struct WsTransport(pub WebSocket);

#[async_trait]
impl Transport for WsTransport {
    async fn recv(&mut self) -> Incoming {
        loop {
            match self.0.recv().await {
                Some(Ok(Message::Text(text))) => return Incoming::Text(text),
                Some(Ok(Message::Binary(data))) => return Incoming::Binary(data),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Incoming::Closed,
                Some(Ok(_)) => {}
            }
        }
    }

    async fn send_text(&mut self, text: String) -> anyhow::Result<()> {
        self.0.send(Message::Text(text)).await.context("browser disconnected")
    }

    async fn send_binary(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        self.0.send(Message::Binary(data)).await.context("browser disconnected")
    }

    /// A WebSocket ping; the browser answers with a pong by itself, so traffic
    /// flows both ways.
    async fn keepalive(&mut self) -> anyhow::Result<()> {
        self.0.send(Message::Ping(Vec::new())).await.context("browser disconnected")
    }

    async fn close(&mut self) {
        let _ = self.0.send(Message::Close(None)).await;
    }
}

// ---------------------------------------------------------------------------
// WebRTC data channel
// ---------------------------------------------------------------------------

pub struct RtcTransport {
    rx: mpsc::UnboundedReceiver<Incoming>,
    channel: Arc<dyn DataChannel>,
    /// Tells the task that owns the peer connection to shut it down.
    done: Option<oneshot::Sender<()>>,
    /// The browser is on the local network: the link runs between two local
    /// addresses, not through a relay or the internet.
    lan: bool,
}

impl RtcTransport {
    /// Whether this link stays inside the local network.
    pub fn lan(&self) -> bool {
        self.lan
    }

    async fn send_chunked(&mut self, data: &[u8], text: bool) -> anyhow::Result<()> {
        let base = if text { FLAG_TEXT } else { 0 };
        let mut chunks = data.chunks(CHUNK).peekable();
        if chunks.peek().is_none() {
            let mut buf = BytesMut::with_capacity(1);
            buf.extend_from_slice(&[base | FLAG_LAST]);
            return self.channel.send(buf).await.map_err(|e| anyhow!("direct link closed: {e}"));
        }
        while let Some(chunk) = chunks.next() {
            let flag = if chunks.peek().is_none() { base | FLAG_LAST } else { base };
            let mut buf = BytesMut::with_capacity(chunk.len() + 1);
            buf.extend_from_slice(&[flag]);
            buf.extend_from_slice(chunk);
            self.channel.send(buf).await.map_err(|e| anyhow!("direct link closed: {e}"))?;
        }
        Ok(())
    }
}

#[async_trait]
impl Transport for RtcTransport {
    async fn recv(&mut self) -> Incoming {
        self.rx.recv().await.unwrap_or(Incoming::Closed)
    }

    async fn send_text(&mut self, text: String) -> anyhow::Result<()> {
        self.send_chunked(text.as_bytes(), true).await
    }

    async fn send_binary(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        self.send_chunked(&data, false).await
    }

    async fn close(&mut self) {
        let _ = self.channel.close().await;
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

impl Drop for RtcTransport {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

/// Called once, when the browser's data channel is open and ready.
pub type OnOpen = Box<dyn FnOnce(RtcTransport) + Send>;

struct Handler {
    runtime: Arc<dyn Runtime>,
    gathered: mpsc::Sender<()>,
    /// Where to hand the channel once it opens, plus the shutdown signal that
    /// goes with it. Taken by the first data channel; later ones are ignored.
    start: std::sync::Mutex<Option<(oneshot::Sender<RtcTransport>, oneshot::Sender<()>)>>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::debug!("direct link state: {state}");
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let Some((opened, done)) = self.start.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        // Must not block here: that would stall the connection's event loop.
        self.runtime.spawn(Box::pin(async move {
            // Wait for the channel to open.
            loop {
                match channel.poll().await {
                    Some(DataChannelEvent::OnOpen) => break,
                    Some(DataChannelEvent::OnClose) | None => return,
                    _ => {}
                }
            }
            let (tx, rx) = mpsc::unbounded_channel();
            if opened.send(RtcTransport { rx, channel: channel.clone(), done: Some(done), lan: false }).is_err() {
                return;
            }

            // Reassemble chunked messages from the browser.
            let mut pending: Vec<u8> = Vec::new();
            loop {
                match channel.poll().await {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        let Some((&flags, body)) = message.data.split_first() else { continue };
                        if pending.len() + body.len() > 8 * 1024 * 1024 {
                            pending.clear(); // oversized: drop it
                            continue;
                        }
                        pending.extend_from_slice(body);
                        if flags & FLAG_LAST != 0 {
                            let whole = std::mem::take(&mut pending);
                            let message = if flags & FLAG_TEXT != 0 {
                                Incoming::Text(String::from_utf8_lossy(&whole).into_owned())
                            } else {
                                Incoming::Binary(whole)
                            };
                            if tx.send(message).is_err() {
                                return;
                            }
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => {
                        let _ = tx.send(Incoming::Closed);
                        return;
                    }
                    _ => {}
                }
            }
        }));
    }
}

/// Answer a browser's WebRTC offer.
///
/// Returns the answer to hand back to the browser. If the browser then manages
/// to open its data channel, `on_open` receives the ready transport; if it never
/// does, everything is torn down after a short wait.
pub async fn answer_offer(
    offer: RTCSessionDescription,
    ice_servers: Vec<RTCIceServer>,
    on_open: OnOpen,
) -> anyhow::Result<RTCSessionDescription> {
    let runtime = default_runtime().ok_or_else(|| anyhow!("no async runtime for WebRTC"))?;
    let (gathered_tx, mut gathered_rx) = mpsc::channel(1);
    let (opened_tx, opened_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();

    let mut media = MediaEngine::default();
    media.register_default_codecs().map_err(|e| anyhow!("webrtc setup: {e}"))?;
    let registry = register_default_interceptors(Registry::new(), &mut media)
        .map_err(|e| anyhow!("webrtc setup: {e}"))?;

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            runtime: runtime.clone(),
            gathered: gathered_tx,
            start: std::sync::Mutex::new(Some((opened_tx, done_tx))),
        }))
        .with_runtime(runtime)
        // One UDP socket per local network interface, on ephemeral ports.
        .with_udp_addrs(vec!["0.0.0.0:0".to_owned()])
        .build()
        .await
        .map_err(|e| anyhow!("webrtc setup: {e}"))?;

    pc.set_remote_description(offer).await.map_err(|e| anyhow!("bad offer: {e}"))?;
    let answer = pc.create_answer(None).await.map_err(|e| anyhow!("creating answer: {e}"))?;
    pc.set_local_description(answer).await.map_err(|e| anyhow!("applying answer: {e}"))?;

    // Non-trickle: give candidate gathering a moment so the answer is complete,
    // but do not let an unreachable STUN server hold the user up.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), gathered_rx.recv()).await;
    let local = pc.local_description().await.ok_or_else(|| anyhow!("no local description"))?;

    // Keep the connection alive for as long as the session uses it.
    tokio::spawn(async move {
        let opened = tokio::time::timeout(std::time::Duration::from_secs(30), opened_rx).await;
        if let Ok(Ok(mut link)) = opened {
            link.lan = on_local_network(&pc).await;
            on_open(link);
            let _ = done_rx.await;
        }
        let _ = pc.close().await;
    });

    Ok(local)
}

/// Round trips at or under this count as the local network. Wi-Fi in the same
/// office measures 2–6 ms; the internet, even within one city, more.
const LAN_RTT_SECS: f64 = 0.008;

/// Whether the path ICE chose stays inside the local network: a direct path
/// (not a relay) between two local addresses, or between two Tailscale
/// addresses with a local-network round trip (both computers also run
/// Tailscale, and ICE picked that path although they sit side by side). A
/// browser's own address may be hidden behind an mDNS name (`….local`), which
/// only resolves on the same network.
async fn on_local_network(pc: &impl PeerConnection) -> bool {
    let Some(sctp) = pc.sctp().await else { return false };
    let Ok(Some(pair)) = sctp.transport().ice_transport().get_selected_candidate_pair().await else {
        return false;
    };
    let (local, remote) = (&pair.local().address, &pair.remote().address);
    let rtt = pc
        .get_stats(std::time::Instant::now(), StatsSelector::None)
        .await
        .candidate_pairs()
        .filter(|p| p.nominated && p.current_round_trip_time > 0.0)
        .map(|p| p.current_round_trip_time)
        .fold(None, |best: Option<f64>, r| Some(best.map_or(r, |b| b.min(r))));
    let result = local_path(local, remote, rtt);
    tracing::debug!("direct link path: {pair}, round trip {rtt:?} s; local network: {result}");
    result
}

/// The rule behind `on_local_network`, for the two ends of the chosen path
/// and its measured round trip in seconds.
fn local_path(local: &str, remote: &str, rtt: Option<f64>) -> bool {
    use crate::network::{is_lan, is_tailscale};
    let lan = local.parse().is_ok_and(is_lan) && (remote.ends_with(".local") || remote.parse().is_ok_and(is_lan));
    let tailscale = local.parse().is_ok_and(is_tailscale) && remote.parse().is_ok_and(is_tailscale);
    lan || (tailscale && rtt.is_some_and(|r| r <= LAN_RTT_SECS))
}

#[cfg(test)]
mod tests {
    use super::local_path;

    #[test]
    fn local_addresses_are_local_whatever_the_round_trip() {
        assert!(local_path("192.168.32.55", "192.168.32.226", Some(0.004)));
        assert!(local_path("10.0.0.2", "10.0.0.9", None));
        assert!(local_path("192.168.1.5", "172.16.4.2", Some(0.020)));
        // A browser hiding its address behind an mDNS name is on the same network.
        assert!(local_path("192.168.1.5", "0b1f6c2e-8a4d-4c1e-9f0a-1a2b3c4d5e6f.local", None));
    }

    #[test]
    fn tailscale_counts_only_with_a_local_round_trip() {
        assert!(local_path("100.82.214.46", "100.74.90.44", Some(0.004)));
        assert!(local_path("100.82.214.46", "100.74.90.44", Some(0.008)));
        assert!(!local_path("100.82.214.46", "100.74.90.44", Some(0.030)));
        assert!(!local_path("100.82.214.46", "100.74.90.44", None));
    }

    #[test]
    fn internet_relay_and_mixed_paths_are_not_local() {
        // A public address (a relay or the other end's router).
        assert!(!local_path("192.168.1.5", "203.0.113.7", Some(0.002)));
        assert!(!local_path("203.0.113.7", "192.168.1.5", Some(0.002)));
        // One end on Tailscale, the other on the local network.
        assert!(!local_path("100.82.214.46", "192.168.1.5", Some(0.002)));
        // This computer only, and unparseable names.
        assert!(!local_path("127.0.0.1", "127.0.0.1", Some(0.0001)));
        assert!(!local_path("192.168.1.5", "example.com", Some(0.002)));
    }
}
