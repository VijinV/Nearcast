# Contributing to Nearcast

Thanks for helping! Bug reports, fixes, new platform support and docs improvements are all welcome.

## Before you start

- **Bugs:** open an issue using the *Bug report* template. Include your OS, GPU, the viewer's browser and device, and the numbers from the viewer's **Stats** overlay (press **S**).
- **Bigger changes** (new protocol, new encoder, UI redesign): open an issue first so we can agree on the approach before you spend time on it.
- **Security problems:** don't open a public issue. See [SECURITY.md](SECURITY.md).

## Development setup

You need [Rust](https://rustup.rs) (stable), Node.js 18+, and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS.

```bash
npm install
```
```bash
npm run dev
```

Optional: install [NASM](https://www.nasm.us/) before building so the CPU H.264 fallback (OpenH264) uses its assembly optimisations.

## Project map

| Path | What it does |
|---|---|
| `src-tauri/src/capture.rs` | Capture loop: grabs the screen, scales it, feeds both stream paths |
| `src-tauri/src/dxgi.rs` | Windows DXGI Desktop Duplication capture |
| `src-tauri/src/mf.rs` | Windows GPU H.264 encoder (Media Foundation: NVENC / AMF / Quick Sync) |
| `src-tauri/src/video.rs` | Shared H.264 encoder thread, OpenH264 fallback, cursor overlay |
| `src-tauri/src/rtc.rs` | WebRTC peers (str0m): SDP answer, UDP loop, keyframes |
| `src-tauri/src/tiles.rs` | "Sharp text" mode: tile diffing, JPEG encoding, wire format |
| `src-tauri/src/server.rs` | HTTP / WebSocket server, signaling, LAN address discovery |
| `src-tauri/src/viewer.html` | The page viewers open in their browser |
| `ui/` | The desktop app's control panel |

## Checks before opening a pull request

Run these from `src-tauri/`:

```bash
cargo fmt
```
```bash
cargo clippy --release -- -D warnings
```
```bash
cargo test --release
```

Benchmarks (they use your real screen and GPU, so they're opt-in):

```bash
cargo test --release -- --ignored --nocapture
```

`serve_for_manual_testing` serves the live screen on port 18090 for three minutes. Open `http://<your-LAN-IP>:18090` from another device to try changes end to end.

## Pull request guidelines

- Keep each PR focused on one change, and explain **why** in the description.
- For performance changes, include before/after numbers: encode ms, viewer latency, fps and bandwidth from the Stats overlay or the benchmarks.
- Test on at least one real viewer device. Say which ones you used, and which OS you built on.
- Platform-specific code goes behind `#[cfg(windows)]` / `#[cfg(target_os = "macos")]` with a working fallback, so every platform still builds.
- By contributing, you agree that your contribution is licensed under the [MIT License](LICENSE).

## Good first contributions

- **macOS GPU encoding** with VideoToolbox (Windows already uses the GPU through `mf.rs`).
- **Audio sharing** (system audio as Opus over WebRTC).
- **Optional PIN or password** for viewers.
- **Linux capture** (PipeWire) and **VA-API** encoding.
