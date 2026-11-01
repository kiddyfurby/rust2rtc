#![allow(dead_code)]

use crate::backchannel::pcm::{
    transcode_alaw_to_ulaw, transcode_ulaw_to_alaw, G711Repacker,
};
use crate::core::{MediaKind, RtpPacket};
use crate::ffmpeg::FfmpegManager;
use crate::streams::Stream;
use anyhow::{Context, Result};
use bytes::Bytes;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Active SIP Media Bridge Session for a Call
pub struct SipCallSession {
    pub call_id: String,
    pub local_audio_port: u16,
    pub local_video_port: Option<u16>,
    tasks: Vec<JoinHandle<()>>,
    stream: Arc<Stream>,
}

impl SipCallSession {
    /// Start bidirectional RTP audio/video bridge between SIP peer and Stream
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        call_id: String,
        stream: Arc<Stream>,
        rtsp_port: u16,
        ffmpeg: Arc<FfmpegManager>,
        _local_bind_ip: &str,
        remote_audio_addr: SocketAddr,
        alt_audio_addr: Option<SocketAddr>,
        negotiated_audio_pt: u8,
        remote_video_addr: Option<SocketAddr>,
        alt_video_addr: Option<SocketAddr>,
        negotiated_video_pt: u8,
    ) -> Result<Self> {
        // Bind local UDP socket on 0.0.0.0 so kernel routes across any interface/VLAN
        let audio_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind local audio RTP socket")?;
        let local_audio_port = audio_socket.local_addr()?.port();
        let audio_socket = Arc::new(audio_socket);

        info!(
            call_id = %call_id,
            stream = %stream.name,
            local_audio = local_audio_port,
            remote_audio = %remote_audio_addr,
            alt_audio = ?alt_audio_addr,
            audio_pt = negotiated_audio_pt,
            remote_video = ?remote_video_addr,
            alt_video = ?alt_video_addr,
            video_pt = negotiated_video_pt,
            "Starting SIP call media bridge"
        );

        let mut tasks = Vec::new();
        let audio_targets = Arc::new(Mutex::new((remote_audio_addr, alt_audio_addr)));

        // Ensure stream producer and camera backchannel start in the background without blocking 200 OK
        let stream_init = Arc::clone(&stream);
        let ffmpeg_init = Arc::clone(&ffmpeg);
        tasks.push(tokio::spawn(async move {
            if let Err(e) = stream_init.ensure_started(rtsp_port, &ffmpeg_init).await {
                warn!(stream = %stream_init.name, "Failed to start stream producer for SIP call: {}", e);
            }
        }));

        if stream.has_backchannel() {
            let stream_bc_init = Arc::clone(&stream);
            tasks.push(tokio::spawn(async move {
                if let Err(e) = stream_bc_init.start_backchannel().await {
                    warn!(stream = %stream_bc_init.name, "Could not start backchannel for stream: {}", e);
                }
            }));
        }

        // 1. Task: Receive audio from ESPHome/SIP peer -> Feed to camera backchannel & update Symmetric RTP target
        let stream_bc = Arc::clone(&stream);
        let rx_audio_sock = Arc::clone(&audio_socket);
        let rx_targets = Arc::clone(&audio_targets);
        let call_id_clone = call_id.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut locked_symmetric = false;
            loop {
                match rx_audio_sock.recv_from(&mut buf).await {
                    Ok((len, src_addr)) => {
                        let packet_bytes = Bytes::copy_from_slice(&buf[..len]);
                        if let Some(rtp) = RtpPacket::parse(packet_bytes) {
                            if !locked_symmetric {
                                let mut t = rx_targets.lock().await;
                                if t.0 != src_addr || t.1.is_some() {
                                    info!(
                                        call_id = %call_id_clone,
                                        peer_rtp = %src_addr,
                                        "Symmetric RTP locked onto peer audio address"
                                    );
                                    *t = (src_addr, None);
                                }
                                locked_symmetric = true;
                            }
                            let is_pcmu = rtp.payload_type == 0;
                            let payload = Bytes::copy_from_slice(rtp.payload());
                            let _ = stream_bc.send_backchannel_audio(payload, is_pcmu).await;
                        }
                    }
                    Err(e) => {
                        debug!(call_id = %call_id_clone, "Audio RTP receive loop ended: {}", e);
                        break;
                    }
                }
            }
        }));

        // 2. Task: Subscribe to camera audio track and buffer decoded/transcoded G.711 bytes
        let pcm_queue: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::with_capacity(4096)));
        let pcm_queue_prod = Arc::clone(&pcm_queue);
        let stream_tx = Arc::clone(&stream);
        let call_id_audio = call_id.clone();
        tasks.push(tokio::spawn(async move {
            let mut audio_track = None;
            for _ in 0..80 {
                let tracks = stream_tx.get_tracks().await;
                if let Some(t) = tracks.iter().find(|t| t.kind == MediaKind::Audio) {
                    audio_track = Some(Arc::clone(t));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if let Some(audio_track) = audio_track {
                info!(
                    call_id = %call_id_audio,
                    stream = %stream_tx.name,
                    codec = %audio_track.codec.name,
                    pt = audio_track.codec.payload_type,
                    "Attached camera audio track to SIP call"
                );
                let track_is_pcma = audio_track.codec.payload_type == 8
                    || audio_track.codec.name.eq_ignore_ascii_case("PCMA");
                let track_is_pcmu = audio_track.codec.payload_type == 0
                    || audio_track.codec.name.eq_ignore_ascii_case("PCMU");

                let mut sub = audio_track.subscribe();
                while let Ok(packet) = sub.recv().await {
                    let payload = packet.payload();
                    if payload.is_empty() {
                        continue;
                    }
                    let mut chunk = payload.to_vec();
                    if track_is_pcma && negotiated_audio_pt == 0 {
                        transcode_alaw_to_ulaw(&mut chunk);
                    } else if track_is_pcmu && negotiated_audio_pt == 8 {
                        transcode_ulaw_to_alaw(&mut chunk);
                    } else if !track_is_pcma && !track_is_pcmu {
                        // Non-G.711 track (e.g., AAC/Opus if not transcoded): skip raw payload
                        continue;
                    }

                    let mut q = pcm_queue_prod.lock().await;
                    // Cap queue at ~400ms (3200 bytes) to keep latency low
                    if q.len() > 3200 {
                        let drop_len = q.len() - 1600;
                        q.drain(..drop_len);
                    }
                    q.extend(chunk);
                }
            } else {
                warn!(
                    call_id = %call_id_audio,
                    stream = %stream_tx.name,
                    "No audio track found on stream after 8s; sending comfort silence"
                );
            }
        }));

        // 2b. Task: Paced 20ms (160-byte) G.711 RTP transmitter to SIP peer
        let tx_audio_sock = Arc::clone(&audio_socket);
        let tx_targets = Arc::clone(&audio_targets);
        tasks.push(tokio::spawn(async move {
            let mut repacker = G711Repacker::new(160, negotiated_audio_pt, 0x52544301);
            let silence_byte = if negotiated_audio_pt == 8 { 0xd5u8 } else { 0xffu8 };
            let silence_frame = [silence_byte; 160];
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;

                let frame: Vec<u8> = {
                    let mut q = pcm_queue.lock().await;
                    if q.len() >= 160 {
                        q.drain(..160).collect()
                    } else {
                        silence_frame.to_vec()
                    }
                };

                let packets = repacker.push_payload(&frame);
                let (primary, alt) = *tx_targets.lock().await;
                for pkt in packets {
                    if let Err(e) = tx_audio_sock.send_to(&pkt, primary).await {
                        debug!("Error forwarding audio to primary SIP peer {}: {}", primary, e);
                    }
                    if let Some(alt_addr) = alt {
                        if alt_addr != primary {
                            let _ = tx_audio_sock.send_to(&pkt, alt_addr).await;
                        }
                    }
                }
            }
        }));

        // 3. Task: Send doorbell camera video (H.264) -> ESPHome / HA Softphone
        let mut local_video_port = None;
        if let Some(remote_video) = remote_video_addr {
            let video_socket = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("Failed to bind local video RTP socket")?;
            let v_port = video_socket.local_addr()?.port();
            local_video_port = Some(v_port);
            let video_socket = Arc::new(video_socket);

            info!(
                call_id = %call_id,
                stream = %stream.name,
                local_video_port = v_port,
                remote_video = %remote_video,
                alt_video = ?alt_video_addr,
                video_pt = negotiated_video_pt,
                "Initializing SIP video forwarder"
            );

            let stream_video = Arc::clone(&stream);
            let call_id_video = call_id.clone();
            let video_targets = Arc::new(Mutex::new((remote_video, alt_video_addr)));
            let tx_v_targets = Arc::clone(&video_targets);
            let rx_v_targets = Arc::clone(&video_targets);

            // Listen for peer RTCP / video packets on video_socket to lock symmetric RTP
            let rx_video_sock = Arc::clone(&video_socket);
            let call_id_vrx = call_id.clone();
            tasks.push(tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let mut locked_symmetric = false;
                loop {
                    match rx_video_sock.recv_from(&mut buf).await {
                        Ok((_, src_addr)) => {
                            if !locked_symmetric {
                                let mut t = rx_v_targets.lock().await;
                                if t.0 != src_addr || t.1.is_some() {
                                    info!(
                                        call_id = %call_id_vrx,
                                        peer_video_rtp = %src_addr,
                                        "Symmetric RTP locked onto peer video address"
                                    );
                                    *t = (src_addr, None);
                                }
                                locked_symmetric = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }));

            tasks.push(tokio::spawn(async move {
                let mut video_track = None;
                for _ in 0..80 {
                    let tracks = stream_video.get_tracks().await;
                    if let Some(t) = tracks.iter().find(|t| t.kind == MediaKind::Video) {
                        video_track = Some(Arc::clone(t));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if let Some(video_track) = video_track {
                    info!(
                        call_id = %call_id_video,
                        stream = %stream_video.name,
                        codec = %video_track.codec.name,
                        source_pt = video_track.codec.payload_type,
                        negotiated_pt = negotiated_video_pt,
                        remote_video = %remote_video,
                        "Attached camera video track (H.264) to SIP call - streaming RTP"
                    );

                    let mut param_set_payload = video_track
                        .codec
                        .fmtp
                        .as_ref()
                        .and_then(|f| crate::webrtc::extract_h264_sps_pps(f))
                        .map(|(sps, pps)| crate::webrtc::build_h264_stap_a(&sps, &pps));

                    let mut sub = video_track.subscribe();
                    let mut pkt_count = 0u64;
                    let out_ssrc = 0x52544302u32;
                    let mut out_seq: u16 = (uuid::Uuid::new_v4().as_u128() & 0xFFFF) as u16;
                    let mut sent_first_keyframe = false;
                    let mut last_was_param_set = false;

                    loop {
                        let packet = match sub.recv().await {
                            Ok(p) => p,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                debug!("Video forwarder lagged by {} packets - resuming", n);
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        };

                        let payload_slice = if packet.payload_offset + packet.payload_len <= packet.raw.len() {
                            &packet.raw[packet.payload_offset..packet.payload_offset + packet.payload_len]
                        } else {
                            continue;
                        };

                        let (is_keyframe, is_param_set) = crate::webrtc::is_keyframe_or_param_set(false, payload_slice);

                        // Dynamically cache STAP-A parameter sets if seen in stream
                        if is_param_set && !payload_slice.is_empty() && (payload_slice[0] & 0x1F) == 24 {
                            param_set_payload = Some(payload_slice.to_vec());
                        }

                        // Do not forward P-frames before we have sent the initial keyframe / SPS/PPS
                        if !sent_first_keyframe {
                            if !is_keyframe && !is_param_set {
                                continue;
                            }
                            sent_first_keyframe = true;
                        }

                        // Before keyframes, inject the parameter sets packet (STAP-A) if not immediately preceded by parameter sets
                        if is_keyframe && !last_was_param_set {
                            if let Some(ref ps_bytes) = param_set_payload {
                                let mut stap_rtp = Vec::with_capacity(12 + ps_bytes.len());
                                stap_rtp.push(0x80); // V=2
                                stap_rtp.push(negotiated_video_pt & 0x7F); // M=0 (parameter sets packet)
                                stap_rtp.extend_from_slice(&out_seq.to_be_bytes());
                                out_seq = out_seq.wrapping_add(1);
                                stap_rtp.extend_from_slice(&packet.timestamp.to_be_bytes());
                                stap_rtp.extend_from_slice(&out_ssrc.to_be_bytes());
                                stap_rtp.extend_from_slice(ps_bytes);

                                let (primary, alt) = *tx_v_targets.lock().await;
                                let _ = video_socket.send_to(&stap_rtp, primary).await;
                                if let Some(alt_addr) = alt {
                                    if alt_addr != primary {
                                        let _ = video_socket.send_to(&stap_rtp, alt_addr).await;
                                    }
                                }
                                pkt_count += 1;
                                info!(call_id = %call_id_video, "Injected H.264 SPS/PPS STAP-A packet before keyframe into SIP video stream");
                            }
                        }

                        last_was_param_set = is_param_set;

                        let mut raw = packet.raw.to_vec();
                        if raw.len() >= 12 {
                            if negotiated_video_pt != 0 {
                                raw[1] = (raw[1] & 0x80) | (negotiated_video_pt & 0x7F);
                            }
                            // Strictly monotonic sequence numbers
                            raw[2..4].copy_from_slice(&out_seq.to_be_bytes());
                            out_seq = out_seq.wrapping_add(1);
                            // Strictly consistent SSRC matching STAP-A
                            raw[8..12].copy_from_slice(&out_ssrc.to_be_bytes());
                        }

                        let (primary, alt) = *tx_v_targets.lock().await;
                        if let Err(e) = video_socket.send_to(&raw, primary).await {
                            debug!("Error forwarding video to SIP peer: {}", e);
                            break;
                        }
                        if let Some(alt_addr) = alt {
                            if alt_addr != primary {
                                let _ = video_socket.send_to(&raw, alt_addr).await;
                            }
                        }
                        pkt_count += 1;
                        if pkt_count == 1 || pkt_count % 300 == 0 {
                            info!(
                                call_id = %call_id_video,
                                packets_sent = pkt_count,
                                target = %primary,
                                "Actively streaming video RTP to SIP peer"
                            );
                        }
                    }
                } else {
                    warn!(
                        call_id = %call_id_video,
                        stream = %stream_video.name,
                        "No video track found on stream after 8s for SIP call"
                    );
                }
            }));
        } else {
            info!(
                call_id = %call_id,
                stream = %stream.name,
                "No remote video port negotiated - audio-only SIP call"
            );
        }

        Ok(Self {
            call_id,
            local_audio_port,
            local_video_port,
            tasks,
            stream,
        })
    }

    /// Stop the call session and release backchannel resources
    pub async fn stop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.stream.stop_backchannel().await;
        info!(call_id = %self.call_id, "SIP call media bridge stopped");
    }
}

impl Drop for SipCallSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
