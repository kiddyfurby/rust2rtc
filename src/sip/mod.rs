#![allow(dead_code)]

pub mod client;
pub mod ha;
pub mod message;
pub mod session;

use anyhow::Result;
use client::{SipAccountConfig, SipAccountStatus, SipCameraAccount};
use crate::ffmpeg::FfmpegManager;
use crate::streams::StreamManager;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

pub struct SipManager {
    accounts: RwLock<HashMap<String, Arc<SipCameraAccount>>>,
    stream_manager: Arc<StreamManager>,
    ffmpeg: Arc<FfmpegManager>,
    rtsp_port: u16,
    used_extensions: Arc<Mutex<HashSet<String>>>,
}

impl SipManager {
    pub fn new(
        stream_manager: Arc<StreamManager>,
        ffmpeg: Arc<FfmpegManager>,
        rtsp_port: u16,
    ) -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
            stream_manager,
            ffmpeg,
            rtsp_port,
            used_extensions: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Add and start a SIP camera account
    pub async fn add_account(&self, config: SipAccountConfig) -> Result<()> {
        {
            let mut used = self.used_extensions.lock().await;
            used.insert(config.extension.clone());
        }
        let stream = self.stream_manager.get_or_create(&config.stream_name).await;
        let account = Arc::new(
            SipCameraAccount::new(
                config.clone(),
                stream,
                self.rtsp_port,
                Arc::clone(&self.ffmpeg),
                Arc::clone(&self.used_extensions),
            )
            .await?,
        );

        let stream_name = config.stream_name.clone();
        let account_clone = Arc::clone(&account);
        tokio::spawn(async move {
            if let Err(e) = account_clone.run().await {
                warn!(stream = %stream_name, "SIP account runner exited: {}", e);
            }
        });

        let mut accounts = self.accounts.write().await;
        accounts.insert(config.stream_name.clone(), account);
        info!(
            stream = %config.stream_name,
            ext = %config.extension,
            auth_user = %config.auth_user,
            "Added SIP Camera UA"
        );

        Ok(())
    }

    /// Trigger an outbound call (doorbell button press -> ring ESPHome)
    pub async fn trigger_call(&self, stream_name: &str, target_ext: Option<&str>) -> Result<()> {
        let accounts = self.accounts.read().await;
        let account = accounts
            .get(stream_name)
            .ok_or_else(|| anyhow::anyhow!("No SIP account registered for stream '{}'", stream_name))?;

        let destination = target_ext
            .map(|s| s.to_string())
            .or_else(|| account.config.ring_group.clone())
            .ok_or_else(|| anyhow::anyhow!("No target extension or ring_group specified for '{}'", stream_name))?;

        account.trigger_outbound_call(&destination).await
    }

    /// List active SIP account statuses
    pub async fn list_accounts(&self) -> Vec<SipAccountStatus> {
        let accounts = self.accounts.read().await;
        let mut statuses = Vec::with_capacity(accounts.len());
        for acc in accounts.values() {
            statuses.push(acc.status().await);
        }
        statuses.sort_by(|a, b| a.extension.cmp(&b.extension));
        statuses
    }
}
