# Frigate NVR Integration & Zero-FFmpeg Segment Recording

`rust2rtc` is designed as a drop-in binary replacement for `go2rtc` inside Frigate NVR containers, adding an optional **in-process progressive MP4 segment recorder** that eliminates per-camera `ffmpeg -f segment` recording subprocesses.

---

## 1. Drop-In `go2rtc` Binary Replacement

Frigate's `s6-overlay` supervisor automatically checks `/config/go2rtc` at container startup and runs it instead of the bundled `go2rtc` binary if present:

```bash
# Copy the statically linked Linux binary into Frigate's mounted /config directory:
cp dist/rust2rtc_linux_arm64 /path/to/frigate/config/go2rtc
chmod +x /path/to/frigate/config/go2rtc

# Restart Frigate container
docker restart frigate
```

Frigate invokes `/config/go2rtc -config=/config/go2rtc_homekit.yml -config=/dev/shm/go2rtc.yaml`, which `rust2rtc` parses and merges automatically alongside any `/config/rust2rtc.yaml` file.

---

## 2. In-Process MP4 Segment Recording (`ffmpeg-shim`)

By default, Frigate spawns a separate `ffmpeg` process per camera for `record` roles (`~25–38 MB` RSS each, or `~180–250 MB` across 7 cameras). With `rust2rtc`, progressive `.mp4` segment files are muxed directly in-process in pure Rust and written to `/tmp/cache/`, while a lightweight `ffmpeg-shim` process (`~1.5–1.9 MB` RSS, `0.0%` CPU) satisfies Frigate's process supervisor.

### Setup Steps (100% Stock Frigate)

1. Create a custom `ffmpeg/bin` directory inside your mounted Frigate `/config` folder:
   ```bash
   mkdir -p /path/to/frigate/config/ffmpeg/bin
   ln -sf ../../go2rtc /path/to/frigate/config/ffmpeg/bin/ffmpeg
   ln -sf /usr/lib/ffmpeg/6.0/bin/ffprobe /path/to/frigate/config/ffmpeg/bin/ffprobe
   ```
2. Point Frigate's `config.yml` at the shim directory:
   ```yaml
   ffmpeg:
     path: /config/ffmpeg
   ```
3. Restart Frigate.

### How the Shim Works & Fallback Behavior

- **Common Path (Zero-FFmpeg)**: When Frigate launches a segment recording command (`ffmpeg ... -f segment /tmp/cache/<stream>@%Y%m%d%H%M%S%z.mp4`), the `rust2rtc` binary detects that it was invoked as `ffmpeg` with `-f segment`, calls `http://127.0.0.1:1984/api/record?stream=<stream>&action=start`, and sleeps until Frigate signals termination.
- **Keyframe Segment Cutting**: `rust2rtc` cuts standalone ISO BMFF `.mp4` files (`avc1` for H.264, `hvc1` for H.265/HEVC, plus `mp4a` AAC audio) on IDR keyframes and atomically renames them into `/tmp/cache/` so Frigate's `RecordingMaintainer` indexes them without race conditions.
- **Fallback Path (Real FFmpeg)**: For any non-recording invocation (e.g. `detect` role raw video decoding, hardware RKMPP/VAAPI/NVDEC scaling, or if the `rust2rtc` API is unreachable), the shim immediately `exec`s the real system `ffmpeg` binary (`REAL_FFMPEG_BIN` or `/usr/lib/ffmpeg/*/bin/ffmpeg`).
