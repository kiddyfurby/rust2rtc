use crate::core::{Codec, MediaKind, RtpPacket, Track};
use crate::streams::Stream;
use anyhow::{bail, Context, Result};
use base64::prelude::*;
use bytes::{Buf, BytesMut};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::{info, trace, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct SdpTrackInfo {
    pub track: Arc<Track>,
    pub control: String,
    pub kind: MediaKind,
    pub pt: u8,
}

#[derive(Debug, Clone)]
enum AuthMethod {
    Basic,
    Digest {
        realm: String,
        nonce: String,
        qop: Option<String>,
        algorithm: Option<String>,
    },
}

pub struct RtspClient;

impl RtspClient {
    /// Start the in-process RTSP client loop for a stream, reconnecting automatically until cancelled.
    pub async fn run(
        url: String,
        stream: Arc<Stream>,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let stream_name = stream.name.clone();
        info!(stream = %stream_name, url = %url, "Starting in-process native RTSP client");

        loop {
            // Check if cancelled before reconnecting
            if cancel_rx.try_recv().is_ok() {
                info!(stream = %stream_name, "In-process RTSP client cancelled");
                break;
            }

            match Self::connect_and_stream(&url, &stream, &mut cancel_rx).await {
                Ok(true) => {
                    info!(stream = %stream_name, "In-process RTSP client shut down normally (cancelled)");
                    break;
                }
                Ok(false) => {
                    warn!(stream = %stream_name, "RTSP connection closed by camera. Reconnecting in 2s...");
                }
                Err(e) => {
                    warn!(stream = %stream_name, "RTSP client error: {}. Reconnecting in 2s...", e);
                }
            }

            tokio::select! {
                _ = &mut cancel_rx => {
                    info!(stream = %stream_name, "In-process RTSP client cancelled during reconnect backoff");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    /// Connect to RTSP source, perform handshake, and process interleaved RTP frames.
    /// Returns Ok(true) if cancelled by caller, Ok(false) if closed by remote server.
    async fn connect_and_stream(
        url_str: &str,
        stream: &Arc<Stream>,
        cancel_rx: &mut oneshot::Receiver<()>,
    ) -> Result<bool> {
        let parsed_url = Url::parse(url_str).context("Invalid RTSP URL")?;
        let host = parsed_url.host_str().context("Missing host in RTSP URL")?;
        let port = parsed_url.port().unwrap_or(554);
        let username = parsed_url.username().to_string();
        let password = parsed_url.password().unwrap_or("").to_string();

        let clean_path = parsed_url.path();
        let clean_url = if let Some(query) = parsed_url.query() {
            format!("rtsp://{}:{}{}?{}", host, port, clean_path, query)
        } else {
            format!("rtsp://{}:{}{}", host, port, clean_path)
        };

        let target_addr = format!("{}:{}", host, port);
        trace!(addr = %target_addr, "Connecting to camera RTSP server");

        let mut tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&target_addr))
            .await
            .context("RTSP connection timed out")?
            .with_context(|| format!("Failed to connect to RTSP server at {}", target_addr))?;

        let _ = tcp.set_nodelay(true);

        let mut cseq = 1u32;
        let mut auth_info: Option<AuthMethod> = None;
        let mut session_id: Option<String> = None;
        let mut content_base = clean_url.clone();
        let mut buffer = BytesMut::with_capacity(65536);

        // 1. Send OPTIONS
        let (_, _opt_headers, opt_code) = Self::send_request_with_auth(
            &mut tcp,
            &mut buffer,
            "OPTIONS",
            &clean_url,
            &mut cseq,
            &username,
            &password,
            &mut auth_info,
            None,
            None,
            None,
            None,
        ).await?;

        if opt_code == 401 {
            bail!("RTSP OPTIONS authentication failed for {}", clean_url);
        }

        // 2. Send DESCRIBE
        let (sdp_body, desc_headers, desc_code) = Self::send_request_with_auth(
            &mut tcp,
            &mut buffer,
            "DESCRIBE",
            &clean_url,
            &mut cseq,
            &username,
            &password,
            &mut auth_info,
            session_id.as_deref(),
            Some(&[("Accept", "application/sdp")]),
            None,
            None,
        ).await?;

        if desc_code != 200 {
            bail!("RTSP DESCRIBE returned HTTP status {}: body={}", desc_code, String::from_utf8_lossy(&sdp_body));
        }

        if let Some(cb) = desc_headers.get("content-base") {
            content_base = cb.trim().to_string();
        }

        let sdp_str = String::from_utf8_lossy(&sdp_body);
        let existing_tracks = stream.get_tracks().await;
        let sdp_tracks = parse_sdp_tracks_with_control(&sdp_str, &content_base, &existing_tracks);
        if sdp_tracks.is_empty() {
            bail!("No supported media tracks found in camera SDP: {}", sdp_str);
        }

        // 3. Perform SETUP for each track
        let mut channel_map: HashMap<u8, Arc<Track>> = HashMap::new();
        let mut registered_tracks: Vec<Arc<Track>> = Vec::new();

        let mut next_ch = 0u8;
        for t_info in sdp_tracks {
            let rtp_ch = next_ch;
            let rtcp_ch = next_ch + 1;
            next_ch += 2;

            let transport_header = format!("RTP/AVP/TCP;unicast;interleaved={}-{}", rtp_ch, rtcp_ch);
            let (_, setup_headers, setup_code) = Self::send_request_with_auth(
                &mut tcp,
                &mut buffer,
                "SETUP",
                &t_info.control,
                &mut cseq,
                &username,
                &password,
                &mut auth_info,
                session_id.as_deref(),
                Some(&[("Transport", &transport_header)]),
                None,
                None,
            ).await?;

            if setup_code != 200 {
                warn!(
                    track = %t_info.control,
                    code = setup_code,
                    "RTSP SETUP failed for track, skipping"
                );
                continue;
            }

            if session_id.is_none()
                && let Some(sess) = setup_headers.get("session") {
                    let id = sess.split(';').next().unwrap_or(sess).trim().to_string();
                    session_id = Some(id);
                }

            channel_map.insert(rtp_ch, Arc::clone(&t_info.track));
            registered_tracks.push(Arc::clone(&t_info.track));
        }

        if registered_tracks.is_empty() {
            bail!("Failed to SETUP any media tracks on camera {}", clean_url);
        }

        // Drop any GOP cached from a previous session so subscribers never get
        // a replay mixing old-session and new-session RTP timelines.
        for t in &registered_tracks {
            t.clear_gop_cache();
        }

        // 4. Update Stream with native tracks (reusing existing if unchanged)
        let current_tracks = stream.get_tracks().await;
        let same = current_tracks.len() == registered_tracks.len()
            && current_tracks.iter().zip(&registered_tracks).all(|(a, b)| Arc::ptr_eq(a, b));
        if !same {
            for t in &registered_tracks {
                info!(
                    stream = %stream.name,
                    kind = ?t.kind,
                    codec = %t.codec.name,
                    pt = t.codec.payload_type,
                    "Registered in-process camera media track"
                );
            }
            stream.set_tracks(registered_tracks).await;
        }

        // 5. Send PLAY
        let (_, _, play_code) = Self::send_request_with_auth(
            &mut tcp,
            &mut buffer,
            "PLAY",
            &clean_url,
            &mut cseq,
            &username,
            &password,
            &mut auth_info,
            session_id.as_deref(),
            Some(&[("Range", "npt=0.000-")]),
            Some(&channel_map),
            Some(stream),
        ).await?;

        if play_code != 200 {
            bail!("RTSP PLAY returned HTTP status {}", play_code);
        }

        info!(stream = %stream.name, "RTSP PLAY active, streaming in-process interleaved RTP");

        // Fast flat-array channel lookup table (O(1) index without hash calculation)
        let mut channel_table: [Option<Arc<Track>>; 16] = Default::default();
        for (&ch, track) in &channel_map {
            if (ch as usize) < channel_table.len() {
                channel_table[ch as usize] = Some(Arc::clone(track));
            }
        }

        // 6. Interleaved RTP packet loop
        let mut keepalive_timer = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(25),
            Duration::from_secs(25),
        );
        keepalive_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut idle_keepalives = 0u32;
        // On-demand producers stop after ~50s without consumers by default;
        // streams with a keep-warm window stay up longer (rounded up to the
        // 25s keepalive tick) so the GOP cache stays hot for fast SIP pickup.
        let idle_stop_ticks = idle_stop_ticks(stream.keep_warm_secs);

        let read_timeout = tokio::time::sleep(Duration::from_secs(8));
        tokio::pin!(read_timeout);

        loop {
            // Process all buffered binary packets first
            while !buffer.is_empty() && buffer[0] == b'$' {
                if buffer.len() < 4 {
                    break;
                }

                let ch = buffer[1];
                let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;

                if buffer.len() < 4 + length {
                    break;
                }

                // Consume packet header
                let _dollar = buffer.get_u8();
                let _ch_byte = buffer.get_u8();
                let _len_bytes = buffer.get_u16();
                let packet_bytes = buffer.split_to(length).freeze();

                // Channel is RTP
                let packet_len = packet_bytes.len() as u64;
                let track_opt = if (ch as usize) < channel_table.len() {
                    channel_table[ch as usize].as_ref()
                } else {
                    channel_map.get(&ch)
                };
                if let Some(track) = track_opt
                    && let Some(rtp) = RtpPacket::parse(packet_bytes) {
                        stream.packets_recv.fetch_add(1, Ordering::Relaxed);
                        stream.bytes_recv.fetch_add(packet_len, Ordering::Relaxed);
                        track.send(rtp);
                    }
            }

            // Check if buffer starts with plain text (e.g. RTSP response to keepalive) or desynchronized data
            if !buffer.is_empty() && buffer[0] != b'$' {
                if buffer.starts_with(b"RTSP/") {
                    if let Some(pos) = find_crlf_crlf(&buffer) {
                        let header_bytes = &buffer[..pos + 4];
                        let mut body_len = 0usize;
                        let header_str = String::from_utf8_lossy(header_bytes);
                        for line in header_str.lines() {
                            if let Some((k, v)) = line.split_once(':')
                                && k.trim().eq_ignore_ascii_case("content-length") {
                                    body_len = v.trim().parse::<usize>().unwrap_or(0);
                                }
                        }
                        if buffer.len() >= pos + 4 + body_len {
                            let _ = buffer.split_to(pos + 4 + body_len);
                            continue;
                        }
                    }
                } else if let Some(dollar_pos) = memchr::memchr(b'$', &buffer) {
                    // Resynchronize to next interleaved RTP packet!
                    warn!(stream = %stream.name, discarded = dollar_pos, "Resynchronizing interleaved RTSP stream to next '$'");
                    let _ = buffer.split_to(dollar_pos);
                    continue;
                } else {
                    // Buffer contains unparseable garbage with no '$' delimiter - clear it to unblock
                    warn!(stream = %stream.name, discarded = buffer.len(), "Discarding unparseable RTSP buffer garbage");
                    buffer.clear();
                }
            }

            tokio::select! {
                _ = &mut *cancel_rx => {
                    info!(stream = %stream.name, "Stopping in-process RTSP stream (cancellation received)");
                    if let Some(ref sess) = session_id {
                        let _ = Self::send_raw_request(
                            &mut tcp,
                            &mut buffer,
                            "TEARDOWN",
                            &clean_url,
                            &mut cseq,
                            Some(sess),
                            None,
                            None,
                            None,
                            None,
                        ).await;
                    }
                    return Ok(true);
                }
                _ = keepalive_timer.tick() => {
                    if stream.total_consumers().await == 0 {
                        idle_keepalives += 1;
                        if (idle_keepalives as u64) >= idle_stop_ticks {
                            info!(
                                stream = %stream.name,
                                idle_secs = idle_stop_ticks * 25,
                                "No active consumers on RTSP stream; stopping on-demand producer"
                            );
                            if let Some(ref sess) = session_id {
                                let _ = Self::send_raw_request(
                                    &mut tcp,
                                    &mut buffer,
                                    "TEARDOWN",
                                    &clean_url,
                                    &mut cseq,
                                    Some(sess),
                                    None,
                                    None,
                                    None,
                                    None,
                                ).await;
                            }
                            *stream.producer.lock().await = None;
                            stream.clear_tracks().await;
                            return Ok(true);
                        }
                    } else {
                        idle_keepalives = 0;
                    }

                    trace!(stream = %stream.name, "Sending RTSP keepalive");
                    // Try GET_PARAMETER with session first (RFC 2326 standard, preferred by Reolink)
                    let gp_res = tokio::time::timeout(
                        Duration::from_millis(5000),
                        Self::send_request_with_auth(
                            &mut tcp,
                            &mut buffer,
                            "GET_PARAMETER",
                            &clean_url,
                            &mut cseq,
                            &username,
                            &password,
                            &mut auth_info,
                            session_id.as_deref(),
                            None,
                            Some(&channel_map),
                            Some(stream),
                        ),
                    ).await;

                    if let Ok(Ok((_, _, status))) = gp_res
                        && (status == 405 || status == 501) {
                            // Fall back to OPTIONS with auth if GET_PARAMETER is not implemented
                            let _ = tokio::time::timeout(
                                Duration::from_millis(5000),
                                Self::send_request_with_auth(
                                    &mut tcp,
                                    &mut buffer,
                                    "OPTIONS",
                                    &clean_url,
                                    &mut cseq,
                                    &username,
                                    &password,
                                    &mut auth_info,
                                    session_id.as_deref(),
                                    None,
                                    Some(&channel_map),
                                    Some(stream),
                                ),
                            ).await;
                        }
                }
                res = tcp.read_buf(&mut buffer) => {
                    match res {
                        Ok(0) => {
                            warn!(stream = %stream.name, "RTSP connection reached EOF");
                            return Ok(false);
                        }
                        Ok(_) => {
                            read_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(8));
                        }
                        Err(e) => {
                            warn!(stream = %stream.name, "RTSP TCP socket read error: {}", e);
                            return Err(e.into());
                        }
                    }
                }
                _ = &mut read_timeout => {
                    warn!(stream = %stream.name, "RTSP stream read timeout (no packets for 8s), reconnecting");
                    return Ok(false);
                }
            }
        }
    }

    /// Send an RTSP request, handling 401 Unauthorized challenge automatically
    async fn send_request_with_auth(
        tcp: &mut TcpStream,
        buffer: &mut BytesMut,
        method: &str,
        url: &str,
        cseq: &mut u32,
        user: &str,
        pass: &str,
        auth_info: &mut Option<AuthMethod>,
        session: Option<&str>,
        extra_headers: Option<&[(&str, &str)]>,
        channel_map: Option<&HashMap<u8, Arc<Track>>>,
        stream: Option<&Arc<Stream>>,
    ) -> Result<(Vec<u8>, HashMap<String, String>, u16)> {
        // Try initial request with existing auth header if known
        let auth_hdr = auth_info.as_ref().and_then(|a| a.generate_header(method, url, user, pass));
        let (body, headers, status) = Self::send_raw_request(
            tcp,
            buffer,
            method,
            url,
            cseq,
            session,
            auth_hdr.as_deref(),
            extra_headers,
            channel_map,
            stream,
        ).await?;

        if status == 401
            && let Some(auth_str) = headers.get("www-authenticate")
                && let Some(parsed_auth) = AuthMethod::parse(auth_str) {
                    *auth_info = Some(parsed_auth);
                    let new_auth_hdr = auth_info.as_ref().and_then(|a| a.generate_header(method, url, user, pass));
                    return Self::send_raw_request(
                        tcp,
                        buffer,
                        method,
                        url,
                        cseq,
                        session,
                        new_auth_hdr.as_deref(),
                        extra_headers,
                        channel_map,
                        stream,
                    ).await;
                }

        Ok((body, headers, status))
    }

    /// Send a single raw RTSP request and parse the response, draining any interleaved binary RTP/RTCP packets
    async fn send_raw_request(
        tcp: &mut TcpStream,
        buffer: &mut BytesMut,
        method: &str,
        url: &str,
        cseq: &mut u32,
        session: Option<&str>,
        auth_header: Option<&str>,
        extra_headers: Option<&[(&str, &str)]>,
        channel_map: Option<&HashMap<u8, Arc<Track>>>,
        stream: Option<&Arc<Stream>>,
    ) -> Result<(Vec<u8>, HashMap<String, String>, u16)> {
        let cur_cseq = *cseq;
        *cseq += 1;

        let mut req = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: rust2rtc\r\n", method, url, cur_cseq);
        if let Some(s) = session {
            req.push_str(&format!("Session: {}\r\n", s));
        }
        if let Some(a) = auth_header {
            req.push_str(&format!("Authorization: {}\r\n", a));
        }
        if let Some(extras) = extra_headers {
            for (k, v) in extras {
                req.push_str(&format!("{}: {}\r\n", k, v));
            }
        }
        req.push_str("\r\n");

        tcp.write_all(req.as_bytes()).await.context("Failed to send RTSP request")?;

        let mut content_length = 0usize;
        let mut status_code = 0u16;
        let mut headers = HashMap::new();

        loop {
            // 1. Process/drain any interleaved binary packets ($) preceding or interleaved with RTSP response
            while !buffer.is_empty() && buffer[0] == b'$' {
                if buffer.len() < 4 {
                    break;
                }
                let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
                if buffer.len() < 4 + length {
                    break;
                }

                let _dollar = buffer.get_u8();
                let ch = buffer.get_u8();
                let _len = buffer.get_u16();
                let packet_bytes = buffer.split_to(length).freeze();

                let packet_len = packet_bytes.len() as u64;
                if let Some(cmap) = channel_map
                    && let Some(track) = cmap.get(&ch)
                        && let Some(rtp) = RtpPacket::parse(packet_bytes) {
                            if let Some(st) = stream {
                                st.packets_recv.fetch_add(1, Ordering::Relaxed);
                                st.bytes_recv.fetch_add(packet_len, Ordering::Relaxed);
                            }
                            track.send(rtp);
                        }
            }

            // 2. Discard leading non-RTSP, non-$ bytes (whitespace, newlines, or dangling body bytes)
            while !buffer.is_empty() && buffer[0] != b'$' && !buffer.starts_with(b"RTSP/") {
                let _ = buffer.get_u8();
            }

            // 3. Check if buffer starts with RTSP/ and contains complete headers
            if buffer.starts_with(b"RTSP/")
                && let Some(pos) = find_crlf_crlf(buffer) {
                    let header_bytes = buffer.split_to(pos + 4);
                    let header_str = String::from_utf8_lossy(&header_bytes);
                    let mut lines = header_str.lines();

                    if let Some(status_line) = lines.next() {
                        let parts: Vec<&str> = status_line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            status_code = parts[1].parse::<u16>().unwrap_or(0);
                        }
                    }

                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                        }
                    }

                    if let Some(len_str) = headers.get("content-length") {
                        content_length = len_str.parse::<usize>().unwrap_or(0);
                    }
                    break;
                }

            let n = tcp.read_buf(buffer).await.context("Failed reading RTSP response headers")?;
            if n == 0 {
                bail!("Premature EOF while reading RTSP response headers");
            }
        }

        // Read body if expected
        while buffer.len() < content_length {
            let n = tcp.read_buf(buffer).await.context("Failed reading RTSP response body")?;
            if n == 0 {
                bail!("Premature EOF while reading RTSP response body");
            }
        }

        let body = buffer.split_to(content_length).to_vec();
        Ok((body, headers, status_code))
    }
}

impl AuthMethod {
    pub fn parse(header: &str) -> Option<Self> {
        let trimmed = header.trim();
        if trimmed.to_lowercase().starts_with("basic") {
            return Some(AuthMethod::Basic);
        }

        if let Some(digest_params) = trimmed.strip_prefix("Digest ") {
            let mut realm = String::new();
            let mut nonce = String::new();
            let mut qop = None;
            let mut algorithm = None;

            for part in digest_params.split(',') {
                let part = part.trim();
                if let Some((k, v)) = part.split_once('=') {
                    let k = k.trim().to_lowercase();
                    let v = v.trim().trim_matches('"');
                    match k.as_str() {
                        "realm" => realm = v.to_string(),
                        "nonce" => nonce = v.to_string(),
                        "qop" => qop = Some(v.to_string()),
                        "algorithm" => algorithm = Some(v.to_string()),
                        _ => {}
                    }
                }
            }

            if !realm.is_empty() && !nonce.is_empty() {
                return Some(AuthMethod::Digest {
                    realm,
                    nonce,
                    qop,
                    algorithm,
                });
            }
        }

        None
    }

    pub fn generate_header(&self, method: &str, uri: &str, user: &str, pass: &str) -> Option<String> {
        if user.is_empty() {
            return None;
        }

        match self {
            AuthMethod::Basic => {
                let token = BASE64_STANDARD.encode(format!("{}:{}", user, pass));
                Some(format!("Basic {}", token))
            }
            AuthMethod::Digest { realm, nonce, .. } => {
                let ha1 = hex_md5(&format!("{}:{}:{}", user, realm, pass));
                let ha2 = hex_md5(&format!("{}:{}", method, uri));
                let response = hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2));
                Some(format!(
                    "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
                    user, realm, nonce, uri, response
                ))
            }
        }
    }
}

fn hex_md5(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    memchr::memmem::find(buf, b"\r\n\r\n")
}

/// Number of 25s keepalive ticks to wait with zero consumers before stopping
/// an on-demand producer. `keep_warm_secs == 0` keeps the default ~50s stop;
/// a positive value is rounded up to whole ticks (25s granularity).
fn idle_stop_ticks(keep_warm_secs: u64) -> u64 {
    if keep_warm_secs > 0 {
        keep_warm_secs.div_ceil(25).max(1)
    } else {
        2
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
    control: Option<String>,
    is_sendonly: bool,
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
            control: None,
            is_sendonly: false,
        }
    }

    fn to_track_info(&self, content_base: &str, existing_tracks: &[Arc<Track>]) -> SdpTrackInfo {
        let codec = Codec {
            name: self.name.to_uppercase(),
            clock_rate: self.clock_rate,
            channels: self.channels,
            payload_type: self.pt,
            fmtp: self.fmtp.clone(),
        };

        // Reuse existing track broadcaster if stream already had one of matching kind/codec
        let existing = existing_tracks.iter().find(|t| {
            t.kind == self.kind && t.codec.name.eq_ignore_ascii_case(&self.name)
        });

        let track = if let Some(et) = existing {
            Arc::clone(et)
        } else {
            let cap = if self.kind == MediaKind::Video {
                Track::DEFAULT_VIDEO_CAPACITY
            } else {
                Track::DEFAULT_AUDIO_CAPACITY
            };
            Arc::new(Track::new(codec, self.kind, cap))
        };

        // Resolve track control URL
        let control_url = match &self.control {
            Some(ctrl) if ctrl.starts_with("rtsp://") => ctrl.clone(),
            Some(ctrl) if ctrl == "*" || ctrl.is_empty() => content_base.to_string(),
            Some(ctrl) => {
                let base = content_base.trim_end_matches('/');
                let ctrl_clean = ctrl.trim_start_matches('/');
                format!("{}/{}", base, ctrl_clean)
            }
            None => content_base.to_string(),
        };

        SdpTrackInfo {
            track,
            control: control_url,
            kind: self.kind,
            pt: self.pt,
        }
    }
}

/// Parse SDP body and extract media tracks with their RTSP control URLs
fn parse_sdp_tracks_with_control(sdp: &str, content_base: &str, existing_tracks: &[Arc<Track>]) -> Vec<SdpTrackInfo> {
    let mut tracks = Vec::new();
    let mut current_draft: Option<TrackDraft> = None;

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(m) = line.strip_prefix("m=") {
            if let Some(draft) = current_draft.take()
                && !draft.is_sendonly {
                    tracks.push(draft.to_track_info(content_base, existing_tracks));
                }

            let parts: Vec<&str> = m.split_whitespace().collect();
            if !parts.is_empty() {
                let kind = match parts[0] {
                    "audio" => Some(MediaKind::Audio),
                    "video" => Some(MediaKind::Video),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let pt = if parts.len() >= 4 {
                        parts[3].parse::<u8>().unwrap_or(96)
                    } else {
                        96
                    };
                    current_draft = Some(TrackDraft::new(kind, pt));
                }
            }
        } else if line == "a=sendonly" {
            if let Some(ref mut draft) = current_draft {
                draft.is_sendonly = true;
            }
        } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            let parts: Vec<&str> = rtpmap.split_whitespace().collect();
            if parts.len() >= 2
                && let Ok(pt) = parts[0].parse::<u8>() {
                    let (name, rate_chan) = parts[1].split_once('/').unwrap_or((parts[1], "90000"));
                    let rate = rate_chan.split('/').next().unwrap_or("90000").parse::<u32>().unwrap_or(90000);
                    let chan = rate_chan.split('/').nth(1).and_then(|c| c.parse::<u8>().ok());

                    if let Some(ref mut draft) = current_draft
                        && (draft.pt == pt || draft.name.is_empty()) {
                            draft.pt = pt;
                            draft.name = name.to_uppercase();
                            draft.clock_rate = rate;
                            if draft.kind == MediaKind::Audio {
                                draft.channels = Some(chan.unwrap_or(1));
                            }
                        }
                }
        } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
            if let Some((pt_str, params)) = fmtp.split_once(' ')
                && let Ok(pt) = pt_str.parse::<u8>()
                    && let Some(ref mut draft) = current_draft
                        && draft.pt == pt {
                            draft.fmtp = Some(params.trim().to_string());
                        }
        } else if let Some(ctrl) = line.strip_prefix("a=control:")
            && let Some(ref mut draft) = current_draft {
                draft.control = Some(ctrl.trim().to_string());
            }
    }

    if let Some(draft) = current_draft.take()
        && !draft.is_sendonly {
            tracks.push(draft.to_track_info(content_base, existing_tracks));
        }

    tracks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sdp_ignores_application_tracks() {
        let sdp = "\
v=0\r\n\
o=- 1711200000 1 IN IP4 198.51.100.100\r\n\
s=TP-LINK RTSP Server\r\n\
t=0 0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H265/90000\r\n\
a=control:track0\r\n\
m=audio 0 RTP/AVP 8\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=control:track1\r\n\
m=application/TP-LINK 0 RTP/AVP smart/1/90000\r\n\
a=control:track3\r\n";

        let tracks = parse_sdp_tracks_with_control(sdp, "rtsp://198.51.100.100/stream1/", &[]);
        assert_eq!(tracks.len(), 2, "Should only have 2 tracks (video and audio), ignoring application");
        assert_eq!(tracks[0].kind, MediaKind::Video);
        assert_eq!(tracks[0].track.codec.name, "H265");
        assert_eq!(tracks[0].control, "rtsp://198.51.100.100/stream1/track0");
        assert_eq!(tracks[1].kind, MediaKind::Audio);
        assert_eq!(tracks[1].track.codec.name, "PCMA");
        assert_eq!(tracks[1].control, "rtsp://198.51.100.100/stream1/track1");
    }

    #[test]
    fn test_parse_sdp_reuses_existing_tracks() {
        let sdp = "\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:track0\r\n";

        let existing_video = Arc::new(Track::new(Codec::new_h264(), MediaKind::Video, 100));
        let existing = vec![Arc::clone(&existing_video)];

        let tracks = parse_sdp_tracks_with_control(sdp, "rtsp://198.51.100.100/stream1/", &existing);
        assert_eq!(tracks.len(), 1);
        assert!(Arc::ptr_eq(&tracks[0].track, &existing_video), "Must reuse the existing Track Arc reference");
    }

    #[test]
    fn test_idle_stop_ticks() {
        // Default: two 25s ticks (~50s) without consumers stops the producer.
        assert_eq!(idle_stop_ticks(0), 2);
        // Explicit windows round up to whole 25s ticks.
        assert_eq!(idle_stop_ticks(1), 1);
        assert_eq!(idle_stop_ticks(25), 1);
        assert_eq!(idle_stop_ticks(26), 2);
        assert_eq!(idle_stop_ticks(50), 2);
        assert_eq!(idle_stop_ticks(600), 24);
        assert_eq!(idle_stop_ticks(86400), 3456);
    }
}
