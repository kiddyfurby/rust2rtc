#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use md5::{Digest, Md5};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info};

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct FfmpegManager {
    bin: String,
    global_args: String,
    pub hardware: Option<String>,
    pub templates: HashMap<String, String>,
}

impl FfmpegManager {
    pub fn new(
        bin: String,
        global_args: String,
        hardware: Option<String>,
        templates: HashMap<String, String>,
    ) -> Self {
        Self {
            bin,
            global_args,
            hardware,
            templates,
        }
    }

    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// Resolve H.264 encoder arguments based on configured templates or hardware profile
    pub fn h264_encoder_args(&self) -> Vec<String> {
        if let Some(custom) = self.templates.get("h264") {
            return shell_words(custom);
        }
        match self.hardware.as_deref() {
            Some("rkmpp") => vec![
                "-c:v".to_string(),
                "h264_rkmpp".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("vaapi") => vec![
                "-c:v".to_string(),
                "h264_vaapi".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("cuda") | Some("nvenc") => vec![
                "-c:v".to_string(),
                "h264_nvenc".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("videotoolbox") => vec![
                "-c:v".to_string(),
                "h264_videotoolbox".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            _ => vec![
                "-c:v".to_string(),
                "libx264".to_string(),
                "-preset".to_string(),
                "superfast".to_string(),
                "-tune".to_string(),
                "zerolatency".to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
            ],
        }
    }

    /// Resolve H.264 encoder arguments tailored for SIP softphones:
    /// Constrained Baseline profile (level 3.1), repeated headers (SPS/PPS on every keyframe),
    /// short GOP (1s) for instant recovery, and downscaling to max 720p (matching level 3.1 max-fs limit).
    pub fn sip_h264_encoder_args(&self) -> Vec<String> {
        if let Some(custom) = self.templates.get("sip_h264") {
            return shell_words(custom);
        }
        vec![
            "-vf".to_string(),
            "scale=w='min(1280,iw)':h=-2".to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            "ultrafast".to_string(),
            "-tune".to_string(),
            "zerolatency".to_string(),
            "-profile:v".to_string(),
            "baseline".to_string(),
            "-level".to_string(),
            "3.1".to_string(),
            "-x264-params".to_string(),
            "repeat-headers=1".to_string(),
            "-g".to_string(),
            "15".to_string(),
            "-keyint_min".to_string(),
            "15".to_string(),
            "-b:v".to_string(),
            "1500k".to_string(),
            "-maxrate".to_string(),
            "2000k".to_string(),
            "-bufsize".to_string(),
            "3000k".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ]
    }

    /// Check if FFmpeg is available and return version info
    pub async fn check_version(&self) -> Result<String> {
        let output = Command::new(&self.bin)
            .arg("-version")
            .output()
            .await
            .with_context(|| format!("Failed to execute '{}'", self.bin))?;

        if !output.status.success() {
            bail!("FFmpeg returned non-zero status");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let first_line = stdout.lines().next().unwrap_or("FFmpeg (unknown version)");
        Ok(first_line.to_string())
    }

    /// Resolve an input URL or exec string into command line arguments
    /// Replaces `{output}` with the loopback RTSP publish URL
    pub fn build_command(&self, source: &str, rtsp_port: u16, stream_id: &str) -> (String, Vec<String>) {
        if let Some(cmd_str) = source.strip_prefix("exec:") {
            let output_url = if !cmd_str.contains("-rtsp_transport") {
                format!("-rtsp_transport tcp rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id)
            } else {
                format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id)
            };
            let replaced = cmd_str.replace("{output}", &output_url);
            let parts = shell_words(&replaced);
            if parts.is_empty() {
                return (self.bin.clone(), vec![]);
            }
            return (parts[0].clone(), parts[1..].to_vec());
        }

        if let Some(ffmpeg_url) = source.strip_prefix("ffmpeg:") {
            return self.build_ffmpeg_url_args(ffmpeg_url, rtsp_port, stream_id);
        }

        let resolved = resolve_source(source, rtsp_port);

        // Default RTSP or file remux: stream copy video and audio without re-encoding
        let output_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id);
        let args = vec![
            "-hide_banner".to_string(),
            "-v".to_string(),
            "warning".to_string(),
            "-fflags".to_string(),
            "nobuffer".to_string(),
            "-flags".to_string(),
            "low_delay".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            "-i".to_string(),
            resolved,
            "-c".to_string(),
            "copy".to_string(),
            "-f".to_string(),
            "rtsp".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            output_url,
        ];

        (self.bin.clone(), args)
    }

    /// Build FFmpeg arguments from a `ffmpeg:input#param=val` style string
    fn build_ffmpeg_url_args(&self, url_str: &str, rtsp_port: u16, stream_id: &str) -> (String, Vec<String>) {
        let (raw_input, params_str) = match url_str.split_once('#') {
            Some((i, p)) => (i, Some(p)),
            None => (url_str, None),
        };

        let resolved_input = resolve_source(raw_input, rtsp_port);

        let mut args = vec!["-hide_banner".to_string(), "-v".to_string(), "warning".to_string()];

        // Check if stream explicitly requests hardware acceleration (e.g. #hardware=rkmpp)
        let mut hardware_profile = self.hardware.clone();
        if let Some(params) = params_str {
            for param in params.split('#') {
                if let Some((k, v)) = param.split_once('=')
                    && (k == "hardware" || k == "hw") {
                        hardware_profile = Some(v.to_string());
                    }
            }
        }

        // Apply hardware decode acceleration before input
        if let Some(ref hw) = hardware_profile {
            match hw.as_str() {
                "rkmpp" => {
                    args.extend([
                        "-hwaccel".to_string(),
                        "rkmpp".to_string(),
                        "-hwaccel_output_format".to_string(),
                        "drm_prime".to_string(),
                    ]);
                }
                "vaapi" => {
                    args.extend(["-hwaccel".to_string(), "vaapi".to_string()]);
                }
                "cuda" => {
                    args.extend(["-hwaccel".to_string(), "cuda".to_string()]);
                }
                "videotoolbox" => {
                    args.extend(["-hwaccel".to_string(), "videotoolbox".to_string()]);
                }
                custom => {
                    if let Some(custom_flags) = self.templates.get(custom) {
                        args.extend(shell_words(custom_flags));
                    }
                }
            }
        }

        // If input looks like RTSP, use TCP low delay
        if resolved_input.starts_with("rtsp://") {
            args.extend([
                "-fflags".to_string(),
                "nobuffer".to_string(),
                "-flags".to_string(),
                "low_delay".to_string(),
                "-rtsp_transport".to_string(),
                "tcp".to_string(),
            ]);
        }

        args.extend(["-i".to_string(), resolved_input]);

        let mut has_video = false;
        let mut has_audio = false;

        if let Some(params) = params_str {
            for param in params.split('#') {
                if let Some((k, v)) = param.split_once('=') {
                    match k {
                        "video" => {
                            has_video = true;
                            if let Some(custom) = self.templates.get(v) {
                                args.extend(shell_words(custom));
                            } else {
                                match v {
                                    "h264" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "h264_rkmpp".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("vaapi") {
                                            args.extend(["-c:v".to_string(), "h264_vaapi".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("cuda") || hardware_profile.as_deref() == Some("nvenc") {
                                            args.extend(["-c:v".to_string(), "h264_nvenc".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("videotoolbox") {
                                            args.extend(["-c:v".to_string(), "h264_videotoolbox".to_string()]);
                                        } else {
                                            args.extend([
                                                "-c:v".to_string(),
                                                "libx264".to_string(),
                                                "-preset".to_string(),
                                                "superfast".to_string(),
                                                "-tune".to_string(),
                                                "zerolatency".to_string(),
                                                "-pix_fmt".to_string(),
                                                "yuv420p".to_string(),
                                            ]);
                                        }
                                    }
                                    "h265" | "hevc" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "hevc_rkmpp".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("vaapi") {
                                            args.extend(["-c:v".to_string(), "hevc_vaapi".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("cuda") || hardware_profile.as_deref() == Some("nvenc") {
                                            args.extend(["-c:v".to_string(), "hevc_nvenc".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("videotoolbox") {
                                            args.extend(["-c:v".to_string(), "hevc_videotoolbox".to_string()]);
                                        } else {
                                            args.extend([
                                                "-c:v".to_string(),
                                                "libx265".to_string(),
                                                "-preset".to_string(),
                                                "superfast".to_string(),
                                                "-tune".to_string(),
                                                "zerolatency".to_string(),
                                            ]);
                                        }
                                    }
                                    "mjpeg" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "mjpeg_rkmpp".to_string()]);
                                        } else {
                                            args.extend(["-c:v".to_string(), "mjpeg".to_string()]);
                                        }
                                    }
                                    "copy" => {
                                        args.extend(["-c:v".to_string(), "copy".to_string()]);
                                    }
                                    _ => {
                                        args.extend(["-c:v".to_string(), v.to_string()]);
                                    }
                                }
                            }
                        }
                        "audio" => {
                            has_audio = true;
                            match v {
                                "opus" => {
                                    args.extend([
                                        "-c:a".to_string(),
                                        "libopus".to_string(),
                                        "-application".to_string(),
                                        "lowdelay".to_string(),
                                        "-ar".to_string(),
                                        "48000".to_string(),
                                        "-ac".to_string(),
                                        "2".to_string(),
                                    ]);
                                }
                                "pcmu" => {
                                    args.extend([
                                        "-c:a".to_string(),
                                        "pcm_mulaw".to_string(),
                                        "-ar".to_string(),
                                        "8000".to_string(),
                                        "-ac".to_string(),
                                        "1".to_string(),
                                    ]);
                                }
                                "copy" => {
                                    args.extend(["-c:a".to_string(), "copy".to_string()]);
                                }
                                _ => {
                                    args.extend(["-c:a".to_string(), v.to_string()]);
                                }
                            }
                        }
                        "raw" => {
                            args.extend(shell_words(v));
                        }
                        "height" => {
                            args.extend(["-vf".to_string(), format!("scale=-2:{}", v)]);
                        }
                        _ => {}
                    }
                }
            }
        }

        if !has_video && !has_audio {
            args.extend(["-c".to_string(), "copy".to_string()]);
        } else if !has_video {
            args.extend(["-vn".to_string()]);
        } else if !has_audio {
            args.extend(["-an".to_string()]);
        }

        let output_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id);
        args.extend([
            "-f".to_string(),
            "rtsp".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            output_url,
        ]);

        (self.bin.clone(), args)
    }

    /// Spawns an external FFmpeg child process and streams its stderr for logs
    pub fn spawn_process(&self, bin: &str, args: &[String]) -> Result<Child> {
        info!(bin = bin, args = ?args, "Spawning FFmpeg process");
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn {}", bin))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    debug!(target: "ffmpeg", "{}", line);
                }
            });
        }

        Ok(child)
    }

    /// Spawns an external FFmpeg child process with piped stdout for streaming transcoded media
    pub fn spawn_stdout_process(&self, args: &[String]) -> Result<Child> {
        info!(bin = %self.bin, args = ?args, "Spawning FFmpeg stdout transcode process");
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn {}", self.bin))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    debug!(target: "ffmpeg_stdout", "{}", line);
                }
            });
        }

        Ok(child)
    }

    /// Generate a snapshot JPEG image for a given input source
    pub async fn take_snapshot(&self, source: &str, rtsp_port: u16, is_hevc: bool) -> Result<Bytes> {
        let resolved = resolve_source(source, rtsp_port);
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-hide_banner", "-v", "error"]);

        if resolved.starts_with("rtsp://") {
            cmd.args(["-rtsp_transport", "tcp"]);
            if is_hevc {
                cmd.args(["-skip_frame", "nokey"]);
            } else {
                cmd.args([
                    "-fflags",
                    "nobuffer",
                    "-flags",
                    "low_delay",
                ]);
            }
        }

        cmd.args(["-i", &resolved, "-vframes", "1", "-f", "image2", "-c:v", "mjpeg", "-"]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| "Failed to spawn snapshot FFmpeg")?;
        let mut stdout = child.stdout.take().context("No stdout")?;
        let mut buffer = Vec::new();

        let read_res = tokio::time::timeout(tokio::time::Duration::from_secs(12), stdout.read_to_end(&mut buffer)).await;
        if read_res.is_err() {
            let _ = child.kill().await;
            bail!("FFmpeg snapshot timed out");
        }
        let status = child.wait().await?;

        if !status.success() || buffer.is_empty() {
            bail!("FFmpeg snapshot failed or produced empty image");
        }

        Ok(Bytes::from(buffer))
    }

    /// Generate a snapshot JPEG image by piping an in-memory Annex-B keyframe into FFmpeg stdin
    pub async fn take_snapshot_from_keyframe(&self, is_hevc: bool, annex_b: &[u8]) -> Result<Bytes> {
        if annex_b.is_empty() {
            bail!("Empty keyframe Annex-B buffer");
        }

        let mut cmd = Command::new(&self.bin);
        cmd.args(["-hide_banner", "-v", "error"]);

        if is_hevc {
            cmd.args(["-f", "hevc"]);
        } else {
            cmd.args(["-f", "h264"]);
        }

        cmd.args(["-i", "-", "-vframes", "1", "-f", "image2", "-c:v", "mjpeg", "-"]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| "Failed to spawn snapshot FFmpeg stdin worker")?;
        let mut stdin = child.stdin.take().context("No stdin")?;
        let mut stdout = child.stdout.take().context("No stdout")?;

        stdin.write_all(annex_b).await?;
        drop(stdin);

        let mut buffer = Vec::new();
        let read_res = tokio::time::timeout(tokio::time::Duration::from_secs(5), stdout.read_to_end(&mut buffer)).await;
        if read_res.is_err() {
            let _ = child.kill().await;
            bail!("FFmpeg stdin snapshot timed out");
        }
        let status = child.wait().await?;

        if !status.success() || buffer.is_empty() {
            bail!("FFmpeg stdin snapshot failed or produced empty image");
        }

        Ok(Bytes::from(buffer))
    }

    /// Build FFmpeg arguments for low-latency live MJPEG stdout streaming
    pub fn build_mjpeg_stream_args(
        &self,
        source: &str,
        custom_pipeline: Option<&str>,
        width: Option<u32>,
        height: Option<u32>,
        fps: Option<u32>,
        quality: Option<u32>,
    ) -> Vec<String> {
        let mut args = vec![
            "-hide_banner".to_string(),
            "-v".to_string(),
            "warning".to_string(),
            "-fflags".to_string(),
            "nobuffer".to_string(),
            "-flags".to_string(),
            "low_delay".to_string(),
        ];

        if source.starts_with("rtsp://") {
            args.extend([
                "-rtsp_transport".to_string(),
                "tcp".to_string(),
            ]);
        }

        args.extend(["-i".to_string(), source.to_string()]);
        args.extend(["-an".to_string()]);

        if self.hardware.as_deref() == Some("rkmpp") {
            args.extend(["-c:v".to_string(), "mjpeg_rkmpp".to_string()]);
        } else {
            args.extend(["-c:v".to_string(), "mjpeg".to_string()]);
        }

        let mut custom_vf = None;
        let mut custom_r = None;
        let mut custom_q = None;
        let mut custom_raw = Vec::new();

        if let Some(pipeline) = custom_pipeline {
            if let Some((_, params)) = pipeline.split_once('#') {
                for param in params.split('#') {
                    if let Some((k, v)) = param.split_once('=') {
                        match k {
                            "height" => {
                                custom_vf = Some(format!("scale=-2:{}", v));
                            }
                            "vf" => {
                                custom_vf = Some(v.to_string());
                            }
                            "fps" | "r" => {
                                custom_r = Some(v.to_string());
                            }
                            "q" | "q:v" => {
                                custom_q = Some(v.to_string());
                            }
                            "raw" => {
                                custom_raw.extend(shell_words(v));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let final_q = quality
            .map(|q| q.to_string())
            .or(custom_q)
            .unwrap_or_else(|| "5".to_string());
        args.extend(["-q:v".to_string(), final_q]);

        let final_r = fps
            .map(|f| f.to_string())
            .or(custom_r)
            .unwrap_or_else(|| "15".to_string());
        args.extend(["-r".to_string(), final_r]);

        let final_vf = if let (Some(w), Some(h)) = (width, height) {
            Some(format!("scale={}:{}", w, h))
        } else if let Some(w) = width {
            Some(format!("scale={}:-2", w))
        } else if let Some(h) = height {
            Some(format!("scale=-2:{}", h))
        } else {
            custom_vf
        };

        if let Some(vf) = final_vf {
            args.extend(["-vf".to_string(), vf]);
        }

        if !custom_raw.is_empty() {
            args.extend(custom_raw);
        }

        args.extend([
            "-pix_fmt".to_string(),
            "yuvj420p".to_string(),
            "-huffman".to_string(),
            "default".to_string(),
            "-f".to_string(),
            "mjpeg".to_string(),
            "pipe:1".to_string(),
        ]);

        args
    }
}

/// Locate a two-byte marker in a buffer using SIMD pattern search
pub fn find_marker(buf: &[u8], marker: &[u8; 2]) -> Option<usize> {
    memchr::memmem::find(buf, marker)
}

pub fn resolve_source(source: &str, rtsp_port: u16) -> String {
    let clean_source = source
        .split('#')
        .next()
        .unwrap_or(source)
        .trim_end_matches("?backchannel=1");

    if clean_source.starts_with("onvif://")
        && let Ok(url) = url::Url::parse(clean_source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            let user = url.username();
            let pass = url.password().unwrap_or("");
            let userinfo = if !user.is_empty() || !pass.is_empty() {
                format!("{}:{}@", user, pass)
            } else {
                String::new()
            };
            let port_part = match url.port() {
                Some(8000) | Some(80) | None => String::new(),
                Some(p) => format!(":{}", p),
            };
            let path = if url.path().is_empty() || url.path() == "/" {
                "/Preview_01_main"
            } else {
                url.path()
            };
            return format!("rtsp://{}{}{}{}", userinfo, host, port_part, path);
        }

    if clean_source.starts_with("tapo://")
        && let Ok(url) = url::Url::parse(clean_source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            let user = url.username();
            let pass = url.password().unwrap_or("");
            let userinfo = if !user.is_empty() || !pass.is_empty() {
                format!("{}:{}@", user, pass)
            } else {
                String::new()
            };
            let port_part = match url.port() {
                Some(554) | None => String::new(),
                Some(p) => format!(":{}", p),
            };
            let path = if url.path().is_empty() || url.path() == "/" {
                "/stream1"
            } else {
                url.path()
            };
            return format!("rtsp://{}{}{}{}", userinfo, host, port_part, path);
        }

    if !clean_source.contains("://") && !clean_source.starts_with("exec:") && !clean_source.starts_with("ffmpeg:") {
        // References another stream by name
        return format!("rtsp://127.0.0.1:{}/{}", rtsp_port, clean_source);
    }

    clean_source.to_string()
}

/// Helper function to generate an MD5 stream hash path like go2rtc
pub fn generate_stream_hash(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Simple shell words splitter for exec strings
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';

    for ch in s.chars() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_marker() {
        let buf = [0x00, 0x12, 0xFF, 0xD8, 0xAA, 0xBB, 0xFF, 0xD9, 0x00];
        assert_eq!(find_marker(&buf, &[0xFF, 0xD8]), Some(2));
        assert_eq!(find_marker(&buf[4..], &[0xFF, 0xD9]), Some(2));
        assert_eq!(find_marker(&buf, &[0xAA, 0x00]), None);
    }

    #[test]
    fn test_build_mjpeg_stream_args_default() {
        let ffmpeg = FfmpegManager::new("ffmpeg".to_string(), String::new(), None, HashMap::new());
        let args = ffmpeg.build_mjpeg_stream_args("rtsp://127.0.0.1:8554/doorbell", None, None, None, None, None);

        assert!(args.contains(&"-c:v".to_string()));
        assert!(args.contains(&"mjpeg".to_string()));
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"pipe:1".to_string()));
        assert!(args.contains(&"-r".to_string()));
        assert!(args.contains(&"15".to_string()));
        assert!(args.contains(&"-q:v".to_string()));
        assert!(args.contains(&"5".to_string()));
    }

    #[test]
    fn test_build_mjpeg_stream_args_with_custom_pipeline() {
        let ffmpeg = FfmpegManager::new("ffmpeg".to_string(), String::new(), None, HashMap::new());
        let pipeline = "ffmpeg:doorbell#video=mjpeg#height=480#raw=-r 10 -q:v 3";
        let args = ffmpeg.build_mjpeg_stream_args("rtsp://127.0.0.1:8554/doorbell", Some(pipeline), None, None, None, None);

        assert!(args.contains(&"-vf".to_string()));
        assert!(args.contains(&"scale=-2:480".to_string()));
        assert!(args.contains(&"-r".to_string()));
        assert!(args.contains(&"10".to_string()));
        assert!(args.contains(&"-q:v".to_string()));
        assert!(args.contains(&"3".to_string()));
    }

    #[test]
    fn test_resolve_source() {
        assert_eq!(
            resolve_source("tapo://admin:cloudpass@192.0.2.27", 8554),
            "rtsp://admin:cloudpass@192.0.2.27/stream1"
        );
        assert_eq!(
            resolve_source("tapo://admin:cloudpass@192.0.2.27:8554/stream2", 8554),
            "rtsp://admin:cloudpass@192.0.2.27:8554/stream2"
        );
        assert_eq!(
            resolve_source("onvif://admin:pass@192.0.2.42:8000/live/ch0", 8554),
            "rtsp://admin:pass@192.0.2.42/live/ch0"
        );
        assert_eq!(
            resolve_source("onvif://admin:pass@192.0.2.42:8000", 8554),
            "rtsp://admin:pass@192.0.2.42/Preview_01_main"
        );
        assert_eq!(
            resolve_source("doorbell", 8554),
            "rtsp://127.0.0.1:8554/doorbell"
        );
    }
}
