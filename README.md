# 🦀 rust2rtc

> **High-performance streaming media gateway in Rust, inspired by [go2rtc](https://github.com/AlexxIT/go2rtc).**

`rust2rtc` is a modern, memory-safe, asynchronous streaming gateway built from the ground up in Rust using [Tokio](https://tokio.rs/), [Axum](https://github.com/tokio-rs/axum), and [webrtc-rs](https://github.com/webrtc-rs/webrtc).

> [!NOTE]
> **Project Notice**: This project is an experimental, AI-assisted rewrite of `go2rtc` in Rust. While the core RTSP relay, FFmpeg remuxing, WebRTC (WHEP) live streaming, and snapshot pipelines are working, it is **not yet 100% feature-complete** compared to upstream `go2rtc`. **Contributions, issues, and PRs are warmly welcomed!**

---

## 🌟 Key Features

- **⚡ Zero-Cost Asynchronous Architecture**: Built on Tokio with multi-producer, multi-consumer lock-free broadcast pipelines.
- **🌐 Real-Time WebRTC Streaming & go2rtc Compatibility**:
  - Low-latency browser playback via standard WHEP (`/api/webrtc`).
  - **`go2rtc` WebSocket Signaling (`/api/ws?src=<stream>`)**: Native drop-in signaling compatibility for **`advanced-camera-card`**, **Frigate**, and Home Assistant WebRTC cards supporting `webrtc/offer`, `webrtc/answer`, and trickle ICE.
  - Automatic codec negotiation supporting **H.264** (Baseline, Constrained Baseline, High Profile) and **H.265 / HEVC** (in supported browsers like Safari).
  - Audio passthrough for Opus, PCMA (G.711a), and PCMU (G.711u).
  - In-band parameter sets (SPS / PPS / VPS) caching and keyframe injection.
- **🔄 go2rtc-Compatible REST API**:
  - `GET /api/streams` returning standard go2rtc `{ "stream": { "producers": [...], "consumers": [...] } }` schema.
  - Dynamic stream management via `PUT / POST / DELETE /api/streams` for Home Assistant and Frigate automated provisioning.
- **📹 RTSP Gateway**:
  - Built-in RTSP server on port `:8554` supporting TCP interleaved media transport.
  - Native RTSP relay from IP cameras, NVRs, and video encoders.
- **🎞️ External FFmpeg Integration**:
  - Remuxes and transcodes on-demand without overhead when idle.
  - Custom pipeline syntax compatible with go2rtc (e.g. `ffmpeg:stream#video=mjpeg#raw=-r 15 -q:v 3`).
- **📸 High-Speed Snapshots**:
  - Instant JPEG frame capture via `/api/frame.jpeg?src=<stream>` for both H.264 and H.265 (HEVC) streams.
  - Optimized pipeline with low-delay decoding and track readiness polling.
- **📞 SIP Gateway & Intercom Integration**:
  - Asynchronous SIP protocol engine supporting standard Digest MD5 authentication, auto-registration, Keep-Alive `OPTIONS` pings, and graceful unregistration (`REGISTER Expires: 0`) on shutdown.
  - Inbound call handling with symmetric RTP and Call-ID response routing.
  - Auto-configuration of Home Assistant SIP integration via REST API with dynamic account provisioning.
- **🔊 Two-Way Audio (Backchannel)**:
  - Full-duplex backchannel for Reolink and ONVIF-compatible smart doorbells and intercom cameras (`backchannel=1`).
  - High-fidelity PCM / G.711a (PCMA) / G.711u (PCMU) transcoding and repacking into standard 20ms audio frames (160 samples @ 8kHz).
- **📺 Video over SIP (H.264 & RFC 2435 JPEG RTP)**:
  - **H.264 Video Relay**: SSRC normalization, dynamic payload type rewriting, and SPS/PPS parameter set injection for SIP video phones.
  - **RFC 2435 JPEG RTP Streaming**: Hardware-friendly MJPEG RTP streaming for low-power smart panels and intercom screens (e.g. Waveshare ESP32-P4 Touch panels, Grandstream, Akuvox).
  - Real-time 15 fps frame pacing and backlog-dropping pipeline to ensure zero lag and prevent socket buffer bloat.
- **🖥️ Built-in Web Dashboard**:
  - Web UI on port `:1984` featuring auto-refreshing snapshot tiles, stream statistics (packets, bytes, active consumers), and instant WebRTC live preview.

---

## ⚡ Resource Efficiency (RAM & CPU Savings)

Rewriting in Rust eliminates garbage collection runtime overhead, goroutine stack allocations, and excessive memory fragmentation.

| Metric | `go2rtc` (Go Runtime) | `rust2rtc` (Native Rust) | Improvement |
| :--- | :---: | :---: | :---: |
| **Idle Memory (RSS)** | ~40 MB – 80 MB | **~4.5 MB** | **~85% – 90% less RAM** |
| **Idle CPU Usage** | ~0.5% – 2.0% (GC / scheduler) | **0.0%** | **Near-zero idle CPU** |
| **Garbage Collection Pauses** | Periodic GC cycles | **None (Zero GC)** | Deterministic latency |
| **Memory Management** | Go GC heap & runtime allocator | Manual / RAII stack + zero-copy `Bytes` | Flat, non-fragmenting footprint |
| **On-Demand Processing** | Persistent or background remux | Processes spawned strictly on-demand | 0 MB / 0% CPU for idle streams |

*Measured on macOS (Darwin arm64) with 14 active camera configurations loaded in `rust2rtc.yaml`.*

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
| **SIP Audio & Backchannel** | SIP / G.711a / G.711u | macOS (Darwin) | Home Assistant / FreePBX | ✅ **Verified** – Symmetric RTP, 20ms audio repacking & Reolink backchannel |
| **SIP Video (H.264)** | SIP / RFC 6184 H.264 RTP | macOS (Darwin) | SIP Video Phone / Softphone | ✅ **Verified** – Live video forwarder with dynamic PT & SSRC normalization |
| **SIP Video (RFC 2435 JPEG)**| SIP / RFC 2435 JPEG RTP | macOS (Darwin) | Waveshare ESP32-P4 Touch Panel | ✅ **Verified** – Hardware-decoded MJPEG with 15fps pacing & backlog drop |
| **SIP Registration & Exit** | SIP Digest MD5 | macOS (Darwin) | Asterisk / FreePBX | ✅ **Verified** – Auto-registration, keep-alive OPTIONS, graceful unregister |
| **Web Dashboard** | HTTP / HTML5 / JS (`:1984`) | macOS (Darwin) | Google Chrome (macOS) | ✅ **Verified** – Live stream cards, bitrates, snapshot previews |

---

## 📋 General Status & Roadmap

| Feature | `go2rtc` | `rust2rtc` | Status |
| :--- | :---: | :---: | :---: |
| **RTSP Server & Client (TCP)** | ✅ | ✅ | Working |
| **WebRTC Gateway (WHEP)** | ✅ | ✅ | Working (H.264 & H.265) |
| **go2rtc WS Signaling (`/api/ws`)** | ✅ | ✅ | Working (`advanced-camera-card` / Frigate) |
| **go2rtc Streams API (`/api/streams`)** | ✅ | ✅ | Working (GET/PUT/POST/DELETE) |
| **External FFmpeg Remux / Transcode** | ✅ | ✅ | Working |
| **JPEG Snapshots API** | ✅ | ✅ | Working (H.264 & H.265) |
| **SIP Gateway & Intercom** | ⚠️ | ✅ | Working (Audio, H.264 & RFC 2435 JPEG) |
| **Two-Way Audio (Backchannel)** | ✅ | ✅ | Working (Reolink & SIP G.711) |
| **Web UI Dashboard** | ✅ | ✅ | Working |
| **RTSP UDP / Multicast** | ✅ | 🚧 | Planned |
| **HLS / fMP4 Streaming** | ✅ | 🚧 | In progress |
| **HomeKit Integration** | ✅ | ⏳ | Planned |
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

# Build macOS release binary
just mac          # or: cargo build --release

# Cross-compile for Linux (statically-linked musl ELF binaries):
just linux-amd64  # for x86_64 Linux servers / NVRs
just linux-arm64  # for Raspberry Pi / ARM64 Home Assistant OS

# Build and package all targets into ./dist/:
just dist
```

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

sip:
  server: "198.51.100.1:5060"
  domain: "198.51.100.1"
  # Optional Home Assistant auto-provisioning
  ha_url: "http://198.51.100.1:8123"
  ha_token: "YOUR_HA_LONG_LIVED_ACCESS_TOKEN"
  auto_register_streams: true
  base_extension: 101
  auto_password: "YourSecurePasswordHere"
  accounts:
    doorbell:
      extension: "101"
      auth_user: "camera_doorbell"
      password: "YourSecurePasswordHere"
      display_name: "Front Doorbell"
      auto_answer: true
```

---

## 🖥️ Usage

Run `rust2rtc`:

```bash
./target/release/rust2rtc
```

- **Web Dashboard**: Open [http://localhost:1984](http://localhost:1984) in your browser.
- **WebSocket Signaling**: `ws://localhost:1984/api/ws?src=<stream_name>` (WebRTC offer/answer/candidate)
- **Snapshot API**: `GET http://localhost:1984/api/frame.jpeg?src=<stream_name>`
- **WebRTC WHEP**: `POST http://localhost:1984/api/webrtc?src=<stream_name>` (Body: Offer SDP, returns Answer SDP)
- **Streams API**: `GET / PUT / DELETE http://localhost:1984/api/streams`
- **RTSP Relay**: `rtsp://localhost:8554/<stream_name>`

### 🏠 Home Assistant & Advanced Camera Card Setup

`rust2rtc` functions as a drop-in replacement for `go2rtc` with [advanced-camera-card](https://card.camera/):

```yaml
type: custom:advanced-camera-card
cameras:
  - camera_entity: camera.doorbell
    live_provider: go2rtc
    go2rtc:
      url: http://198.51.100.4:1984   # Your rust2rtc IP and port
      stream: doorbell               # Stream name in rust2rtc.yaml
```

Works automatically using native WebRTC WebSocket signaling (`/api/ws`) and instant JPEG snapshot preview (`/api/frame.jpeg`).

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
