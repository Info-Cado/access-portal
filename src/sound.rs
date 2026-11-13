//! Sound from the session, played in the browser.
//!
//! Windows sends the session's audio over the RDP sound channel in a format
//! picked from our list. Browsers that can decode AAC (WebCodecs) get AAC,
//! about 16 KB/s for CD-quality stereo; others get plain 16-bit PCM, about
//! 176 KB/s. Each block goes to the browser as it arrives (see `rdp.rs`,
//! binary kinds 7 and 9), where the page schedules it with a small jitter
//! buffer. Windows only sends while something is playing.

use std::borrow::Cow;

use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};

/// One block of sound for the browser.
pub struct Block {
    pub rate: u32,
    pub channels: u8,
    /// AAC frames (ADTS) when true; interleaved little-endian 16-bit PCM otherwise.
    pub aac: bool,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct Backend {
    tx: std::sync::mpsc::Sender<Block>,
    formats: Vec<AudioFormat>,
    /// The browser can decode AAC.
    aac: bool,
}

/// AAC bitrate aimed for (bytes per second): 128 kbit/s stereo.
const AAC_TARGET: u32 = 16_000;

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
    pub fn new(tx: std::sync::mpsc::Sender<Block>, aac: bool) -> Self {
        // In order of preference: CD quality first; the browser resamples.
        Self { tx, formats: vec![pcm(44_100, 2), pcm(48_000, 2), pcm(22_050, 2), pcm(44_100, 1), pcm(22_050, 1)], aac }
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
                .map(|f| format!(
                    "{:?} {}ch {}Hz {}bit {}B/s align {} data {}",
                    f.format,
                    f.n_channels,
                    f.n_samples_per_sec,
                    f.bits_per_sample,
                    f.n_avg_bytes_per_sec,
                    f.n_block_align,
                    f.data.as_deref().map(hex::encode).unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut chosen = Vec::new();
        if self.aac {
            // Stereo AAC at 44.1 or 48 kHz closest to the target bitrate, exactly
            // as Windows described it (formats must match the server's).
            let mut aac: Vec<&AudioFormat> = server_formats
                .iter()
                .filter(|f| f.format == WaveFormat::AAC_MS && f.n_channels == 2 && matches!(f.n_samples_per_sec, 44_100 | 48_000))
                .collect();
            // Windows moves between these quality levels with the link's bandwidth.
            aac.sort_by_key(|f| f.n_avg_bytes_per_sec.abs_diff(AAC_TARGET));
            chosen.extend(aac.into_iter().cloned());
        }
        chosen.extend(self.formats.iter().cloned());
        chosen
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if data.is_empty() {
            return;
        }
        let aac = match format.format {
            WaveFormat::AAC_MS => true,
            WaveFormat::PCM if format.bits_per_sample == 16 => false,
            _ => return,
        };
        let _ = self.tx.send(Block {
            rate: format.n_samples_per_sec,
            channels: format.n_channels.min(2) as u8,
            aac,
            data: data.into_owned(),
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
