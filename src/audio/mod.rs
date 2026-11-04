#![allow(dead_code)]

use crate::core::{Codec, RtpPacket};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
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
            // Re-sync only if camera clock drifts or jumps significantly (> 5 seconds = 240,000 ticks)
            let diff = packet_ts_48k.wrapping_sub(self.out_ts);
            if diff > 240_000 && diff < (u32::MAX - 240_000) {
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
    let au_headers_len_bytes = au_headers_len_bits.div_ceil(8);

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
            if let Some(val) = trimmed.strip_prefix("config=")
                && let Ok(bytes) = hex::decode(val.trim())
                    && !bytes.is_empty() {
                        return bytes;
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

pub struct OpusToG711Decoder {
    decoder: opus_pure::OpusDecoder,
    pcm_out: [i16; 960],
}

impl OpusToG711Decoder {
    pub fn new() -> Result<Self> {
        let decoder = opus_pure::OpusDecoder::new(8000, 1)
            .map_err(|e| anyhow::anyhow!("Failed to init Opus decoder: {:?}", e))?;
        Ok(Self {
            decoder,
            pcm_out: [0i16; 960],
        })
    }

    /// Decode incoming Opus packet and return G.711 (PCMU or PCMA) audio bytes.
    pub fn decode_to_g711(&mut self, opus_data: &[u8], is_pcmu: bool) -> Result<Bytes> {
        let n_samples = self
            .decoder
            .decode_s16(opus_data, 960, &mut self.pcm_out)
            .map_err(|e| anyhow::anyhow!("Opus decode error: {:?}", e))?;
        if n_samples == 0 {
            return Ok(Bytes::new());
        }

        let mut out = Vec::with_capacity(n_samples);
        if is_pcmu {
            for &s in &self.pcm_out[..n_samples] {
                out.push(crate::backchannel::pcm::s16_to_ulaw(s));
            }
        } else {
            for &s in &self.pcm_out[..n_samples] {
                out.push(crate::backchannel::pcm::s16_to_alaw(s));
            }
        }
        Ok(Bytes::from(out))
    }
}

/// Pure-Rust AAC to G.711 (PCMU or PCMA) decoder for SIP two-way talk
pub struct AacToG711Decoder {
    decoder: AacDecoder,
    clock_rate: u32,
    channels: usize,
}

impl AacToG711Decoder {
    pub fn new(source_codec: &Codec) -> Result<Self> {
        let clock_rate = source_codec.clock_rate;
        let channels = source_codec.channels.unwrap_or(1) as usize;

        let supported = AacDecoder::supported_codecs();
        if supported.is_empty() {
            bail!("No supported AAC codecs found in symphonia-codec-aac");
        }

        let mut params = AudioCodecParameters::new();
        params.for_codec(supported[0].id);
        params.with_sample_rate(clock_rate);

        let extra_data = parse_aac_config(source_codec.fmtp.as_deref(), clock_rate, channels);
        params.with_extra_data(extra_data.into_boxed_slice());

        let decoder = match AacDecoder::try_new(&params, &AudioDecoderOptions::default()) {
            Ok(dec) => dec,
            Err(_) => {
                let fallback = generate_asc(clock_rate, channels);
                params.with_extra_data(fallback.into_boxed_slice());
                AacDecoder::try_new(&params, &AudioDecoderOptions::default())
                    .context("Failed to initialize pure-Rust AAC decoder with fallback config")?
            }
        };

        Ok(Self {
            decoder,
            clock_rate,
            channels,
        })
    }

    /// Decode incoming AAC RTP packet and return G.711 (PCMU or PCMA) audio bytes at 8000 Hz.
    pub fn decode_to_g711(&mut self, payload: &[u8], is_pcmu: bool) -> Bytes {
        let f32_samples = decode_aac_payload(&mut self.decoder, payload, self.clock_rate);
        if f32_samples.is_empty() {
            return Bytes::new();
        }

        // 1. Downmix to mono if stereo
        let mono_samples: Vec<f32> = if self.channels == 2 {
            f32_samples.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect()
        } else {
            f32_samples
        };

        // 2. Resample from self.clock_rate to 8000 Hz
        let ratio = self.clock_rate as f64 / 8000.0;
        let num_out = ((mono_samples.len() as f64) / ratio).floor() as usize;
        let mut out = Vec::with_capacity(num_out);

        let mut src_pos = 0.0f64;
        while (src_pos as usize) < mono_samples.len() {
            let sample = mono_samples[src_pos as usize];
            let s16 = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            if is_pcmu {
                out.push(crate::backchannel::pcm::s16_to_ulaw(s16));
            } else {
                out.push(crate::backchannel::pcm::s16_to_alaw(s16));
            }
            src_pos += ratio;
        }

        Bytes::from(out)
    }
}

/// Unified camera microphone audio decoder for SIP phone listeners.
/// Automatically handles native PCMU/PCMA, Opus, or AAC (MPEG4-GENERIC).
pub enum CameraAudioDecoder {
    Direct { is_pcma: bool, is_pcmu: bool },
    Opus(Box<OpusToG711Decoder>),
    Aac(Box<AacToG711Decoder>),
}

impl CameraAudioDecoder {
    pub fn new(codec: &Codec) -> Result<Self> {
        let name_upper = codec.name.to_uppercase();
        if name_upper.contains("MPEG4-GENERIC") || name_upper.contains("AAC") {
            Ok(Self::Aac(Box::new(AacToG711Decoder::new(codec)?)))
        } else if name_upper.contains("OPUS") {
            Ok(Self::Opus(Box::new(OpusToG711Decoder::new()?)))
        } else {
            let is_pcma = codec.payload_type == 8 || name_upper == "PCMA";
            let is_pcmu = codec.payload_type == 0 || name_upper == "PCMU";
            Ok(Self::Direct { is_pcma, is_pcmu })
        }
    }

    /// Decode or transcode incoming camera audio payload to 8000 Hz G.711 (PCMU or PCMA)
    pub fn decode_to_g711(&mut self, payload: &[u8], target_is_pcmu: bool) -> Bytes {
        match self {
            Self::Direct { is_pcma, is_pcmu } => {
                if *is_pcma && target_is_pcmu {
                    let mut data = payload.to_vec();
                    crate::backchannel::pcm::transcode_alaw_to_ulaw(&mut data);
                    Bytes::from(data)
                } else if *is_pcmu && !target_is_pcmu {
                    let mut data = payload.to_vec();
                    crate::backchannel::pcm::transcode_ulaw_to_alaw(&mut data);
                    Bytes::from(data)
                } else if *is_pcma || *is_pcmu {
                    Bytes::copy_from_slice(payload)
                } else {
                    Bytes::new()
                }
            }
            Self::Opus(dec) => dec.decode_to_g711(payload, target_is_pcmu).unwrap_or_default(),
            Self::Aac(dec) => dec.decode_to_g711(payload, target_is_pcmu),
        }
    }
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

    #[test]
    fn test_opus_to_g711_decoder() {
        let codec = Codec::new_pcmu();
        let mut transcoder = AudioTranscoder::new(&codec).expect("transcoder init");
        let rtp = RtpPacket::create(0, 1, 0, 1234, false, &[0xFF; 160]);
        let opus_pkts = transcoder.process_packet(&rtp);
        assert!(!opus_pkts.is_empty());

        let mut dec = OpusToG711Decoder::new().expect("decoder init");
        let g711 = dec.decode_to_g711(opus_pkts[0].payload(), true).expect("decode");
        assert_eq!(g711.len(), 160); // 20ms at 8kHz is 160 samples
    }

    #[test]
    fn test_camera_audio_decoder_matrix() {
        // 1. PCMU / PCMA direct
        let pcmu_codec = Codec::new_pcmu();
        let mut pcmu_dec = CameraAudioDecoder::new(&pcmu_codec).expect("pcmu decoder");
        let dummy = vec![0xFFu8; 160];
        let out_pcmu = pcmu_dec.decode_to_g711(&dummy, true);
        assert_eq!(out_pcmu.len(), 160);

        // 2. AAC decoder
        let aac_codec = Codec::new_aac();
        let aac_dec = CameraAudioDecoder::new(&aac_codec);
        assert!(aac_dec.is_ok());

        // 3. Opus decoder
        let opus_codec = Codec::new_opus();
        let opus_dec = CameraAudioDecoder::new(&opus_codec);
        assert!(opus_dec.is_ok());
    }
}
