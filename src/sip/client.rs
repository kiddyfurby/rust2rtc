#![allow(dead_code)]

use super::message::{
    build_sdp, compute_digest_response, compute_digest_response_qop, extract_param,
    parse_sdp_media, SipMessage, SipMethod,
};
use super::session::SipCallSession;
use crate::streams::Stream;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct SipAccountConfig {
    pub stream_name: String,
    pub server: String,
    pub domain: String,
    pub extension: String,
    pub auth_user: String,
    pub password: String,
    pub display_name: String,
    pub ring_group: Option<String>,
    pub auto_answer: bool,
    pub local_ip: String,
}

pub struct SipCameraAccount {
    pub config: SipAccountConfig,
    pub socket: Arc<UdpSocket>,
    stream: Arc<Stream>,
    active_call: Arc<Mutex<Option<SipCallSession>>>,
    pending_responses: Arc<Mutex<HashMap<String, mpsc::Sender<SipMessage>>>>,
    cseq: AtomicU32,
    local_tag: String,
}

impl SipCameraAccount {
    pub async fn new(config: SipAccountConfig, stream: Arc<Stream>) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind SIP UDP socket")?;
        let local_addr = socket.local_addr()?;
        info!(
            stream = %config.stream_name,
            ext = %config.extension,
            port = local_addr.port(),
            "SIP Camera UA initialized"
        );

        let local_tag = uuid::Uuid::new_v4().to_string()[..8].to_string();

        Ok(Self {
            config,
            socket: Arc::new(socket),
            stream,
            active_call: Arc::new(Mutex::new(None)),
            pending_responses: Arc::new(Mutex::new(HashMap::new())),
            cseq: AtomicU32::new(1),
            local_tag,
        })
    }

    /// Run the background listener loop and periodic registration
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let account = Arc::clone(&self);

        // 1. Spawn Registration Loop
        tokio::spawn(async move {
            loop {
                if let Err(e) = account.register().await {
                    warn!(stream = %account.config.stream_name, "SIP registration error: {}", e);
                }
                tokio::time::sleep(Duration::from_secs(120)).await;
            }
        });

        // 2. Incoming UDP packet listener
        let mut buf = [0u8; 4096];
        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, peer_addr)) => {
                    let raw = String::from_utf8_lossy(&buf[..len]);
                    match SipMessage::parse(&raw) {
                        Ok(msg) => {
                            let account = Arc::clone(&self);
                            tokio::spawn(async move {
                                if let Err(e) = account.handle_message(msg, peer_addr).await {
                                    debug!("SIP handle message error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            debug!("Ignored invalid SIP packet from {}: {}", peer_addr, e);
                        }
                    }
                }
                Err(e) => {
                    error!("SIP UDP socket error: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Perform SIP registration with digest authentication challenge
    pub async fn register(&self) -> Result<()> {
        let server_addr: SocketAddr = tokio::net::lookup_host(&self.config.server)
            .await?
            .next()
            .context("Failed to resolve SIP server address")?;

        let local_port = self.socket.local_addr()?.port();
        let reg_user = if !self.config.auth_user.is_empty() {
            &self.config.auth_user
        } else {
            &self.config.extension
        };
        let from_uri = format!("sip:{}@{}", reg_user, self.config.domain);
        let to_uri = from_uri.clone();
        let contact = format!("sip:{}@{}:{}", reg_user, self.config.local_ip, local_port);
        let call_id = format!("reg-{}@{}", uuid::Uuid::new_v4(), self.config.local_ip);

        // Register a channel to receive SIP responses routed from the main UDP listener
        let (tx, mut rx) = mpsc::channel::<SipMessage>(8);
        self.pending_responses.lock().await.insert(call_id.clone(), tx);

        let seq = self.cseq.fetch_add(1, Ordering::SeqCst);
        let branch = format!("z9hG4bK-{}", uuid::Uuid::new_v4());

        // Step 1: Initial REGISTER
        let mut reg_msg = SipMessage::new_request(SipMethod::Register, &format!("sip:{}", self.config.domain));
        reg_msg.set_header("Via", &format!("SIP/2.0/UDP {}:{};branch={};rport", self.config.local_ip, local_port, branch));
        reg_msg.set_header("From", &format!("<{}>;tag={}", from_uri, self.local_tag));
        reg_msg.set_header("To", &format!("<{}>", to_uri));
        reg_msg.set_header("Call-ID", &call_id);
        reg_msg.set_header("CSeq", &format!("{} REGISTER", seq));
        reg_msg.set_header("Contact", &format!("<{}>;expires=300", contact));
        reg_msg.set_header("Max-Forwards", "70");
        reg_msg.set_header("User-Agent", "rust2rtc");

        self.socket.send_to(reg_msg.serialize().as_bytes(), server_addr).await?;

        // Wait for response routed by listener (skip provisional 1xx)
        let resp = loop {
            let msg = match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => {
                    self.pending_responses.lock().await.remove(&call_id);
                    bail!("SIP response channel closed");
                }
                Err(_) => {
                    self.pending_responses.lock().await.remove(&call_id);
                    bail!("SIP registration timeout");
                }
            };
            if let Some(code) = msg.status_code {
                if code >= 200 {
                    break msg;
                }
            }
        };

        if resp.status_code == Some(200) {
            self.pending_responses.lock().await.remove(&call_id);
            info!(stream = %self.config.stream_name, ext = %self.config.extension, "SIP registered successfully");
            return Ok(());
        }

        if resp.status_code == Some(401) || resp.status_code == Some(407) {
            let auth_hdr = resp
                .get_header("www-authenticate")
                .or_else(|| resp.get_header("proxy-authenticate"))
                .context("Missing WWW-Authenticate in 401 response")?;

            let realm = extract_param(auth_hdr, "realm").unwrap_or_else(|| self.config.domain.clone());
            let nonce = extract_param(auth_hdr, "nonce").context("Missing nonce in 401 response")?;
            let qop = extract_param(auth_hdr, "qop");
            let opaque = extract_param(auth_hdr, "opaque");
            let algorithm = extract_param(auth_hdr, "algorithm");
            let uri = format!("sip:{}", self.config.domain);

            info!(
                stream = %self.config.stream_name,
                auth_hdr = %auth_hdr,
                realm = %realm,
                nonce = %nonce,
                qop = ?qop,
                algorithm = ?algorithm,
                "Received 401/407 authentication challenge from SIP server"
            );

            let auth_val = if let Some(_qop_val) = qop.as_deref().filter(|q| q.contains("auth")) {
                let nc = "00000001";
                let cnonce = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let digest_resp = compute_digest_response_qop(
                    &self.config.auth_user,
                    &realm,
                    &self.config.password,
                    "REGISTER",
                    &uri,
                    &nonce,
                    "auth",
                    nc,
                    &cnonce,
                );
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}", qop=auth, nc={}, cnonce="{}""#,
                    self.config.auth_user, realm, nonce, uri, digest_resp, nc, cnonce
                );
                if let Some(op) = opaque {
                    val.push_str(&format!(r#", opaque="{}""#, op));
                }
                if let Some(alg) = algorithm {
                    val.push_str(&format!(r#", algorithm={}"#, alg));
                }
                val
            } else {
                let digest_resp = compute_digest_response(
                    &self.config.auth_user,
                    &realm,
                    &self.config.password,
                    "REGISTER",
                    &uri,
                    &nonce,
                );
                let mut val = format!(
                    r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}""#,
                    self.config.auth_user, realm, nonce, uri, digest_resp
                );
                if let Some(op) = opaque {
                    val.push_str(&format!(r#", opaque="{}""#, op));
                }
                if let Some(alg) = algorithm {
                    val.push_str(&format!(r#", algorithm={}"#, alg));
                }
                val
            };

            info!(
                stream = %self.config.stream_name,
                auth_val = %auth_val,
                "Sending authenticated REGISTER"
            );

            let seq2 = self.cseq.fetch_add(1, Ordering::SeqCst);
            let branch2 = format!("z9hG4bK-{}", uuid::Uuid::new_v4());

            let mut auth_reg = SipMessage::new_request(SipMethod::Register, &uri);
            auth_reg.set_header("Via", &format!("SIP/2.0/UDP {}:{};branch={};rport", self.config.local_ip, local_port, branch2));
            auth_reg.set_header("From", &format!("<{}>;tag={}", from_uri, self.local_tag));
            auth_reg.set_header("To", &format!("<{}>", to_uri));
            auth_reg.set_header("Call-ID", &call_id);
            auth_reg.set_header("CSeq", &format!("{} REGISTER", seq2));
            auth_reg.set_header("Contact", &format!("<{}>;expires=300", contact));
            let auth_hdr_name = if resp.status_code == Some(407) {
                "Proxy-Authorization"
            } else {
                "Authorization"
            };
            auth_reg.set_header(auth_hdr_name, &auth_val);
            auth_reg.set_header("Max-Forwards", "70");
            auth_reg.set_header("User-Agent", "rust2rtc");

            self.socket.send_to(auth_reg.serialize().as_bytes(), server_addr).await?;

            let resp2 = loop {
                let msg = match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                    Ok(Some(m)) => m,
                    Ok(None) => {
                        self.pending_responses.lock().await.remove(&call_id);
                        bail!("SIP response channel closed");
                    }
                    Err(_) => {
                        self.pending_responses.lock().await.remove(&call_id);
                        bail!("SIP authenticated registration timeout");
                    }
                };
                if let Some(code) = msg.status_code {
                    if code >= 200 {
                        break msg;
                    }
                }
            };

            self.pending_responses.lock().await.remove(&call_id);

            if resp2.status_code == Some(200) {
                info!(stream = %self.config.stream_name, ext = %self.config.extension, "SIP registered successfully with auth");
                return Ok(());
            } else if resp2.status_code == Some(401) || resp2.status_code == Some(403) {
                bail!(
                    "SIP registration rejected with status {:?} (check extension '{}', auth_user '{}', and password in rust2rtc.yaml)",
                    resp2.status_code, self.config.extension, self.config.auth_user
                );
            } else {
                bail!("SIP registration rejected with status {:?}", resp2.status_code);
            }
        }

        self.pending_responses.lock().await.remove(&call_id);
        bail!("SIP unexpected response: {:?}", resp.status_code);
    }

    /// Handle incoming SIP request or response
    async fn handle_message(&self, msg: SipMessage, peer_addr: SocketAddr) -> Result<()> {
        if msg.is_response {
            debug!(code = ?msg.status_code, "Received SIP response");
            if let Some(call_id) = msg.get_header("call-id") {
                let pending = self.pending_responses.lock().await;
                if let Some(tx) = pending.get(call_id.trim()) {
                    let _ = tx.send(msg).await;
                    return Ok(());
                }
            }
            return Ok(());
        }

        match msg.method {
            Some(SipMethod::Invite) => self.handle_invite(msg, peer_addr).await,
            Some(SipMethod::Ack) => {
                debug!("Received SIP ACK - Call connected");
                Ok(())
            }
            Some(SipMethod::Bye) => self.handle_bye(msg, peer_addr).await,
            Some(SipMethod::Cancel) => self.handle_cancel(msg, peer_addr).await,
            Some(SipMethod::Options) => self.handle_options(msg, peer_addr).await,
            _ => {
                debug!("Unhandled SIP method: {:?}", msg.method);
                Ok(())
            }
        }
    }

    /// Handle incoming call (e.g. from ESPHome intercom dialing doorbell)
    async fn handle_invite(&self, req: SipMessage, peer_addr: SocketAddr) -> Result<()> {
        let call_id = req.get_header("call-id").unwrap_or("unknown").to_string();
        info!(
            call_id = %call_id,
            from = ?req.get_header("from"),
            stream = %self.config.stream_name,
            "Incoming SIP call received"
        );

        // 1. Send 100 Trying
        let trying = self.create_reply(&req, 100, "Trying");
        self.socket.send_to(trying.serialize().as_bytes(), peer_addr).await?;

        // 2. Send 180 Ringing
        let ringing = self.create_reply(&req, 180, "Ringing");
        self.socket.send_to(ringing.serialize().as_bytes(), peer_addr).await?;

        // 3. Parse SDP Media from caller
        let media_info = parse_sdp_media(&req.body);
        let remote_ip = media_info
            .connection_ip
            .unwrap_or_else(|| peer_addr.ip().to_string());

        let remote_audio_addr = match media_info.audio_port {
            Some(port) => format!("{}:{}", remote_ip, port).parse::<SocketAddr>()?,
            None => bail!("Caller SDP did not specify audio port"),
        };

        let remote_video_addr = media_info
            .video_port
            .and_then(|p| format!("{}:{}", remote_ip, p).parse::<SocketAddr>().ok());

        // 4. Start media bridge session
        let session = SipCallSession::start(
            call_id.clone(),
            Arc::clone(&self.stream),
            &self.config.local_ip,
            remote_audio_addr,
            remote_video_addr,
        )
        .await?;

        let audio_port = session.local_audio_port;
        let video_port = session.local_video_port;

        // Store active call
        let mut active = self.active_call.lock().await;
        *active = Some(session);

        // 5. Send 200 OK with Answer SDP
        let answer_sdp = build_sdp(&self.config.local_ip, audio_port, video_port);
        let local_port = self.socket.local_addr()?.port();
        let contact = format!("sip:{}@{}:{}", self.config.extension, self.config.local_ip, local_port);

        let mut ok_resp = self.create_reply(&req, 200, "OK");
        ok_resp.set_header("Contact", &format!("<{}>", contact));
        ok_resp.set_header("Content-Type", "application/sdp");
        ok_resp.set_header("Content-Length", &answer_sdp.len().to_string());
        ok_resp.body = answer_sdp;

        self.socket.send_to(ok_resp.serialize().as_bytes(), peer_addr).await?;
        info!(call_id = %call_id, "SIP call answered with 200 OK");

        Ok(())
    }

    /// Handle BYE (call hung up)
    async fn handle_bye(&self, req: SipMessage, peer_addr: SocketAddr) -> Result<()> {
        let call_id = req.get_header("call-id").unwrap_or("unknown");
        info!(call_id = %call_id, "SIP call terminated by peer (BYE)");

        let mut active = self.active_call.lock().await;
        if let Some(mut session) = active.take() {
            session.stop().await;
        }

        let ok_resp = self.create_reply(&req, 200, "OK");
        self.socket.send_to(ok_resp.serialize().as_bytes(), peer_addr).await?;
        Ok(())
    }

    /// Handle CANCEL
    async fn handle_cancel(&self, req: SipMessage, peer_addr: SocketAddr) -> Result<()> {
        let ok_resp = self.create_reply(&req, 200, "OK");
        self.socket.send_to(ok_resp.serialize().as_bytes(), peer_addr).await?;

        let mut active = self.active_call.lock().await;
        if let Some(mut session) = active.take() {
            session.stop().await;
        }
        Ok(())
    }

    /// Handle OPTIONS (keepalive ping)
    async fn handle_options(&self, req: SipMessage, peer_addr: SocketAddr) -> Result<()> {
        let ok_resp = self.create_reply(&req, 200, "OK");
        self.socket.send_to(ok_resp.serialize().as_bytes(), peer_addr).await?;
        Ok(())
    }

    /// Trigger an outbound call to an extension or ring group (e.g. Doorbell button pressed)
    pub async fn trigger_outbound_call(&self, target_extension: &str) -> Result<()> {
        let server_addr: SocketAddr = tokio::net::lookup_host(&self.config.server)
            .await?
            .next()
            .context("Failed to resolve SIP server address")?;

        let local_port = self.socket.local_addr()?.port();
        let from_uri = format!("sip:{}@{}", self.config.extension, self.config.domain);
        let to_uri = format!("sip:{}@{}", target_extension, self.config.domain);
        let contact = format!("sip:{}@{}:{}", self.config.extension, self.config.local_ip, local_port);
        let call_id = format!("call-{}@{}", uuid::Uuid::new_v4(), self.config.local_ip);

        let seq = self.cseq.fetch_add(1, Ordering::SeqCst);
        let branch = format!("z9hG4bK-{}", uuid::Uuid::new_v4());

        // Temporary audio port for offer
        let dummy_audio_port = 20000;
        let offer_sdp = build_sdp(&self.config.local_ip, dummy_audio_port, None);

        let mut invite = SipMessage::new_request(SipMethod::Invite, &to_uri);
        invite.set_header("Via", &format!("SIP/2.0/UDP {}:{};branch={};rport", self.config.local_ip, local_port, branch));
        invite.set_header("From", &format!(r#""{}" <{}>;tag={}"#, self.config.display_name, from_uri, self.local_tag));
        invite.set_header("To", &format!("<{}>", to_uri));
        invite.set_header("Call-ID", &call_id);
        invite.set_header("CSeq", &format!("{} INVITE", seq));
        invite.set_header("Contact", &format!("<{}>", contact));
        invite.set_header("Content-Type", "application/sdp");
        invite.set_header("Content-Length", &offer_sdp.len().to_string());
        invite.set_header("Max-Forwards", "70");
        invite.set_header("User-Agent", "rust2rtc");
        invite.body = offer_sdp;

        info!(
            target = %target_extension,
            stream = %self.config.stream_name,
            "Calling ESPHome / Ring Group via SIP"
        );
        self.socket.send_to(invite.serialize().as_bytes(), server_addr).await?;

        Ok(())
    }

    fn create_reply(&self, req: &SipMessage, code: u16, reason: &str) -> SipMessage {
        let mut resp = SipMessage::new_response(code, reason);
        if let Some(via) = req.get_header("via") {
            resp.set_header("Via", via);
        }
        if let Some(from) = req.get_header("from") {
            resp.set_header("From", from);
        }
        if let Some(to) = req.get_header("to") {
            if to.contains("tag=") {
                resp.set_header("To", to);
            } else {
                resp.set_header("To", &format!("{};tag={}", to, self.local_tag));
            }
        }
        if let Some(call_id) = req.get_header("call-id") {
            resp.set_header("Call-ID", call_id);
        }
        if let Some(cseq) = req.get_header("cseq") {
            resp.set_header("CSeq", cseq);
        }
        resp.set_header("User-Agent", "rust2rtc");
        resp
    }
}
