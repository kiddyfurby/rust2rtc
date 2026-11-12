#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use url::Url;

use super::pcm::{transcode_alaw_to_ulaw, G711Repacker};

/// ONVIF Profile T RTSP Backchannel Client (Reolink, Dahua, Hikvision, etc.)
pub struct OnvifBackchannelClient {
    url: Url,
    stream: Option<TcpStream>,
    session_id: Option<String>,
    interleaved_channel: u8,
    cseq: u32,
    realm: Option<String>,
    nonce: Option<String>,
}

impl OnvifBackchannelClient {
    pub fn new(raw_url: &str) -> Result<Self> {
        let clean_url = raw_url
            .split('#')
            .next()
            .unwrap_or(raw_url)
            .trim_end_matches("?backchannel=1")
            .trim_end_matches("&backchannel=1");
        let mut url = Url::parse(clean_url).context("Failed to parse RTSP/ONVIF URL")?;
        // If scheme is onvif, normalize to standard rtsp port 554 if needed
        if url.scheme() == "onvif" {
            let host = url.host_str().unwrap_or("127.0.0.1").to_string();
            let user = url.username().to_string();
            let pass = url.password().unwrap_or("").to_string();
            let rtsp_url_str = format!("rtsp://{}:{}@{}:554/Preview_01_main", user, pass, host);
            url = Url::parse(&rtsp_url_str)?;
        }

        Ok(Self {
            url,
            stream: None,
            session_id: None,
            interleaved_channel: 0,
            cseq: 1,
            realm: None,
            nonce: None,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        let host = self.url.host_str().context("Missing host in URL")?;
        let port = self.url.port().unwrap_or(554);
        let addr = format!("{}:{}", host, port);

        debug!(addr = %addr, "Connecting to camera for ONVIF backchannel");
        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .context("Connection timed out")?
            .with_context(|| format!("Failed to connect to {}", addr))?;

        let rtsp_url = self.url.as_str();

        // 1. Initial DESCRIBE
        let desc_req = format!(
            "DESCRIBE {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            User-Agent: rust2rtc\r\n\
            Require: www.onvif.org/ver20/backchannel\r\n\
            Accept: application/sdp\r\n\r\n",
            rtsp_url, self.cseq
        );
        self.cseq += 1;
        stream.write_all(desc_req.as_bytes()).await?;

        let (mut status, headers, mut body) = read_rtsp_response(&mut stream).await?;

        // Handle 401 Digest Auth
        if status == 401 {
            let auth_hdr = headers
                .get("www-authenticate")
                .cloned()
                .context("Missing WWW-Authenticate header")?;

            let realm = extract_param(&auth_hdr, "realm=").context("Missing realm in auth header")?;
            let nonce = extract_param(&auth_hdr, "nonce=").context("Missing nonce in auth header")?;
            self.realm = Some(realm.clone());
            self.nonce = Some(nonce.clone());

            let auth_val = self.build_auth_header("DESCRIBE", rtsp_url, &realm, &nonce);

            let auth_desc_req = format!(
                "DESCRIBE {} RTSP/1.0\r\n\
                CSeq: {}\r\n\
                User-Agent: rust2rtc\r\n\
                Authorization: {}\r\n\
                Require: www.onvif.org/ver20/backchannel\r\n\
                Accept: application/sdp\r\n\r\n",
                rtsp_url, self.cseq, auth_val
            );
            self.cseq += 1;
            stream.write_all(auth_desc_req.as_bytes()).await?;

            let resp = read_rtsp_response(&mut stream).await?;
            status = resp.0;
            let _ = resp.1;
            body = resp.2;
        }

        if status != 200 {
            bail!("RTSP DESCRIBE with backchannel failed with status {}", status);
        }

        // Parse SDP for backchannel track control URL
        let control_url = parse_backchannel_control(&body, rtsp_url)
            .context("Failed to find backchannel track in camera SDP")?;
        info!(control_url = %control_url, "Found ONVIF backchannel control URL");

        // 2. SETUP
        // Strip any userinfo from the control URL: the Reolink camera rejects
        // SETUP request lines carrying `user:pass@host` with 400 Bad Request
        // (the DESCRIBE tolerates it, but SETUP does not). Credentials travel
        // in the Digest Authorization header instead.
        let control_url = strip_userinfo(&control_url);
        let mut setup_req = format!(
            "SETUP {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            User-Agent: rust2rtc\r\n\
            Require: www.onvif.org/ver20/backchannel\r\n\
            Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n",
            control_url, self.cseq
        );
        self.cseq += 1;

        if let (Some(realm), Some(nonce)) = (&self.realm, &self.nonce) {
            let auth_val = self.build_auth_header("SETUP", &control_url, realm, nonce);
            setup_req.push_str(&format!("Authorization: {}\r\n", auth_val));
        }
        setup_req.push_str("\r\n");
        stream.write_all(setup_req.as_bytes()).await?;

        let (setup_status, setup_headers, setup_body) = read_rtsp_response(&mut stream).await?;
        if setup_status != 200 {
            bail!(
                "RTSP SETUP for backchannel failed with status {} (body: {})",
                setup_status,
                setup_body.trim()
            );
        }

        let session = setup_headers
            .get("session")
            .cloned()
            .context("Missing Session in SETUP response")?;
        let session_id = session.split(';').next().unwrap_or(&session).trim().to_string();

        let transport = setup_headers
            .get("transport")
            .cloned()
            .unwrap_or_else(|| "interleaved=2-3".to_string());
        let channel = extract_interleaved_channel(&transport).unwrap_or(2);
        self.interleaved_channel = channel;
        self.session_id = Some(session_id.clone());

        // 3. PLAY
        let mut play_req = format!(
            "PLAY {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            User-Agent: rust2rtc\r\n\
            Session: {}\r\n\
            Require: www.onvif.org/ver20/backchannel\r\n",
            rtsp_url, self.cseq, session_id
        );
        self.cseq += 1;

        if let (Some(realm), Some(nonce)) = (&self.realm, &self.nonce) {
            let auth_val = self.build_auth_header("PLAY", rtsp_url, realm, nonce);
            play_req.push_str(&format!("Authorization: {}\r\n", auth_val));
        }
        play_req.push_str("\r\n");
        stream.write_all(play_req.as_bytes()).await?;

        let (play_status, _, _) = read_rtsp_response(&mut stream).await?;
        if play_status != 200 {
            bail!("RTSP PLAY for backchannel failed with status {}", play_status);
        }

        info!(
            channel = channel,
            session = %session_id,
            "Reolink/ONVIF RTSP backchannel active"
        );
        self.stream = Some(stream);

        Ok(())
    }

    /// Stream audio chunks to camera speaker.
    /// Handles PCMU/8000 conversion and Reolink 1024-byte repacking with zero/monotonic timestamps.
    pub async fn run_sender(
        mut self,
        mut rx: mpsc::Receiver<(Bytes, bool)>, // (payload, is_pcmu)
    ) -> Result<()> {
        let stream = self.stream.take().context("Not connected")?;
        let channel = self.interleaved_channel;
        let mut repacker = G711Repacker::new(1024, 0, 0x99887766); // 1024 bytes (128ms PCMU/8000)

        let (mut reader, mut writer) = stream.into_split();

        // Background task to drain and discard incoming RTCP reports from camera
        let drain_handle = tokio::spawn(async move {
            let mut discard = [0u8; 1024];
            while let Ok(n) = reader.read(&mut discard).await {
                if n == 0 {
                    break;
                }
            }
        });

        let mut scratch = Vec::new();
        while let Some((payload, is_pcmu)) = rx.recv().await {
            let rtp_packets = if is_pcmu {
                repacker.push_payload(&payload)
            } else {
                scratch.clear();
                scratch.extend_from_slice(&payload);
                transcode_alaw_to_ulaw(&mut scratch);
                repacker.push_payload(&scratch)
            };
            for rtp in rtp_packets {
                let size = rtp.len();
                let mut frame = BytesMut::with_capacity(4 + size);
                frame.extend_from_slice(&[b'$', channel, (size >> 8) as u8, (size & 0xff) as u8]);
                frame.extend_from_slice(&rtp);

                if let Err(e) = writer.write_all(&frame).await {
                    error!("Error writing RTSP backchannel frame: {}", e);
                    drain_handle.abort();
                    return Err(e.into());
                }
            }
        }

        drain_handle.abort();
        debug!("ONVIF backchannel sender finished");
        Ok(())
    }

    fn build_auth_header(&self, method: &str, uri: &str, realm: &str, nonce: &str) -> String {
        let user = self.url.username();
        let pass = self.url.password().unwrap_or("");

        let ha1 = hex_md5(&format!("{}:{}:{}", user, realm, pass));
        let ha2 = hex_md5(&format!("{}:{}", method, uri));
        let response = hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2));

        format!(
            r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}""#,
            user, realm, nonce, uri, response
        )
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

fn extract_interleaved_channel(transport: &str) -> Option<u8> {
    if let Some(pos) = transport.find("interleaved=") {
        let after = &transport[pos + 12..];
        let num_str = after.split(['-', ';', ',']).next()?;
        return num_str.parse().ok();
    }
    None
}

fn parse_backchannel_control(sdp: &str, base_url: &str) -> Option<String> {
    let mut current_media_is_audio = false;
    let mut current_is_sendonly = false;
    let mut current_control: Option<String> = None;

    for line in sdp.lines() {
        let line = line.trim();
        if line.starts_with("m=audio") {
            // Check previous section
            if current_media_is_audio && (current_is_sendonly || current_control.as_deref().is_some_and(|c| c.contains("backchannel")))
                && let Some(ctrl) = current_control {
                    return Some(resolve_control_url(&ctrl, base_url));
                }
            current_media_is_audio = true;
            current_is_sendonly = false;
            current_control = None;
        } else if line.starts_with("m=video") {
            if current_media_is_audio && (current_is_sendonly || current_control.as_deref().is_some_and(|c| c.contains("backchannel")))
                && let Some(ctrl) = current_control {
                    return Some(resolve_control_url(&ctrl, base_url));
                }
            current_media_is_audio = false;
        } else if current_media_is_audio {
            if line == "a=sendonly" {
                current_is_sendonly = true;
            } else if let Some(ctrl) = line.strip_prefix("a=control:") {
                current_control = Some(ctrl.trim().to_string());
            }
        }
    }

    if current_media_is_audio && (current_is_sendonly || current_control.as_deref().is_some_and(|c| c.contains("backchannel")))
        && let Some(ctrl) = current_control {
            return Some(resolve_control_url(&ctrl, base_url));
        }

    None
}

fn resolve_control_url(control: &str, base_url: &str) -> String {
    if control.starts_with("rtsp://") {
        control.to_string()
    } else if control == "*" {
        base_url.to_string()
    } else {
        let base_no_query = base_url.split('?').next().unwrap_or(base_url);
        let base = base_no_query.trim_end_matches('/');
        format!("{}/{}", base, control.trim_start_matches('/'))
    }
}

/// Remove `user:pass@` userinfo from an RTSP URL (`rtsp://u:p@host/path` ->
/// `rtsp://host/path`). Cameras like Reolink reject SETUP request lines that
/// embed credentials even though DESCRIBE tolerates them.
fn strip_userinfo(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        // userinfo ends at the first `@` that precedes the first `/`.
        let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
        if let Some(at_pos) = after_scheme[..authority_end].find('@') {
            let host_and_path = &after_scheme[at_pos + 1..];
            return format!("{}{}", &url[..scheme_end + 3], host_and_path);
        }
    }
    url.to_string()
}

async fn read_rtsp_response(
    stream: &mut TcpStream,
) -> Result<(u16, HashMap<String, String>, String)> {
    let mut buf = Vec::new();
    let mut header_end = None;

    while header_end.is_none() {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("Unexpected EOF reading RTSP response");
        }
        buf.extend_from_slice(&chunk[..n]);

        // Discard any interleaved RTP/RTCP binary frames preceding the RTSP response line
        if let Some(rtsp_pos) = find_subsequence(&buf, b"RTSP/1.") {
            if rtsp_pos > 0 {
                buf.drain(..rtsp_pos);
            }
            if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                header_end = Some(pos);
            }
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

    let mut headers = HashMap::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_backchannel_sdp() {
        let sdp = "\
v=0\r\n\
o=- 12345 1 IN IP4 192.0.2.42\r\n\
s=RTSP Session\r\n\
m=video 0 RTP/AVP 96\r\n\
a=control:trackID=0\r\n\
a=rtpmap:96 H264/90000\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=control:trackID=1\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=recvonly\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=control:trackID=2\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendonly\r\n";

        let ctrl = parse_backchannel_control(sdp, "rtsp://192.0.2.42/Preview_01_main");
        assert_eq!(
            ctrl,
            Some("rtsp://192.0.2.42/Preview_01_main/trackID=2".to_string())
        );
    }

    #[test]
    fn test_parse_backchannel_sdp_rtspserver_living() {
        let sdp = "\
v=0\r\n\
o=- 1793567930791778 1 IN IP4 192.0.2.25\r\n\
s=Session streamed by \"rRTSPServer\"\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:track1\r\n\
m=audio 0 RTP/AVP 97\r\n\
a=rtpmap:97 MPEG4-GENERIC/16000\r\n\
a=control:track2\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=sendonly\r\n\
a=control:track3\r\n";

        let ctrl = parse_backchannel_control(sdp, "rtsp://192.0.2.25/ch0_0.h264");
        assert_eq!(
            ctrl,
            Some("rtsp://192.0.2.25/ch0_0.h264/track3".to_string())
        );
    }

    #[test]
    fn test_strip_userinfo() {
        assert_eq!(
            strip_userinfo("rtsp://admin:pass@192.0.2.42:554/Preview_01_main/track3"),
            "rtsp://192.0.2.42:554/Preview_01_main/track3"
        );
        assert_eq!(
            strip_userinfo("rtsp://192.0.2.42:554/Preview_01_main"),
            "rtsp://192.0.2.42:554/Preview_01_main"
        );
        assert_eq!(strip_userinfo("rtsp://admin:pass@192.0.2.42/track3"), "rtsp://192.0.2.42/track3");
        assert_eq!(strip_userinfo("not a url"), "not a url");
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_living_connect() {
        let url = std::env::var("TEST_LIVING_URL").unwrap_or_else(|_| "rtsp://192.0.2.25/ch0_0.h264#backchannel=1".to_string());
        let mut client = OnvifBackchannelClient::new(&url).unwrap();
        let res = client.connect().await;
        println!("Connect result: {:?}", res);
        assert!(res.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_living_send_audio() {
        let url = std::env::var("TEST_LIVING_URL").unwrap_or_else(|_| "rtsp://192.0.2.25/ch0_0.h264#backchannel=1".to_string());
        let mut client = OnvifBackchannelClient::new(&url).unwrap();
        client.connect().await.unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel::<(bytes::Bytes, bool)>(32);
        let sender_handle = tokio::spawn(async move {
            client.run_sender(rx).await
        });

        // 1 second tone
        let mut samples = Vec::new();
        for i in 0..8000 {
            let s = (16000.0 * (2.0 * std::f32::consts::PI * 800.0 * (i as f32) / 8000.0).sin()) as i16;
            samples.push(crate::backchannel::pcm::s16_to_ulaw(s));
        }

        for chunk in samples.chunks(320) {
            tx.send((bytes::Bytes::copy_from_slice(chunk), true)).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }

        drop(tx);
        let res = sender_handle.await.unwrap();
        println!("Sender result: {:?}", res);
        assert!(res.is_ok());
    }
}
