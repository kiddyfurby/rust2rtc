#![allow(dead_code)]

use crate::core::MediaKind;
use crate::ffmpeg::FfmpegManager;
use crate::mp4::{
    build_standalone_mp4, parse_h264_sps_dimensions, parse_h265_sps, H264RtpDepacketizer,
    H265RtpDepacketizer, VideoCodecConfig, VideoSample,
};
use crate::streams::Stream;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

pub struct SegmentRecorder {
    pub stream_name: String,
    pub stream: Arc<Stream>,
    pub output_dir: PathBuf,
    pub segment_duration: u32,
    pub rtsp_port: u16,
    pub ffmpeg: Arc<FfmpegManager>,
}

struct ActiveSegment {
    start_utc: chrono::DateTime<chrono::Utc>,
    start_instant: tokio::time::Instant,
    last_rtp_ts: u32,
    samples: Vec<VideoSample>,
}

impl SegmentRecorder {
    pub fn new(
        stream_name: String,
        stream: Arc<Stream>,
        output_dir: PathBuf,
        segment_duration: u32,
        rtsp_port: u16,
        ffmpeg: Arc<FfmpegManager>,
    ) -> Self {
        Self {
            stream_name,
            stream,
            output_dir,
            segment_duration,
            rtsp_port,
            ffmpeg,
        }
    }

    pub async fn run(self: Arc<Self>) {
        info!(
            stream = %self.stream_name,
            dir = %self.output_dir.display(),
            duration = self.segment_duration,
            "Starting in-process MP4 segment recorder"
        );

        let _ = tokio::fs::create_dir_all(&self.output_dir).await;

        loop {
            // 1. Ensure producer is running
            if let Err(e) = self.stream.ensure_started(self.rtsp_port, &self.ffmpeg).await {
                warn!(stream = %self.stream_name, error = %e, "Failed to start stream producer; retrying in 3s");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                continue;
            }

            // 2. Discover video track
            let mut video_track = None;
            for _ in 0..30 {
                let tracks = self.stream.get_tracks().await;
                if let Some(t) = tracks.iter().find(|t| t.kind == MediaKind::Video) {
                    video_track = Some(Arc::clone(t));
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            let video_track = match video_track {
                Some(t) => t,
                None => {
                    warn!(stream = %self.stream_name, "No video track available; retrying in 5s");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let is_hevc = video_track.codec.name.eq_ignore_ascii_case("H265")
                || video_track.codec.name.eq_ignore_ascii_case("HEVC");

            info!(
                stream = %self.stream_name,
                codec = %video_track.codec.name,
                "Recorder subscribed to video track"
            );

            let mut rx = video_track.subscribe();
            let mut h264_depack = H264RtpDepacketizer::new();
            let mut h265_depack = H265RtpDepacketizer::new();

            // Pre-seed parameter sets if already cached in track
            let initial_params = video_track.get_param_sets_data();
            if is_hevc {
                if let Some(ref vps) = initial_params.h265_vps {
                    h265_depack.vps = Some(vps.clone());
                }
                if let Some(ref sps) = initial_params.h265_sps {
                    h265_depack.sps = Some(sps.clone());
                }
                if let Some(ref pps) = initial_params.h265_pps {
                    h265_depack.pps = Some(pps.clone());
                }
            } else {
                if let Some(ref sps) = initial_params.h264_sps {
                    h264_depack.sps = Some(sps.clone());
                }
                if let Some(ref pps) = initial_params.h264_pps {
                    h264_depack.pps = Some(pps.clone());
                }
            }

            let mut current_segment: Option<ActiveSegment> = None;

            loop {
                let packet = match rx.recv().await {
                    Ok(p) => p,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(stream = %self.stream_name, skipped = n, "Recorder subscriber lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        warn!(stream = %self.stream_name, "Track channel closed; reconnecting");
                        break;
                    }
                };

                let frame_opt = if is_hevc {
                    h265_depack.feed_packet(&packet)
                } else {
                    h264_depack.feed_packet(&packet)
                };

                let (is_keyframe, rtp_ts, nalus) = match frame_opt {
                    Some(f) => f,
                    None => continue,
                };

                // A segment MUST always start with a keyframe
                if current_segment.is_none() {
                    if !is_keyframe {
                        continue;
                    }
                    current_segment = Some(ActiveSegment {
                        start_utc: chrono::Utc::now(),
                        start_instant: tokio::time::Instant::now(),
                        last_rtp_ts: rtp_ts,
                        samples: vec![VideoSample {
                            is_keyframe: true,
                            duration: 6000, // Default 15fps duration placeholder, updated when next frame arrives
                            nalus,
                        }],
                    });
                    continue;
                }

                let seg = current_segment.as_mut().unwrap();

                // Compute exact delta from RTP timestamp
                let ts_delta = rtp_ts.wrapping_sub(seg.last_rtp_ts);
                seg.last_rtp_ts = rtp_ts;
                let sample_duration = if ts_delta > 0 && ts_delta < 90000 {
                    ts_delta
                } else {
                    6000
                };

                // Fix up previous sample's duration with the true delta
                if let Some(last_sample) = seg.samples.last_mut() {
                    last_sample.duration = sample_duration;
                }

                let elapsed = seg.start_instant.elapsed();
                let should_cut = elapsed >= tokio::time::Duration::from_secs(self.segment_duration as u64)
                    && is_keyframe;

                if should_cut {
                    let finished_seg = current_segment.take().unwrap();

                    // Resolve codec config
                    let codec_config_opt = if is_hevc {
                        let track_params = video_track.get_param_sets_data();
                        let vps = h265_depack.vps.clone().or(track_params.h265_vps);
                        let sps = h265_depack.sps.clone().or(track_params.h265_sps);
                        let pps = h265_depack.pps.clone().or(track_params.h265_pps);

                        if let (Some(vps), Some(sps), Some(pps)) = (vps, sps, pps) {
                            let (w, h, _) = parse_h265_sps(&sps);
                            Some(VideoCodecConfig::H265 {
                                vps,
                                sps,
                                pps,
                                width: w,
                                height: h,
                            })
                        } else {
                            None
                        }
                    } else {
                        let track_params = video_track.get_param_sets_data();
                        let sps = h264_depack.sps.clone().or(track_params.h264_sps);
                        let pps = h264_depack.pps.clone().or(track_params.h264_pps);

                        if let (Some(sps), Some(pps)) = (sps, pps) {
                            let (w, h) = parse_h264_sps_dimensions(&sps);
                            Some(VideoCodecConfig::H264 {
                                sps,
                                pps,
                                width: w,
                                height: h,
                            })
                        } else {
                            None
                        }
                    };

                    if let Some(codec_config) = codec_config_opt {
                        let mp4_bytes = build_standalone_mp4(&codec_config, &finished_seg.samples);
                        let date_str = finished_seg.start_utc.format("%Y%m%d%H%M%S+0000").to_string();
                        let target_file = self
                            .output_dir
                            .join(format!("{}@{}.mp4", self.stream_name, date_str));
                        let tmp_file = self
                            .output_dir
                            .join(format!(".{}@{}.mp4.tmp", self.stream_name, date_str));

                        match tokio::fs::write(&tmp_file, &mp4_bytes).await {
                            Ok(_) => {
                                if let Err(e) = tokio::fs::rename(&tmp_file, &target_file).await {
                                    error!(stream = %self.stream_name, error = %e, "Failed to atomically rename MP4 segment");
                                } else {
                                    info!(
                                        stream = %self.stream_name,
                                        path = %target_file.display(),
                                        bytes = mp4_bytes.len(),
                                        samples = finished_seg.samples.len(),
                                        duration = ?elapsed,
                                        "Saved MP4 segment for Frigate cache"
                                    );
                                }
                            }
                            Err(e) => {
                                error!(stream = %self.stream_name, error = %e, "Failed to write MP4 segment tmp file");
                            }
                        }
                    } else {
                        warn!(stream = %self.stream_name, "Skipping segment write: missing parameter sets (VPS/SPS/PPS)");
                    }

                    // Start new segment with the current keyframe
                    current_segment = Some(ActiveSegment {
                        start_utc: chrono::Utc::now(),
                        start_instant: tokio::time::Instant::now(),
                        last_rtp_ts: rtp_ts,
                        samples: vec![VideoSample {
                            is_keyframe: true,
                            duration: sample_duration,
                            nalus,
                        }],
                    });
                } else {
                    seg.samples.push(VideoSample {
                        is_keyframe,
                        duration: sample_duration,
                        nalus,
                    });
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
}

pub fn start_recording(
    record_config: crate::config::RecordConfig,
    stream_manager: Arc<crate::streams::StreamManager>,
    rtsp_port: u16,
    ffmpeg: Arc<FfmpegManager>,
) {
    let output_dir = PathBuf::from(&record_config.dir);
    let duration = record_config.segment_duration;

    for stream_name in record_config.streams {
        let stream_mgr = Arc::clone(&stream_manager);
        let ffmpeg_clone = Arc::clone(&ffmpeg);
        let out_dir = output_dir.clone();
        let s_name = stream_name.clone();

        tokio::spawn(async move {
            // Wait for stream to be registered in StreamManager
            let mut stream = None;
            for _ in 0..20 {
                if let Some(s) = stream_mgr.get(&s_name).await {
                    stream = Some(s);
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            let stream = match stream {
                Some(s) => s,
                None => {
                    error!(stream = %s_name, "Configured recording stream not found in streams");
                    return;
                }
            };

            let recorder = Arc::new(SegmentRecorder::new(
                s_name,
                stream,
                out_dir,
                duration,
                rtsp_port,
                ffmpeg_clone,
            ));

            recorder.run().await;
        });
    }
}
