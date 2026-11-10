//! Tiny HTTP server: serves the viewer page and pushes screen updates over a
//! WebSocket with ack-based flow control (at most MAX_IN_FLIGHT unacknowledged
//! updates per viewer), so frames never pile up in network buffers.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use str0m::change::{SdpAnswer, SdpOffer};
use tokio::net::TcpListener;

use crate::state::{Shared, ViewerGuard};

const VIEWER_HTML: &str = include_str!("viewer.html");
/// Updates allowed on the wire before the viewer confirms it has drawn one.
/// 2 keeps the pipe busy without letting latency build up.
const MAX_IN_FLIGHT: usize = 2;

/// Binds on all interfaces, trying `port` and the next few ports if busy.
pub async fn bind(port: u16) -> anyhow::Result<(TcpListener, u16)> {
    let mut last_err = None;
    for p in port..=port.saturating_add(10) {
        match TcpListener::bind((IpAddr::V4(Ipv4Addr::UNSPECIFIED), p)).await {
            Ok(l) => return Ok((l, p)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!(
        "Could not open port {port}-{}: {}",
        port.saturating_add(10),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

pub async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let app = Router::new()
        .route("/", get(viewer))
        .route("/ws", get(ws_upgrade))
        .route("/info", get(info))
        .route("/offer", post(offer))
        .route("/stats", post(stats))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .with_state(shared);
    // Disable Nagle so small updates (cursor moves) go out immediately.
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
    }
}

async fn viewer() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(VIEWER_HTML))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    width: u32,
    height: u32,
    viewers: usize,
    /// Average capture-to-encoded time on the host for the video path.
    encode_ms: f32,
}

async fn info(State(shared): State<Arc<Shared>>) -> Json<Info> {
    Json(Info {
        width: shared.out_width.load(Ordering::Relaxed),
        height: shared.out_height.load(Ordering::Relaxed),
        viewers: shared.viewers.load(Ordering::Relaxed),
        encode_ms: shared.encode_ms_x10.load(Ordering::Relaxed) as f32 / 10.0,
    })
}

/// WebRTC signaling: the browser POSTs its offer, we return the answer.
async fn offer(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Json(offer): Json<SdpOffer>,
) -> Result<Json<SdpAnswer>, (StatusCode, String)> {
    // Offer our UDP candidate on the same address the browser used to reach us.
    let host_ip = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| {
            let host = h
                .rsplit_once(':')
                .map(|(a, _)| a)
                .unwrap_or(h)
                .trim_matches(['[', ']']);
            if host.eq_ignore_ascii_case("localhost") {
                return Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
            }
            host.parse::<IpAddr>().ok()
        })
        .or_else(|| lan_addresses().first().map(|(_, ip)| IpAddr::V4(*ip)))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    crate::rtc::accept_offer(shared, offer, host_ip)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewerStats {
    latency_ms: u32,
}

/// Video-mode viewers report their measured latency so the host app can show it.
async fn stats(State(shared): State<Arc<Shared>>, Json(s): Json<ViewerStats>) -> StatusCode {
    shared
        .latency_ms
        .store(s.latency_ms.min(60_000), Ordering::Relaxed);
    StatusCode::NO_CONTENT
}

/// Counts a viewer on the JPEG-tile path.
struct TileViewer(Arc<Shared>);

impl TileViewer {
    fn new(shared: Arc<Shared>) -> Self {
        shared.tile_viewers.fetch_add(1, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for TileViewer {
    fn drop(&mut self) {
        self.0.tile_viewers.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(shared): State<Arc<Shared>>) -> Response {
    ws.write_buffer_size(0)
        .max_write_buffer_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| viewer_session(socket, shared))
}

async fn viewer_session(socket: WebSocket, shared: Arc<Shared>) {
    let _guard = ViewerGuard::new(shared.clone());
    let _tile = TileViewer::new(shared.clone());
    let (mut tx, mut rx) = socket.split();
    let mut notify = shared.notify.subscribe();

    let mut sent_version = 0u64;
    let mut epoch = u32::MAX; // forces a full reset on the first update
    let mut seq = 0u32;
    let mut in_flight: VecDeque<(u32, Instant)> = VecDeque::new();
    let mut latency_ms = 0u16;

    loop {
        notify.borrow_and_update();
        let can_send = in_flight.len() < MAX_IN_FLIGHT;

        if can_send {
            let update = {
                let store = shared.store.lock().unwrap();
                if store.width > 0 && (store.version > sent_version || store.epoch != epoch) {
                    seq = seq.wrapping_add(1);
                    let msg = store.build_message(seq, sent_version, epoch, latency_ms);
                    sent_version = store.version;
                    epoch = store.epoch;
                    Some((msg, store.updated_at))
                } else {
                    None
                }
            };
            if let Some((msg, captured_at)) = update {
                shared
                    .bytes_sent
                    .fetch_add(msg.len() as u64, Ordering::Relaxed);
                in_flight.push_back((seq, captured_at));
                if tx.send(Message::Binary(msg)).await.is_err() {
                    break;
                }
                continue;
            }
        }

        tokio::select! {
            changed = notify.changed(), if can_send => {
                if changed.is_err() { break; }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    // Ack: the viewer has drawn update `seq`.
                    if let Ok(acked) = t.as_str().trim().parse::<u32>() {
                        while let Some(&(s, captured_at)) = in_flight.front() {
                            if s.wrapping_sub(acked) as i32 > 0 { break; }
                            in_flight.pop_front();
                            if s == acked {
                                let ms = captured_at.elapsed().as_millis().min(u16::MAX as u128) as u16;
                                latency_ms = ms;
                                shared.latency_ms.store(ms as u32, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
}

/// LAN IPv4 addresses (interface name, ip), most likely Wi-Fi/Ethernet first.
pub fn lan_addresses() -> Vec<(String, Ipv4Addr)> {
    let primary = local_ip_address::local_ip().ok();
    let mut out: Vec<(String, Ipv4Addr)> = local_ip_address::list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, ip)| match ip {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() => {
                Some((name, v4))
            }
            _ => None,
        })
        .collect();
    out.dedup_by(|a, b| a.1 == b.1);

    let virtual_hint = |n: &str| {
        let n = n.to_lowercase();
        [
            "vethernet",
            "virtual",
            "vmware",
            "vbox",
            "docker",
            "wsl",
            "utun",
            "bridge",
            "tailscale",
            "zerotier",
            "hyper-v",
        ]
        .iter()
        .any(|k| n.contains(k))
    };
    out.sort_by_key(|(name, ip)| {
        (
            Some(IpAddr::V4(*ip)) != primary,
            virtual_hint(name),
            !ip.is_private(),
        )
    });
    out
}
