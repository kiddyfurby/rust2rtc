#![allow(dead_code)]

pub mod client;
pub use client::RtspClient;

use crate::core::{Codec, MediaKind, RtpPacket, Track};
use crate::ffmpeg::FfmpegManager;
use crate::streams::StreamManager;
use anyhow::{bail, Context, Result};
use bytes::{Buf, Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Clone)]
pub struct RtspRequest {
    pub method: String,
    pub uri: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
}

impl RtspRequest {
    pub fn cseq(&self) -> &str {
        self.headers.get("cseq").map(|s| s.as_str()).unwrap_or("0")
    }
}

enum RtspOutgoing {
    Rtp(u8, Bytes),
    Msg(Vec<u8>),
}

pub struct RtspServer {
    listen_addr: String,
    rtsp_port: u16,
    streams: Arc<StreamManager>,
    ffmpeg: Arc<FfmpegManager>,
}

impl RtspServer {
    pub fn new(
        listen_addr: String,
        rtsp_port: u16,
        streams: Arc<StreamManager>,
        ffmpeg: Arc<FfmpegManager>,
    ) -> Self {
        Self {
            listen_addr,
            rtsp_port,
            streams,
            ffmpeg,
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        let listener = TcpListener::bind(&self.listen_addr)
            .await
            .with_context(|| format!("Failed to bind RTSP on {}", self.listen_addr))?;

        info!("RTSP server listening on {}", self.listen_addr);

        loop {
            match listener.accept().await {
                Ok((stream, remote_addr)) => {
                    let server = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = server.handle_connection(stream, remote_addr).await {
                            trace!(remote = %remote_addr, "RTSP connection closed: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("RTSP accept error: {}", e);
                }
            }
        }
    }

    async fn handle_connection(
        &self,
        mut stream: TcpStream,
        _remote_addr: SocketAddr,
    ) -> Result<()> {
        let mut buffer = BytesMut::with_capacity(8192);
        let session_id = "12345678";
        let mut _active_stream_id: Option<String> = None;
        let mut _is_publisher = false;
        let mut track_channels: HashMap<u8, Arc<Track>> = HashMap::new();

        loop {
            // Check if buffer contains interleaved binary packet ($)
            if !buffer.is_empty() && buffer[0] == b'$' {
                if buffer.len() < 4 {
                    let n = stream.read_buf(&mut buffer).await?;
                    if n == 0 {
                        return Ok(());
                    }
                    continue;
                }

                let _channel = buffer[1];
                let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;

                if buffer.len() < 4 + length {
                    let n = stream.read_buf(&mut buffer).await?;
                    if n == 0 {
                        return Ok(());
                    }
                    continue;
                }

                // Consume packet
                let _dollar = buffer.get_u8();
                let ch = buffer.get_u8();
                let _len = buffer.get_u16();
                let packet_bytes = buffer.split_to(length).freeze();

                // Channel % 2 == 0 is RTP (channel + 1 is RTCP)
                if ch % 2 == 0 {
                    if let Some(track) = track_channels.get(&ch) {
                        if let Some(rtp) = RtpPacket::parse(packet_bytes) {
                            track.send(rtp);
                        } else {
                            trace!(ch = ch, len = length, "RtpPacket parse returned None");
                        }
                    } else {
                        trace!(ch = ch, "No track channel found for ch");
                    }
                }
                continue;
            }

            // Otherwise, read RTSP text request header
            if let Some(pos) = find_crlf_crlf(&buffer) {
                let header_bytes = buffer.split_to(pos + 4);
                let (method, uri, path, headers) = parse_rtsp_headers(&header_bytes)?;

                let content_length = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);

                while buffer.len() < content_length {
                    let n = stream.read_buf(&mut buffer).await?;
                    if n == 0 {
                        return Ok(());
                    }
                }

                let body = buffer.split_to(content_length).freeze();
                let req = RtspRequest {
                    method: method.clone(),
                    uri,
                    path: path.clone(),
                    headers,
                    body,
                };

                let stream_id = extract_stream_name(&path);
                _active_stream_id = Some(stream_id.clone());

                match method.as_str() {
                    "OPTIONS" => {
                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Public: OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, RECORD, TEARDOWN, GET_PARAMETER\r\n\r\n",
                            req.cseq()
                        );
                        stream.write_all(response.as_bytes()).await?;
                    }
                    "GET_PARAMETER" => {
                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Session: {}\r\n\r\n",
                            req.cseq(),
                            session_id
                        );
                        stream.write_all(response.as_bytes()).await?;
                    }
                    "ANNOUNCE" => {
                        _is_publisher = true;
                        info!(stream = %stream_id, "RTSP publisher announced (FFmpeg/producer)");

                        // Parse SDP from announce body
                        let sdp_str = String::from_utf8_lossy(&req.body);
                        debug!(sdp = %sdp_str, "Received SDP on ANNOUNCE");
                        let tracks = parse_sdp_tracks(&sdp_str);

                        let stream_obj = self.streams.get_or_create(&stream_id).await;
                        stream_obj.clear_tracks().await;

                        for (idx, track) in tracks.into_iter().enumerate() {
                            let ch = (idx * 2) as u8;
                            track_channels.insert(ch, Arc::clone(&track));
                            stream_obj.add_track(track).await;
                        }

                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\r\n",
                            req.cseq()
                        );
                        stream.write_all(response.as_bytes()).await?;
                    }
                    "DESCRIBE" => {
                        // RTSP Consumer asking for stream SDP
                        let stream_obj = self.streams.get_or_create(&stream_id).await;

                        // Ensure on-demand producer is started
                        if let Err(e) = stream_obj.ensure_started(self.rtsp_port, &self.ffmpeg).await {
                            warn!(stream = %stream_id, error = %e, "Failed to start producer on DESCRIBE");
                        }

                        // Wait for tracks to be announced if stream is starting up
                        let start = tokio::time::Instant::now();
                        while stream_obj.get_tracks().await.is_empty()
                            && start.elapsed() < tokio::time::Duration::from_secs(6)
                        {
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        }

                        let sdp = stream_obj.generate_sdp(&self.listen_addr).await;

                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Content-Type: application/sdp\r\n\
                            Content-Length: {}\r\n\r\n\
                            {}",
                            req.cseq(),
                            sdp.len(),
                            sdp
                        );
                        stream.write_all(response.as_bytes()).await?;
                    }
                    "SETUP" => {
                        let transport = req
                            .headers
                            .get("transport")
                            .cloned()
                            .unwrap_or_else(|| "RTP/AVP/TCP;unicast;interleaved=0-1".to_string());

                        // If client requested UDP transport without TCP / interleaved, reject with 461 Unsupported Transport
                        // so clients like ffmpeg/ffprobe immediately switch to TCP interleaved transport without timing out
                        let is_tcp = transport.to_uppercase().contains("TCP") || transport.contains("interleaved");
                        if !is_tcp {
                            let response = format!(
                                "RTSP/1.0 461 Unsupported Transport\r\n\
                                CSeq: {}\r\n\
                                Server: rust2rtc\r\n\
                                Session: {}\r\n\r\n",
                                req.cseq(),
                                session_id
                            );
                            stream.write_all(response.as_bytes()).await?;
                            continue;
                        }

                        let stream_obj = self.streams.get_or_create(&stream_id).await;
                        let tracks = stream_obj.get_tracks().await;

                        let track_idx = req
                            .path
                            .rsplit('/')
                            .next()
                            .and_then(|seg| seg.strip_prefix("track"))
                            .and_then(|idx_str| idx_str.parse::<usize>().ok());

                        let mut resp_transport = transport.clone();
                        let mut assigned_ch = None;

                        // Map requested interleaved channels to track
                        if let Some(pos) = transport.find("interleaved=") {
                            let after = &transport[pos + 12..];
                            let chan_str = after.split(['-', ';', ',']).next().unwrap_or("0");
                            if let Ok(ch) = chan_str.parse::<u8>() {
                                assigned_ch = Some(ch);
                            }
                        } else {
                            // Assign interleaved channel if client asked for TCP without explicit channel numbers
                            let next_ch = (track_channels.len() * 2) as u8;
                            resp_transport.push_str(&format!(";interleaved={}-{}", next_ch, next_ch + 1));
                            assigned_ch = Some(next_ch);
                        }

                        if let Some(ch) = assigned_ch {
                            let selected_track = if let Some(idx) = track_idx {
                                tracks.get(idx).cloned()
                            } else {
                                tracks.get((ch / 2) as usize).cloned()
                            };

                            if let Some(track) = selected_track {
                                track_channels.insert(ch, track);
                            }
                        }

                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Session: {}\r\n\
                            Transport: {}\r\n\r\n",
                            req.cseq(),
                            session_id,
                            resp_transport
                        );
                        stream.write_all(response.as_bytes()).await?;
                    }
                    "RECORD" => {
                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Session: {}\r\n\r\n",
                            req.cseq(),
                            session_id
                        );
                        stream.write_all(response.as_bytes()).await?;
                        info!(stream = %stream_id, "RTSP publisher recording started");
                    }
                    "PLAY" => {
                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\
                            Session: {}\r\n\
                            Range: npt=0.000-\r\n\r\n",
                            req.cseq(),
                            session_id
                        );
                        stream.write_all(response.as_bytes()).await?;

                        if !track_channels.is_empty() {
                            let (tx, mut rx) = mpsc::channel::<RtspOutgoing>(1024);
                            let (mut reader, mut writer) = stream.into_split();

                            // Spawn subscriber per mapped channel
                            for (&ch, track) in &track_channels {
                                let mut track_rx = track.subscribe();
                                let tx_clone = tx.clone();
                                tokio::spawn(async move {
                                    while let Ok(packet) = track_rx.recv().await {
                                        if tx_clone.send(RtspOutgoing::Rtp(ch, packet.raw)).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                            }

                            // Writer task
                            tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    match msg {
                                        RtspOutgoing::Rtp(ch, raw) => {
                                            let mut header = [0u8; 4];
                                            header[0] = b'$';
                                            header[1] = ch;
                                            let len = raw.len() as u16;
                                            header[2..4].copy_from_slice(&len.to_be_bytes());

                                            if writer.write_all(&header).await.is_err() {
                                                break;
                                            }
                                            if writer.write_all(&raw).await.is_err() {
                                                break;
                                            }
                                        }
                                        RtspOutgoing::Msg(data) => {
                                            if writer.write_all(&data).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            });

                            // Reader loop for keepalive & teardown
                            let tx_resp = tx.clone();
                            let mut read_buf = BytesMut::with_capacity(4096);
                            loop {
                                if let Some(pos) = find_crlf_crlf(&read_buf) {
                                    let header_bytes = read_buf.split_to(pos + 4);
                                    if let Ok((cmd, _, _, hdrs)) = parse_rtsp_headers(&header_bytes) {
                                        let cseq = hdrs.get("cseq").cloned().unwrap_or_else(|| "0".to_string());
                                        let content_len = hdrs
                                            .get("content-length")
                                            .and_then(|v| v.parse::<usize>().ok())
                                            .unwrap_or(0);
                                        while read_buf.len() < content_len {
                                            if reader.read_buf(&mut read_buf).await.unwrap_or(0) == 0 {
                                                return Ok(());
                                            }
                                        }
                                        let _ = read_buf.split_to(content_len);

                                        match cmd.as_str() {
                                            "GET_PARAMETER" => {
                                                let resp = format!(
                                                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\n\r\n",
                                                    cseq, session_id
                                                );
                                                let _ = tx_resp.send(RtspOutgoing::Msg(resp.into_bytes())).await;
                                            }
                                            "OPTIONS" => {
                                                let resp = format!(
                                                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER\r\n\r\n",
                                                    cseq
                                                );
                                                let _ = tx_resp.send(RtspOutgoing::Msg(resp.into_bytes())).await;
                                            }
                                            "TEARDOWN" => {
                                                let resp = format!(
                                                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n",
                                                    cseq
                                                );
                                                let _ = tx_resp.send(RtspOutgoing::Msg(resp.into_bytes())).await;
                                                return Ok(());
                                            }
                                            _ => {}
                                        }
                                    }
                                } else {
                                    let n = reader.read_buf(&mut read_buf).await?;
                                    if n == 0 {
                                        break;
                                    }
                                }
                            }
                            return Ok(());
                        }
                    }
                    "TEARDOWN" => {
                        let response = format!(
                            "RTSP/1.0 200 OK\r\n\
                            CSeq: {}\r\n\
                            Server: rust2rtc\r\n\r\n",
                            req.cseq()
                        );
                        stream.write_all(response.as_bytes()).await?;
                        return Ok(());
                    }
                    _ => {
                        warn!("Unhandled RTSP method: {}", method);
                    }
                }
            } else {
                let n = stream.read_buf(&mut buffer).await?;
                if n == 0 {
                    break;
                }
            }
        }

        Ok(())
    }
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

fn parse_rtsp_headers(raw: &[u8]) -> Result<(String, String, String, HashMap<String, String>)> {
    let text = std::str::from_utf8(raw)?;
    let mut lines = text.lines();

    let req_line = lines.next().context("Empty request line")?;
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        bail!("Malformed RTSP request line: {}", req_line);
    }

    let method = parts[0].to_uppercase();
    let uri = parts[1].to_string();

    let path = if let Ok(parsed_url) = url::Url::parse(&uri) {
        parsed_url.path().to_string()
    } else if let Some(idx) = uri.find("://") {
        let after_scheme = &uri[idx + 3..];
        if let Some(slash_idx) = after_scheme.find('/') {
            after_scheme[slash_idx..].to_string()
        } else {
            "/".to_string()
        }
    } else {
        uri.clone()
    };

    let mut headers = HashMap::new();
    for line in lines {
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    Ok((method, uri, path, headers))
}

fn extract_stream_name(path: &str) -> String {
    let clean = path.trim_matches('/');
    if let Some((first, _)) = clean.split_once('/') {
        first.to_string()
    } else {
        clean.to_string()
    }
}

#[derive(Debug, Clone)]
struct TrackDraft {
    kind: MediaKind,
    pt: u8,
    name: String,
    clock_rate: u32,
    channels: Option<u8>,
    fmtp: Option<String>,
}

impl TrackDraft {
    fn new(kind: MediaKind, pt: u8) -> Self {
        let (name, clock_rate, channels) = match (kind, pt) {
            (MediaKind::Audio, 0) => ("PCMU".to_string(), 8000, Some(1)),
            (MediaKind::Audio, 8) => ("PCMA".to_string(), 8000, Some(1)),
            (MediaKind::Audio, 111) => ("OPUS".to_string(), 48000, Some(2)),
            (MediaKind::Audio, _) => ("PCMA".to_string(), 8000, Some(1)),
            (MediaKind::Video, 26) => ("JPEG".to_string(), 90000, None),
            (MediaKind::Video, 96) => ("H264".to_string(), 90000, None),
            (MediaKind::Video, _) => ("H264".to_string(), 90000, None),
        };

        Self {
            kind,
            pt,
            name,
            clock_rate,
            channels,
            fmtp: None,
        }
    }

    fn to_track(&self) -> Arc<Track> {
        let codec = Codec {
            name: self.name.to_uppercase(),
            clock_rate: self.clock_rate,
            channels: self.channels,
            payload_type: self.pt,
            fmtp: self.fmtp.clone(),
        };
        let cap = if self.kind == MediaKind::Video { 2048 } else { 256 };
        Arc::new(Track::new(codec, self.kind, cap))
    }
}

/// Simple parser for SDP payload types and tracks
fn parse_sdp_tracks(sdp: &str) -> Vec<Arc<Track>> {
    let mut tracks = Vec::new();
    let mut current_draft: Option<TrackDraft> = None;

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(m) = line.strip_prefix("m=") {
            if let Some(draft) = current_draft.take() {
                tracks.push(draft.to_track());
            }

            let parts: Vec<&str> = m.split_whitespace().collect();
            if !parts.is_empty() {
                let kind = match parts[0] {
                    "audio" => MediaKind::Audio,
                    _ => MediaKind::Video,
                };
                let pt = if parts.len() >= 4 {
                    parts[3].parse::<u8>().unwrap_or(96)
                } else {
                    96
                };
                current_draft = Some(TrackDraft::new(kind, pt));
            }
        } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            // e.g. a=rtpmap:96 H264/90000
            let parts: Vec<&str> = rtpmap.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(pt) = parts[0].parse::<u8>() {
                    let (name, rate_chan) = parts[1].split_once('/').unwrap_or((parts[1], "90000"));
                    let rate = rate_chan.split('/').next().unwrap_or("90000").parse::<u32>().unwrap_or(90000);
                    let chan = rate_chan.split('/').nth(1).and_then(|c| c.parse::<u8>().ok());

                    if let Some(ref mut draft) = current_draft {
                        if draft.pt == pt || draft.name.is_empty() {
                            draft.pt = pt;
                            draft.name = name.to_uppercase();
                            draft.clock_rate = rate;
                            if draft.kind == MediaKind::Audio {
                                draft.channels = Some(chan.unwrap_or(1));
                            }
                        }
                    }
                }
            }
        } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
            // e.g. a=fmtp:96 sprop-vps=...; sprop-sps=...
            if let Some((pt_str, params)) = fmtp.split_once(' ') {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    if let Some(ref mut draft) = current_draft {
                        if draft.pt == pt {
                            draft.fmtp = Some(params.trim().to_string());
                        }
                    }
                }
            }
        }
    }

    if let Some(draft) = current_draft.take() {
        tracks.push(draft.to_track());
    }

    if tracks.is_empty() {
        // Fallback default H264 track
        tracks.push(Arc::new(Track::new(Codec::new_h264(), MediaKind::Video, 2048)));
    }

    tracks
}
