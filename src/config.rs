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
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: default_api_listen(),
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
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
            global: default_ffmpeg_global(),
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
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}
