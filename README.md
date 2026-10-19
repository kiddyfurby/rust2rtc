# 🦀 rust2rtc

> **High-performance streaming media gateway in Rust, inspired by [go2rtc](https://github.com/AlexxIT/go2rtc).**

`rust2rtc` is a modern, memory-safe, asynchronous streaming gateway built from the ground up in Rust using [Tokio](https://tokio.rs/), [Axum](https://github.com/tokio-rs/axum), and [webrtc-rs](https://github.com/webrtc-rs/webrtc).

> [!NOTE]
> **Project Notice**: This project is an experimental, AI-assisted rewrite of `go2rtc` in Rust. While the core RTSP relay, FFmpeg remuxing, WebRTC (WHEP) live streaming, and snapshot pipelines are working, it is **not yet 100% feature-complete** compared to upstream `go2rtc`. **Contributions, issues, and PRs are warmly welcomed!**

---

## 🌟 Key Features

- **⚡ Zero-Cost Asynchronous Architecture**: Built on Tokio with multi-producer, multi-consumer lock-free broadcast pipelines.
- **🌐 Real-Time WebRTC Streaming**:
  - Low-latency browser playback via standard WHEP (`/api/webrtc`).
  - Automatic codec negotiation supporting **H.264** (Baseline, Constrained Baseline, High Profile) and **H.265 / HEVC** (in supported browsers like Safari).
  - Audio passthrough for Opus, PCMA (G.711a), and PCMU (G.711u).
  - In-band parameter sets (SPS / PPS / VPS) caching and keyframe injection.
- **📹 RTSP Gateway**:
  - Built-in RTSP server on port `:8554` supporting TCP interleaved media transport.
  - Native RTSP relay from IP cameras, NVRs, and video encoders.
- **🎞️ External FFmpeg Integration**:
  - Remuxes and transcodes on-demand without overhead when idle.
  - Custom pipeline syntax compatible with go2rtc (e.g. `ffmpeg:stream#video=mjpeg#raw=-r 15 -q:v 3`).
- **📸 High-Speed Snapshots**:
  - Instant JPEG frame capture via `/api/frame.jpeg?src=<stream>` for both H.264 and H.265 (HEVC) streams.
  - Optimized pipeline with low-delay decoding and track readiness polling.
- **🖥️ Built-in Web Dashboard**:
  - Web UI on port `:1984` featuring auto-refreshing snapshot tiles, stream statistics (packets, bytes, active consumers), and instant WebRTC live preview.

---

## 🧪 Tested Environment & Implemented Features

The following features have been implemented and verified in live testing:

| Feature | Protocol / Codec | Server Environment | Client / Browser | Test Result & Notes |
| :--- | :--- | :--- | :--- | :--- |
| **H.264 WebRTC Live** | H.264 (Baseline, High Profile) | macOS (Darwin) | Google Chrome (macOS) | ✅ **Verified** – Low latency, instant playout clock & rendering |
| **H.265 WebRTC Live** | H.265 (HEVC) | macOS (Darwin) | Safari / HEVC-enabled Chrome | ✅ **Verified** – Negotiated via WebRTC when browser supports HEVC |
| **H.264 Snapshots** | H.264 (`/api/frame.jpeg`) | macOS (Darwin) | HTTP Client / Browser | ✅ **Verified** – Low-delay instant capture (`-flags low_delay`) |
| **H.265 Snapshots** | H.265 (`/api/frame.jpeg`) | macOS (Darwin) | HTTP Client / Browser | ✅ **Verified** – Keyframe extraction without grey artifacts (`-skip_frame nokey`) |
| **RTSP TCP Relay** | RTSP Interleaved TCP (`:8554`) | macOS (Darwin) | FFmpeg / RTSP Clients | ✅ **Verified** – Interleaved channel demuxing & broadcast |
| **Multi-Source (`multitrans`)**| `multitrans://` & fallback lists | macOS (Darwin) | Internal Engine | ✅ **Verified** – Source fallback and candidate selection |
| **FFmpeg On-Demand** | `ffmpeg:stream#params` | macOS (Darwin) | External FFmpeg | ✅ **Verified** – Audio transcoding to Opus, video filtering, scaling |
| **Web Dashboard** | HTTP / HTML5 / JS (`:1984`) | macOS (Darwin) | Google Chrome (macOS) | ✅ **Verified** – Live stream cards, bitrates, snapshot previews |

---

## 📋 General Status & Roadmap

| Feature | `go2rtc` | `rust2rtc` | Status |
| :--- | :---: | :---: | :---: |
| **RTSP Server & Client (TCP)** | ✅ | ✅ | Working |
| **WebRTC Gateway (WHEP)** | ✅ | ✅ | Working (H.264 & H.265) |
| **External FFmpeg Remux / Transcode** | ✅ | ✅ | Working |
| **JPEG Snapshots API** | ✅ | ✅ | Working (H.264 & H.265) |
| **Web UI Dashboard** | ✅ | ✅ | Working |
| **RTSP UDP / Multicast** | ✅ | 🚧 | Planned |
| **HLS / fMP4 Streaming** | ✅ | 🚧 | In progress |
| **HomeKit Integration** | ✅ | ⏳ | Planned |
| **Two-Way Audio (Backchannel)** | ✅ | ⏳ | Planned |
| **Native ONVIF Discovery** | ✅ | ⏳ | Planned |

---

## 🚀 Getting Started

### Prerequisites

- **Rust** (stable toolchain 1.80+ or nightly)
- **FFmpeg** installed and accessible in your system `PATH`

### Installation & Build

```bash
# Clone the repository
git clone https://github.com/kiddyfurby/rust2rtc.git
cd rust2rtc

# Build optimized release binary
cargo build --release
```

The compiled binary will be placed at `./target/release/rust2rtc`.

---

## ⚙️ Configuration

`rust2rtc` reads configuration from `rust2rtc.yaml` in the current working directory:

```yaml
streams:
  # Direct RTSP camera stream
  driveway:
    - rtsp://admin:password@198.51.100.10:554/stream1

  # Stream with FFmpeg audio transcoding to Opus for WebRTC
  doorbell:
    - rtsp://admin:password@198.51.100.20/Preview_01_main
    - ffmpeg:doorbell#audio=opus

  # Stream with custom FFmpeg video filters
  garden:
    - rtsp://admin:password@198.51.100.30:554/h264Preview_01_main
    - ffmpeg:garden#video=mjpeg#raw=-r 15 -q:v 3 -vf scale=-2:480

webrtc:
  candidates:
    - stun:stun.l.google.com:19302
    # Add your public IP or local IP if behind NAT:
    # - 198.51.100.50:8555

api:
  listen: ":1984"

rtsp:
  listen: ":8554"

ffmpeg:
  bin: "ffmpeg"
```

---

## 🖥️ Usage

Run `rust2rtc`:

```bash
./target/release/rust2rtc
```

- **Web Dashboard**: Open [http://localhost:1984](http://localhost:1984) in your browser.
- **Snapshot API**: `GET http://localhost:1984/api/frame.jpeg?src=<stream_name>`
- **WebRTC WHEP**: `POST http://localhost:1984/api/webrtc?src=<stream_name>` (Body: Offer SDP, returns Answer SDP)
- **Streams API**: `GET http://localhost:1984/api/streams`
- **RTSP Relay**: `rtsp://localhost:8554/<stream_name>`

---

## 🤝 Contributing

Contributions are very welcome! Whether you are interested in:
- Adding support for more protocols (HLS, MSE, RTMP, HomeKit)
- Enhancing WebRTC ICE/STUN/TURN handling
- Writing unit and integration tests
- Optimizing zero-copy packet forwarding and memory utilization

Please feel free to open an issue or submit a pull request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'feat: add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

---

## 📜 License & Acknowledgments

- Inspired by the brilliant work of [AlexxIT](https://github.com/AlexxIT) and the [`go2rtc`](https://github.com/AlexxIT/go2rtc) project.
- Licensed under the [MIT License](LICENSE).
