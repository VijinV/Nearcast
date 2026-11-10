//! H.264 encoding for the WebRTC path.
//!
//! One shared encoder serves every WebRTC viewer, so all of them receive the
//! exact same frames at the same moment (they stay in sync). It runs on its own
//! thread fed with the newest RGB frame from the capture loop; if encoding falls
//! behind, older frames are dropped rather than queued.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use rayon::prelude::*;

use crate::state::Shared;

/// The newest captured frame, handed from the capture thread to the encoder.
pub struct RawRgb {
    pub rgb: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub captured_at: Instant,
}

#[derive(Default)]
pub struct FrameSlot {
    inner: Mutex<SlotState>,
    cv: Condvar,
}

#[derive(Default)]
struct SlotState {
    latest: Option<Arc<RawRgb>>,
    fresh: bool,
}

impl FrameSlot {
    pub fn put(&self, f: RawRgb) {
        let mut s = self.inner.lock().unwrap();
        s.latest = Some(Arc::new(f));
        s.fresh = true;
        self.cv.notify_one();
    }

    pub fn wake(&self) {
        self.cv.notify_one();
    }

    /// Waits for a new frame. Returns (frame, is_new). With `want_repeat` the last
    /// frame is returned again even if unchanged (used to answer keyframe requests).
    fn take(&self, timeout: Duration, want_repeat: bool) -> Option<(Arc<RawRgb>, bool)> {
        let mut s = self.inner.lock().unwrap();
        if !s.fresh && !want_repeat {
            s = self.cv.wait_timeout(s, timeout).unwrap().0;
        }
        let latest = s.latest.clone()?;
        let fresh = std::mem::replace(&mut s.fresh, false);
        (fresh || want_repeat).then_some((latest, fresh))
    }
}

/// One encoded access unit (Annex-B), shared by all peers.
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Arc<[u8]>,
    pub keyframe: bool,
    pub captured_at: Instant,
    /// Monotonic 90 kHz RTP timestamp.
    pub rtp_90k: u64,
}

pub struct H264 {
    enc: Encoder,
    width: u32,
    height: u32,
    bitrate: u32,
    yuv: Vec<u8>,
}

impl H264 {
    pub fn new(width: u32, height: u32, bitrate: u32, fps: u32) -> anyhow::Result<Self> {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 4) as u16;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            // Camera mode handles fast motion (video playback) much better than
            // the screen-content mode, which is tuned for slides and text.
            .usage_type(UsageType::CameraVideoRealTime)
            .complexity(Complexity::Low)
            .skip_frames(true)
            .scene_change_detect(true)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // Periodic IDR every ~4 s so viewers recover quickly from any loss.
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1) * 4))
            .num_threads(threads);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), config)?;
        Ok(Self {
            enc,
            width,
            height,
            bitrate,
            yuv: Vec::new(),
        })
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.bitrate {
            return;
        }
        self.bitrate = bps;
        let mut info = openh264_sys2::TagBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: bps as i32,
        };
        unsafe {
            let api = self.enc.raw_api();
            api.set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&mut info as *mut openh264_sys2::TagBitrateInfo).cast(),
            );
            info.iLayer = openh264_sys2::SPATIAL_LAYER_0;
            api.set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&mut info as *mut openh264_sys2::TagBitrateInfo).cast(),
            );
            let mut max = openh264_sys2::TagBitrateInfo {
                iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                iBitrate: (bps as f32 * 1.5) as i32,
            };
            api.set_option(
                openh264_sys2::ENCODER_OPTION_MAX_BITRATE,
                (&mut max as *mut openh264_sys2::TagBitrateInfo).cast(),
            );
        }
    }

    pub fn force_keyframe(&mut self) {
        self.enc.force_intra_frame();
    }

    /// Encodes packed RGB (even dimensions). Returns Annex-B data, or None if the
    /// rate controller skipped the frame.
    pub fn encode(&mut self, rgb: &[u8]) -> anyhow::Result<Option<(Vec<u8>, bool)>> {
        let (w, h) = (self.width as usize, self.height as usize);
        rgb_to_i420(rgb, w, h, &mut self.yuv);
        let (y, uv) = self.yuv.split_at(w * h);
        let (u, v) = uv.split_at((w / 2) * (h / 2));
        let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        let bs = self.enc.encode(&src)?;
        let keyframe = matches!(
            bs.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        let data = bs.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some((data, keyframe)))
    }
}

/// BT.601 limited-range RGB → I420, parallel over pairs of rows.
pub fn rgb_to_i420(rgb: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    let (cw, ch) = (w / 2, h / 2);
    out.resize(w * h + 2 * cw * ch, 0);
    let (y_plane, uv) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(cw * ch);

    y_plane
        .par_chunks_mut(w * 2)
        .zip(u_plane.par_chunks_mut(cw))
        .zip(v_plane.par_chunks_mut(cw))
        .enumerate()
        .for_each(|(cy, ((yrows, urow), vrow))| {
            let r0 = &rgb[(cy * 2) * w * 3..(cy * 2 + 1) * w * 3];
            let r1 = &rgb[(cy * 2 + 1) * w * 3..(cy * 2 + 2) * w * 3];
            let (y0, y1) = yrows.split_at_mut(w);
            for x in 0..w {
                let p = &r0[x * 3..x * 3 + 3];
                y0[x] = luma(p[0], p[1], p[2]);
                let p = &r1[x * 3..x * 3 + 3];
                y1[x] = luma(p[0], p[1], p[2]);
            }
            for cx in 0..cw {
                let mut r = 0i32;
                let mut g = 0i32;
                let mut b = 0i32;
                for row in [r0, r1] {
                    for dx in 0..2 {
                        let i = (cx * 2 + dx) * 3;
                        r += row[i] as i32;
                        g += row[i + 1] as i32;
                        b += row[i + 2] as i32;
                    }
                }
                let (r, g, b) = (r / 4, g / 4, b / 4);
                urow[cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                vrow[cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        });
}

#[inline(always)]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    (((66 * r as i32 + 129 * g as i32 + 25 * b as i32 + 128) >> 8) + 16) as u8
}

/// Target bitrate for the user's quality setting (30..=95), before network caps.
pub fn target_bitrate(width: u32, height: u32, fps: u32, quality: u8) -> u32 {
    // bits per pixel per frame: 0.05 (q30) .. 0.16 (q95)
    let bpp = 0.05 + (quality.clamp(30, 95) as f64 - 30.0) / 65.0 * 0.11;
    let bps = width as f64 * height as f64 * fps.clamp(5, 60) as f64 * bpp;
    bps.clamp(800_000.0, 20_000_000.0) as u32
}

/// Hardware encoder (GPU) when available, OpenH264 (CPU) otherwise.
enum AnyEncoder {
    #[cfg(windows)]
    Hw(Box<crate::mf::MfEncoder>),
    Sw(Box<H264>),
}

impl AnyEncoder {
    fn open(
        w: u32,
        h: u32,
        bitrate: u32,
        fps: u32,
        try_hw: bool,
    ) -> anyhow::Result<(Self, String)> {
        #[cfg(windows)]
        if try_hw {
            match crate::mf::MfEncoder::new(w, h, fps, bitrate) {
                Ok(e) => {
                    let name = format!("GPU: {}", e.name);
                    return Ok((AnyEncoder::Hw(Box::new(e)), name));
                }
                Err(e) => eprintln!("hardware encoder unavailable ({e}); using CPU encoder"),
            }
        }
        let _ = try_hw;
        Ok((
            AnyEncoder::Sw(Box::new(H264::new(w, h, bitrate, fps)?)),
            "CPU: OpenH264".into(),
        ))
    }

    fn is_hw(&self) -> bool {
        !matches!(self, AnyEncoder::Sw(_))
    }

    fn set_bitrate(&mut self, bps: u32) {
        match self {
            #[cfg(windows)]
            AnyEncoder::Hw(e) => e.set_bitrate(bps),
            AnyEncoder::Sw(e) => e.set_bitrate(bps),
        }
    }

    fn force_keyframe(&mut self) {
        match self {
            #[cfg(windows)]
            AnyEncoder::Hw(e) => e.force_keyframe(),
            AnyEncoder::Sw(e) => e.force_keyframe(),
        }
    }

    fn encode(&mut self, rgb: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, bool)>> {
        match self {
            #[cfg(windows)]
            AnyEncoder::Hw(e) => e.encode(rgb),
            AnyEncoder::Sw(e) => Ok(e.encode(rgb)?.into_iter().collect()),
        }
    }
}

/// Video widths to step through when the encoder can't keep up.
const WIDTH_STEPS: [u32; 5] = [1920, 1600, 1280, 1024, 854];

/// Encoder thread: encodes the newest frame whenever WebRTC viewers exist and
/// broadcasts the result to all of them.
pub fn run(shared: Arc<Shared>) {
    let start = Instant::now();
    let mut enc: Option<AnyEncoder> = None;
    let mut enc_key = (0u32, 0u32, 0u32);
    let mut hw_ok = true;
    let mut last_keyframe_req = Instant::now() - Duration::from_secs(10);
    let mut step = 0usize;
    let mut slow_since: Option<Instant> = None;
    let mut fast_since: Option<Instant> = None;
    let mut avg_ms = 0.0f64;
    let mut scaled: Vec<u8> = Vec::new();
    let mut last_rtp = 0u64;

    while !shared.stop.load(Ordering::Relaxed) {
        if shared.rtc_peers.load(Ordering::Relaxed) == 0 {
            enc = None;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let wants_key = shared.keyframe_requested.load(Ordering::Relaxed)
            && last_keyframe_req.elapsed() > Duration::from_millis(250);
        let Some((frame, _fresh)) = shared
            .video_slot
            .take(Duration::from_millis(100), wants_key)
        else {
            continue;
        };

        let settings = shared.settings.lock().unwrap().clone();
        let fps = settings.fps.clamp(5, 60);
        let interval_ms = 1000.0 / fps as f64;

        // Video size: never above the captured size or the current speed step.
        let vw = WIDTH_STEPS[step].min(frame.width) & !1;
        let vh = (((frame.height as u64 * vw as u64) / frame.width.max(1) as u64) as u32) & !1;
        if vw < 16 || vh < 16 {
            continue;
        }

        let mut bitrate = target_bitrate(vw, vh, fps, settings.quality);
        let net_cap = shared.min_peer_bwe.load(Ordering::Relaxed);
        if net_cap > 0 {
            bitrate = bitrate.min((net_cap as f64 * 0.85) as u32).max(500_000);
        }

        if enc.is_none() || enc_key != (vw, vh, fps) {
            enc = None; // release the old encoder (GPU sessions are limited)
            match AnyEncoder::open(vw, vh, bitrate, fps, hw_ok) {
                Ok((e, name)) => {
                    *shared.encoder_name.lock().unwrap() = name;
                    enc_key = (vw, vh, fps);
                    enc = Some(e);
                    shared.keyframe_requested.store(false, Ordering::Relaxed);
                }
                Err(e) => {
                    shared.set_error(Some(format!("H.264 encoder failed: {e}")));
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
            avg_ms = 0.0;
        }
        let e = enc.as_mut().unwrap();
        e.set_bitrate(bitrate);
        shared
            .video_kbps_target
            .store(bitrate / 1000, Ordering::Relaxed);
        shared.video_width.store(vw, Ordering::Relaxed);
        shared.video_height.store(vh, Ordering::Relaxed);

        if wants_key {
            shared.keyframe_requested.store(false, Ordering::Relaxed);
            last_keyframe_req = Instant::now();
            e.force_keyframe();
        }

        let t = Instant::now();
        scale_rgb(&frame.rgb, frame.width, frame.height, &mut scaled, vw, vh);
        let cursor = shared.store.lock().unwrap().cursor;
        if cursor.visible {
            let cx = (cursor.x as i64 * vw as i64 / frame.width.max(1) as i64) as i32;
            let cy = (cursor.y as i64 * vh as i64 / frame.height.max(1) as i64) as i32;
            draw_cursor(&mut scaled, vw, vh, cx, cy);
        }

        match e.encode(&scaled) {
            Ok(outputs) => {
                if !outputs.is_empty() {
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    avg_ms = if avg_ms == 0.0 {
                        ms
                    } else {
                        avg_ms * 0.9 + ms * 0.1
                    };
                    shared
                        .encode_ms_x10
                        .store((avg_ms * 10.0) as u32, Ordering::Relaxed);
                }
                for (data, keyframe) in outputs {
                    let mut rtp = start.elapsed().as_micros() as u64 * 9 / 100;
                    if rtp <= last_rtp {
                        rtp = last_rtp + 1;
                    }
                    last_rtp = rtp;
                    let _ = shared.video_tx.send(EncodedFrame {
                        data: data.into(),
                        keyframe,
                        captured_at: frame.captured_at,
                        rtp_90k: rtp,
                    });
                }
            }
            Err(err) => {
                if e.is_hw() {
                    // GPU encoder broke (driver reset, GPU switched…): use the CPU from now on.
                    eprintln!("hardware encode failed ({err}); switching to CPU encoder");
                    hw_ok = false;
                    enc = None;
                    shared.keyframe_requested.store(true, Ordering::Relaxed);
                } else {
                    shared.set_error(Some(format!("H.264 encode failed: {err}")));
                }
                continue;
            }
        }

        // CPU encoder only: step resolution down if it can't keep up. The GPU
        // encoder always runs at full resolution; if it is ever slower than the
        // frame rate, the newest-frame slot simply skips frames (no queueing).
        let is_hw = enc.as_ref().map(|e| e.is_hw()).unwrap_or(false);
        if is_hw {
            step = 0;
            slow_since = None;
            fast_since = None;
            continue;
        }
        let now = Instant::now();
        if avg_ms > interval_ms * 0.85 {
            fast_since = None;
            let since = *slow_since.get_or_insert(now);
            if now - since > Duration::from_millis(700) && step + 1 < WIDTH_STEPS.len() {
                step += 1;
                slow_since = None;
            }
        } else if avg_ms < interval_ms * 0.4 {
            slow_since = None;
            let since = *fast_since.get_or_insert(now);
            if now - since > Duration::from_secs(8) && step > 0 && WIDTH_STEPS[step] < frame.width {
                step -= 1;
                fast_since = None;
            }
        } else {
            slow_since = None;
            fast_since = None;
        }
    }
}

/// Packed RGB → packed RGB at (dw, dh) with 2×2 super-sampling (or a copy).
fn scale_rgb(src: &[u8], sw: u32, sh: u32, dst: &mut Vec<u8>, dw: u32, dh: u32) {
    dst.resize((dw * dh * 3) as usize, 0);
    let src_stride = sw as usize * 3;
    if sw == dw && sh == dh {
        dst.copy_from_slice(&src[..(dw * dh * 3) as usize]);
        return;
    }
    let rx = sw as f32 / dw as f32;
    let ry = sh as f32 / dh as f32;
    let xs: Vec<(usize, usize)> = (0..dw)
        .map(|x| {
            let a = (((x as f32 + 0.25) * rx) as u32).min(sw - 1) as usize * 3;
            let b = (((x as f32 + 0.75) * rx) as u32).min(sw - 1) as usize * 3;
            (a, b)
        })
        .collect();
    dst.par_chunks_mut(dw as usize * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let ya = (((y as f32 + 0.25) * ry) as u32).min(sh - 1) as usize * src_stride;
            let yb = (((y as f32 + 0.75) * ry) as u32).min(sh - 1) as usize * src_stride;
            let ra = &src[ya..ya + src_stride];
            let rb = &src[yb..yb + src_stride];
            for (px, &(xa, xb)) in row.as_chunks_mut::<3>().0.iter_mut().zip(xs.iter()) {
                for c in 0..3 {
                    let sum = ra[xa + c] as u16
                        + ra[xb + c] as u16
                        + rb[xa + c] as u16
                        + rb[xb + c] as u16;
                    px[c] = ((sum + 2) >> 2) as u8;
                }
            }
        });
}

/// Arrow pointer bitmap: '#' = black outline, '.' = white fill.
const CURSOR: [&str; 19] = [
    "#           ",
    "##          ",
    "#.#         ",
    "#..#        ",
    "#...#       ",
    "#....#      ",
    "#.....#     ",
    "#......#    ",
    "#.......#   ",
    "#........#  ",
    "#.........# ",
    "#......#####",
    "#...#..#    ",
    "#..##..#    ",
    "#.#  #..#   ",
    "##   #..#   ",
    "#     #..#  ",
    "      #..#  ",
    "       ##   ",
];

/// Draws the pointer into the frame so it is exactly in sync with the video.
fn draw_cursor(rgb: &mut [u8], w: u32, h: u32, x: i32, y: i32) {
    let scale = if w >= 1600 { 2 } else { 1 };
    for (ry, line) in CURSOR.iter().enumerate() {
        for (rx, ch) in line.bytes().enumerate() {
            let color = match ch {
                b'#' => 0u8,
                b'.' => 255u8,
                _ => continue,
            };
            for sy in 0..scale {
                for sx in 0..scale {
                    let px = x + rx as i32 * scale + sx;
                    let py = y + ry as i32 * scale + sy;
                    if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                        continue;
                    }
                    let i = (py as usize * w as usize + px as usize) * 3;
                    rgb[i..i + 3].fill(color);
                }
            }
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test --release h264_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn h264_speed() {
        for (w, h) in [(1920u32, 1080u32), (1280, 720)] {
            // Moving gradient + noise approximates video content.
            let mut enc = H264::new(w, h, target_bitrate(w, h, 30, 70), 30).unwrap();
            let mut rgb = vec![0u8; (w * h * 3) as usize];
            let mut total = Duration::ZERO;
            let mut bytes = 0usize;
            let n = 60;
            let mut seed = 1u32;
            for f in 0..n {
                for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let x = (i as u32 % w + f * 8) as u8;
                    let y = (i as u32 / w + f * 4) as u8;
                    px[0] = x;
                    px[1] = y.wrapping_add((seed >> 28) as u8);
                    px[2] = x ^ y;
                }
                let t = Instant::now();
                let out = enc.encode(&rgb).unwrap();
                total += t.elapsed();
                bytes += out.map(|o| o.0.len()).unwrap_or(0);
            }
            println!(
                "{w}x{h}: {:?}/frame (incl. RGB→YUV), {:.1} Mbps at 30 fps",
                total / n,
                bytes as f64 * 8.0 / (n as f64 / 30.0) / 1e6
            );
        }
    }
}
