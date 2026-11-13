#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use md5::{Digest, Md5};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use url::Url;

use super::pcm::{transcode_ulaw_to_alaw, G711Repacker};

/// TP-Link / Tapo MULTITRANS Backchannel Client (supports both RTSP:554 and HTTP Streamd:8800)
pub struct MultitransClient {
    url: Url,
    uuid: String,
    is_http_tapo: bool,
    stream: Option<TcpStream>,
    session_id: Option<String>,
}

impl MultitransClient {
    pub fn new(raw_url: &str) -> Result<Self> {
        let is_tapo = raw_url.starts_with("tapo://");
        let mut clean_url = raw_url.to_string();
        if is_tapo {
            clean_url = clean_url.replacen("tapo://", "http://", 1);
        } else if clean_url.starts_with("multitrans://") {
            clean_url = clean_url.replacen("multitrans://", "rtsp://", 1);
        }
        let url = Url::parse(&clean_url).context("Failed to parse multitrans/tapo URL")?;
        let is_http_tapo = is_tapo || url.port() == Some(8800);
        let uuid = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            url,
            uuid,
            is_http_tapo,
            stream: None,
            session_id: None,
        })
    }

    /// Perform handshake with camera over TCP
    pub async fn connect(&mut self) -> Result<()> {
        let host = self.url.host_str().context("Missing host in URL")?;

        if self.is_http_tapo {
            let port = self.url.port().unwrap_or(8800);
            let addr = format!("{}:{}", host, port);

            debug!(addr = %addr, "Connecting to Tapo camera for HTTP Streamd backchannel");
            let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
                .await
                .context("Connection timed out")?
                .with_context(|| format!("Failed to connect to {}", addr))?;

            // 1. Challenge request
            let challenge_req = format!(
                "POST /stream HTTP/1.1\r\n\
                Host: {addr}\r\n\
                Content-Type: multipart/mixed; boundary=----client-stream-boundary--\r\n\
                Connection: keep-alive\r\n\
                Content-Length: 0\r\n\r\n"
            );
            stream.write_all(challenge_req.as_bytes()).await?;

            let (status0, headers0, _) = read_rtsp_response(&mut stream).await?;
            if status0 != 401 {
                bail!("Expected 401 Digest challenge from Tapo camera, got {}", status0);
            }

            let auth_hdr = headers0
                .get("www-authenticate")
                .cloned()
                .context("Missing WWW-Authenticate header")?;

            let realm = extract_param(&auth_hdr, "realm=").unwrap_or_else(|| "TP-Link IP-Camera".to_string());
            let nonce = extract_param(&auth_hdr, "nonce=").context("Missing nonce in auth header")?;
            let opaque = extract_param(&auth_hdr, "opaque=");
            let qop = extract_param(&auth_hdr, "qop=");

            let user = self.url.username();
            // The camera expects the URL password AS-IS in the digest response
            // (this mirrors go2rtc pkg/tapo). The SHA256 derivation only
            // applies to the username-only `tapo://{cloud-password}@host`
            // form; URLs carrying a pre-hashed password must use it directly.
            let pass = self.url.password().unwrap_or("");

            let ha1 = hex_md5(&format!("{}:{}:{}", user, realm, pass));
            let ha2 = hex_md5("POST:/stream");
            let uri = "/stream";

            let auth_val = if let Some(ref qop_val) = qop {
                let cnonce = format!("{:08x}{:08x}{:08x}{:08x}", rand_seed(), rand_seed(), rand_seed(), rand_seed());
                let nc = "00000001";
                let resp = hex_md5(&format!("{}:{}:{}:{}:{}:{}", ha1, nonce, nc, cnonce, qop_val, ha2));
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}", qop={}, nc={}, cnonce="{}""#,
                    user, realm, nonce, uri, resp, qop_val, nc, cnonce
                );
                if let Some(op) = &opaque {
                    val.push_str(&format!(r#", opaque="{}", algorithm=MD5"#, op));
                }
                val
            } else {
                let resp = hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2));
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}""#,
                    user, realm, nonce, uri, resp
                );
                if let Some(op) = &opaque {
                    val.push_str(&format!(r#", opaque="{}", algorithm=MD5"#, op));
                }
                val
            };

            // 2. Authenticated request: empty body. The camera replies 200 OK and
            //    keeps the connection open, switching to multipart part framing.
            let boundary = "----client-stream-boundary--";
            let req = format!(
                "POST /stream HTTP/1.1\r\n\
                Host: {addr}\r\n\
                Authorization: {auth_val}\r\n\
                Content-Type: multipart/mixed; boundary={boundary}\r\n\
                Connection: keep-alive\r\n\
                Content-Length: 0\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await?;

            let (status, _, _) = read_rtsp_response(&mut stream).await?;
            if status != 200 {
                bail!("Tapo HTTP Streamd authentication failed with status {}", status);
            }

            // 3. Open the talk channel: JSON request as the first client part
            //    (mirrors go2rtc pkg/tapo SetupBackchannel -> Request).
            let talk = r#"{"params":{"talk":{"mode":"aec"},"method":"get"},"seq":3,"type":"request"}"#;
            let talk_part = format!(
                "--{boundary}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}\r\n",
                talk.len(),
                talk
            );
            stream.write_all(talk_part.as_bytes()).await?;

            // 4. Read device parts until the JSON response carrying
            //    params.session_id (error_code != 0 means the camera rejected
            //    the talk request).
            let mut rbuf: Vec<u8> = Vec::new();
            let session = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match read_streamd_part(&mut stream, &mut rbuf).await? {
                        None => bail!("Tapo connection closed while opening talk channel"),
                        Some((part_headers, body)) => {
                            if part_headers.get("content-type").map(String::as_str)
                                != Some("application/json")
                            {
                                continue;
                            }
                            let v: serde_json::Value = match serde_json::from_slice(&body) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let params = v.get("params");
                            if let Some(ec) = params
                                .and_then(|p| p.get("error_code"))
                                .and_then(|e| e.as_i64())
                                .filter(|ec| *ec != 0)
                            {
                                bail!("Tapo talk request rejected (error_code {})", ec);
                            }
                            if let Some(sid) = params
                                .and_then(|p| p.get("session_id"))
                                .and_then(|s| s.as_str())
                            {
                                return Ok::<String, anyhow::Error>(sid.to_string());
                            }
                        }
                    }
                }
            })
            .await
            .context("Timed out waiting for Tapo talk session response")??;

            // Go's net package enables TCP_NODELAY by default; without it the
            // small, 80ms-cadence audio parts can be delayed by Nagle.
            stream.set_nodelay(true)?;

            info!(host = %host, session = %session, "Tapo HTTP Streamd backchannel ready");
            self.stream = Some(stream);
            self.session_id = Some(session);
            return Ok(());
        }

        // Standard RTSP MULTITRANS (port 554)
        let port = self.url.port().unwrap_or(554);
        let addr = format!("{}:{}", host, port);

        debug!(addr = %addr, "Connecting to Tapo camera for MULTITRANS backchannel");
        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .context("Connection timed out")?
            .with_context(|| format!("Failed to connect to {}", addr))?;

        let uri = format!("rtsp://{}/multitrans", host);

        // Step 1: Challenge
        let mut cseq = 0;
        let req0 = format!(
            "MULTITRANS {} RTSP/1.0\r\nCSeq: {}\r\nX-Client-UUID: {}\r\n\r\n",
            uri, cseq, self.uuid
        );
        stream.write_all(req0.as_bytes()).await?;

        let (status0, headers0, _) = read_rtsp_response(&mut stream).await?;
        let session = if status0 == 200 {
            headers0
                .get("session")
                .cloned()
                .context("Missing Session header from camera")?
        } else if status0 == 401 {
            let auth_hdr = headers0
                .get("www-authenticate")
                .cloned()
                .context("Missing WWW-Authenticate header")?;

            let realm = extract_param(&auth_hdr, "realm=").context("Missing realm in auth header")?;
            let nonce = extract_param(&auth_hdr, "nonce=").context("Missing nonce in auth header")?;

            // Step 2: Digest Authentication
            let user = self.url.username();
            let pass = self.url.password().unwrap_or("");

            let ha1 = hex_md5(&format!("{}:{}:{}", user, realm, pass));
            let ha2 = hex_md5(&format!("MULTITRANS:{}", uri));
            let response = hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2));

            let auth_val = format!(
                r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}""#,
                user, realm, nonce, uri, response
            );

            cseq += 1;
            let req1 = format!(
                "MULTITRANS {} RTSP/1.0\r\nCSeq: {}\r\nAuthorization: {}\r\nX-Client-UUID: {}\r\n\r\n",
                uri, cseq, auth_val, self.uuid
            );
            stream.write_all(req1.as_bytes()).await?;

            let (status1, headers1, _) = read_rtsp_response(&mut stream).await?;
            if status1 != 200 {
                bail!("MULTITRANS authentication failed with status {}", status1);
            }

            headers1
                .get("session")
                .cloned()
                .context("Missing Session header from camera")?
        } else {
            bail!("MULTITRANS expected 200 or 401 status, got {}", status0);
        };

        debug!(session = %session, "MULTITRANS session established");

        // Step 3: Open Talk Channel (Full Duplex)
        cseq += 1;
        let payload = r#"{"type":"request","seq":0,"params":{"method":"get","talk":{"mode":"full_duplex"}}}"#;
        let req2 = format!(
            "MULTITRANS {} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            uri, cseq, session, payload.len(), payload
        );
        stream.write_all(req2.as_bytes()).await?;

        let (status, _, body) = read_rtsp_response(&mut stream).await?;
        if status != 200 || !body.contains(r#""error_code":0"#) {
            bail!("MULTITRANS open talkback failed: status {}, body: {}", status, body);
        }

        info!(host = %host, "Tapo MULTITRANS backchannel ready");
        self.stream = Some(stream);
        self.session_id = Some(session);

        Ok(())
    }

    /// Run the backchannel audio loop. Accepts incoming audio payload chunks (PCMA or PCMU),
    /// repacks them and writes them to the camera connection.
    pub async fn run_sender(
        mut self,
        mut rx: mpsc::Receiver<(Bytes, bool)>, // (payload, is_pcmu)
    ) -> Result<()> {
        let mut stream = self.stream.take().context("Not connected")?;

        let mut scratch = Vec::new();
        if self.is_http_tapo {
            let session = self
                .session_id
                .clone()
                .context("Missing Tapo Streamd session id")?;
            let boundary = "----client-stream-boundary--";

            // Split the connection: the write half carries our audio parts,
            // the read half is drained in the loop below. The camera streams
            // its encrypted downlink (A/V parts) on this same connection; if
            // we never read it, its TCP send window fills up and the talkback
            // stalls.
            let (mut rd, mut wr) = stream.into_split();
            let mut sink = [0u8; 8192];

            // PAT/PMT header goes first, as its own audio/mp2t part
            // (go2rtc: WriteBackchannel(muxer.GetHeader()) before any audio).
            let mut muxer = super::mpegts::TapoTsMuxer::new();
            write_streamd_part(&mut wr, &session, boundary, &muxer.header()).await?;

            let mut repacker = G711Repacker::new(640, 8, 0x11223344); // 640 bytes PCMA (80ms @ 8kHz)

            loop {
                tokio::select! {
                    maybe = rx.recv() => {
                        let (payload, is_pcmu) = match maybe {
                            Some(p) => p,
                            None => break,
                        };
                        let chunks = if is_pcmu {
                            scratch.clear();
                            scratch.extend_from_slice(&payload);
                            transcode_ulaw_to_alaw(&mut scratch);
                            repacker.push_payload(&scratch)
                        } else {
                            repacker.push_payload(&payload)
                        };
                        for pkt in chunks {
                            // Strip the 12-byte RTP header; the RTP timestamp
                            // drives the MPEG-TS PTS (go2rtc passes
                            // packet.Timestamp/packet.Payload to the muxer).
                            if pkt.len() <= 12 {
                                continue;
                            }
                            let rtp_ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
                            let ts = muxer.payload(rtp_ts, &pkt[12..]);
                            if let Err(e) = write_streamd_part(&mut wr, &session, boundary, &ts).await {
                                error!("Error sending Tapo HTTP audio frame: {}", e);
                                return Err(e);
                            }
                        }
                    }
                    read = rd.read(&mut sink) => {
                        if read.unwrap_or(0) == 0 {
                            debug!("Tapo HTTP Streamd connection closed by camera");
                            break;
                        }
                    }
                }
            }
        } else {
            let mut repacker = G711Repacker::new(640, 8, 0x11223344); // 640 bytes (80ms PCMA/8000)

            while let Some((payload, is_pcmu)) = rx.recv().await {
                let rtp_packets = if is_pcmu {
                    scratch.clear();
                    scratch.extend_from_slice(&payload);
                    transcode_ulaw_to_alaw(&mut scratch);
                    repacker.push_payload(&scratch)
                } else {
                    repacker.push_payload(&payload)
                };
                for rtp in rtp_packets {
                    let size = rtp.len();
                    let mut frame = BytesMut::with_capacity(4 + size);
                    frame.extend_from_slice(&[b'$', 1, (size >> 8) as u8, (size & 0xff) as u8]);
                    frame.extend_from_slice(&rtp);

                    if let Err(e) = stream.write_all(&frame).await {
                        error!("Error sending MULTITRANS audio frame: {}", e);
                        return Err(e.into());
                    }
                }
            }
        }

        debug!("MULTITRANS backchannel sender finished");
        Ok(())
    }
}

/// Write one client multipart part (Streamd `audio/mp2t` framing), byte-identical
/// to go2rtc `pkg/tapo` `WriteBackchannel`.
async fn write_streamd_part<W>(
    w: &mut W,
    session: &str,
    boundary: &str,
    body: &[u8],
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let head = format!(
        "--{boundary}\r\nContent-Type: audio/mp2t\r\nX-If-Encrypt: 0\r\nX-Session-Id: {session}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body).await?;
    Ok(())
}

/// Read one multipart part from the camera (device boundary framing), keeping
/// any unconsumed bytes in `buf` between calls. Returns `None` when the camera
/// closes the connection.
async fn read_streamd_part(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> Result<Option<(std::collections::HashMap<String, String>, Vec<u8>)>> {
    const DEVICE_DELIM: &[u8] = b"----device-stream-boundary--";
    const DELIM_LINE: &[u8] = b"\r\n----device-stream-boundary--";

    loop {
        // The buffer always starts at a part boundary (or is empty), so the
        // first boundary found marks the start of the next part.
        let delim_at = if buf.starts_with(DEVICE_DELIM) {
            Some(0)
        } else {
            find_subsequence(buf, DELIM_LINE).map(|p| p + 2)
        };

        if let Some(d) = delim_at {
            let after = d + DEVICE_DELIM.len();
            if let Some(rel) = find_subsequence(&buf[after..], b"\r\n") {
                let headers_start = after + rel + 2;
                if let Some(rel2) = find_subsequence(&buf[headers_start..], b"\r\n\r\n") {
                    let body_start = headers_start + rel2 + 4;

                    let mut headers = std::collections::HashMap::new();
                    for line in String::from_utf8_lossy(&buf[headers_start..body_start - 4]).lines() {
                        if let Some((k, v)) = line.split_once(':') {
                            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                        }
                    }

                    let content_len = headers
                        .get("content-length")
                        .and_then(|v| v.parse::<usize>().ok());

                    if let Some(cl) = content_len {
                        if buf.len() >= body_start + cl {
                            let body = buf[body_start..body_start + cl].to_vec();
                            buf.drain(..body_start + cl);
                            return Ok(Some((headers, body)));
                        }
                    } else if let Some(next) = find_subsequence(&buf[body_start..], DELIM_LINE) {
                        // No Content-Length: the body runs to the next boundary.
                        let body = buf[body_start..body_start + next].to_vec();
                        buf.drain(..body_start + next + 2);
                        return Ok(Some((headers, body)));
                    }
                }
            }
        }

        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn hex_md5(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn extract_param(header: &str, param: &str) -> Option<String> {
    if let Some(pos) = header.find(param) {
        let after = &header[pos + param.len()..];
        let trimmed = after.trim_start_matches('"');
        if let Some(end) = trimmed.find('"') {
            return Some(trimmed[..end].to_string());
        }
        let end = trimmed.find(',').unwrap_or(trimmed.len());
        return Some(trimmed[..end].trim().to_string());
    }
    None
}

async fn read_rtsp_response(
    stream: &mut TcpStream,
) -> Result<(u16, std::collections::HashMap<String, String>, String)> {
    let mut buf = Vec::new();
    let mut header_end = None;

    while header_end.is_none() {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("Unexpected EOF reading RTSP response");
        }
        buf.extend_from_slice(&chunk[..n]);

        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            header_end = Some(pos);
        }
    }

    let end = header_end.unwrap();
    let header_bytes = &buf[..end];
    let header_str = String::from_utf8_lossy(header_bytes);

    let mut lines = header_str.lines();
    let status_line = lines.next().context("Empty RTSP response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("Invalid RTSP status line")?
        .parse()?;

    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let body_start = end + 4;
    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    while buf.len() < body_start + content_len {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = if buf.len() >= body_start {
        String::from_utf8_lossy(&buf[body_start..body_start + content_len]).to_string()
    } else {
        String::new()
    };

    Ok((status, headers, body))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}

fn extract_session_id(body: &str) -> Option<String> {
    if let Some(pos) = body.find(r#""session_id""#) {
        let after = &body[pos + 12..];
        let trimmed = after.trim_start_matches([':', ' ', '"']);
        if let Some(end) = trimmed.find('"') {
            return Some(trimmed[..end].to_string());
        }
    }
    None
}

fn rand_seed() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0x42424242);
    SEED.fetch_add(0x9e3779b9, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tapo_url_parsing() {
        let client = MultitransClient::new("tapo://admin:mypass@192.0.2.100").unwrap();
        assert!(client.is_http_tapo);
        assert_eq!(client.url.host_str(), Some("192.0.2.100"));
        assert_eq!(client.url.username(), "admin");
        assert_eq!(client.url.password(), Some("mypass"));
    }

    #[test]
    fn test_extract_session_id() {
        let body = r#"{"result":{"session_id":"abc-123-session","status":0}}"#;
        assert_eq!(extract_session_id(body), Some("abc-123-session".to_string()));
    }
}
