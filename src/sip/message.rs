#![allow(dead_code)]

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipMethod {
    Register,
    Invite,
    Ack,
    Bye,
    Cancel,
    Options,
    Other(String),
}

impl SipMethod {
    pub fn as_str(&self) -> &str {
        match self {
            SipMethod::Register => "REGISTER",
            SipMethod::Invite => "INVITE",
            SipMethod::Ack => "ACK",
            SipMethod::Bye => "BYE",
            SipMethod::Cancel => "CANCEL",
            SipMethod::Options => "OPTIONS",
            SipMethod::Other(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "REGISTER" => SipMethod::Register,
            "INVITE" => SipMethod::Invite,
            "ACK" => SipMethod::Ack,
            "BYE" => SipMethod::Bye,
            "CANCEL" => SipMethod::Cancel,
            "OPTIONS" => SipMethod::Options,
            other => SipMethod::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SipMessage {
    pub is_response: bool,
    pub method: Option<SipMethod>,
    pub uri: Option<String>,
    pub status_code: Option<u16>,
    pub reason_phrase: Option<String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl SipMessage {
    pub fn new_request(method: SipMethod, uri: &str) -> Self {
        Self {
            is_response: false,
            method: Some(method),
            uri: Some(uri.to_string()),
            status_code: None,
            reason_phrase: None,
            headers: HashMap::new(),
            body: String::new(),
        }
    }

    pub fn new_response(status_code: u16, reason_phrase: &str) -> Self {
        Self {
            is_response: true,
            method: None,
            uri: None,
            status_code: Some(status_code),
            reason_phrase: Some(reason_phrase.to_string()),
            headers: HashMap::new(),
            body: String::new(),
        }
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers.insert(name.to_lowercase(), value.to_string());
    }

    pub fn get_header(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        // Check standard and short header aliases
        let alias = match lower.as_str() {
            "from" | "f" => Some("from"),
            "to" | "t" => Some("to"),
            "call-id" | "i" => Some("call-id"),
            "cseq" => Some("cseq"),
            "contact" | "m" => Some("contact"),
            "via" | "v" => Some("via"),
            "content-type" | "c" => Some("content-type"),
            "content-length" | "l" => Some("content-length"),
            _ => None,
        };

        if let Some(target) = alias {
            if let Some(val) = self.headers.get(target) {
                return Some(val.as_str());
            }
        }
        self.headers.get(&lower).map(|s| s.as_str())
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let (header_part, body_part) = match raw.split_once("\r\n\r\n") {
            Some((h, b)) => (h, b),
            None => match raw.split_once("\n\n") {
                Some((h, b)) => (h, b),
                None => (raw, ""),
            },
        };

        let mut lines = header_part.lines();
        let start_line = lines.next().context("Empty SIP message")?.trim();

        let mut msg = if start_line.starts_with("SIP/2.0") {
            // Response: SIP/2.0 200 OK
            let mut parts = start_line.split_whitespace();
            let _proto = parts.next();
            let code: u16 = parts.next().context("Missing status code")?.parse()?;
            let reason = parts.collect::<Vec<_>>().join(" ");
            SipMessage::new_response(code, &reason)
        } else {
            // Request: INVITE sip:100@host SIP/2.0
            let mut parts = start_line.split_whitespace();
            let method_str = parts.next().context("Missing SIP method")?;
            let uri = parts.next().context("Missing SIP URI")?;
            let method = SipMethod::from_str(method_str);
            SipMessage::new_request(method, uri)
        };

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                msg.set_header(k.trim(), v.trim());
            }
        }

        msg.body = body_part.to_string();
        Ok(msg)
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        if self.is_response {
            out.push_str(&format!(
                "SIP/2.0 {} {}\r\n",
                self.status_code.unwrap_or(200),
                self.reason_phrase.as_deref().unwrap_or("OK")
            ));
        } else {
            out.push_str(&format!(
                "{} {} SIP/2.0\r\n",
                self.method.as_ref().map(|m| m.as_str()).unwrap_or("INVITE"),
                self.uri.as_deref().unwrap_or("sip:unknown")
            ));
        }

        for (k, v) in &self.headers {
            // Capitalize headers nicely
            let cap_key = format_header_key(k);
            out.push_str(&format!("{}: {}\r\n", cap_key, v));
        }

        if !self.headers.contains_key("content-length") {
            out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }

        out.push_str("\r\n");
        out.push_str(&self.body);
        out
    }

    /// Extract tag parameter from From or To header
    pub fn get_tag(&self, header_name: &str) -> Option<String> {
        let val = self.get_header(header_name)?;
        extract_param(val, "tag=")
    }

    /// Extract branch from Via header
    pub fn get_branch(&self) -> Option<String> {
        let via = self.get_header("via")?;
        extract_param(via, "branch=")
    }

    /// Extract CSeq number and method
    pub fn get_cseq(&self) -> Option<(u32, String)> {
        let cseq = self.get_header("cseq")?;
        let mut parts = cseq.split_whitespace();
        let num: u32 = parts.next()?.parse().ok()?;
        let method = parts.next()?.to_string();
        Some((num, method))
    }
}

fn format_header_key(k: &str) -> String {
    match k {
        "call-id" => "Call-ID".to_string(),
        "cseq" => "CSeq".to_string(),
        "www-authenticate" => "WWW-Authenticate".to_string(),
        "content-type" => "Content-Type".to_string(),
        "content-length" => "Content-Length".to_string(),
        "max-forwards" => "Max-Forwards".to_string(),
        "user-agent" => "User-Agent".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        }
    }
}

pub fn extract_param(header: &str, param_name: &str) -> Option<String> {
    let target = param_name.trim_end_matches('=').to_lowercase();
    let header_lower = header.to_lowercase();
    let mut search_from = 0;

    while let Some(rel_pos) = header_lower[search_from..].find(&target) {
        let pos = search_from + rel_pos;
        let before_ok = pos == 0 || {
            let prev = header.as_bytes()[pos - 1];
            prev == b' ' || prev == b',' || prev == b';' || prev == b'\t'
        };
        let after_idx = pos + target.len();
        if before_ok && after_idx < header.len() {
            let rem = &header[after_idx..];
            let trimmed = rem.trim_start();
            if trimmed.starts_with('=') {
                let val_part = trimmed[1..].trim_start();
                if val_part.starts_with('"') {
                    // Quoted string: extract until closing quote
                    let content = &val_part[1..];
                    if let Some(end_quote) = content.find('"') {
                        return Some(content[..end_quote].to_string());
                    }
                } else {
                    // Token: ends at comma, semicolon, space, or end of string
                    let end = val_part
                        .find([',', ';', ' ', '\r', '\n'])
                        .unwrap_or(val_part.len());
                    return Some(val_part[..end].to_string());
                }
            }
        }
        search_from = pos + target.len();
    }
    None
}

/// Compute HTTP / SIP MD5 Digest response (without qop)
pub fn compute_digest_response(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
) -> String {
    let ha1 = hex_md5(&format!("{}:{}:{}", username, realm, password));
    let ha2 = hex_md5(&format!("{}:{}", method, uri));
    hex_md5(&format!("{}:{}:{}", ha1, nonce, ha2))
}

/// Compute HTTP / SIP MD5 Digest response with qop (auth)
pub fn compute_digest_response_qop(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    qop: &str,
    nc: &str,
    cnonce: &str,
) -> String {
    let ha1 = hex_md5(&format!("{}:{}:{}", username, realm, password));
    let ha2 = hex_md5(&format!("{}:{}", method, uri));
    hex_md5(&format!("{}:{}:{}:{}:{}:{}", ha1, nonce, nc, cnonce, qop, ha2))
}

fn hex_md5(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Helper to parse remote RTP port and host from SDP
#[derive(Debug, Clone)]
pub struct SdpMediaInfo {
    pub audio_port: Option<u16>,
    pub audio_payload_types: Vec<u8>,
    pub video_port: Option<u16>,
    pub video_payload_types: Vec<u8>,
    pub h264_payload_type: Option<u8>,
    pub connection_ip: Option<String>,
}

impl SdpMediaInfo {
    /// Pick the preferred G.711 payload type (0 = PCMU, 8 = PCMA) offered by the peer
    pub fn preferred_g711_pt(&self) -> u8 {
        for &pt in &self.audio_payload_types {
            if pt == 0 || pt == 8 {
                return pt;
            }
        }
        0
    }

    /// Pick the negotiated H.264 video payload type offered by the peer (e.g. 103, 96, etc.)
    pub fn preferred_h264_pt(&self) -> u8 {
        if let Some(pt) = self.h264_payload_type {
            return pt;
        }
        if let Some(&pt) = self.video_payload_types.first() {
            return pt;
        }
        96
    }
}

pub fn parse_sdp_media(sdp: &str) -> SdpMediaInfo {
    let mut info = SdpMediaInfo {
        audio_port: None,
        audio_payload_types: Vec::new(),
        video_port: None,
        video_payload_types: Vec::new(),
        h264_payload_type: None,
        connection_ip: None,
    };

    let mut global_c_ip = None;

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(c) = line.strip_prefix("c=IN IP4 ") {
            let ip = c.trim();
            if !ip.is_empty() && ip != "0.0.0.0" {
                global_c_ip = Some(ip.to_string());
            }
        } else if let Some(m) = line.strip_prefix("m=audio ") {
            let mut parts = m.split_whitespace();
            let port_str = parts.next().unwrap_or("0");
            if let Ok(p) = port_str.parse::<u16>() {
                if p > 0 {
                    info.audio_port = Some(p);
                }
            }
            // Skip transport (e.g., RTP/AVP)
            let _transport = parts.next();
            for pt_str in parts {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    info.audio_payload_types.push(pt);
                }
            }
        } else if let Some(m) = line.strip_prefix("m=video ") {
            let mut parts = m.split_whitespace();
            let port_str = parts.next().unwrap_or("0");
            if let Ok(p) = port_str.parse::<u16>() {
                if p > 0 {
                    info.video_port = Some(p);
                }
            }
            // Skip transport (e.g., RTP/AVP)
            let _transport = parts.next();
            for pt_str in parts {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    info.video_payload_types.push(pt);
                }
            }
        } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            // e.g. a=rtpmap:103 H264/90000
            let mut parts = rtpmap.split_whitespace();
            if let Some(pt_str) = parts.next() {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    if let Some(codec) = parts.next() {
                        if codec.to_ascii_uppercase().starts_with("H264/") && info.h264_payload_type.is_none() {
                            info.h264_payload_type = Some(pt);
                        }
                    }
                }
            }
        }
    }

    info.connection_ip = global_c_ip;
    info
}

/// Generate SDP for 200 OK or INVITE
pub fn build_sdp(
    local_ip: &str,
    audio_port: u16,
    audio_pt: u8,
    video_port: Option<u16>,
    video_pt: u8,
    sprop_parameter_sets: Option<&str>,
) -> String {
    let audio_section = if audio_pt == 8 {
        format!(
            "m=audio {} RTP/AVP 8 0\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=ptime:20\r\n\
            a=sendrecv\r\n",
            audio_port
        )
    } else {
        format!(
            "m=audio {} RTP/AVP 0 8\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=ptime:20\r\n\
            a=sendrecv\r\n",
            audio_port
        )
    };

    let mut sdp = format!(
        "v=0\r\n\
        o=rust2rtc 1000 1 IN IP4 {}\r\n\
        s=Talk\r\n\
        c=IN IP4 {}\r\n\
        t=0 0\r\n\
        {}",
        local_ip, local_ip, audio_section
    );

    if let Some(v_port) = video_port {
        let sprop_fmtp = if let Some(sprop) = sprop_parameter_sets {
            format!(";sprop-parameter-sets={}", sprop)
        } else {
            String::new()
        };
        sdp.push_str(&format!(
            "m=video {} RTP/AVP {}\r\n\
            a=rtpmap:{} H264/90000\r\n\
            a=fmtp:{} profile-level-id=42801f;packetization-mode=1{}\r\n\
            a=sendrecv\r\n",
            v_port, video_pt, video_pt, video_pt, sprop_fmtp
        ));
    }

    sdp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sip_request() {
        let raw = "\
INVITE sip:100@192.168.1.10:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK-12345\r\n\
From: <sip:esphome@homeassistant.local>;tag=abcde\r\n\
To: <sip:100@homeassistant.local>\r\n\
Call-ID: call-998877@192.168.1.50\r\n\
CSeq: 101 INVITE\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 15\r\n\
\r\n\
v=0\r\nm=audio ...";

        let msg = SipMessage::parse(raw).unwrap();
        assert!(!msg.is_response);
        assert_eq!(msg.method, Some(SipMethod::Invite));
        assert_eq!(msg.get_header("From"), Some("<sip:esphome@homeassistant.local>;tag=abcde"));
        assert_eq!(msg.get_tag("from"), Some("abcde".to_string()));
        assert_eq!(msg.get_branch(), Some("z9hG4bK-12345".to_string()));
        assert_eq!(msg.get_cseq(), Some((101, "INVITE".to_string())));
        assert_eq!(msg.body, "v=0\r\nm=audio ...");
    }

    #[test]
    fn test_digest_calc() {
        let resp = compute_digest_response(
            "doorbell",
            "asterisk",
            "secret",
            "REGISTER",
            "sip:192.168.1.100",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
        );
        assert_eq!(resp.len(), 32);
    }

    #[test]
    fn test_parse_sdp_media() {
        let sdp = "\
v=0\r\n\
o=- 1 1 IN IP4 192.168.1.200\r\n\
s=SIP Call\r\n\
c=IN IP4 192.168.1.200\r\n\
t=0 0\r\n\
m=audio 16384 RTP/AVP 0 8\r\n\
a=rtpmap:0 PCMU/8000\r\n\
m=video 16386 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n";

        let info = parse_sdp_media(sdp);
        assert_eq!(info.connection_ip, Some("192.168.1.200".to_string()));
        assert_eq!(info.audio_port, Some(16384));
        assert_eq!(info.video_port, Some(16386));
    }

    #[test]
    fn test_extract_param_and_digest_qop() {
        let auth_hdr = r#"Digest realm="asterisk", nonce="1730412549/3c4155df", algorithm=MD5, qop="auth", opaque="opaque-123""#;
        assert_eq!(extract_param(auth_hdr, "realm"), Some("asterisk".to_string()));
        assert_eq!(extract_param(auth_hdr, "nonce"), Some("1730412549/3c4155df".to_string()));
        assert_eq!(extract_param(auth_hdr, "qop"), Some("auth".to_string()));
        assert_eq!(extract_param(auth_hdr, "opaque"), Some("opaque-123".to_string()));
        assert_eq!(extract_param(auth_hdr, "algorithm"), Some("MD5".to_string()));

        let resp = compute_digest_response_qop(
            "doorbell",
            "asterisk",
            "secret",
            "REGISTER",
            "sip:192.168.1.4",
            "1730412549/3c4155df",
            "auth",
            "00000001",
            "abcdef12",
        );
        assert_eq!(resp.len(), 32);
    }
}
