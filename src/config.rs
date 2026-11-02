use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub streams: HashMap<String, StreamConfig>,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub rtsp: RtspConfig,
    #[serde(default)]
    pub ffmpeg: FfmpegConfig,
    #[serde(default)]
    pub webrtc: Option<WebRtcConfig>,
    #[serde(default)]
    pub sip: Option<SipConfig>,
    #[serde(default)]
    pub ha: Option<HaConfig>,
    #[serde(default)]
    pub doorbells: HashMap<String, DoorbellConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HaConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DoorbellConfig {
    pub entity: String,
    #[serde(default)]
    pub sip_targets: Vec<String>,
    #[serde(default)]
    pub ha_destination: Option<String>,
    #[serde(default = "default_doorbell_cooldown")]
    pub cooldown_seconds: u64,
}

fn default_doorbell_cooldown() -> u64 {
    15
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebRtcConfig {
    #[serde(default)]
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SipConfig {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub local_ip: Option<String>,
    #[serde(default)]
    pub ha_url: Option<String>,
    #[serde(default)]
    pub ha_token: Option<String>,
    #[serde(default)]
    pub auto_register_streams: bool,
    #[serde(default = "default_base_extension")]
    pub base_extension: u16,
    #[serde(default = "default_auto_password")]
    pub auto_password: String,
    #[serde(default)]
    pub accounts: HashMap<String, SipAccountYamlConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipAccountYamlConfig {
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub auth_user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_display_name")]
    pub display_name: String,
    #[serde(default)]
    pub ring_group: Option<String>,
    #[serde(default = "default_auto_answer")]
    pub auto_answer: bool,
}

fn default_base_extension() -> u16 {
    101
}

fn default_auto_password() -> String {
    "MySecretPassword123".to_string()
}

fn default_display_name() -> String {
    "Camera Doorbell".to_string()
}

fn default_auto_answer() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            streams: HashMap::new(),
            api: ApiConfig::default(),
            rtsp: RtspConfig::default(),
            ffmpeg: FfmpegConfig::default(),
            webrtc: None,
            sip: None,
            ha: None,
            doorbells: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamConfig {
    Single(String),
    Multiple(Vec<String>),
}

impl StreamConfig {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StreamConfig::Single(s) => vec![s.clone()],
            StreamConfig::Multiple(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_listen")]
    pub listen: String,
    #[serde(default)]
    pub origin: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: default_api_listen(),
            origin: None,
        }
    }
}

fn default_api_listen() -> String {
    ":1984".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspConfig {
    #[serde(default = "default_rtsp_listen")]
    pub listen: String,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            listen: default_rtsp_listen(),
        }
    }
}

fn default_rtsp_listen() -> String {
    ":8554".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FfmpegConfig {
    #[serde(default = "default_ffmpeg_bin")]
    pub bin: String,
    #[serde(default = "default_ffmpeg_global")]
    pub global: String,
    #[serde(default)]
    pub hardware: Option<String>,
    #[serde(flatten, default)]
    pub templates: HashMap<String, String>,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
            global: default_ffmpeg_global(),
            hardware: None,
            templates: HashMap::new(),
        }
    }
}

fn default_ffmpeg_bin() -> String {
    "ffmpeg".to_string()
}

fn default_ffmpeg_global() -> String {
    "-hide_banner -v error".to_string()
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    pub fn merge(&mut self, other: Config) {
        for (name, stream) in other.streams {
            self.streams.insert(name, stream);
        }
        if other.api.listen != default_api_listen() {
            self.api = other.api;
        }
        if other.rtsp.listen != default_rtsp_listen() {
            self.rtsp = other.rtsp;
        }
        if other.ffmpeg.bin != default_ffmpeg_bin() || other.ffmpeg.global != default_ffmpeg_global() {
            self.ffmpeg = other.ffmpeg;
        }
        if let Some(webrtc) = other.webrtc {
            if let Some(ref mut our_webrtc) = self.webrtc {
                our_webrtc.candidates.extend(webrtc.candidates);
            } else {
                self.webrtc = Some(webrtc);
            }
        }
        if let Some(sip) = other.sip {
            self.sip = Some(sip);
        }
        if let Some(ha) = other.ha {
            self.ha = Some(ha);
        }
        for (name, db) in other.doorbells {
            self.doorbells.insert(name, db);
        }
    }

    pub fn ha_url(&self) -> Option<String> {
        self.ha
            .as_ref()
            .and_then(|h| h.url.clone())
            .or_else(|| self.sip.as_ref().and_then(|s| s.ha_url.clone()))
            .or_else(|| std::env::var("HA_URL").ok())
    }

    pub fn ha_token(&self) -> Option<String> {
        self.ha
            .as_ref()
            .and_then(|h| h.token.clone())
            .or_else(|| self.sip.as_ref().and_then(|s| s.ha_token.clone()))
            .or_else(|| std::env::var("HA_TOKEN").ok())
            .or_else(|| std::env::var("SUPERVISOR_TOKEN").ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffmpeg_config() {
        let yaml = r#"
ffmpeg:
  hardware: "rkmpp"
  h264: "-c:v h264_rkmpp -b:v 2M"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("Failed to parse YAML");
        assert_eq!(cfg.ffmpeg.hardware.as_deref(), Some("rkmpp"));
        assert_eq!(cfg.ffmpeg.templates.get("h264").unwrap(), "-c:v h264_rkmpp -b:v 2M");
    }
}
