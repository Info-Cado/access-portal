//! Sound from the session, played in the browser.
//!
//! Windows sends the session's audio over the RDP sound channel as plain PCM
//! (16-bit, the rate it picks from our list). Each block goes to the browser as
//! it arrives (see `rdp.rs`, binary kind 7), where the page schedules it with
//! a small jitter buffer. Windows only sends while something is playing.

use std::borrow::Cow;

use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};

/// One block of samples for the browser.
pub struct Block {
    pub rate: u32,
    pub channels: u8,
    /// Interleaved little-endian 16-bit samples.
    pub pcm: Vec<u8>,
}

#[derive(Debug)]
pub struct Backend {
    tx: std::sync::mpsc::Sender<Block>,
    formats: Vec<AudioFormat>,
}

fn pcm(rate: u32, channels: u16) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(channels) * 2,
        n_block_align: channels * 2,
        bits_per_sample: 16,
        data: None,
    }
}

impl Backend {
    pub fn new(tx: std::sync::mpsc::Sender<Block>) -> Self {
        // In order of preference: CD quality first; the browser resamples.
        Self { tx, formats: vec![pcm(44_100, 2), pcm(48_000, 2), pcm(22_050, 2), pcm(44_100, 1), pcm(22_050, 1)] }
    }
}

impl RdpsndClientHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_formats(&self, server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
        tracing::debug!(
            "sound formats offered: {}",
            server_formats
                .iter()
                .map(|f| format!("{:?} {}ch {}Hz {}bit", f.format, f.n_channels, f.n_samples_per_sec, f.bits_per_sample))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.formats.clone()
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if format.format != WaveFormat::PCM || format.bits_per_sample != 16 || data.is_empty() {
            return;
        }
        let _ = self.tx.send(Block {
            rate: format.n_samples_per_sec,
            channels: format.n_channels.min(2) as u8,
            pcm: data.into_owned(),
        });
    }

    // The browser holds about this much before playing.
    fn playback_latency(&self) -> std::time::Duration {
        std::time::Duration::from_millis(80)
    }

    // Windows applies the session's volume to the samples itself.
    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}
