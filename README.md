# Nearcast

**Cast your screen to devices near you.** Open a link (or scan a QR code) on any phone, tablet, TV or laptop on the same Wi-Fi to watch live, with nothing to install.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE) ![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS-lightgrey) ![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB)

Free, open-source and private: your screen goes straight from your computer to devices on your own network. No accounts, no cloud, and nothing to install for viewers.

A small Tauri + Rust desktop app (Windows and macOS) that shares your screen to any device on the same Wi-Fi. Click **Start sharing**, then open the link it shows (for example `http://192.168.1.23:8080`) in any browser on a phone, tablet, TV or laptop, or scan the QR code. Viewers don't need to install anything.

## How it works

Viewers can watch in two ways. Both run at the same time from one capture:

```
                    capture: DXGI Desktop Duplication (Windows) / xcap (macOS)
                                          │  scale → RGB
                 ┌────────────────────────┴────────────────────────┐
     VIDEO (default)                                       SHARP TEXT
     one shared H.264 encoder (openh264)                   changed 128×128 tiles → JPEG
     pointer drawn into the frame                          pointer sent as x/y
     → WebRTC (str0m): UDP, NACK, keyframe requests        → WebSocket, 2 frames in flight max
     playout-delay 0: browser renders immediately          browser draws tiles on a canvas
```

**Video mode** (WebRTC + H.264) is the default and is built for YouTube, games and anything else that moves:
- **About 10× less bandwidth** than JPEG for motion (1080p video is about 4–8 Mbps instead of 40+).
- **All viewers stay in sync:** one shared encoder sends identical frames to everyone at the same moment. In testing, 3 viewers received each frame within 1 ms of each other.
- **No buffering:** packets are sent immediately with no pacing, and the browser is told to render each frame as soon as it arrives (playout-delay 0, `jitterBufferTarget = 0`). The jitter buffer measured 0.2–0.5 ms per frame.
- **Adapts to your CPU:** if encoding can't keep up, the video steps down (1920 → 1600 → 1280 …) and steps back up when it can.
- **Recovers from Wi-Fi hiccups:** lost packets are re-sent (NACK), and a viewer that falls behind gets a fresh keyframe instead of a backlog.

**Sharp text mode** (JPEG tiles over WebSocket) gives pixel-crisp text and only sends what changed. Viewers switch with the **Mode** button. It's also the automatic fallback if WebRTC can't connect, for example when a firewall blocks UDP.

Measured on this PC over the LAN address at 30 fps: video-mode viewers reported **16–37 ms** latency, and sharp mode about 3 ms on loopback. Press **S** or tap **Stats** on the viewer page to see live numbers.

### GPU (hardware) encoding
On Windows, video is encoded by the graphics card through Media Foundation: **NVIDIA NVENC**, **AMD VCN** or **Intel Quick Sync**, whichever the drivers provide (NVIDIA is preferred). It runs on dedicated video hardware, so it barely touches the CPU and the video always stays at the full captured resolution.

| Encoder | 1080p frame | Result |
|---|---|---|
| GPU (NVENC, RTX 3050 Laptop) | ~7–12 ms | full 1080p at 60 fps |
| CPU (OpenH264, Ryzen 7 6800H, no NASM) | ~40 ms | drops to 1600/1280 wide to keep up |

If no hardware encoder is available, or the GPU encoder fails (for example after a driver reset), the app switches to OpenH264 on the CPU automatically. The app window shows which encoder is active. Installing [NASM](https://www.nasm.us/) before building makes the CPU fallback 2–3× faster; the GitHub workflow does this for you. On macOS the CPU encoder is used for now (VideoToolbox support isn't done yet).

## Project layout

```
ui/                     App control panel (plain HTML/CSS/JS, no bundler)
src-tauri/
  src/main.rs           Tauri commands: start/stop, status, settings
  src/capture.rs        Capture loop, scaling, cursor, xcap fallback (macOS)
  src/dxgi.rs           Windows DXGI Desktop Duplication capture
  src/tiles.rs          Tile diffing, parallel JPEG encode, tile store and wire format
  src/video.rs          Shared H.264 encoder thread, adaptive resolution, cursor overlay
  src/mf.rs             Windows GPU encoder (Media Foundation: NVENC / AMF / Quick Sync)
  src/rtc.rs            WebRTC peers (str0m): SDP answer, UDP loop, keyframe handling
  src/server.rs         HTTP + WebSocket server with ack flow control, LAN IP discovery
  src/state.rs          Shared state and settings
  src/viewer.html       Page that viewers see in their browser
  tauri.conf.json       App/window/bundle config
assets/app-icon.svg     Source icon (regenerate with `npx tauri icon assets/app-icon.svg`)
```

## Prerequisites

- **Rust** (stable): https://rustup.rs
- **Node.js 18+** (only used for the Tauri CLI)
- **Windows:** Microsoft C++ Build Tools and WebView2 (preinstalled on Windows 10/11)
- **macOS:** Xcode Command Line Tools (`xcode-select --install`)

Then install the CLI once:

```bash
npm install
```

## Run in development

```bash
npm run dev
```

## Build installers

```bash
npm run build
```

Output goes in `src-tauri/target/release/bundle/`:
- Windows: `.msi` and NSIS `-setup.exe`
- macOS: `.app` and `.dmg`

Tauri can't cross-compile, so build the macOS version on a Mac. You can also push to GitHub and use the included workflow (`.github/workflows/build.yml`), which builds both platforms on every `v*` tag or when run manually.

## First-run notes

**Windows:** the first time you click **Start sharing**, Windows Firewall asks whether to allow network access. Tick **Private networks** and allow it, or other devices can't connect.

**macOS:** the first capture triggers the **Screen Recording** permission. Enable *Nearcast* in **System Settings → Privacy & Security → Screen Recording**, then restart the app. macOS may also ask to allow incoming connections; click **Allow**.

## Troubleshooting

| Problem | Fix |
|---|---|
| Viewer shows "Mode: Sharp text" even though you want video | WebRTC couldn't connect: usually the firewall blocks UDP. Allow *Nearcast* on **Private networks** in Windows Firewall, then tap **Mode** to switch back to video. |
| Phone can't open the link | Make sure both devices are on the same Wi-Fi (not a guest network, which often blocks devices from reaching each other). Check the firewall prompt above. Try another address under **Other network addresses**. |
| Stream is laggy | Open **Stats** on the viewer. High latency with high bandwidth means the Wi-Fi is the bottleneck: lower the resolution to 1280 or the quality to 50–60. Use 5 GHz Wi-Fi if you can. |
| Black screen on macOS | Grant Screen Recording permission and restart the app. |
| Port already in use | The app automatically tries the next 10 ports. You can also change the port in Settings. |

## Security

Anyone on your network who has the link can watch while sharing is on. Click **Stop sharing** when you're done, and avoid using it on public Wi-Fi.

## Tests

```bash
cd src-tauri
cargo test --release -- --nocapture
```

The test captures the real screen, starts the server, and receives and acknowledges tile updates over a raw WebSocket.
To benchmark capture and encode speed: `cargo test --release stage_timings -- --ignored --nocapture`.

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks and project layout, and please follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately as described in [SECURITY.md](SECURITY.md).

## License

Nearcast is released under the [MIT License](LICENSE). Third-party components and a note on H.264 patents are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
