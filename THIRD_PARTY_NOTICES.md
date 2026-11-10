# Third-party notices

Nearcast's own code is under the [MIT License](LICENSE). It is built on these open-source projects, each under its own license:

| Component | Used for | License |
|---|---|---|
| [Tauri](https://github.com/tauri-apps/tauri) | Desktop app shell | MIT or Apache-2.0 |
| [str0m](https://github.com/algesten/str0m) | WebRTC (ICE, DTLS-SRTP, RTP) | MIT or Apache-2.0 |
| [openh264-rs](https://github.com/ralfbiedert/openh264-rs) + [Cisco OpenH264](https://github.com/cisco/openh264) | CPU H.264 encoding (fallback) | BSD-2-Clause |
| [xcap](https://github.com/nashaofu/xcap) | Screen capture (macOS / fallback) | Apache-2.0 |
| [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder) | JPEG tiles ("sharp text" mode) | (MIT or Apache-2.0) and IJG |
| [axum](https://github.com/tokio-rs/axum), [tokio](https://github.com/tokio-rs/tokio) | HTTP / WebSocket server, async runtime | MIT |
| [rayon](https://github.com/rayon-rs/rayon) | Parallel image processing | MIT or Apache-2.0 |
| [windows-rs](https://github.com/microsoft/windows-rs) | DXGI capture, Media Foundation GPU encoding | MIT or Apache-2.0 |
| [qrcode](https://github.com/kennytm/qrcode-rust), [local-ip-address](https://github.com/LeoBorai/local-ip-address) | QR code, LAN address discovery | MIT or Apache-2.0 |

The complete list of Rust dependencies and exact versions is in [`src-tauri/Cargo.lock`](src-tauri/Cargo.lock). To generate a full license report:

```bash
cargo install cargo-about
```
```bash
cd src-tauri && cargo about generate --format json > licenses.json
```

## H.264 patents

H.264 is covered by patents (licensed through Via LA, formerly MPEG LA). **Copyright licenses like the ones above don't grant patent rights.**

- **GPU encoding on Windows** uses the encoders built into Windows and your graphics drivers (NVIDIA, AMD, Intel) through Media Foundation.
- **The CPU fallback** compiles Cisco's OpenH264 from source into the app. Cisco covers patent royalties only for its own prebuilt OpenH264 binaries, which users download from Cisco. That coverage **does not extend** to OpenH264 compiled from source, as this app does.
- Viewers' browsers decode H.264 with their own licensed decoders.

For personal and non-commercial use this is rarely a concern. If you **distribute Nearcast commercially**, check H.264 licensing for your situation. Possible changes: load Cisco's binary at runtime (`openh264` crate feature `libloading`), use only the OS/GPU encoders, or switch to a royalty-free codec such as VP8, VP9 or AV1. str0m supports VP8 and VP9.

*This is general information, not legal advice.*
