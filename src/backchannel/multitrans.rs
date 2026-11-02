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

/// TP-Link / Tapo MULTITRANS Backchannel Client
pub struct MultitransClient {
    url: Url,
    uuid: String,
    stream: Option<TcpStream>,
    session_id: Option<String>,
}

impl MultitransClient {
    pub fn new(raw_url: &str) -> Result<Self> {
        let url = Url::parse(raw_url).context("Failed to parse multitrans URL")?;
        let uuid = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            url,
            uuid,
            stream: None,
            session_id: None,
        })
    }

    /// Perform handshake with camera over TCP
    pub async fn connect(&mut self) -> Result<()> {
        let host = self.url.host_str().context("Missing host in multitrans URL")?;
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
    /// repacks them into framed RTP packets, encapsulates them in RTSP interleaved frames ($ \x01 ...),
    /// and writes them to the camera connection.
    pub async fn run_sender(
        mut self,
        mut rx: mpsc::Receiver<(Bytes, bool)>, // (payload, is_pcmu)
    ) -> Result<()> {
        let mut stream = self.stream.take().context("Not connected")?;
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
