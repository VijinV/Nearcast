//! WebRTC sending side, built on str0m (sans-IO, pure Rust crypto).
//!
//! Each browser POSTs an SDP offer to `/offer`; we answer with a host candidate
//! on a fresh UDP socket and then push the shared H.264 stream to it. WebRTC
//! gives us real-time UDP delivery, NACK retransmission, keyframe requests and
//! congestion control (TWCC). The browser's jitter buffer is told to stay at 0.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use str0m::bwe::BweKind;
use str0m::rtp::Extension;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::broadcast::error::RecvError;

use crate::state::{Shared, ViewerGuard};

static CRYPTO: Once = Once::new();
static NEXT_PEER: AtomicU64 = AtomicU64::new(1);

/// Per-peer congestion-control estimates; the encoder follows the slowest
/// viewer so every viewer stays real-time and in sync.
fn bwe_table() -> &'static Mutex<HashMap<u64, u32>> {
    static T: OnceLock<Mutex<HashMap<u64, u32>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn update_min_bwe(shared: &Shared) {
    let t = bwe_table().lock().unwrap();
    shared.min_peer_bwe.store(t.values().copied().min().unwrap_or(0), Ordering::Relaxed);
}

pub async fn accept_offer(shared: Arc<Shared>, offer: SdpOffer, local_ip: IpAddr) -> anyhow::Result<SdpAnswer> {
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

    // No BWE: on a LAN, str0m's pacer would spread every frame over ~50-200 ms
    // (it paces at 1.1x the estimate), and the browser reads that spread as
    // jitter and buffers more. Without BWE packets leave immediately; bitrate
    // follows the user's quality setting instead.
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_h264(true)
        // Lets us tell the browser "render frames immediately" (min = max = 0).
        .set_extension(12, Extension::PlayoutDelay)
        .build(Instant::now());

    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let local = socket.local_addr()?;
    rtc.add_local_candidate(Candidate::host(local, "udp")?)
        .ok_or_else(|| anyhow::anyhow!("candidate rejected"))?;
    let answer = rtc.sdp_api().accept_offer(offer)?;

    tokio::spawn(peer_loop(rtc, socket, local, shared));
    Ok(answer)
}

/// Counts a connected WebRTC peer and cleans up its BWE entry on drop.
struct PeerGuard {
    id: u64,
    shared: Arc<Shared>,
    connected: bool,
}

impl PeerGuard {
    fn connect(&mut self) {
        if !self.connected {
            self.connected = true;
            self.shared.rtc_peers.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        if self.connected {
            self.shared.rtc_peers.fetch_sub(1, Ordering::SeqCst);
        }
        bwe_table().lock().unwrap().remove(&self.id);
        update_min_bwe(&self.shared);
    }
}

fn pick_h264_pt(rtc: &mut Rtc, mid: Mid) -> Option<Pt> {
    let writer = rtc.writer(mid)?;
    let mut best: Option<(u8, Pt)> = None;
    for p in writer.payload_params() {
        let spec = p.spec();
        if spec.codec != Codec::H264 || spec.format.packetization_mode != Some(1) {
            continue;
        }
        // Prefer constrained baseline (what OpenH264 produces).
        let rank = match spec.format.profile_level_id {
            Some(0x42e01f) => 0,
            Some(0x42001f) => 1,
            _ => 2,
        };
        if best.map(|(r, _)| rank < r).unwrap_or(true) {
            best = Some((rank, p.pt()));
        }
    }
    best.map(|(_, pt)| pt)
}

async fn peer_loop(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, shared: Arc<Shared>) {
    let id = NEXT_PEER.fetch_add(1, Ordering::Relaxed);
    let _viewer = ViewerGuard::new(shared.clone());
    let mut peer = PeerGuard { id, shared: shared.clone(), connected: false };
    let mut video_rx = shared.video_tx.subscribe();
    let mut mid: Option<Mid> = None;
    let mut pt: Option<Pt> = None;
    let mut need_keyframe = true;
    let mut buf = vec![0u8; 2048];
    let created = Instant::now();

    loop {
        // Drive str0m until it wants to wait.
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(ev)) => match ev {
                    Event::Connected => {
                        peer.connect();
                        shared.keyframe_requested.store(true, Ordering::Relaxed);
                        shared.video_slot.wake();
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return,
                    Event::MediaAdded(m) => {
                        mid = Some(m.mid);
                        pt = pick_h264_pt(&mut rtc, m.mid);
                    }
                    Event::KeyframeRequest(_) => {
                        shared.keyframe_requested.store(true, Ordering::Relaxed);
                        shared.video_slot.wake();
                    }
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        bwe_table().lock().unwrap().insert(id, estimate.as_u64() as u32);
                        update_min_bwe(&shared);
                    }
                    _ => {}
                },
                Err(e) => {
                    eprintln!("webrtc peer {id}: {e}");
                    return;
                }
            }
        };
        if !rtc.is_alive() || shared.stop.load(Ordering::Relaxed) {
            return;
        }
        if !peer.connected && created.elapsed() > Duration::from_secs(20) {
            return; // never connected (firewall / unreachable)
        }
        let wait = timeout.saturating_duration_since(Instant::now());
        let can_send = peer.connected && mid.is_some() && pt.is_some();

        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = r else { return };
                let Ok(contents) = buf[..n].try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                if rtc.accepts(&input) {
                    if let Err(e) = rtc.handle_input(input) {
                        eprintln!("webrtc peer {id}: {e}");
                        return;
                    }
                }
            }
            f = video_rx.recv(), if can_send => match f {
                Ok(frame) => {
                    if need_keyframe && !frame.keyframe {
                        shared.keyframe_requested.store(true, Ordering::Relaxed);
                        continue;
                    }
                    need_keyframe = false;
                    let Some(writer) = rtc.writer(mid.unwrap()) else { continue };
                    let res = writer
                        // min = max = 0: ask the browser to render frames immediately.
                        .playout_delay(MediaTime::from_millis(0), MediaTime::from_millis(0))
                        .write(pt.unwrap(), frame.captured_at, MediaTime::from_90khz(frame.rtp_90k), frame.data.clone());
                    if let Err(e) = res {
                        eprintln!("webrtc peer {id}: write failed: {e}");
                    }
                    shared.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                }
                Err(RecvError::Lagged(_)) => {
                    // We skipped frames; the decoder needs a fresh keyframe.
                    need_keyframe = true;
                    shared.keyframe_requested.store(true, Ordering::Relaxed);
                }
                Err(RecvError::Closed) => return,
            },
            _ = tokio::time::sleep(wait) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    eprintln!("webrtc peer {id}: {e}");
                    return;
                }
            }
        }
    }
}
