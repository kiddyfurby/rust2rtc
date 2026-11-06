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
    pub close_tx: tokio::sync::broadcast::Sender<()>,
    pub connected_tx: tokio::sync::broadcast::Sender<()>,
    pub negotiated_video_pt: u8,
    pub negotiated_audio_pt: u8,
    pub audio_clock_rate: u32,
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
            t.codec.fmtp.as_deref().is_some_and(|f| f.contains("640033"))
        });
        let is_high = tracks.iter().any(|t| {
            t.codec.fmtp.as_deref().is_none_or(|f| f.contains("6400"))
        });

        let default_v_pt = if is_stream_hevc {
            100
        } else if is_high_51 {
            102
        } else if is_high {
            98
        } else {
            96
        };
        let negotiated_video_pt =
            super::find_negotiated_video_pt(offer_sdp, is_stream_hevc, is_high, default_v_pt);

        // Detect whether the stream has a native PCMA or PCMU audio track (zero transcoding needed)
        let source_audio_codec = tracks
            .iter()
            .find(|t| t.kind == MediaKind::Audio)
            .map(|t| t.codec.name.to_uppercase());
        let is_pcma = source_audio_codec.as_deref().is_some_and(|c| c.contains("PCMA"));
        let is_pcmu = source_audio_codec.as_deref().is_some_and(|c| c.contains("PCMU"));

        let (negotiated_audio_pt, audio_codec_name, audio_clock_rate, audio_channels, audio_fmtp) =
            if is_pcma {
                (
                    super::find_negotiated_audio_pt(offer_sdp, 8),
                    "PCMA".to_string(),
                    8000,
                    1,
                    None,
                )
            } else if is_pcmu {
                (
                    super::find_negotiated_audio_pt(offer_sdp, 0),
                    "PCMU".to_string(),
                    8000,
                    1,
                    None,
                )
            } else {
                (
                    super::find_negotiated_audio_pt(offer_sdp, 111),
                    "opus".to_string(),
                    48000,
                    2,
                    Some("minptime=10;useinbandfec=1".to_string()),
                )
            };

        let rtcp_fbs = vec![
            "goog-remb".to_string(),
            "ccm fir".to_string(),
            "nack".to_string(),
            "nack pli".to_string(),
        ];

        let mut video_caps = Vec::new();
        if is_stream_hevc {
            let fmtp = if offer_sdp.contains("profile-id=1") {
                Some("level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST".to_string())
            } else {
                None
            };
            video_caps.push(VideoCapability {
                payload_type: negotiated_video_pt,
                codec_name: "H265".to_string(),
                clock_rate: 90000,
                fmtp,
                rtcp_fbs: rtcp_fbs.clone(),
                rtx_payload_type: None,
            });
        } else {
            let fmtp = if is_high_51 {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033"
            } else if is_high {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"
            } else {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            };
            video_caps.push(VideoCapability {
                payload_type: negotiated_video_pt,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some(fmtp.to_string()),
                rtcp_fbs,
                rtx_payload_type: None,
            });
        }

        let audio_caps = vec![AudioCapability {
            payload_type: negotiated_audio_pt,
            codec_name: audio_codec_name.clone(),
            clock_rate: audio_clock_rate,
            channels: audio_channels,
            fmtp: audio_fmtp,
            rtcp_fbs: vec![],
        }];

        config.media_capabilities = Some(MediaCapabilities {
            video: video_caps,
            audio: audio_caps,
            application: None,
            image: vec![],
        });

        let pc = Arc::new(PeerConnection::new(config));

        // 1. Set Remote Description
        pc.set_remote_description(offer).await?;

        let session_id = uuid::Uuid::new_v4().to_string();
        let (close_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let (connected_tx, _) = tokio::sync::broadcast::channel::<()>(1);

        // Monitor PeerConnectionState changes
        {
            let mut state_rx = pc.subscribe_peer_state();
            let sid = session_id.clone();
            let close_tx_clone = close_tx.clone();
            let connected_tx_clone = connected_tx.clone();
            tokio::spawn(async move {
                loop {
                    let state = *state_rx.borrow_and_update();
                    debug!(session = %sid, state = ?state, "rustrtc peer connection state changed");
                    match state {
                        rustrtc::PeerConnectionState::Connected => {
                            let _ = connected_tx_clone.send(());
                        }
                        rustrtc::PeerConnectionState::Disconnected
                        | rustrtc::PeerConnectionState::Failed
                        | rustrtc::PeerConnectionState::Closed => {
                            let _ = close_tx_clone.send(());
                            break;
                        }
                        _ => {}
                    }
                    if state_rx.changed().await.is_err() {
                        let _ = close_tx_clone.send(());
                        break;
                    }
                }
            });
        }

        // 2. Add Video Track if stream has video
        let mut video_source_opt = None;
        if tracks.iter().any(|t| t.kind == MediaKind::Video) {
            let (v_source, v_track, mut v_fb) = sample_track(rustrtc::media::MediaKind::Video, 512);
            video_source_opt = Some(v_source.clone());

            let v_params = RtpCodecParameters {
                name: if is_stream_hevc { "H265".to_string() } else { "H264".to_string() },
                clock_rate: 90000,
                channels: 1,
                payload_type: negotiated_video_pt,
            };

            let _ = pc.add_track(v_track, v_params);

            // Listen for keyframe requests (PLI/FIR)
            let s_clone = Arc::clone(&stream);
            let v_source_fb = v_source.clone();
            let mut close_rx = close_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = close_rx.recv() => break,
                        fb_opt = v_fb.recv() => match fb_opt {
                            Some(FeedbackEvent::RequestKeyFrame) => {
                                debug!(stream = %s_clone.name, "Received WebRTC RequestKeyFrame; replaying cached keyframe");
                                let tracks = s_clone.get_tracks().await;
                                if let Some(vt) = tracks.iter().find(|t| t.kind == MediaKind::Video) {
                                    let kf_pkts = vt.get_keyframe_packets();
                                    for pkt in &kf_pkts {
                                        if !forward_packet_to_webrtc(&v_source_fb, pkt, negotiated_video_pt, is_stream_hevc) {
                                            break;
                                        }
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
            });
        }

        // 3. Add Audio Track if stream has audio
        let mut audio_source_opt = None;
        if tracks.iter().any(|t| t.kind == MediaKind::Audio) {
            let (a_source, a_track, _) = sample_track(rustrtc::media::MediaKind::Audio, 256);
            audio_source_opt = Some(a_source);

            let a_params = RtpCodecParameters {
                name: audio_codec_name,
                clock_rate: audio_clock_rate,
                channels: audio_channels,
                payload_type: negotiated_audio_pt,
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

        // 6. Spawn incoming event listener for two-way audio (backchannel)
        let pc_events = Arc::clone(&pc);
        let s_clone = Arc::clone(&stream);
        let mut close_rx = close_tx.subscribe();
        let close_tx_for_bc = close_tx.clone();
        tokio::spawn(async move {
            use rustrtc::media::track::MediaStreamTrack;
            loop {
                let event = tokio::select! {
                    _ = close_rx.recv() => break,
                    ev = pc_events.recv() => match ev {
                        Some(ev) => ev,
                        None => break,
                    }
                };
                if let rustrtc::peer_connection::PeerConnectionEvent::Track(transceiver) = event {
                    if transceiver.kind() == rustrtc::MediaKind::Audio {
                        if let Some(receiver) = transceiver.receiver() {
                            let s = Arc::clone(&s_clone);
                            let mut bc_close_rx = close_tx_for_bc.subscribe();
                            tokio::spawn(async move {
                                if let Ok(Some(_)) = s.start_backchannel().await {
                                    tracing::info!(stream = %s.name, "rustrtc incoming backchannel audio track established");
                                    let mut decoder = match crate::audio::OpusToG711Decoder::new() {
                                        Ok(d) => d,
                                        Err(e) => {
                                            tracing::error!("Failed to create Opus to G.711 decoder: {}", e);
                                            return;
                                        }
                                    };
                                    let track = receiver.track();
                                    loop {
                                        tokio::select! {
                                            _ = bc_close_rx.recv() => break,
                                            res = track.recv() => match res {
                                                Ok(rustrtc::media::frame::MediaSample::Audio(frame)) => {
                                                    let g711_res = if frame.payload_type == Some(0) {
                                                        Ok(frame.data.clone())
                                                    } else if frame.payload_type == Some(8) {
                                                        let ulaw: Vec<u8> = frame
                                                            .data
                                                            .iter()
                                                            .map(|&a| crate::backchannel::pcm::alaw_to_ulaw(a))
                                                            .collect();
                                                        Ok(bytes::Bytes::from(ulaw))
                                                    } else {
                                                        decoder.decode_to_g711(&frame.data, true)
                                                    };
                                                    if let Ok(g711) = g711_res {
                                                        if !g711.is_empty() {
                                                            let _ = s.send_backchannel_audio(g711, true).await;
                                                        }
                                                    }
                                                }
                                                Ok(_) => continue,
                                                Err(_) => break,
                                            }
                                        }
                                    }
                                    s.stop_backchannel().await;
                                }
                            });
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                session_id,
                pc,
                video_source: video_source_opt,
                audio_source: audio_source_opt,
                close_tx,
                connected_tx,
                negotiated_video_pt,
                negotiated_audio_pt,
                audio_clock_rate,
            },
            answer_sdp,
        ))
    }

    pub fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.pc.add_ice_candidate(candidate)?;
        Ok(())
    }

    /// Spawn background video and audio forwarding workers from stream tracks
    pub fn start_forwarding(&self, stream: Arc<Stream>, is_hevc: bool, answer_sdp: &str) {
        let actual_video_pt = self.negotiated_video_pt;
        let actual_audio_pt = self.negotiated_audio_pt;
        let audio_clock_rate = self.audio_clock_rate;

        // 1. Video forwarder
        if let Some(v_source) = self.video_source.clone() {
            let s_clone = Arc::clone(&stream);
            let mut close_rx = self.close_tx.subscribe();
            let mut connected_rx = self.connected_tx.subscribe();
            tracing::info!(
                stream = %stream.name,
                pt = actual_video_pt,
                is_hevc = is_hevc,
                "Starting rustrtc WebRTC video forwarder"
            );

            tokio::spawn(async move {
                let tracks = s_clone.get_tracks().await;
                let video_track = match tracks.iter().find(|t| t.kind == MediaKind::Video) {
                    Some(t) => Arc::clone(t),
                    None => return,
                };

                let mut rx = video_track.subscribe();
                let mut sent_first_keyframe = false;
                let mut last_was_param_set = false;
                let mut last_keyframe_ts: Option<u32> = None;
                let mut connected_fired = false;

                let mut cached_h264_sps: Option<Vec<u8>> = None;
                let mut cached_h264_pps: Option<Vec<u8>> = None;
                let mut cached_h265_vps: Option<Vec<u8>> = None;
                let mut cached_h265_sps: Option<Vec<u8>> = None;
                let mut cached_h265_pps: Option<Vec<u8>> = None;

                // Pre-generate parameter sets packets (STAP-A for H264, individual VPS/SPS/PPS for H265) if available
                let mut param_set_packets = if let Some(ref fmtp) = video_track.codec.fmtp {
                    if is_hevc {
                        crate::core::extract_h265_vps_sps_pps(fmtp)
                            .map(|(vps, sps, pps)| {
                                cached_h265_vps = Some(vps.clone());
                                cached_h265_sps = Some(sps.clone());
                                cached_h265_pps = Some(pps.clone());
                                vec![vps, sps, pps]
                            })
                            .unwrap_or_default()
                    } else {
                        crate::core::extract_h264_sps_pps(fmtp)
                            .map(|(sps, pps)| {
                                cached_h264_sps = Some(sps.clone());
                                cached_h264_pps = Some(pps.clone());
                                vec![crate::core::build_h264_stap_a(&sps, &pps)]
                            })
                            .unwrap_or_default()
                    }
                } else {
                    Vec::new()
                };

                let cached_gop = video_track.get_gop_packets();
                if !cached_gop.is_empty() {
                    tracing::info!(
                        stream = %s_clone.name,
                        packets = cached_gop.len(),
                        "Replaying cached GOP for instant WebRTC startup"
                    );
                    for pkt in &cached_gop {
                        if !forward_packet_to_webrtc(&v_source, pkt, actual_video_pt, is_hevc) {
                            return;
                        }
                    }
                    sent_first_keyframe = true;
                }

                loop {
                    let packet = tokio::select! {
                        _ = close_rx.recv() => break,
                        Ok(()) = connected_rx.recv(), if !connected_fired => {
                            connected_fired = true;
                            sent_first_keyframe = false;
                            last_was_param_set = false;
                            last_keyframe_ts = None;
                            continue;
                        }
                        res = rx.recv() => match res {
                            Ok(pkt) => pkt,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("rustrtc video receiver lagged by {} packets", n);
                                sent_first_keyframe = false;
                                last_keyframe_ts = None;
                                last_was_param_set = false;
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    };

                    let payload_slice = if packet.payload_offset + packet.payload_len <= packet.raw.len() {
                        &packet.raw[packet.payload_offset..packet.payload_offset + packet.payload_len]
                    } else {
                        continue;
                    };
                    if payload_slice.is_empty() {
                        continue;
                    }

                    let (is_keyframe, is_param_set) =
                        crate::core::is_keyframe_or_param_set(is_hevc, payload_slice);

                    // Dynamically cache parameter sets seen in stream
                    if is_param_set {
                        if !is_hevc {
                            let nalu_type = payload_slice[0] & 0x1F;
                            if nalu_type == 7 {
                                cached_h264_sps = Some(payload_slice.to_vec());
                            } else if nalu_type == 8 {
                                cached_h264_pps = Some(payload_slice.to_vec());
                            } else if nalu_type == 24 {
                                param_set_packets = vec![payload_slice.to_vec()];
                            }
                            if let (Some(sps), Some(pps)) = (&cached_h264_sps, &cached_h264_pps) {
                                param_set_packets = vec![crate::core::build_h264_stap_a(sps, pps)];
                            }
                        } else {
                            let nalu_type = (payload_slice[0] >> 1) & 0x3F;
                            if nalu_type == 32 {
                                cached_h265_vps = Some(payload_slice.to_vec());
                            } else if nalu_type == 33 {
                                cached_h265_sps = Some(payload_slice.to_vec());
                            } else if nalu_type == 34 {
                                cached_h265_pps = Some(payload_slice.to_vec());
                            } else if nalu_type == 48 {
                                let unpacked = crate::core::unpack_h265_ap(payload_slice);
                                for nalu in unpacked {
                                    if !nalu.is_empty() {
                                        let sub_type = (nalu[0] >> 1) & 0x3F;
                                        if sub_type == 32 {
                                            cached_h265_vps = Some(nalu);
                                        } else if sub_type == 33 {
                                            cached_h265_sps = Some(nalu);
                                        } else if sub_type == 34 {
                                            cached_h265_pps = Some(nalu);
                                        }
                                    }
                                }
                            }
                            let mut ps = Vec::new();
                            if let Some(ref v) = cached_h265_vps {
                                ps.push(v.clone());
                            }
                            if let Some(ref s) = cached_h265_sps {
                                ps.push(s.clone());
                            }
                            if let Some(ref p) = cached_h265_pps {
                                ps.push(p.clone());
                            }
                            if !ps.is_empty() {
                                param_set_packets = ps;
                            }
                        }
                    }

                    if !sent_first_keyframe {
                        if !is_keyframe {
                            continue;
                        }
                        sent_first_keyframe = true;
                    }

                    if is_keyframe && last_keyframe_ts != Some(packet.timestamp) {
                        last_keyframe_ts = Some(packet.timestamp);
                        if !last_was_param_set {
                            for ps_bytes in &param_set_packets {
                                let ps_frame = rustrtc::media::frame::VideoFrame {
                                    rtp_timestamp: packet.timestamp,
                                    width: 0,
                                    height: 0,
                                    format: rustrtc::media::VideoPixelFormat::Unspecified,
                                    rotation_deg: 0,
                                    is_last_packet: false,
                                    data: bytes::Bytes::copy_from_slice(ps_bytes),
                                    header_extension: None,
                                    csrcs: vec![],
                                    sequence_number: None,
                                    payload_type: Some(actual_video_pt),
                                    source_addr: None,
                                    raw_packet: None,
                                };
                                let _ = v_source.send_video(ps_frame);
                            }
                        }
                    }

                    last_was_param_set = is_param_set;

                    let chunks =
                        super::fragment_payload_for_webrtc(is_hevc, payload_slice, packet.marker);
                    for (chunk_bytes, chunk_marker) in chunks {
                        let v_frame = rustrtc::media::frame::VideoFrame {
                            rtp_timestamp: packet.timestamp,
                            width: 0,
                            height: 0,
                            format: rustrtc::media::VideoPixelFormat::Unspecified,
                            rotation_deg: 0,
                            is_last_packet: chunk_marker,
                            data: chunk_bytes,
                            header_extension: None,
                            csrcs: vec![],
                            sequence_number: None,
                            payload_type: Some(actual_video_pt),
                            source_addr: None,
                            raw_packet: None,
                        };

                        if v_source.send_video(v_frame).is_err() {
                            return;
                        }
                    }
                }
            });
        }

        // 2. Audio forwarder
        if let Some(a_source) = self.audio_source.clone() {
            if super::is_audio_active(answer_sdp) {
                tracing::info!(
                    stream = %stream.name,
                    pt = actual_audio_pt,
                    clock_rate = audio_clock_rate,
                    "Starting rustrtc in-process WebRTC audio forwarder"
                );
                let s_clone = Arc::clone(&stream);
                let mut close_rx = self.close_tx.subscribe();
                tokio::spawn(async move {
                    if let Some(audio_track) = s_clone.get_or_create_opus_track().await {
                        let mut rx = audio_track.subscribe();
                        loop {
                            let pkt = tokio::select! {
                                _ = close_rx.recv() => break,
                                res = rx.recv() => match res {
                                    Ok(pkt) => pkt,
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!("rustrtc audio receiver lagged by {} packets", n);
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                }
                            };

                            let payload = pkt.payload();
                            if payload.is_empty() {
                                continue;
                            }

                            let a_frame = rustrtc::media::frame::AudioFrame {
                                rtp_timestamp: pkt.timestamp,
                                clock_rate: audio_clock_rate,
                                data: bytes::Bytes::copy_from_slice(payload),
                                sequence_number: None,
                                payload_type: Some(actual_audio_pt),
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
}

fn forward_packet_to_webrtc(
    v_source: &SampleStreamSource,
    packet: &crate::core::RtpPacket,
    actual_video_pt: u8,
    is_hevc: bool,
) -> bool {
    let payload_slice = if packet.payload_offset + packet.payload_len <= packet.raw.len() {
        &packet.raw[packet.payload_offset..packet.payload_offset + packet.payload_len]
    } else {
        return true;
    };
    if payload_slice.is_empty() {
        return true;
    }
    let chunks = super::fragment_payload_for_webrtc(is_hevc, payload_slice, packet.marker);
    for (chunk_bytes, chunk_marker) in chunks {
        let v_frame = rustrtc::media::frame::VideoFrame {
            rtp_timestamp: packet.timestamp,
            width: 0,
            height: 0,
            format: rustrtc::media::VideoPixelFormat::Unspecified,
            rotation_deg: 0,
            is_last_packet: chunk_marker,
            data: chunk_bytes,
            header_extension: None,
            csrcs: vec![],
            sequence_number: None,
            payload_type: Some(actual_video_pt),
            source_addr: None,
            raw_packet: None,
        };

        if v_source.send_video(v_frame).is_err() {
            return false;
        }
    }
    true
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
