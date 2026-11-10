use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use tokio::sync::broadcast;

use crate::tiles::Store;
use crate::video::{EncodedFrame, FrameSlot};

/// User-tunable stream settings. Can be changed live while sharing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// xcap monitor id; `None` = primary monitor.
    pub monitor_id: Option<u32>,
    /// Max frames per second (1..=60).
    pub fps: u32,
    /// JPEG quality (10..=100).
    pub quality: u8,
    /// Max output width in pixels; 0 = native resolution.
    pub max_width: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { monitor_id: None, fps: 60, quality: 75, max_width: 1920 }
    }
}

/// State shared between the capture thread, the HTTP server and Tauri commands
/// for one sharing session.
pub struct Shared {
    pub settings: Arc<Mutex<Settings>>,
    /// Current screen as tiles + cursor.
    pub store: Mutex<Store>,
    /// Pinged with the store version whenever it changes.
    pub notify: watch::Sender<u64>,
    /// Number of connected viewers.
    pub viewers: AtomicUsize,
    /// Changed frames encoded per second * 10.
    pub fps_x10: AtomicU32,
    /// Most recent capture → on-screen latency reported by a viewer (ms).
    pub latency_ms: AtomicU32,
    /// Total bytes sent to viewers, and the measured rate.
    pub bytes_sent: AtomicU64,
    pub kbps: AtomicU32,
    pub out_width: AtomicU32,
    pub out_height: AtomicU32,
    /// Set to stop the capture thread.
    pub stop: AtomicBool,
    /// Last capture error (e.g. missing screen-recording permission on macOS).
    pub last_error: Mutex<Option<String>>,

    // ---- WebRTC / H.264 path ----
    /// Connected WebRTC viewers (subset of `viewers`).
    pub rtc_peers: AtomicUsize,
    /// Newest RGB frame for the H.264 encoder thread.
    pub video_slot: FrameSlot,
    /// Encoded H.264 frames, fanned out to every WebRTC peer.
    pub video_tx: broadcast::Sender<EncodedFrame>,
    /// A viewer joined, lost packets or asked for a keyframe.
    pub keyframe_requested: AtomicBool,
    /// Lowest congestion-control estimate across peers (bps, 0 = unknown).
    pub min_peer_bwe: AtomicU32,
    pub video_kbps_target: AtomicU32,
    pub encode_ms_x10: AtomicU32,
    pub video_width: AtomicU32,
    pub video_height: AtomicU32,
    /// Viewers on the JPEG-tile WebSocket path (incl. the in-app preview).
    pub tile_viewers: AtomicUsize,
    /// Which H.264 encoder is active, e.g. "GPU: NVIDIA H.264 Encoder MFT".
    pub encoder_name: Mutex<String>,
}

impl Shared {
    pub fn new(settings: Arc<Mutex<Settings>>) -> Self {
        let (notify, _) = watch::channel(0);
        Self {
            settings,
            store: Mutex::new(Store::new()),
            notify,
            viewers: AtomicUsize::new(0),
            fps_x10: AtomicU32::new(0),
            latency_ms: AtomicU32::new(0),
            bytes_sent: AtomicU64::new(0),
            kbps: AtomicU32::new(0),
            out_width: AtomicU32::new(0),
            out_height: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            last_error: Mutex::new(None),
            rtc_peers: AtomicUsize::new(0),
            video_slot: FrameSlot::default(),
            video_tx: broadcast::channel(8).0,
            keyframe_requested: AtomicBool::new(false),
            min_peer_bwe: AtomicU32::new(0),
            video_kbps_target: AtomicU32::new(0),
            encode_ms_x10: AtomicU32::new(0),
            video_width: AtomicU32::new(0),
            video_height: AtomicU32::new(0),
            tile_viewers: AtomicUsize::new(0),
            encoder_name: Mutex::new(String::new()),
        }
    }

    pub fn set_error(&self, err: Option<String>) {
        let mut e = self.last_error.lock().unwrap();
        if *e != err {
            *e = err;
        }
    }

    /// Wake all viewer tasks after the store changed.
    pub fn publish(&self) {
        let v = self.store.lock().unwrap().version;
        self.notify.send_replace(v);
    }
}

/// Increments the viewer count while alive; decrements when the client disconnects.
pub struct ViewerGuard(Arc<Shared>);

impl ViewerGuard {
    pub fn new(shared: Arc<Shared>) -> Self {
        shared.viewers.fetch_add(1, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        if self.0.viewers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.latency_ms.store(0, Ordering::Relaxed);
        }
    }
}
