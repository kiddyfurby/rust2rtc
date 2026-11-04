#![allow(dead_code)]

use crate::backchannel::pcm::{
    g711_8k_to_l16_16k, l16_16k_to_g711_8k,
    G711Repacker,
};
use super::message::{AudioCodecKind, VideoCodecKind};
use crate::core::{MediaKind, RtpPacket};
use crate::ffmpeg::FfmpegManager;
use crate::streams::Stream;
use anyhow::{Context, Result};
use bytes::Bytes;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Active SIP Media Bridge Session for a Call
pub struct SipCallSession {
    pub call_id: String,
    pub local_audio_port: u16,
    pub local_video_port: Option<u16>,
    tasks: Vec<JoinHandle<()>>,
    transcode_child: Arc<Mutex<Option<Child>>>,
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
        local_bind_ip: &str,
        remote_audio_addr: SocketAddr,
        alt_audio_addr: Option<SocketAddr>,
        audio_codec_kind: AudioCodecKind,
        negotiated_audio_pt: u8,
        remote_video_addr: Option<SocketAddr>,
        alt_video_addr: Option<SocketAddr>,
        negotiated_video_codec: VideoCodecKind,
        negotiated_video_pt: u8,
    ) -> Result<Self> {
        let audio_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind local audio RTP socket")?;
        let audio_socket = Arc::new(audio_socket);

        let video_socket = if remote_video_addr.is_some() {
            let v_sock = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("Failed to bind local video RTP socket")?;
            Some(Arc::new(v_sock))
        } else {
            None
        };

        Self::start_with_sockets(
            call_id,
            stream,
            rtsp_port,
            ffmpeg,
            local_bind_ip,
            audio_socket,
            video_socket,
            remote_audio_addr,
            alt_audio_addr,
            audio_codec_kind,
            negotiated_audio_pt,
            remote_video_addr,
            alt_video_addr,
            negotiated_video_codec,
            negotiated_video_pt,
        )
        .await
    }

    /// Start bidirectional RTP audio/video bridge using pre-bound sockets (e.g. for outbound calls)
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_sockets(
        call_id: String,
        stream: Arc<Stream>,
        rtsp_port: u16,
        ffmpeg: Arc<FfmpegManager>,
        _local_bind_ip: &str,
        audio_socket: Arc<UdpSocket>,
        video_socket: Option<Arc<UdpSocket>>,
        remote_audio_addr: SocketAddr,
        alt_audio_addr: Option<SocketAddr>,
        audio_codec_kind: AudioCodecKind,
        negotiated_audio_pt: u8,
        remote_video_addr: Option<SocketAddr>,
        alt_video_addr: Option<SocketAddr>,
        negotiated_video_codec: VideoCodecKind,
        negotiated_video_pt: u8,
    ) -> Result<Self> {
        let local_audio_port = audio_socket.local_addr()?.port();
        let stream_video = Arc::clone(&stream);

        info!(
            call_id = %call_id,
            stream = %stream.name,
            local_audio = local_audio_port,
            remote_audio = %remote_audio_addr,
            alt_audio = ?alt_audio_addr,
            audio_codec = ?audio_codec_kind,
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
                            let payload = rtp.payload();
                            if audio_codec_kind == AudioCodecKind::L16_16k {
                                let g711_bytes = l16_16k_to_g711_8k(payload, false);
                                let _ = stream_bc.send_backchannel_audio(Bytes::from(g711_bytes), true).await;
                            } else {
                                let is_pcmu = rtp.payload_type == 0;
                                let _ = stream_bc.send_backchannel_audio(Bytes::copy_from_slice(payload), is_pcmu).await;
                            }
                        }
                    }
                    Err(e) => {
                        debug!(call_id = %call_id_clone, "Audio RTP receive loop ended: {}", e);
                        break;
                    }
                }
            }
        }));

        // 2. Task: Subscribe to camera audio track and buffer decoded/transcoded audio bytes
        let pcm_queue: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::with_capacity(8192)));
        let pcm_queue_prod = Arc::clone(&pcm_queue);
        let stream_tx = Arc::clone(&stream);
        let call_id_audio = call_id.clone();
        tasks.push(tokio::spawn(async move {
            let mut audio_track = None;
            for _ in 0..30 {
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
                let mut audio_decoder = crate::audio::CameraAudioDecoder::new(&audio_track.codec).ok();

                let mut sub = audio_track.subscribe();
                while let Ok(packet) = sub.recv().await {
                    let payload = packet.payload();
                    if payload.is_empty() {
                        continue;
                    }
                    if audio_codec_kind == AudioCodecKind::L16_16k {
                        let l16_bytes = g711_8k_to_l16_16k(payload, track_is_pcma);
                        let mut q = pcm_queue_prod.lock().await;
                        if q.len() > 12800 {
                            let drop_len = q.len() - 6400;
                            q.drain(..drop_len);
                        }
                        q.extend(l16_bytes);
                    } else if let Some(decoder) = &mut audio_decoder {
                        let target_is_pcmu = negotiated_audio_pt == 0;
                        let g711_chunk = decoder.decode_to_g711(payload, target_is_pcmu);
                        if !g711_chunk.is_empty() {
                            let mut q = pcm_queue_prod.lock().await;
                            // Cap queue at ~400ms (3200 bytes) to keep latency low
                            if q.len() > 3200 {
                                let drop_len = q.len() - 1600;
                                q.drain(..drop_len);
                            }
                            q.extend(g711_chunk);
                        }
                    }
                }
            } else {
                warn!(
                    call_id = %call_id_audio,
                    stream = %stream_tx.name,
                    "No audio track found on stream after 3s; sending comfort silence"
                );
            }
        }));

        // 2b. Task: Paced RTP transmitter to SIP peer (10ms for L16, 20ms for G.711)
        let tx_audio_sock = Arc::clone(&audio_socket);
        let tx_targets = Arc::clone(&audio_targets);
        tasks.push(tokio::spawn(async move {
            let (frame_size, step, ptime_ms) = if audio_codec_kind == AudioCodecKind::L16_16k {
                (320usize, 160u32, 10u64)
            } else {
                (160usize, 160u32, 20u64)
            };
            let mut repacker = G711Repacker::new_with_step(frame_size, step, negotiated_audio_pt, 0x52544301);
            let silence_byte = if audio_codec_kind == AudioCodecKind::L16_16k {
                0x00u8
            } else if negotiated_audio_pt == 8 {
                0xd5u8
            } else {
                0xffu8
            };
            let silence_frame = vec![silence_byte; frame_size];
            let mut ticker = tokio::time::interval(Duration::from_millis(ptime_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;

                let frame: Vec<u8> = {
                    let mut q = pcm_queue.lock().await;
                    if q.len() >= frame_size {
                        q.drain(..frame_size).collect()
                    } else {
                        silence_frame.clone()
                    }
                };

                let packets = repacker.push_payload(&frame);
                let (primary, alt) = *tx_targets.lock().await;
                for pkt in packets {
                    if let Err(e) = tx_audio_sock.send_to(&pkt, primary).await {
                        debug!("Error forwarding audio to primary SIP peer {}: {}", primary, e);
                    }
                    if let Some(alt_addr) = alt
                        && alt_addr != primary {
                            let _ = tx_audio_sock.send_to(&pkt, alt_addr).await;
                        }
                }
            }
        }));

        // 3. Task: Send doorbell camera video (H.264 or RFC 2435 JPEG) -> ESPHome / HA Softphone
        let mut local_video_port = None;
        let transcode_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        if let (Some(remote_video), Some(video_socket)) = (remote_video_addr, video_socket) {
            let v_port = video_socket.local_addr()?.port();
            local_video_port = Some(v_port);

            info!(
                call_id = %call_id,
                stream = %stream.name,
                local_video_port = v_port,
                remote_video = %remote_video,
                alt_video = ?alt_video_addr,
                codec = ?negotiated_video_codec,
                video_pt = negotiated_video_pt,
                "Initializing SIP video forwarder"
            );

            let stream_video = Arc::clone(&stream_video);
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

            match negotiated_video_codec {
                VideoCodecKind::H264 => {
                    let ffmpeg_video = Arc::clone(&ffmpeg);
                    let transcode_child_clone = Arc::clone(&transcode_child);
                    let tx_v_targets_fallback = Arc::clone(&tx_v_targets);
                    let tx_video_sock = Arc::clone(&video_socket);

                    tasks.push(tokio::spawn(async move {
                        let mut video_track = None;
                        for _ in 0..30 {
                            let tracks = stream_video.get_tracks().await;
                            if let Some(t) = tracks.iter().find(|t| t.kind == MediaKind::Video && t.codec.name.eq_ignore_ascii_case("H264")) {
                                video_track = Some(Arc::clone(t));
                                break;
                            }
                            if tracks.iter().any(|t| t.kind == MediaKind::Video) {
                                // Camera already registered its video track and it is not H.264 (e.g. H.265 / HEVC);
                                // break immediately instead of blocking the call for 8s waiting for H.264 to appear
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
                                if is_keyframe && !last_was_param_set
                                    && let Some(ref ps_bytes) = param_set_payload {
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
                                        if let Some(alt_addr) = alt
                                            && alt_addr != primary {
                                                let _ = video_socket.send_to(&stap_rtp, alt_addr).await;
                                            }
                                        pkt_count += 1;
                                        info!(call_id = %call_id_video, "Injected H.264 SPS/PPS STAP-A packet before keyframe into SIP video stream");
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
                                if let Some(alt_addr) = alt
                                    && alt_addr != primary {
                                        let _ = video_socket.send_to(&raw, alt_addr).await;
                                    }
                                pkt_count += 1;
                                if pkt_count == 1 || pkt_count.is_multiple_of(300) {
                                    info!(
                                        call_id = %call_id_video,
                                        packets_sent = pkt_count,
                                        target = %primary,
                                        "Actively streaming video RTP to SIP peer"
                                    );
                                }
                            }
                        } else {
                            let (primary_target, _) = *tx_v_targets_fallback.lock().await;
                            info!(
                                call_id = %call_id_video,
                                stream = %stream_video.name,
                                target = %primary_target,
                                "No native H.264 track found; starting FFmpeg H.264 transcode for SIP video"
                            );

                            let internal_sock = match UdpSocket::bind("127.0.0.1:0").await {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!(call_id = %call_id_video, "Failed to bind loopback socket for FFmpeg RTP transcode: {}", e);
                                    return;
                                }
                            };
                            let internal_port = match internal_sock.local_addr() {
                                Ok(a) => a.port(),
                                Err(e) => {
                                    warn!(call_id = %call_id_video, "Failed to get loopback port: {}", e);
                                    return;
                                }
                            };

                            let rtsp_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_video.name);
                            let h264_args = ffmpeg_video.sip_h264_encoder_args();
                            let mut transcode_args = vec![
                                "-hide_banner".to_string(),
                                "-v".to_string(),
                                "warning".to_string(),
                                "-fflags".to_string(),
                                "nobuffer".to_string(),
                                "-flags".to_string(),
                                "low_delay".to_string(),
                                "-rtsp_transport".to_string(),
                                "tcp".to_string(),
                                "-i".to_string(),
                                rtsp_url,
                                "-an".to_string(),
                            ];
                            transcode_args.extend(h264_args);
                            transcode_args.extend([
                                "-payload_type".to_string(),
                                negotiated_video_pt.to_string(),
                                "-ssrc".to_string(),
                                "1381254914".to_string(),
                                "-f".to_string(),
                                "rtp".to_string(),
                                format!("rtp://127.0.0.1:{}", internal_port),
                            ]);

                            match ffmpeg_video.spawn_stdout_process(&transcode_args) {
                                Ok(c) => {
                                    *transcode_child_clone.lock().await = Some(c);
                                    info!(call_id = %call_id_video, "FFmpeg H.264 transcode process started for SIP call");
                                }
                                Err(e) => {
                                    warn!(call_id = %call_id_video, "Failed to spawn FFmpeg H.264 transcode: {}", e);
                                    return;
                                }
                            }

                            let mut buf = [0u8; 2048];
                            let mut pkt_count = 0u64;
                            loop {
                                match internal_sock.recv_from(&mut buf).await {
                                    Ok((len, _)) => {
                                        let (primary, alt) = *tx_v_targets_fallback.lock().await;
                                        let packet = &buf[..len];
                                        if let Err(e) = tx_video_sock.send_to(packet, primary).await {
                                            debug!("Error forwarding transcoded H.264 video to SIP peer: {}", e);
                                            break;
                                        }
                                        if let Some(alt_addr) = alt
                                            && alt_addr != primary {
                                                let _ = tx_video_sock.send_to(packet, alt_addr).await;
                                            }
                                        pkt_count += 1;
                                        if pkt_count == 1 || pkt_count.is_multiple_of(300) {
                                            info!(
                                                call_id = %call_id_video,
                                                packets_sent = pkt_count,
                                                target = %primary,
                                                "Transcoded H.264 video RTP actively streaming to SIP peer"
                                            );
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    }));
                }
                VideoCodecKind::Jpeg => {
                    let ffmpeg_video = Arc::clone(&ffmpeg);
                    let transcode_child_clone = Arc::clone(&transcode_child);

                    tasks.push(tokio::spawn(async move {
                        // Wait for stream producer to be established
                        let mut video_track = None;
                        for _ in 0..80 {
                            let tracks = stream_video.get_tracks().await;
                            if let Some(t) = tracks.iter().find(|t| t.kind == MediaKind::Video) {
                                video_track = Some(Arc::clone(t));
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }

                        if video_track.is_none() {
                            warn!(
                                call_id = %call_id_video,
                                stream = %stream_video.name,
                                "No video track found on stream after 8s for SIP JPEG transcode"
                            );
                            return;
                        }

                        info!(
                            call_id = %call_id_video,
                            stream = %stream_video.name,
                            remote_video = %remote_video,
                            "Spawning FFmpeg transcode to baseline MJPEG for RFC 2435 SIP video"
                        );

                        let rtsp_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_video.name);
                        let transcode_args = vec![
                            "-hide_banner".to_string(),
                            "-v".to_string(),
                            "warning".to_string(),
                            "-fflags".to_string(),
                            "nobuffer".to_string(),
                            "-flags".to_string(),
                            "low_delay".to_string(),
                            "-rtsp_transport".to_string(),
                            "tcp".to_string(),
                            "-i".to_string(),
                            rtsp_url,
                            "-an".to_string(),
                            "-c:v".to_string(),
                            "mjpeg".to_string(),
                            "-q:v".to_string(),
                            "5".to_string(),
                            "-r".to_string(),
                            "15".to_string(),
                            "-vf".to_string(),
                            "scale='min(640,iw)':-2".to_string(),
                            "-pix_fmt".to_string(),
                            "yuvj420p".to_string(),
                            "-huffman".to_string(),
                            "default".to_string(),
                            "-f".to_string(),
                            "mjpeg".to_string(),
                            "pipe:1".to_string(),
                        ];

                        let mut child = match ffmpeg_video.spawn_stdout_process(&transcode_args) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!(call_id = %call_id_video, "Failed to spawn FFmpeg MJPEG transcoder: {}", e);
                                return;
                            }
                        };

                        let mut stdout = match child.stdout.take() {
                            Some(s) => s,
                            None => {
                                warn!(call_id = %call_id_video, "Failed to capture FFmpeg stdout");
                                return;
                            }
                        };

                        {
                            let mut guard = transcode_child_clone.lock().await;
                            *guard = Some(child);
                        }

                        let mut stream_buf = Vec::with_capacity(65536);
                        let mut chunk = [0u8; 8192];
                        let mut pkt_count = 0u64;
                        let mut out_seq: u16 = (uuid::Uuid::new_v4().as_u128() & 0xFFFF) as u16;
                        let out_ssrc = 0x52544302u32;
                        let base_timestamp: u32 = (uuid::Uuid::new_v4().as_u128() & 0xFFFFFFFF) as u32;
                        let start_time = tokio::time::Instant::now();
                        let mut next_frame_time = tokio::time::Instant::now();

                        loop {
                            let n = match stdout.read(&mut chunk).await {
                                Ok(0) => {
                                    debug!(call_id = %call_id_video, "FFmpeg stdout reached EOF");
                                    break;
                                }
                                Ok(n) => n,
                                Err(e) => {
                                    debug!(call_id = %call_id_video, "Error reading from FFmpeg stdout: {}", e);
                                    break;
                                }
                            };

                            stream_buf.extend_from_slice(&chunk[..n]);

                            // Extract complete frames. If backlog has accumulated, drop stale frames and keep only the latest live frame
                            let mut latest_frame: Option<Vec<u8>> = None;
                            let mut dropped_count = 0;

                            while let Some(soi_pos) = find_marker(&stream_buf, &[0xFF, 0xD8]) {
                                if soi_pos > 0 {
                                    stream_buf.drain(..soi_pos);
                                }
                                if stream_buf.len() < 4 {
                                    break;
                                }
                                if let Some(eoi_rel) = find_marker(&stream_buf[2..], &[0xFF, 0xD9]) {
                                    let eoi_pos = 2 + eoi_rel + 2;
                                    if latest_frame.is_some() {
                                        dropped_count += 1;
                                    }
                                    latest_frame = Some(stream_buf[..eoi_pos].to_vec());
                                    stream_buf.drain(..eoi_pos);
                                } else {
                                    // Incomplete frame, wait for more data
                                    break;
                                }
                            }

                            if dropped_count > 0 {
                                debug!(call_id = %call_id_video, dropped = dropped_count, "Dropped stale buffered JPEG frames to keep zero latency");
                            }

                            if let Some(jpeg_frame) = latest_frame {
                                // Pace transmission to real-time 15 fps (66ms)
                                tokio::time::sleep_until(next_frame_time).await;
                                next_frame_time = tokio::time::Instant::now() + Duration::from_millis(66);

                                let timestamp = base_timestamp.wrapping_add((start_time.elapsed().as_micros() * 9 / 100) as u32);
                                if let Some(packets) = crate::sip::rfc2435::packetize_jpeg_rtp(
                                    &jpeg_frame,
                                    &mut out_seq,
                                    timestamp,
                                    out_ssrc,
                                    1200,
                                ) {
                                    let (primary, alt) = *tx_v_targets.lock().await;
                                    for pkt in packets {
                                        if let Err(e) = video_socket.send_to(&pkt, primary).await {
                                            debug!("Error forwarding RFC 2435 JPEG RTP: {}", e);
                                            break;
                                        }
                                        if let Some(alt_addr) = alt
                                            && alt_addr != primary {
                                                let _ = video_socket.send_to(&pkt, alt_addr).await;
                                            }
                                        pkt_count += 1;
                                    }
                                    if pkt_count == 1 || pkt_count.is_multiple_of(300) {
                                        info!(
                                            call_id = %call_id_video,
                                            packets_sent = pkt_count,
                                            target = %primary,
                                            "Actively streaming RFC 2435 JPEG RTP to SIP peer"
                                        );
                                    }
                                }
                            }

                            if stream_buf.len() > 1_000_000 {
                                stream_buf.clear();
                            }
                        }
                    }));
                }
            }
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
            transcode_child,
            stream,
        })
    }

    /// Stop the call session and release backchannel resources
    pub async fn stop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let mut child = self.transcode_child.lock().await;
        if let Some(mut c) = child.take() {
            let _ = c.kill().await;
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
        if let Ok(mut child) = self.transcode_child.try_lock()
            && let Some(mut c) = child.take() {
                let _ = c.start_kill();
            }
    }
}

fn find_marker(buf: &[u8], marker: &[u8; 2]) -> Option<usize> {
    buf.windows(2).position(|w| w == marker)
}
