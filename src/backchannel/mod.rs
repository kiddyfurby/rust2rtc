#![allow(dead_code)]

pub mod multitrans;
pub mod pcm;
pub mod reolink;

use anyhow::{bail, Result};
use bytes::Bytes;
use multitrans::MultitransClient;
use reolink::OnvifBackchannelClient;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackchannelKind {
    Onvif(String),
    Multitrans(String),
}

/// Active Two-Way Audio Backchannel Session
pub struct BackchannelSession {
    pub kind: BackchannelKind,
    tx: mpsc::Sender<(Bytes, bool)>, // (payload, is_pcmu)
    handle: JoinHandle<()>,
}

impl BackchannelSession {
    /// Send audio payload to the backchannel speaker.
    /// `is_pcmu`: true if the audio is PCMU, false if PCMA.
    pub async fn send_audio(&self, payload: Bytes, is_pcmu: bool) -> Result<()> {
        if let Err(e) = self.tx.send((payload, is_pcmu)).await {
            bail!("Backchannel sender channel closed: {}", e);
        }
        Ok(())
    }

    /// Try send without awaiting
    pub fn try_send_audio(&self, payload: Bytes, is_pcmu: bool) -> Result<()> {
        if let Err(e) = self.tx.try_send((payload, is_pcmu)) {
            bail!("Backchannel try_send failed: {}", e);
        }
        Ok(())
    }
}

impl Drop for BackchannelSession {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Detect backchannel configuration from stream sources
pub fn detect_backchannel(sources: &[String]) -> Option<BackchannelKind> {
    for source in sources {
        if source.starts_with("multitrans://") {
            return Some(BackchannelKind::Multitrans(source.clone()));
        }
        if source.starts_with("onvif://") || source.contains("#backchannel=1") {
            return Some(BackchannelKind::Onvif(source.clone()));
        }
    }
    None
}

/// Start a live backchannel session for a given stream's sources
pub async fn start_backchannel(sources: &[String]) -> Result<Option<BackchannelSession>> {
    let kind = match detect_backchannel(sources) {
        Some(k) => k,
        None => return Ok(None),
    };

    let (tx, rx) = mpsc::channel::<(Bytes, bool)>(64);

    match &kind {
        BackchannelKind::Multitrans(url) => {
            info!(source = %url, "Starting Tapo MULTITRANS backchannel session");
            let mut client = MultitransClient::new(url)?;
            client.connect().await?;

            let handle = tokio::spawn(async move {
                if let Err(e) = client.run_sender(rx).await {
                    warn!("MULTITRANS backchannel worker exited: {}", e);
                }
            });

            Ok(Some(BackchannelSession {
                kind,
                tx,
                handle,
            }))
        }
        BackchannelKind::Onvif(url) => {
            info!(source = %url, "Starting Reolink/ONVIF backchannel session");
            let mut client = OnvifBackchannelClient::new(url)?;
            client.connect().await?;

            let handle = tokio::spawn(async move {
                if let Err(e) = client.run_sender(rx).await {
                    warn!("ONVIF backchannel worker exited: {}", e);
                }
            });

            Ok(Some(BackchannelSession {
                kind,
                tx,
                handle,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_backchannel() {
        let s1 = vec![
            "rtsp://admin:pass@192.168.1.10/main".to_string(),
            "multitrans://admin:pass@192.168.1.10:554/".to_string(),
        ];
        assert_eq!(
            detect_backchannel(&s1),
            Some(BackchannelKind::Multitrans("multitrans://admin:pass@192.168.1.10:554/".to_string()))
        );

        let s2 = vec![
            "onvif://admin:pass@192.168.1.42:8000".to_string(),
            "ffmpeg:doorbell#audio=opus".to_string(),
        ];
        assert_eq!(
            detect_backchannel(&s2),
            Some(BackchannelKind::Onvif("onvif://admin:pass@192.168.1.42:8000".to_string()))
        );

        let s3 = vec!["rtsp://192.168.1.50/cam".to_string()];
        assert_eq!(detect_backchannel(&s3), None);
    }
}
