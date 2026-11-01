#![allow(dead_code)]

use crate::core::{Codec, RtpPacket};
use anyhow::{bail, Context, Result};
use opus_pure::multistream::OpusMSEncoder;
use opus_pure::Application;
use symphonia_codec_aac::AacDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::RegisterableAudioDecoder;
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use tracing::{debug, info, warn};

pub struct AudioTranscoder {
    source_codec: String,
    source_rate: u32,
    source_channels: usize,
    aac_decoder: Option<AacDecoder>,
    opus_encoder: OpusMSEncoder,
    pcm_buffer: Vec<f32>,
    out_seq: u16,
    out_ts: u32,
    out_ssrc: u32,
    last_source_ts: Option<u32>,
}

impl AudioTranscoder {
    pub fn new(source_codec: &Codec) -> Result<Self> {
        let name_upper = source_codec.name.to_uppercase();
        let source_rate = source_codec.clock_rate;
        let source_channels = source_codec.channels.unwrap_or(1) as usize;

        let aac_decoder = if name_upper.contains("MPEG4-GENERIC") || name_upper.contains("AAC") {
            let supported = AacDecoder::supported_codecs();
            if supported.is_empty() {
                bail!("No supported AAC codecs found in symphonia-codec-aac");
            }

            let mut params = AudioCodecParameters::new();
            params.for_codec(supported[0].id);
            params.with_sample_rate(source_rate);

            // Extract AudioSpecificConfig from fmtp if present (e.g. config=1408)
            let extra_data = parse_aac_config(source_codec.fmtp.as_deref(), source_rate, source_channels);
            debug!(config = ?extra_data, rate = source_rate, channels = source_channels, "Initializing pure-Rust AAC decoder");
            params.with_extra_data(extra_data.into_boxed_slice());

            match AacDecoder::try_new(&params, &AudioDecoderOptions::default()) {
                Ok(dec) => Some(dec),
                Err(e) => {
                    warn!("Failed to initialize AAC decoder with params: {}. Retrying with generic config", e);
                    let fallback = generate_asc(source_rate, source_channels);
                    params.with_extra_data(fallback.into_boxed_slice());
                    Some(AacDecoder::try_new(&params, &AudioDecoderOptions::default())
                        .context("Failed to initialize pure-Rust AAC decoder with fallback config")?)
                }
            }
        } else {
            None
        };

        // Standard WebRTC Opus encoder: 48000 Hz, 2 channels (stereo), family 0
        let mut opus_encoder = OpusMSEncoder::new(48000, 2, 0, Application::Voip)
            .map_err(|e| anyhow::anyhow!("Failed to initialize pure-Rust Opus encoder: {:?}", e))?;
        opus_encoder.set_bitrate(48000);

        let out_ssrc = rand::random_u32();

        info!(
            codec = %name_upper,
            rate = source_rate,
            channels = source_channels,
            "Native in-process audio transcoder initialized ({} -> Opus 48kHz)",
            name_upper
        );

        Ok(Self {
            source_codec: name_upper,
            source_rate,
            source_channels,
            aac_decoder,
            opus_encoder,
            pcm_buffer: Vec::with_capacity(3840),
            out_seq: 0,
            out_ts: 0,
            out_ssrc,
            last_source_ts: None,
        })
    }

    /// Process an incoming RTP packet (AAC, PCMU, PCMA) and output Opus RTP packets (20ms frames)
    pub fn process_packet(&mut self, packet: &RtpPacket) -> Vec<RtpPacket> {
        let payload = packet.payload();
        if payload.is_empty() {
            return Vec::new();
        }

        // Synchronize Opus RTP timestamp with camera's audio clock
        let packet_ts_48k = if self.source_rate > 0 {
            (packet.timestamp as u64 * 48000 / self.source_rate as u64) as u32
        } else {
            self.out_ts
        };

        if self.last_source_ts.is_none() {
            self.out_ts = packet_ts_48k;
            self.last_source_ts = Some(packet.timestamp);
        } else {
            // Re-sync if camera clock drifts or jumps significantly (> 1 second = 48000 ticks)
            let diff = packet_ts_48k.wrapping_sub(self.out_ts);
            if diff > 48000 && diff < (u32::MAX - 48000) {
                self.out_ts = packet_ts_48k;
            }
        }

        let pcm_floats = if let Some(decoder) = &mut self.aac_decoder {
            decode_aac_payload(decoder, payload, self.source_rate)
        } else if self.source_codec.contains("PCMU") {
            decode_pcmu_payload(payload)
        } else if self.source_codec.contains("PCMA") {
            decode_pcma_payload(payload)
        } else {
            return Vec::new();
        };

        if pcm_floats.is_empty() {
            return Vec::new();
        }

        // Resample decoded floats to 48000 Hz stereo
        resample_to_48k_stereo(
            &pcm_floats,
            self.source_rate,
            self.source_channels,
            &mut self.pcm_buffer,
        );

        let mut output_packets = Vec::new();
        let mut opus_out = [0u8; 1000];

        // 20ms at 48000Hz stereo = 960 frames * 2 channels = 1920 floats
        const FRAME_SIZE: usize = 960;
        const SAMPLES_PER_FRAME: usize = FRAME_SIZE * 2;

        while self.pcm_buffer.len() >= SAMPLES_PER_FRAME {
            match self.opus_encoder.encode(&self.pcm_buffer[..SAMPLES_PER_FRAME], FRAME_SIZE, &mut opus_out) {
                Ok(encoded_bytes) => {
                    let rtp = RtpPacket::create(
                        111, // Opus standard payload type
                        self.out_seq,
                        self.out_ts,
                        self.out_ssrc,
                        false,
                        &opus_out[..encoded_bytes],
                    );
                    output_packets.push(rtp);

                    self.out_seq = self.out_seq.wrapping_add(1);
                    self.out_ts = self.out_ts.wrapping_add(FRAME_SIZE as u32);
                }
                Err(e) => {
                    warn!("Opus encode error: {:?}", e);
                }
            }

            self.pcm_buffer.drain(..SAMPLES_PER_FRAME);
        }

        output_packets
    }
}

/// Decode RFC 3640 AAC-hbr RTP payload using symphonia-codec-aac
fn decode_aac_payload(decoder: &mut AacDecoder, payload: &[u8], _sample_rate: u32) -> Vec<f32> {
    if payload.len() < 4 {
        return Vec::new();
    }

    // In RFC 3640 mode=AAC-hbr:
    // First 2 bytes is AU-headers-length in bits (big-endian)
    let au_headers_len_bits = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let au_headers_len_bytes = (au_headers_len_bits + 7) / 8;

    let aac_frame = if au_headers_len_bytes > 0 && payload.len() >= 2 + au_headers_len_bytes {
        let au_header = u16::from_be_bytes([payload[2], payload[3]]);
        let au_size = (au_header >> 3) as usize;
        let data_offset = 2 + au_headers_len_bytes;
        if payload.len() >= data_offset + au_size && au_size > 0 {
            &payload[data_offset..data_offset + au_size]
        } else {
            &payload[data_offset..]
        }
    } else {
        // Fallback: raw AAC frame without AU headers
        payload
    };

    let packet_ref = PacketRef::new(0, Timestamp::from(0u32), Duration::from(1024u32), aac_frame);
    match decoder.decode_ref(&packet_ref) {
        Ok(buf_ref) => {
            let mut out = Vec::new();
            match buf_ref {
                GenericAudioBufferRef::F32(b) => {
                    b.copy_to_vec_interleaved(&mut out);
                }
                GenericAudioBufferRef::S16(b) => {
                    b.copy_to_vec_interleaved(&mut out);
                }
                _ => {}
            }
            out
        }
        Err(e) => {
            debug!("AAC decode error (frame len {}): {}", aac_frame.len(), e);
            Vec::new()
        }
    }
}

/// Decode G.711 u-law payload (8000 Hz, mono) to f32 samples
fn decode_pcmu_payload(payload: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(payload.len());
    for &u in payload {
        let sample = ulaw_to_pcm(u);
        out.push(sample as f32 / 32768.0);
    }
    out
}

/// Decode G.711 A-law payload (8000 Hz, mono) to f32 samples
fn decode_pcma_payload(payload: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(payload.len());
    for &a in payload {
        let sample = alaw_to_pcm(a);
        out.push(sample as f32 / 32768.0);
    }
    out
}

/// Standard G.711 u-law expansion to linear 16-bit PCM
#[inline]
fn ulaw_to_pcm(u: u8) -> i16 {
    let u = !u;
    let sign = u & 0x80;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0f;
    let mut sample = ((mantissa as i16) << 3) + 132;
    sample <<= exponent;
    sample -= 132;
    if sign != 0 {
        -sample
    } else {
        sample
    }
}

/// Standard G.711 A-law expansion to linear 16-bit PCM
#[inline]
fn alaw_to_pcm(a: u8) -> i16 {
    let a = a ^ 0x55;
    let sign = a & 0x80;
    let exponent = (a >> 4) & 0x07;
    let mantissa = a & 0x0f;
    let sample = match exponent {
        0 => ((mantissa as i16) << 4) + 8,
        _ => (((mantissa as i16) << 4) + 264) << (exponent - 1),
    };
    if sign != 0 {
        -sample
    } else {
        sample
    }
}

/// Resample arbitrary input audio to 48000 Hz stereo floats via linear interpolation
fn resample_to_48k_stereo(
    input: &[f32],
    in_rate: u32,
    in_channels: usize,
    output_buf: &mut Vec<f32>,
) {
    if input.is_empty() || in_rate == 0 || in_channels == 0 {
        return;
    }

    let in_frames = input.len() / in_channels;
    if in_frames == 0 {
        return;
    }

    let out_frames = ((in_frames as u64 * 48000) / in_rate as u64) as usize;
    output_buf.reserve(out_frames * 2);

    let step = in_rate as f64 / 48000.0;

    for i in 0..out_frames {
        let src_pos = i as f64 * step;
        let idx0 = src_pos.floor() as usize;
        let frac = (src_pos - idx0 as f64) as f32;
        let idx1 = (idx0 + 1).min(in_frames - 1);

        if in_channels == 1 {
            let s0 = input[idx0];
            let s1 = input[idx1];
            let val = s0 + frac * (s1 - s0);
            output_buf.push(val);
            output_buf.push(val); // Duplicate to stereo
        } else {
            let left0 = input[idx0 * in_channels];
            let left1 = input[idx1 * in_channels];
            let left = left0 + frac * (left1 - left0);

            let right0 = input[idx0 * in_channels + 1];
            let right1 = input[idx1 * in_channels + 1];
            let right = right0 + frac * (right1 - right0);

            output_buf.push(left);
            output_buf.push(right);
        }
    }
}

/// Parse or derive MPEG-4 AudioSpecificConfig (ASC) bytes
fn parse_aac_config(fmtp: Option<&str>, rate: u32, channels: usize) -> Vec<u8> {
    if let Some(f) = fmtp {
        for part in f.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix("config=") {
                if let Ok(bytes) = hex::decode(val.trim()) {
                    if !bytes.is_empty() {
                        return bytes;
                    }
                }
            }
        }
    }
    generate_asc(rate, channels)
}

/// Generate a 2-byte AudioSpecificConfig for AAC-LC
fn generate_asc(rate: u32, channels: usize) -> Vec<u8> {
    let rate_idx: u8 = match rate {
        96000 => 0x0,
        88200 => 0x1,
        64000 => 0x2,
        48000 => 0x3,
        44100 => 0x4,
        32000 => 0x5,
        24000 => 0x6,
        22050 => 0x7,
        16000 => 0x8,
        12000 => 0x9,
        11025 => 0xa,
        8000 => 0xb,
        7350 => 0xc,
        _ => 0x4, // Default to 44.1kHz
    };

    let channel_cfg: u8 = match channels {
        1 => 1,
        2 => 2,
        _ => 1,
    };

    // AudioSpecificConfig:
    // audioObjectType (5 bits) = 2 (AAC LC)
    // samplingFrequencyIndex (4 bits)
    // channelConfiguration (4 bits)
    // frameLengthFlag (1 bit) = 0 (1024)
    // dependsOnCoreCoder (1 bit) = 0
    // extensionFlag (1 bit) = 0
    let b0 = (2 << 3) | (rate_idx >> 1);
    let b1 = ((rate_idx & 0x1) << 7) | (channel_cfg << 3);

    vec![b0, b1]
}

mod rand {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0x1337cafe);

    pub fn random_u32() -> u32 {
        let prev = SEED.fetch_add(0x9e3779b9, Ordering::Relaxed);
        prev ^ (prev >> 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_g711_decode() {
        assert_eq!(ulaw_to_pcm(0xFF), 0); // 0xFF in u-law is zero
        let pcm_a = alaw_to_pcm(0xD5);
        assert_eq!(pcm_a, -8);
    }

    #[test]
    fn test_resample_48k() {
        let input = vec![0.5f32; 160]; // 10ms of 16kHz mono
        let mut out = Vec::new();
        resample_to_48k_stereo(&input, 16000, 1, &mut out);
        // 10ms at 48kHz stereo = 480 frames * 2 = 960 samples
        assert_eq!(out.len(), 960);
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!((out[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn test_opus_transcoder_pcmu() {
        let codec = Codec::new_pcmu();
        let mut transcoder = AudioTranscoder::new(&codec).expect("transcoder init");
        // Send 160 bytes of PCMU (20ms at 8000Hz)
        let rtp = RtpPacket::create(0, 1, 0, 1234, false, &[0xFF; 160]);
        let out = transcoder.process_packet(&rtp);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload_type, 111);
        assert!(!out[0].payload().is_empty());
    }

    #[test]
    fn test_opus_transcoder_aac_init() {
        let codec = Codec::new_aac();
        let transcoder = AudioTranscoder::new(&codec);
        assert!(transcoder.is_ok());
    }
}
