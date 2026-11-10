//! Capture loop: grabs the selected monitor, scales it to RGB, diffs it against
//! the previous frame and encodes only the changed tiles. Runs on its own thread.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::Serialize;
use xcap::Monitor;

use crate::state::{Settings, Shared};
use crate::tiles::{self, Cursor};
use crate::video::RawRgb;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    let monitors = Monitor::all()?;
    Ok(monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = m
                .friendly_name()
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| m.name().ok())
                .unwrap_or_else(|| format!("Display {}", i + 1));
            MonitorInfo {
                id: m.id().unwrap_or(i as u32),
                name,
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                primary: m.is_primary().unwrap_or(false),
            }
        })
        .collect())
}

fn pick_monitor(id: Option<u32>) -> anyhow::Result<Monitor> {
    let monitors = Monitor::all()?;
    if let Some(id) = id {
        if let Some(m) = monitors.iter().find(|m| m.id().ok() == Some(id)) {
            return Ok(m.clone());
        }
    }
    monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No display found"))
}

/// A borrowed view of one captured frame (4 bytes per pixel).
pub struct RawFrame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// true = BGRA byte order, false = RGBA.
    pub bgra: bool,
}

pub trait FrameSource {
    /// Waits up to `timeout_ms` for a new frame and calls `on_frame` with it.
    /// Returns Ok(false) if the screen did not change.
    fn grab(&mut self, timeout_ms: u32, on_frame: &mut dyn FnMut(RawFrame)) -> anyhow::Result<bool>;
    /// Cursor position in source pixels, `None` if hidden/unknown.
    fn cursor(&mut self) -> Option<(i32, i32)>;
}

/// Portable fallback (and the macOS path): polls `xcap::Monitor::capture_image`.
struct XcapSource {
    monitor: Monitor,
    origin: (i32, i32),
    /// Monitor size in the OS's logical units (points on macOS).
    logical_width: u32,
    pixel_width: u32,
}

impl XcapSource {
    fn new(monitor: Monitor) -> Self {
        let origin = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
        let logical_width = monitor.width().unwrap_or(1).max(1);
        Self { monitor, origin, logical_width, pixel_width: logical_width }
    }
}

impl FrameSource for XcapSource {
    fn grab(&mut self, _timeout_ms: u32, on_frame: &mut dyn FnMut(RawFrame)) -> anyhow::Result<bool> {
        let img = self.monitor.capture_image()?;
        let (w, h) = img.dimensions();
        self.pixel_width = w;
        on_frame(RawFrame { data: img.as_raw(), width: w, height: h, stride: w as usize * 4, bgra: false });
        Ok(true)
    }

    fn cursor(&mut self) -> Option<(i32, i32)> {
        let (gx, gy) = global_cursor_pos()?;
        let scale = self.pixel_width as f64 / self.logical_width as f64;
        Some((
            ((gx - self.origin.0 as f64) * scale).round() as i32,
            ((gy - self.origin.1 as f64) * scale).round() as i32,
        ))
    }
}

#[cfg(windows)]
fn global_cursor_pos() -> Option<(f64, f64)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x as f64, p.y as f64))
}

#[cfg(target_os = "macos")]
fn global_cursor_pos() -> Option<(f64, f64)> {
    use objc2_core_graphics::CGEvent;
    let ev = CGEvent::new(None)?;
    let p = CGEvent::location(Some(&*ev));
    Some((p.x, p.y))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn global_cursor_pos() -> Option<(f64, f64)> {
    None
}

fn open_source(monitor: Monitor) -> (Box<dyn FrameSource>, &'static str) {
    #[cfg(windows)]
    {
        let (x, y) = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
        match crate::dxgi::DxgiSource::open(x, y) {
            Ok(s) => return (Box::new(s), "dxgi"),
            Err(e) => eprintln!("DXGI capture unavailable ({e}); falling back to GDI"),
        }
    }
    (Box::new(XcapSource::new(monitor)), "xcap")
}

fn output_size(w: u32, h: u32, max_width: u32) -> (u32, u32) {
    if max_width > 0 && w > max_width {
        (max_width, ((h as u64 * max_width as u64) / w as u64).max(1) as u32)
    } else {
        (w, h)
    }
}

pub fn run(shared: Arc<Shared>) {
    let mut source: Option<(Option<u32>, Box<dyn FrameSource>)> = None;
    let mut cur: Vec<u8> = Vec::new();
    let mut prev: Vec<u8> = Vec::new();
    let mut have_prev = false;
    let mut out_dims = (0u32, 0u32);
    let mut src_dims = (1u32, 1u32);

    let mut last_tile_viewers = 0usize;
    let mut last_rtc_peers = 0usize;

    let mut frames = 0u32;
    let mut stats_start = Instant::now();
    let mut stats_bytes = 0u64;

    while !shared.stop.load(Ordering::Relaxed) {
        // Nobody watching: release the capture device and idle.
        if shared.viewers.load(Ordering::Relaxed) == 0 {
            last_tile_viewers = 0;
            last_rtc_peers = 0;
            if source.take().is_some() {
                have_prev = false;
                prev.clear();
                shared.fps_x10.store(0, Ordering::Relaxed);
                shared.kbps.store(0, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }

        // A viewer path just became active: start it from a complete frame.
        let tv = shared.tile_viewers.load(Ordering::Relaxed);
        let rp = shared.rtc_peers.load(Ordering::Relaxed);
        if have_prev_frame(&prev, out_dims) {
            if rp > 0 && last_rtc_peers == 0 {
                shared.video_slot.put(RawRgb { rgb: Arc::new(prev.clone()), width: out_dims.0, height: out_dims.1, captured_at: Instant::now() });
            }
            if tv > 0 && last_tile_viewers == 0 {
                let q = shared.settings.lock().unwrap().quality.clamp(10, 100);
                let _ = tiles::encode_changes(&shared.store, &prev, None, out_dims.0, out_dims.1, q, Instant::now());
                shared.publish();
            }
        } else if tv > 0 && last_tile_viewers == 0 {
            have_prev = false;
        }
        last_tile_viewers = tv;
        last_rtc_peers = rp;

        let settings: Settings = shared.settings.lock().unwrap().clone();
        let interval = Duration::from_secs_f64(1.0 / settings.fps.clamp(1, 60) as f64);
        let started = Instant::now();

        if source.as_ref().map(|(id, _)| *id) != Some(settings.monitor_id) {
            match pick_monitor(settings.monitor_id) {
                Ok(m) => {
                    let (s, _kind) = open_source(m);
                    source = Some((settings.monitor_id, s));
                    have_prev = false;
                }
                Err(e) => {
                    shared.set_error(Some(format!("Could not open display: {e}")));
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let (_, src) = source.as_mut().unwrap();

        let mut encode_err: Option<anyhow::Error> = None;
        let grabbed = src.grab(100, &mut |f: RawFrame| {
            let captured_at = Instant::now();
            src_dims = (f.width, f.height);
            let (ow, oh) = output_size(f.width, f.height, settings.max_width);
            if (ow, oh) != out_dims {
                out_dims = (ow, oh);
                shared.store.lock().unwrap().reset(ow, oh);
                shared.out_width.store(ow, Ordering::Relaxed);
                shared.out_height.store(oh, Ordering::Relaxed);
                have_prev = false;
            }
            cur.resize((ow * oh * 3) as usize, 0);
            scale_to_rgb(&f, &mut cur, ow, oh);

            // WebRTC/H.264 path: hand the newest frame to the encoder thread.
            if shared.rtc_peers.load(Ordering::Relaxed) > 0 {
                shared.video_slot.put(RawRgb { rgb: Arc::new(cur.clone()), width: ow, height: oh, captured_at });
            }

            // JPEG-tile path: only when someone watches that way.
            if shared.tile_viewers.load(Ordering::Relaxed) > 0 {
                let prev_ref = if have_prev { Some(prev.as_slice()) } else { None };
                match tiles::encode_changes(&shared.store, &cur, prev_ref, ow, oh, settings.quality.clamp(10, 100), captured_at) {
                    Ok(true) => frames += 1,
                    Ok(false) => {}
                    Err(e) => encode_err = Some(e),
                }
            } else {
                frames += 1;
            }
            std::mem::swap(&mut cur, &mut prev);
            have_prev = true;
        });

        let got_frame = matches!(grabbed, Ok(true));
        match grabbed {
            Ok(_) => shared.set_error(encode_err.map(|e| format!("Encode failed: {e}"))),
            Err(e) => {
                shared.set_error(Some(capture_error_message(&e.to_string())));
                source = None; // re-open (display change, UAC prompt, unplugged monitor…)
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        }

        // Cursor, mapped from source pixels to output pixels.
        let cursor = match src.cursor() {
            Some((x, y)) if out_dims.0 > 0 => {
                let cx = (x as i64 * out_dims.0 as i64 / src_dims.0.max(1) as i64) as i32;
                let cy = (y as i64 * out_dims.1 as i64 / src_dims.1.max(1) as i64) as i32;
                let inside = cx >= 0 && cy >= 0 && cx < out_dims.0 as i32 && cy < out_dims.1 as i32;
                Cursor { x: cx, y: cy, visible: inside }
            }
            _ => Cursor::default(),
        };
        let cursor_moved = {
            let mut st = shared.store.lock().unwrap();
            let before = st.cursor;
            st.set_cursor(cursor);
            before != cursor
        };
        // The pointer is drawn into the video, so a pure mouse move needs a re-encode.
        if cursor_moved && !got_frame && shared.rtc_peers.load(Ordering::Relaxed) > 0 && have_prev_frame(&prev, out_dims) {
            shared.video_slot.put(RawRgb { rgb: Arc::new(prev.clone()), width: out_dims.0, height: out_dims.1, captured_at: Instant::now() });
        }
        shared.publish();

        let win = stats_start.elapsed();
        if win >= Duration::from_secs(1) {
            let secs = win.as_secs_f64();
            shared.fps_x10.store((frames as f64 / secs * 10.0).round() as u32, Ordering::Relaxed);
            let total = shared.bytes_sent.load(Ordering::Relaxed);
            shared.kbps.store(((total - stats_bytes) as f64 * 8.0 / 1000.0 / secs) as u32, Ordering::Relaxed);
            stats_bytes = total;
            frames = 0;
            stats_start = Instant::now();
        }

        let spent = started.elapsed();
        if spent < interval {
            std::thread::sleep(interval - spent);
        }
    }
}

fn have_prev_frame(prev: &[u8], dims: (u32, u32)) -> bool {
    dims.0 > 0 && prev.len() == (dims.0 * dims.1 * 3) as usize
}

fn capture_error_message(raw: &str) -> String {
    if cfg!(target_os = "macos") {
        format!(
            "Screen capture failed ({raw}). Grant permission in System Settings → Privacy & Security → Screen Recording, then restart the app."
        )
    } else {
        format!("Screen capture failed: {raw}")
    }
}

/// Converts a 4-byte-per-pixel frame to packed RGB at `dw`×`dh`, using 2×2
/// super-sampling when downscaling. Parallel over rows.
fn scale_to_rgb(f: &RawFrame, dst: &mut [u8], dw: u32, dh: u32) {
    let (ri, bi) = if f.bgra { (2, 0) } else { (0, 2) };
    let row_len = dw as usize * 3;

    if dw == f.width && dh == f.height {
        dst.par_chunks_mut(row_len).enumerate().for_each(|(y, row)| {
            let src = &f.data[y * f.stride..y * f.stride + dw as usize * 4];
            for (d, s) in row.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                d[0] = s[ri];
                d[1] = s[1];
                d[2] = s[bi];
            }
        });
        return;
    }

    let (sw, sh) = (f.width, f.height);
    let rx = sw as f32 / dw as f32;
    let ry = sh as f32 / dh as f32;
    let xs: Vec<(usize, usize)> = (0..dw)
        .map(|x| {
            let a = (((x as f32 + 0.25) * rx) as u32).min(sw - 1) as usize * 4;
            let b = (((x as f32 + 0.75) * rx) as u32).min(sw - 1) as usize * 4;
            (a, b)
        })
        .collect();

    dst.par_chunks_mut(row_len).enumerate().for_each(|(y, row)| {
        let ya = (((y as f32 + 0.25) * ry) as u32).min(sh - 1) as usize * f.stride;
        let yb = (((y as f32 + 0.75) * ry) as u32).min(sh - 1) as usize * f.stride;
        let ra = &f.data[ya..ya + sw as usize * 4];
        let rb = &f.data[yb..yb + sw as usize * 4];
        for (px, &(xa, xb)) in row.chunks_exact_mut(3).zip(xs.iter()) {
            for (c, si) in [ri, 1, bi].into_iter().enumerate() {
                let sum = ra[xa + si] as u16 + ra[xb + si] as u16 + rb[xa + si] as u16 + rb[xb + si] as u16;
                px[c] = ((sum + 2) >> 2) as u8;
            }
        }
    });
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::sync::Mutex;

    /// cargo test --release stage_timings -- --ignored --nocapture
    #[test]
    #[ignore]
    fn stage_timings() {
        let m = pick_monitor(None).unwrap();
        let (mut src, kind) = open_source(m);
        let store = Mutex::new(tiles::Store::new());
        let mut prev: Option<Vec<u8>> = None;
        let (mut grab_t, mut enc_t, mut n) = (Duration::ZERO, Duration::ZERO, 0u32);
        let start = Instant::now();
        while n < 20 && start.elapsed() < Duration::from_secs(10) {
            let t = Instant::now();
            let mut enc = Duration::ZERO;
            let got = src
                .grab(200, &mut |f| {
                    let (ow, oh) = output_size(f.width, f.height, 1920);
                    if store.lock().unwrap().width != ow {
                        store.lock().unwrap().reset(ow, oh);
                    }
                    let t2 = Instant::now();
                    let mut cur = vec![0u8; (ow * oh * 3) as usize];
                    scale_to_rgb(&f, &mut cur, ow, oh);
                    // Force a full re-encode to measure the worst case.
                    tiles::encode_changes(&store, &cur, None, ow, oh, 70, Instant::now()).unwrap();
                    enc = t2.elapsed();
                    prev = Some(cur);
                })
                .unwrap();
            if got {
                grab_t += t.elapsed() - enc;
                enc_t += enc;
                n += 1;
            }
        }
        let st = store.lock().unwrap();
        let full = st.build_message(1, 0, st.epoch, 0).len();
        println!(
            "source={kind} frames={n} grab={:?}/frame full-screen scale+encode={:?}/frame full update={} KB",
            grab_t / n.max(1),
            enc_t / n.max(1),
            full / 1024
        );
    }
}
