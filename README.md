<div align="center">

# 🦀 rust2rtc

**A pure-Rust [go2rtc](https://github.com/AlexxIT/go2rtc) replacement with a built-in SIP intercom bridge.**

Drop-in for [Frigate](https://frigate.video/) &nbsp;·&nbsp; Built for [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom) &nbsp;·&nbsp; In-process media pipeline

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange?logo=rust&logoColor=white)
[![Latest release](https://img.shields.io/github/v/release/kiddyfurby/rust2rtc?color=brightgreen)](https://github.com/kiddyfurby/rust2rtc/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/kiddyfurby/rust2rtc/total?color=blue)](https://github.com/kiddyfurby/rust2rtc/releases)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macos-lightgrey)

</div>

---

`rust2rtc` is a high-performance media gateway that speaks the **go2rtc** protocol and adds a **full-duplex SIP bridge**. Point it at your RTSP cameras and you get WebRTC, MSE, RTSP, snapshots, and dialable SIP intercom extensions — all from a single, statically linked binary.

Already running **Frigate**? Drop `rust2rtc` in as its `go2rtc` binary and keep the rest of your setup unchanged.

## Why rust2rtc?

| Capability | What you get |
| :--- | :--- |
| **Drop-in `go2rtc`** | Same REST, WebSocket, and RTSP APIs. Works with Frigate, Advanced Camera Card, Home Assistant, and `video-rtc.js`. |
| **SIP intercom bridge** | Turn ordinary RTSP cameras into SIP extensions with live video and two-way audio — including [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom) panels and VoIP phones. |
| **Performance-optimized** | Pure Rust, zero GC, zero-FFmpeg ingestion/transcoding/recording. Single-digit MB RSS and near-zero idle CPU. |

## How it fits together

```text
                 ┌─► WebRTC (WHEP + WS) ─► browsers / Frigate / Home Assistant
                 ├─► MSE / fMP4
RTSP cameras ──► rust2rtc ─► RTSP relay (:8554)
 (H.264/H.265)   ├─► SIP (H.264 + RFC 2435 JPEG, G.711/L16) ─► ESPHome Intercom / VoIP phones
                 └─► In-process MP4 recording ─► Frigate cache
```

## Feature highlights

- **In-process RTSP client** — no `ffmpeg -c copy` ingestion; native Digest/Basic auth, TCP interleaved transport, H.264 & H.265.
- **WebRTC** — WHEP (`/api/webrtc`), go2rtc WebSocket signaling (`/api/ws?src=`), and MSE/fMP4 for instant preview.
- **Two-way audio** — pure-Rust AAC/G.711 → Opus transcoding with camera-clock lip-sync and shared broadcast tracks.
- **SIP gateway** — auto-registration, parallel forking, H.264 and RFC 2435 JPEG video, PCMA/PCMU/L16 audio.
- **Zero-FFmpeg recording** — in-process MP4 segments for Frigate's cache via a tiny `ffmpeg` shim.
- **Snapshots & MJPEG** — `/api/frame.jpeg` and `/api/stream.mjpeg` with shared transcoder deduplication.
- **REST API** — `GET / PUT / POST / DELETE /api/streams`, plus a built-in dashboard on `:1984`.

## Performance

| Metric | go2rtc | rust2rtc |
| :--- | :---: | :---: |
| Idle memory (RSS) | ~40–80 MB | **~4.5 MB** |
| Recording (per camera) | external FFmpeg, ~25–38 MB | **in-process + ~1.5 MB shim** |
| Idle CPU | ~0.5–2.0% | **~0.0%** |
| Garbage collection pauses | periodic | **none** |
| Audio transcoding | FFmpeg subprocess | **in-process, deduplicated** |

*Measured on macOS (arm64) and Linux (aarch64) with a multi-camera configuration.*

## Quick start

### Download a prebuilt binary

Linux binaries are fully static (musl) — no runtime dependencies.

```bash
# Linux ARM64 (RK3588, Orange Pi, Raspberry Pi, Home Assistant OS)
curl -L -o rust2rtc https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_arm64
chmod +x rust2rtc && ./rust2rtc

# Linux x86_64 (servers, mini PCs, Docker)
curl -L -o rust2rtc https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_amd64
chmod +x rust2rtc && ./rust2rtc
```

macOS builds are on the [Releases](https://github.com/kiddyfurby/rust2rtc/releases/latest) page.

### Build from source

Requires Rust 1.85+ (edition 2024). FFmpeg on `PATH` is optional and only needed for on-demand transcoding.

```bash
git clone https://github.com/kiddyfurby/rust2rtc.git
cd rust2rtc

just mac          # macOS arm64
just linux-amd64  # static Linux x86_64
just linux-arm64  # static Linux ARM64
```

Run it:

```bash
./target/release/rust2rtc
```

- **Dashboard**: <http://localhost:1984>
- **RTSP relay**: `rtsp://localhost:8554/<stream>`
- **WebRTC WHEP**: `POST /api/webrtc?src=<stream>`
- **Snapshot**: `GET /api/frame.jpeg?src=<stream>`

## Drop-in for Frigate

Already using Frigate? Swap in `rust2rtc` and keep your existing setup.

1. Copy the binary into Frigate's config directory as `go2rtc`:

   ```bash
   cp dist/rust2rtc_linux_arm64 /path/to/frigate/config/go2rtc
   chmod +x /path/to/frigate/config/go2rtc
   ```

2. Restart Frigate. Its s6 runner launches `rust2rtc` exactly like `go2rtc`.

**Optional: zero-FFmpeg recording.** Symlink the binary as Frigate's `ffmpeg` to record in-process and cut per-camera recording RAM by ~93%:

```bash
mkdir -p /path/to/frigate/config/ffmpeg/bin
ln -sf ../../go2rtc /path/to/frigate/config/ffmpeg/bin/ffmpeg
ln -sf /usr/lib/ffmpeg/6.0/bin/ffprobe /path/to/frigate/config/ffmpeg/bin/ffprobe
```

Then set `ffmpeg.path: /config/ffmpeg` in `config.yml`. Non-recording calls fall through to real FFmpeg.

## SIP intercom bridge

`rust2rtc` registers your camera streams as SIP extensions and bridges two-way audio and video to any SIP endpoint — including [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom) wall panels.

```yaml
sip:
  server: "198.51.100.5:5060"        # your SIP PBX or Home Assistant VoIP
  auto_register_streams: true
  base_extension: 101
  auto_password: "YourSecurePasswordHere"
```

Dial extension `101` from a phone or intercom panel to see and talk to that camera.

## Configuration

Copy the fully documented reference and edit it:

```bash
cp rust2rtc.sample.yaml rust2rtc.yaml
```

See [`rust2rtc.sample.yaml`](rust2rtc.sample.yaml) for every option — streams, WebRTC ICE, Frigate warm-up, SIP accounts, and doorbell automation.

## Compatibility

| Interface | Status |
| :--- | :--- |
| go2rtc REST streams API | ✅ GET / PUT / POST / DELETE |
| go2rtc WebSocket signaling (`/api/ws`) | ✅ |
| WebRTC WHEP (`/api/webrtc`) | ✅ H.264 & H.265 |
| MSE / fMP4 | ✅ |
| RTSP server & client (TCP) | ✅ |
| SIP audio/video (G.711, L16, H.264, RFC 2435 JPEG) | ✅ |
| JPEG snapshots & MJPEG | ✅ |
| Frigate in-process recording | ✅ via ffmpeg shim |
| RTSP UDP / multicast | 🚧 planned |
| HomeKit | 🚧 planned |

## Contributing

Issues and PRs are welcome — new protocols, codecs, SIP features, tests, and performance work all help. Open an issue or send a pull request.

## 🙏 Acknowledgements

`rust2rtc` would not exist without [go2rtc](https://github.com/AlexxIT/go2rtc).

go2rtc is a remarkable piece of software: a tiny, fast, multi-protocol streaming gateway that quietly became the backbone of the self-hosted camera world. Its clean streams model, its REST/WebSocket API, and the `ffmpeg:` source syntax are so well designed that an entire ecosystem — Frigate, Home Assistant, Advanced Camera Card — builds on top of them. It is portable, dependable, and astonishingly capable for its size.

Massive thanks to [AlexxIT](https://github.com/AlexxIT) for building and sharing it. `rust2rtc` is an attempt to carry that same spirit forward in Rust: keep the API and the go2rtc way of doing things intact, while pushing performance and adding a SIP intercom bridge. If this project is useful to you, go star [go2rtc](https://github.com/AlexxIT/go2rtc) — it deserves it.

## License

Released under the [GNU General Public License v3.0](LICENSE). Copyright © 2026 Garfield Ip. Made possible by [AlexxIT](https://github.com/AlexxIT)'s [go2rtc](https://github.com/AlexxIT/go2rtc).