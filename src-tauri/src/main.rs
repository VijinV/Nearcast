// Hide the console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod mf;
mod rtc;
mod server;
mod state;
mod tiles;
mod video;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use qrcode::render::svg;
use qrcode::QrCode;
use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::State;

use state::{Settings, Shared};

struct Session {
    shared: Arc<Shared>,
    server: JoinHandle<()>,
    port: u16,
    urls: Vec<ShareUrl>,
}

struct AppState {
    settings: Arc<Mutex<Settings>>,
    session: tokio::sync::Mutex<Option<Session>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShareUrl {
    url: String,
    interface: String,
    qr_svg: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    running: bool,
    port: Option<u16>,
    urls: Vec<ShareUrl>,
    viewers: usize,
    fps: f32,
    latency_ms: u32,
    kbps: u32,
    video_peers: usize,
    video_width: u32,
    video_height: u32,
    encode_ms: f32,
    encoder: String,
    width: u32,
    height: u32,
    error: Option<String>,
    settings: Settings,
}

fn qr_svg(text: &str) -> String {
    QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<svg::Color>()
                .min_dimensions(180, 180)
                .quiet_zone(true)
                .dark_color(svg::Color("#000000"))
                .light_color(svg::Color("#ffffff"))
                .build()
        })
        .unwrap_or_default()
}

#[tauri::command]
fn list_monitors() -> Result<Vec<capture::MonitorInfo>, String> {
    capture::list_monitors().map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<Status, String> {
    let settings = state.settings.lock().unwrap().clone();
    let session = state.session.lock().await;
    Ok(match session.as_ref() {
        Some(s) => Status {
            running: true,
            port: Some(s.port),
            urls: s.urls.clone(),
            viewers: s.shared.viewers.load(Ordering::Relaxed),
            fps: s.shared.fps_x10.load(Ordering::Relaxed) as f32 / 10.0,
            latency_ms: s.shared.latency_ms.load(Ordering::Relaxed),
            kbps: s.shared.kbps.load(Ordering::Relaxed),
            video_peers: s.shared.rtc_peers.load(Ordering::Relaxed),
            video_width: s.shared.video_width.load(Ordering::Relaxed),
            video_height: s.shared.video_height.load(Ordering::Relaxed),
            encode_ms: s.shared.encode_ms_x10.load(Ordering::Relaxed) as f32 / 10.0,
            encoder: s.shared.encoder_name.lock().unwrap().clone(),
            width: s.shared.out_width.load(Ordering::Relaxed),
            height: s.shared.out_height.load(Ordering::Relaxed),
            error: s.shared.last_error.lock().unwrap().clone(),
            settings,
        },
        None => Status {
            running: false,
            port: None,
            urls: vec![],
            viewers: 0,
            fps: 0.0,
            latency_ms: 0,
            kbps: 0,
            video_peers: 0,
            video_width: 0,
            video_height: 0,
            encode_ms: 0.0,
            encoder: String::new(),
            width: 0,
            height: 0,
            error: None,
            settings,
        },
    })
}

#[tauri::command]
fn update_settings(state: State<'_, AppState>, settings: Settings) {
    *state.settings.lock().unwrap() = settings;
}

#[tauri::command]
async fn start_sharing(state: State<'_, AppState>, port: u16) -> Result<Status, String> {
    {
        let mut session = state.session.lock().await;
        if session.is_none() {
            let (listener, port) = server::bind(port).await.map_err(|e| e.to_string())?;

            let shared = Arc::new(Shared::new(state.settings.clone()));
            let capture_shared = shared.clone();
            std::thread::Builder::new()
                .name("screen-capture".into())
                .spawn(move || capture::run(capture_shared))
                .map_err(|e| e.to_string())?;
            let video_shared = shared.clone();
            std::thread::Builder::new()
                .name("h264-encoder".into())
                .spawn(move || video::run(video_shared))
                .map_err(|e| e.to_string())?;

            let server = tauri::async_runtime::spawn(server::serve(listener, shared.clone()));

            let mut urls: Vec<ShareUrl> = server::lan_addresses()
                .into_iter()
                .map(|(iface, ip)| {
                    let url = format!("http://{ip}:{port}");
                    ShareUrl { qr_svg: qr_svg(&url), url, interface: iface }
                })
                .collect();
            if urls.is_empty() {
                let url = format!("http://localhost:{port}");
                urls.push(ShareUrl { qr_svg: qr_svg(&url), url, interface: "This computer only (no network found)".into() });
            }

            *session = Some(Session { shared, server, port, urls });
        }
    }
    get_status(state).await
}

#[tauri::command]
async fn stop_sharing(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(s) = state.session.lock().await.take() {
        s.shared.stop.store(true, Ordering::SeqCst);
        // Aborting the server task drops every open connection immediately.
        s.server.abort();
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            settings: Arc::new(Mutex::new(Settings::default())),
            session: tokio::sync::Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            list_monitors,
            get_status,
            update_settings,
            start_sharing,
            stop_sharing
        ])
        .run(tauri::generate_context!())
        .expect("error while running Local Stream");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// End-to-end: capture the real screen, serve it, and receive tile updates
    /// over a raw WebSocket connection, acknowledging each one like the viewer does.
    #[tokio::test(flavor = "multi_thread")]
    async fn streams_tile_updates_over_websocket() {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let shared = Arc::new(Shared::new(settings));
        let (listener, port) = server::bind(18080).await.unwrap();
        let cap = shared.clone();
        std::thread::spawn(move || capture::run(cap));
        tokio::spawn(server::serve(listener, shared.clone()));

        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();

        // Read the 101 response headers.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&head).contains("101"));

        let started = std::time::Instant::now();
        let mut first_len = 0;
        let mut updates = 0;
        while updates < 3 && started.elapsed() < std::time::Duration::from_secs(10) {
            // Server frames are unmasked: [0x82, len...]
            let mut h = [0u8; 2];
            s.read_exact(&mut h).await.unwrap();
            let mut len = (h[1] & 0x7f) as u64;
            if len == 126 { let mut b = [0u8; 2]; s.read_exact(&mut b).await.unwrap(); len = u16::from_be_bytes(b) as u64; }
            else if len == 127 { let mut b = [0u8; 8]; s.read_exact(&mut b).await.unwrap(); len = u64::from_be_bytes(b); }
            let mut payload = vec![0u8; len as usize];
            s.read_exact(&mut payload).await.unwrap();
            if h[0] & 0x0f != 2 { continue; }
            assert_eq!(payload[0], 1);
            let seq = u32::from_le_bytes(payload[1..5].try_into().unwrap());
            let patches = u16::from_le_bytes(payload[16..18].try_into().unwrap());
            if updates == 0 {
                assert!(payload[9] & 1 == 1, "first update must be a reset");
                assert!(patches > 0);
                first_len = payload.len();
            }
            updates += 1;
            // Ack (client frames must be masked; zero mask keeps it simple).
            let text = seq.to_string();
            let mut frame = vec![0x81, 0x80 | text.len() as u8, 0, 0, 0, 0];
            frame.extend_from_slice(text.as_bytes());
            s.write_all(&frame).await.unwrap();
        }
        println!(
            "OK: first (full) update {} KB, {} updates in {:?}, {}x{}, latency {} ms",
            first_len / 1024,
            updates,
            started.elapsed(),
            shared.out_width.load(Ordering::Relaxed),
            shared.out_height.load(Ordering::Relaxed),
            shared.latency_ms.load(Ordering::Relaxed),
        );
        assert!(first_len > 1000);
        assert_eq!(shared.viewers.load(Ordering::SeqCst), 1);
        drop(s);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(shared.viewers.load(Ordering::SeqCst), 0, "viewer count should drop on disconnect");
        shared.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod manual {
    use super::*;

    /// Serves the real screen on port 18090 for manual browser testing.
    /// cargo test --release serve_for_manual_testing -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn serve_for_manual_testing() {
        let shared = Arc::new(Shared::new(Arc::new(Mutex::new(Settings::default()))));
        let (listener, port) = server::bind(18090).await.unwrap();
        let cap = shared.clone();
        std::thread::spawn(move || capture::run(cap));
        tokio::spawn(server::serve(listener, shared.clone()));
        let vid = shared.clone();
        std::thread::spawn(move || video::run(vid));
        println!("serving on http://127.0.0.1:{port}");
        for _ in 0..180 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            println!(
                "viewers={} rtc={} video={}x{} enc={:.1}ms [{}] fps={:.1} latency={}ms kbps={}",
                shared.viewers.load(Ordering::Relaxed),
                shared.rtc_peers.load(Ordering::Relaxed),
                shared.video_width.load(Ordering::Relaxed),
                shared.video_height.load(Ordering::Relaxed),
                shared.encode_ms_x10.load(Ordering::Relaxed) as f32 / 10.0,
                shared.encoder_name.lock().unwrap(),
                shared.fps_x10.load(Ordering::Relaxed) as f32 / 10.0,
                shared.latency_ms.load(Ordering::Relaxed),
                shared.kbps.load(Ordering::Relaxed)
            );
        }
    }
}
