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
            let encrypt_type = extract_param(&auth_hdr, "encrypt_type=").unwrap_or_default();
            let qop = extract_param(&auth_hdr, "qop=");

            let user = self.url.username();
            let pass = self.url.password().unwrap_or("");

            // When encrypt_type="3", the password must be formatted as 64-character uppercase SHA256 hex
            let effective_pass = if encrypt_type == "3" {
                use sha2::{Digest as Sha2Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(pass.as_bytes());
                hex::encode(hasher.finalize()).to_uppercase()
            } else {
                pass.to_string()
            };

            let ha1 = hex_md5(&format!("{}:{}:{}", user, realm, effective_pass));
            let ha2 = hex_md5("POST:/stream");
            let uri = "/stream";

            let auth_val = if let Some(ref qop_val) = qop {
                let cnonce = hex::encode(rand_seed().to_be_bytes());
                let nc = "00000001";
                let resp = hex_md5(&format!("{}:{}:{}:{}:{}:{}", ha1, nonce, nc, cnonce, qop_val, ha2));
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}", qop={}, nc={}, cnonce="{}""#,
                    user, realm, nonce, uri, resp, qop_val, nc, cnonce
                );
                if let Some(op) = &opaque {
                    val.push_str(&format!(r#", opaque="{}""#, op));
                }
                val
            } else {
                let resp = hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2));
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}""#,
                    user, realm, nonce, uri, resp
                );
                if let Some(op) = &opaque {
                    val.push_str(&format!(r#", opaque="{}""#, op));
                }
                val
            };

            // 2. Authenticated handshake with multipart JSON
            let boundary = "----client-stream-boundary--";
            let json_body = r#"{"params":{"talk":{"mode":"aec"},"method":"get"}}"#;
            let multipart_data = format!(
                "--{boundary}\r\nContent-Type: application/json\r\n\r\n{json_body}\r\n--{boundary}--\r\n"
            );

            let req = format!(
                "POST /stream HTTP/1.1\r\n\
                Host: {addr}\r\n\
                Authorization: {auth_val}\r\n\
                Content-Type: multipart/mixed; boundary={boundary}\r\n\
                Connection: keep-alive\r\n\
                Content-Length: {}\r\n\r\n\
                {}",
                multipart_data.len(),
                multipart_data
            );
            stream.write_all(req.as_bytes()).await?;

            let (status, resp_headers, body) = read_rtsp_response(&mut stream).await?;
            if status != 200 {
                bail!("Tapo HTTP Streamd authentication failed with status {}", status);
            }

            let session = extract_session_id(&body)
                .or_else(|| resp_headers.get("session").cloned())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

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

        if self.is_http_tapo {
            let boundary = "----client-stream-boundary--";
            let mut repacker = G711Repacker::new(640, 8, 0x11223344); // 640 bytes PCMA

            while let Some((payload, is_pcmu)) = rx.recv().await {
                let mut data = payload.to_vec();
                if is_pcmu {
                    transcode_ulaw_to_alaw(&mut data);
                }

                let chunks = repacker.push_payload(&data);
                for chunk in chunks {
                    let mut block = Vec::with_capacity(chunk.len() + 128);
                    block.extend_from_slice(
                        format!(
                            "--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                            chunk.len()
                        )
                        .as_bytes(),
                    );
                    block.extend_from_slice(&chunk);
                    block.extend_from_slice(b"\r\n");

                    if let Err(e) = stream.write_all(&block).await {
                        error!("Error sending Tapo HTTP audio frame: {}", e);
                        return Err(e.into());
                    }
                }
            }
        } else {
            let mut repacker = G711Repacker::new(640, 8, 0x11223344); // 640 bytes (80ms PCMA/8000)

            while let Some((payload, is_pcmu)) = rx.recv().await {
                let mut data = payload.to_vec();
                if is_pcmu {
                    transcode_ulaw_to_alaw(&mut data);
                }

                let rtp_packets = repacker.push_payload(&data);
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
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn extract_session_id(body: &str) -> Option<String> {
    if let Some(pos) = body.find(r#""session_id""#) {
        let after = &body[pos + 12..];
        let trimmed = after.trim_start_matches(|c| c == ':' || c == ' ' || c == '"');
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
        let client = MultitransClient::new("tapo://admin:mypass@192.168.1.100").unwrap();
        assert!(client.is_http_tapo);
        assert_eq!(client.url.host_str(), Some("192.168.1.100"));
        assert_eq!(client.url.username(), "admin");
        assert_eq!(client.url.password(), Some("mypass"));
    }

    #[test]
    fn test_extract_session_id() {
        let body = r#"{"result":{"session_id":"abc-123-session","status":0}}"#;
        assert_eq!(extract_session_id(body), Some("abc-123-session".to_string()));
    }
}
