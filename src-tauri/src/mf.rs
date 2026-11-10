//! Hardware H.264 encoding on Windows through Media Foundation.
//!
//! Uses whatever hardware encoder the GPU driver registers: NVIDIA NVENC,
//! AMD VCN/AMF or Intel Quick Sync. These run on dedicated video silicon, so a
//! 1080p frame encodes in a few milliseconds at full quality while the CPU stays
//! free. Hardware MFTs are asynchronous: they ask for input (NeedInput) and
//! announce output (HaveOutput) through an event queue that we pump here.

use std::mem::ManuallyDrop;
use std::sync::Once;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

static MF_INIT: Once = Once::new();

fn init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    MF_INIT.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    nv12: Vec<u8>,
    frame_index: i64,
    pub name: String,
}

// The MFT is only ever used from the encoder thread that created it.
unsafe impl Send for MfEncoder {}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Hardware H.264 encoders, best first (NVIDIA, Intel, AMD, others).
fn enumerate() -> Vec<(String, IMFActivate)> {
    let mut out = Vec::new();
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .is_err()
            || list.is_null()
        {
            return out;
        }
        for i in 0..count as usize {
            if let Some(act) = (*list.add(i)).take() {
                let mut name = String::from("Hardware H.264 encoder");
                let mut p = windows::core::PWSTR::null();
                let mut len = 0u32;
                if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() && !p.is_null() {
                    name = p.to_string().unwrap_or(name);
                    CoTaskMemFree(Some(p.0 as *const _));
                }
                out.push((name, act));
            }
        }
        CoTaskMemFree(Some(list as *const _));
    }
    let rank = |n: &str| {
        let n = n.to_lowercase();
        if n.contains("nvidia") { 0 } else if n.contains("intel") { 1 } else if n.contains("amd") { 2 } else { 3 }
    };
    out.sort_by_key(|(n, _)| rank(n));
    out
}

impl MfEncoder {
    /// Opens the best hardware encoder that accepts this format and proves it
    /// works by encoding a test frame.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> anyhow::Result<Self> {
        init();
        let candidates = enumerate();
        if candidates.is_empty() {
            anyhow::bail!("no hardware H.264 encoder found");
        }
        let mut errors = Vec::new();
        for (name, act) in candidates {
            match Self::open(&name, &act, width, height, fps, bitrate) {
                Ok(mut enc) => {
                    // Smoke test: a black frame must produce output.
                    let black = vec![0u8; (width * height * 3) as usize];
                    match enc.encode(&black) {
                        Ok(_) => {
                            enc.force_keyframe();
                            return Ok(enc);
                        }
                        Err(e) => errors.push(format!("{name}: {e}")),
                    }
                }
                Err(e) => errors.push(format!("{name}: {e}")),
            }
            unsafe {
                let _ = act.ShutdownObject();
            }
        }
        anyhow::bail!("hardware encoders failed: {}", errors.join("; "))
    }

    fn open(name: &str, act: &IMFActivate, width: u32, height: u32, fps: u32, bitrate: u32) -> anyhow::Result<Self> {
        unsafe {
            let transform: IMFTransform = act.ActivateObject()?;
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
            let events: IMFMediaEventGenerator = transform.cast()?;
            let codec: Option<ICodecAPI> = transform.cast().ok();

            if let Some(c) = &codec {
                // Low-latency mode: no frame reordering/lookahead, one-in-one-out.
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = c.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
                let _ = c.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                // Long GOP; viewers that need a keyframe ask for one.
                let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 8));
                // Favour quality: 0 = fastest, 100 = best. Hardware is fast either way.
                let _ = c.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(70u32));
            }

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Constrained Baseline: what WebRTC browsers negotiate everywhere.
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            transform.SetOutputType(0, &out_type, 0)?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &in_type, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            Ok(Self {
                transform,
                events,
                codec,
                width,
                height,
                fps,
                bitrate,
                need_input: 0,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                out_size: info.cbSize.max(1 << 20),
                nv12: Vec::new(),
                frame_index: 0,
                name: name.to_string(),
            })
        }
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        // Ignore tiny changes; re-programming the rate controller isn't free.
        if (bps as i64 - self.bitrate as i64).unsigned_abs() < self.bitrate as u64 / 20 {
            return;
        }
        self.bitrate = bps;
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps));
            }
        }
    }

    pub fn force_keyframe(&mut self) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
        }
    }

    /// Pumps one event. Returns false if none was available (non-blocking).
    fn pump(&mut self, blocking: bool, out: &mut Vec<(Vec<u8>, bool)>) -> anyhow::Result<bool> {
        let flags = if blocking { MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0) } else { MF_EVENT_FLAG_NO_WAIT };
        let ev = match unsafe { self.events.GetEvent(flags) } {
            Ok(ev) => ev,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        match MF_EVENT_TYPE(unsafe { ev.GetType()? } as i32) {
            METransformNeedInput => self.need_input += 1,
            METransformHaveOutput => self.drain(out)?,
            _ => {}
        }
        Ok(true)
    }

    fn drain(&mut self, out: &mut Vec<(Vec<u8>, bool)>) -> anyhow::Result<()> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.transform.ProcessOutput(0, &mut bufs, &mut status);
            let sample = ManuallyDrop::take(&mut bufs[0].pSample);
            let _ = ManuallyDrop::take(&mut bufs[0].pEvents);
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    return Ok(());
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(());
                }
                return Err(e.into());
            }
            let Some(sample) = sample else { return Ok(()) };
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            if !data.is_empty() {
                let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 || has_idr(&data);
                out.push((data, key));
            }
        }
        Ok(())
    }

    /// Encodes one packed-RGB frame. Usually returns that frame's bitstream
    /// right away (low-latency mode); may occasionally return 0 or 2 frames.
    pub fn encode(&mut self, rgb: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, bool)>> {
        let (w, h) = (self.width as usize, self.height as usize);
        rgb_to_nv12(rgb, w, h, &mut self.nv12);
        let mut out = Vec::new();

        // Wait for the encoder to ask for input.
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.need_input == 0 {
            if !self.pump(false, &mut out)? {
                if Instant::now() > deadline {
                    anyhow::bail!("encoder stalled (no input request)");
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }

        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let dur = 10_000_000i64 / self.fps.max(1) as i64;
            sample.SetSampleTime(self.frame_index * dur)?;
            sample.SetSampleDuration(dur)?;
            self.frame_index += 1;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.need_input -= 1;

        // Collect this frame's output (hardware takes a few ms). Spin briefly
        // instead of sleeping: Windows sleeps are ~1-15 ms granular.
        let start = Instant::now();
        while out.is_empty() && start.elapsed() < Duration::from_millis(60) {
            if !self.pump(false, &mut out)? {
                if start.elapsed() < Duration::from_millis(3) {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(250));
                }
            }
        }
        // Also take any NeedInput / extra output already queued.
        while self.pump(false, &mut out)? {}
        Ok(out)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

/// True if the Annex-B stream contains an IDR slice (NAL type 5).
fn has_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// BT.601 limited-range packed RGB → NV12 (Y plane + interleaved UV), parallel.
pub fn rgb_to_nv12(rgb: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h + w * (h / 2), 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    y_plane
        .par_chunks_mut(w * 2)
        .zip(uv_plane.par_chunks_mut(w))
        .enumerate()
        .for_each(|(cy, (yrows, uvrow))| {
            let r0 = &rgb[(cy * 2) * w * 3..(cy * 2 + 1) * w * 3];
            let r1 = &rgb[(cy * 2 + 1) * w * 3..(cy * 2 + 2) * w * 3];
            let (y0, y1) = yrows.split_at_mut(w);
            for x in 0..w {
                let p = &r0[x * 3..x * 3 + 3];
                y0[x] = luma(p[0], p[1], p[2]);
                let p = &r1[x * 3..x * 3 + 3];
                y1[x] = luma(p[0], p[1], p[2]);
            }
            for cx in 0..w / 2 {
                let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
                for row in [r0, r1] {
                    for dx in 0..2 {
                        let i = (cx * 2 + dx) * 3;
                        r += row[i] as i32;
                        g += row[i + 1] as i32;
                        b += row[i + 2] as i32;
                    }
                }
                let (r, g, b) = (r / 4, g / 4, b / 4);
                uvrow[cx * 2] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                uvrow[cx * 2 + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        });
}

#[inline(always)]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    (((66 * r as i32 + 129 * g as i32 + 25 * b as i32 + 128) >> 8) + 16) as u8
}

#[allow(dead_code)]
fn _guid_used(_: GUID) {}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test --release hw_encoder_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn hw_encoder_speed() {
        for (name, _) in enumerate() {
            println!("found: {name}");
        }
        let (w, h) = (1920u32, 1080u32);
        let mut enc = MfEncoder::new(w, h, 60, 12_000_000).expect("hardware encoder");
        println!("using: {}", enc.name);
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        let (mut total, mut convert) = (Duration::ZERO, Duration::ZERO);
        let (mut bytes, mut frames, mut keys) = (0usize, 0usize, 0usize);
        let n = 120u32;
        let mut seed = 1u32;
        for f in 0..n {
            for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let x = (i as u32 % w + f * 8) as u8;
                let y = (i as u32 / w + f * 4) as u8;
                px[0] = x;
                px[1] = y.wrapping_add((seed >> 28) as u8);
                px[2] = x ^ y;
            }
            let t = Instant::now();
            let mut tmp = Vec::new();
            rgb_to_nv12(&rgb, w as usize, h as usize, &mut tmp);
            convert += t.elapsed();
            let t = Instant::now();
            let out = enc.encode(&rgb).unwrap();
            total += t.elapsed();
            for (d, k) in out {
                bytes += d.len();
                frames += 1;
                keys += k as usize;
            }
        }
        println!(
            "1920x1080: {:?}/frame total (RGB→NV12 {:?}), {frames} frames out of {n}, {keys} keyframes, {:.1} Mbps at 60 fps",
            total / n,
            convert / n,
            bytes as f64 * 8.0 / (frames as f64 / 60.0) / 1e6
        );
    }
}
