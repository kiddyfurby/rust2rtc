#![allow(dead_code)]

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use rustrtc::config::{AudioCapability, IceServer, MediaCapabilities, RtcConfiguration, VideoCapability};
use rustrtc::media::track::{sample_track, FeedbackEvent, SampleStreamSource};
use rustrtc::peer_connection::{PeerConnection, RtpCodecParameters};
use rustrtc::sdp::{SdpType, SessionDescription};
use rustrtc::transports::ice::IceCandidate;

use crate::core::MediaKind;
use crate::streams::Stream;

pub struct RustRtcSession {
    pub session_id: String,
    pub pc: Arc<PeerConnection>,
    pub video_source: Option<SampleStreamSource>,
    pub audio_source: Option<SampleStreamSource>,
}

impl RustRtcSession {
    pub async fn create(
        stream: Arc<Stream>,
        offer_sdp: &str,
        candidates: &[String],
    ) -> Result<(Self, String)> {
        let offer = SessionDescription::parse(SdpType::Offer, offer_sdp)?;

        // Build RtcConfiguration
        let mut config = RtcConfiguration::webrtc();

        // Configure STUN/TURN servers if provided
        for cand in candidates {
            if cand.starts_with("stun:") || cand.starts_with("turn:") {
                config.ice_servers.push(IceServer::new(vec![cand.clone()]));
            } else if let Ok(ip) = cand.split(':').next().unwrap_or("").parse() {
                config.external_ips.push(ip);
            }
        }

        // Detect video codec from stream tracks
        let tracks = stream.get_tracks().await;
        let is_stream_hevc = tracks.iter().any(|t| {
            t.kind == MediaKind::Video
                && (t.codec.name.to_uppercase().contains("265")
                    || t.codec.name.to_uppercase().contains("HEVC"))
        });

        let is_high_51 = tracks.iter().any(|t| {
            t.codec.fmtp.as_deref().map_or(false, |f| f.contains("640033"))
        });
        let is_high = tracks.iter().any(|t| {
            t.codec.fmtp.as_deref().map_or(false, |f| f.contains("6400"))
        });

        let mut video_caps = Vec::new();
        if is_stream_hevc {
            video_caps.push(VideoCapability {
                payload_type: 96,
                codec_name: "H265".to_string(),
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec![
                    "nack".to_string(),
                    "nack pli".to_string(),
                    "goog-remb".to_string(),
                ],
                rtx_payload_type: None,
            });
        }
        if is_high_51 {
            video_caps.push(VideoCapability {
                payload_type: 97,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033".to_string()),
                rtcp_fbs: vec![
                    "nack".to_string(),
                    "nack pli".to_string(),
                    "goog-remb".to_string(),
                ],
                rtx_payload_type: None,
            });
        }
        if is_high {
            video_caps.push(VideoCapability {
                payload_type: 98,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032".to_string()),
                rtcp_fbs: vec![
                    "nack".to_string(),
                    "nack pli".to_string(),
                    "goog-remb".to_string(),
                ],
                rtx_payload_type: None,
            });
        }
        video_caps.push(VideoCapability {
            payload_type: 99,
            codec_name: "H264".to_string(),
            clock_rate: 90000,
            fmtp: Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string()),
            rtcp_fbs: vec![
                "nack".to_string(),
                "nack pli".to_string(),
                "goog-remb".to_string(),
            ],
            rtx_payload_type: None,
        });

        let audio_caps = vec![
            AudioCapability {
                payload_type: 111,
                codec_name: "opus".to_string(),
                clock_rate: 48000,
                channels: 2,
                fmtp: Some("minptime=10;useinbandfec=1".to_string()),
                rtcp_fbs: vec![],
            },
            AudioCapability {
                payload_type: 8,
                codec_name: "PCMA".to_string(),
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
                rtcp_fbs: vec![],
            },
            AudioCapability {
                payload_type: 0,
                codec_name: "PCMU".to_string(),
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
                rtcp_fbs: vec![],
            },
        ];

        config.media_capabilities = Some(MediaCapabilities {
            video: video_caps,
            audio: audio_caps,
            application: None,
            image: vec![],
        });

        let pc = Arc::new(PeerConnection::new(config));

        // 1. Set Remote Description
        pc.set_remote_description(offer).await?;

        // 2. Add Video Track if stream has video
        let mut video_source_opt = None;
        if tracks.iter().any(|t| t.kind == MediaKind::Video) {
            let (v_source, v_track, mut v_fb) = sample_track(rustrtc::media::MediaKind::Video, 128);
            video_source_opt = Some(v_source);

            let v_params = RtpCodecParameters {
                name: if is_stream_hevc { "H265".to_string() } else { "H264".to_string() },
                clock_rate: 90000,
                channels: 1,
                payload_type: if is_stream_hevc {
                    96
                } else if is_high_51 {
                    97
                } else if is_high {
                    98
                } else {
                    99
                },
            };

            let _ = pc.add_track(v_track, v_params);

            // Listen for keyframe requests (PLI/FIR)
            let s_clone = Arc::clone(&stream);
            tokio::spawn(async move {
                while let Some(fb) = v_fb.recv().await {
                    match fb {
                        FeedbackEvent::RequestKeyFrame => {
                            debug!(stream = %s_clone.name, "Received WebRTC RequestKeyFrame");
                        }
                    }
                }
            });
        }

        // 3. Add Audio Track if stream has audio
        let mut audio_source_opt = None;
        if tracks.iter().any(|t| t.kind == MediaKind::Audio) {
            let (a_source, a_track, _) = sample_track(rustrtc::media::MediaKind::Audio, 128);
            audio_source_opt = Some(a_source);

            let a_params = RtpCodecParameters {
                name: "opus".to_string(),
                clock_rate: 48000,
                channels: 2,
                payload_type: 111,
            };

            let _ = pc.add_track(a_track, a_params);
        }

        // 4. Create Answer
        let answer = pc.create_answer().await?;
        pc.set_local_description(answer)?;

        // 5. Wait for ICE gathering to complete for non-trickle (WHEP)
        tokio::select! {
            _ = pc.wait_for_gathering_complete() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }

        let local_desc = pc
            .local_description()
            .ok_or_else(|| anyhow::anyhow!("Missing local description"))?;
        let answer_sdp = local_desc.to_sdp_string();

        let session_id = uuid::Uuid::new_v4().to_string();

        Ok((
            Self {
                session_id,
                pc,
                video_source: video_source_opt,
                audio_source: audio_source_opt,
            },
            answer_sdp,
        ))
    }

    pub fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.pc.add_ice_candidate(candidate)?;
        Ok(())
    }

    /// Spawn background video and audio forwarding workers from stream tracks
    pub fn start_forwarding(&self, stream: Arc<Stream>, is_hevc: bool) {
        // 1. Video forwarder
        if let Some(v_source) = self.video_source.clone() {
            let s_clone = Arc::clone(&stream);
            tokio::spawn(async move {
                let tracks = s_clone.get_tracks().await;
                let video_track = match tracks.iter().find(|t| t.kind == MediaKind::Video) {
                    Some(t) => Arc::clone(t),
                    None => return,
                };

                let mut rx = video_track.subscribe();
                let mut sent_first_keyframe = false;

                // Extract parameter sets from SDP fmtp if present
                let mut param_sets = Vec::new();
                if let Some(ref fmtp) = video_track.codec.fmtp {
                    if is_hevc {
                        if let Some((vps, sps, pps)) = crate::core::extract_h265_vps_sps_pps(fmtp) {
                            param_sets.push(vps);
                            param_sets.push(sps);
                            param_sets.push(pps);
                        }
                    } else if let Some((sps, pps)) = crate::core::extract_h264_sps_pps(fmtp) {
                        param_sets.push(sps);
                        param_sets.push(pps);
                    }
                }

                while let Ok(pkt) = rx.recv().await {
                    let payload = pkt.payload();
                    if payload.is_empty() {
                        continue;
                    }

                    // Dynamically cache any in-band parameter sets
                    if !is_hevc {
                        let nalu_type = payload[0] & 0x1F;
                        if nalu_type == 7 || nalu_type == 8 {
                            param_sets.retain(|ps| (ps[0] & 0x1F) != nalu_type);
                            param_sets.push(payload.to_vec());
                        }
                    } else {
                        let nalu_type = (payload[0] >> 1) & 0x3F;
                        if (32..=34).contains(&nalu_type) {
                            param_sets.retain(|ps| ((ps[0] >> 1) & 0x3F) != nalu_type);
                            param_sets.push(payload.to_vec());
                        }
                    }

                    // Keyframe detection to ensure clean playback start
                    if !sent_first_keyframe {
                        let is_kf = if is_hevc {
                            let nalu_type = (payload[0] >> 1) & 0x3F;
                            (16..=21).contains(&nalu_type)
                        } else {
                            let nalu_type = payload[0] & 0x1F;
                            nalu_type == 5
                        };
                        if !is_kf {
                            continue;
                        }

                        // Send parameter sets immediately before first keyframe for instant decode
                        for ps in &param_sets {
                            let ps_frame = rustrtc::media::frame::VideoFrame {
                                rtp_timestamp: pkt.timestamp,
                                width: 0,
                                height: 0,
                                format: rustrtc::media::VideoPixelFormat::Unspecified,
                                rotation_deg: 0,
                                is_last_packet: false,
                                data: bytes::Bytes::copy_from_slice(ps),
                                header_extension: None,
                                csrcs: vec![],
                                sequence_number: None,
                                payload_type: None,
                                source_addr: None,
                                raw_packet: None,
                            };
                            let _ = v_source.send_video(ps_frame);
                        }
                        sent_first_keyframe = true;
                    }

                    let v_frame = rustrtc::media::frame::VideoFrame {
                        rtp_timestamp: pkt.timestamp,
                        width: 0,
                        height: 0,
                        format: rustrtc::media::VideoPixelFormat::Unspecified,
                        rotation_deg: 0,
                        is_last_packet: pkt.marker,
                        data: bytes::Bytes::copy_from_slice(payload),
                        header_extension: None,
                        csrcs: vec![],
                        sequence_number: None,
                        payload_type: None,
                        source_addr: None,
                        raw_packet: None,
                    };

                    if v_source.send_video(v_frame).is_err() {
                        break;
                    }
                }
            });
        }

        // 2. Audio forwarder
        if let Some(a_source) = self.audio_source.clone() {
            let s_clone = Arc::clone(&stream);
            tokio::spawn(async move {
                if let Some(opus_track) = s_clone.get_or_create_opus_track().await {
                    let mut rx = opus_track.subscribe();
                    while let Ok(pkt) = rx.recv().await {
                        let payload = pkt.payload();
                        if payload.is_empty() {
                            continue;
                        }

                        let a_frame = rustrtc::media::frame::AudioFrame {
                            rtp_timestamp: pkt.timestamp,
                            clock_rate: 48000,
                            data: bytes::Bytes::copy_from_slice(payload),
                            sequence_number: None,
                            payload_type: None,
                            marker: pkt.marker,
                            header_extension: None,
                            source_addr: None,
                            raw_packet: None,
                        };

                        if a_source.send_audio(a_frame).is_err() {
                            break;
                        }
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rustrtc_session_sdp_negotiation() {
        let stream = Arc::new(Stream::new("test_stream".to_string(), vec![]));

        // Sample WebRTC offer SDP
        let offer_sdp = "\
v=0\r\n\
o=- 1495799811084970 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:testufrag\r\n\
a=ice-pwd:testpassword0123456789abcdef\r\n\
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

        let (session, answer_sdp) = RustRtcSession::create(stream, offer_sdp, &[]).await.unwrap();
        assert!(!session.session_id.is_empty());
        assert!(answer_sdp.contains("v=0"));
        assert!(answer_sdp.contains("m=video"));
        assert!(answer_sdp.contains("a=setup:"));
    }
}
