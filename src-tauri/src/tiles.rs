//! Tile-based frame store.
//!
//! The screen is split into TILE×TILE blocks. Each captured frame is compared
//! with the previous one and only changed blocks are JPEG-encoded (merged into
//! horizontal runs, encoded in parallel). The store keeps just enough patches to
//! rebuild the current screen, so:
//!   * a new viewer gets every live patch (= the full screen),
//!   * an existing viewer gets only patches newer than what it already has,
//!   * a slow viewer skips intermediate states instead of queueing them.

use std::collections::VecDeque;
use std::time::Instant;

use bytes::{BufMut, Bytes, BytesMut};
use jpeg_encoder::{ColorType, Encoder};
use rayon::prelude::*;

pub const TILE: u32 = 128;

/// One encoded rectangle of the screen.
struct Patch {
    id: u64,
    version: u64,
    x: u16,
    y: u16,
    jpeg: Bytes,
    /// Tiles this patch is still the newest source for. 0 = fully overwritten.
    owned: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

pub struct Store {
    /// Bumped whenever the output size changes; viewers then do a full reset.
    pub epoch: u32,
    pub width: u32,
    pub height: u32,
    cols: u32,
    rows: u32,
    /// Patch id owning each tile (0 = none yet).
    owner: Vec<u64>,
    patches: VecDeque<Patch>,
    next_patch_id: u64,
    /// Monotonic change counter (frames and cursor moves).
    pub version: u64,
    pub cursor: Cursor,
    /// When the newest content was captured — used for latency measurement.
    pub updated_at: Instant,
}

impl Store {
    pub fn new() -> Self {
        Self {
            epoch: 0,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            owner: Vec::new(),
            patches: VecDeque::new(),
            next_patch_id: 1,
            version: 0,
            cursor: Cursor::default(),
            updated_at: Instant::now(),
        }
    }

    pub fn reset(&mut self, width: u32, height: u32) {
        self.epoch = self.epoch.wrapping_add(1);
        self.width = width;
        self.height = height;
        self.cols = width.div_ceil(TILE);
        self.rows = height.div_ceil(TILE);
        self.owner = vec![0; (self.cols * self.rows) as usize];
        self.patches.clear();
        self.version += 1;
    }

    fn apply(&mut self, encoded: Vec<EncodedRun>, captured_at: Instant) {
        if encoded.is_empty() {
            return;
        }
        self.version += 1;
        for run in encoded {
            let id = self.next_patch_id;
            self.next_patch_id += 1;
            let mut owned = 0;
            for col in run.col0..run.col0 + run.cols {
                let idx = (run.row * self.cols + col) as usize;
                let prev = std::mem::replace(&mut self.owner[idx], id);
                if prev != 0 {
                    if let Some(p) = self.patch_mut(prev) {
                        p.owned -= 1;
                    }
                }
                owned += 1;
            }
            self.patches.push_back(Patch {
                id,
                version: self.version,
                x: (run.col0 * TILE) as u16,
                y: (run.row * TILE) as u16,
                jpeg: run.jpeg,
                owned,
            });
        }
        self.patches.retain(|p| p.owned > 0);
        self.updated_at = captured_at;
    }

    fn patch_mut(&mut self, id: u64) -> Option<&mut Patch> {
        // Patches are sorted by id.
        let idx = self.patches.binary_search_by_key(&id, |p| p.id).ok()?;
        self.patches.get_mut(idx)
    }

    pub fn set_cursor(&mut self, c: Cursor) {
        if c != self.cursor {
            self.cursor = c;
            self.version += 1;
            self.updated_at = Instant::now();
        }
    }

    /// Builds the binary update message for a viewer that has seen `since`
    /// (version) of `epoch`.
    ///
    /// Layout (little endian):
    /// u8 kind=1 | u32 seq | u16 w | u16 h | u8 flags (1=reset, 2=cursor visible)
    /// | i16 cx | i16 cy | u16 latency_ms | u16 n | n × (u16 x | u16 y | u32 len | jpeg)
    pub fn build_message(&self, seq: u32, since: u64, epoch: u32, latency_ms: u16) -> Bytes {
        let reset = epoch != self.epoch;
        let since = if reset { 0 } else { since };
        let patches: Vec<&Patch> = self.patches.iter().filter(|p| p.version > since).collect();
        let size: usize = 18 + patches.iter().map(|p| 8 + p.jpeg.len()).sum::<usize>();

        let mut b = BytesMut::with_capacity(size);
        b.put_u8(1);
        b.put_u32_le(seq);
        b.put_u16_le(self.width as u16);
        b.put_u16_le(self.height as u16);
        b.put_u8((reset as u8) | ((self.cursor.visible as u8) << 1));
        b.put_i16_le(self.cursor.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        b.put_i16_le(self.cursor.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        b.put_u16_le(latency_ms);
        b.put_u16_le(patches.len() as u16);
        for p in patches {
            b.put_u16_le(p.x);
            b.put_u16_le(p.y);
            b.put_u32_le(p.jpeg.len() as u32);
            b.put_slice(&p.jpeg);
        }
        b.freeze()
    }
}

struct EncodedRun {
    row: u32,
    col0: u32,
    cols: u32,
    jpeg: Bytes,
}

/// Compares `cur` with `prev` (both RGB, same size), encodes the changed tiles
/// and applies them to the store. Returns true if anything changed.
pub fn encode_changes(
    store: &std::sync::Mutex<Store>,
    cur: &[u8],
    prev: Option<&[u8]>,
    width: u32,
    height: u32,
    quality: u8,
    captured_at: Instant,
) -> anyhow::Result<bool> {
    let cols = width.div_ceil(TILE);
    let rows = height.div_ceil(TILE);
    let stride = width as usize * 3;

    // 1. Which tiles changed? (parallel per tile row)
    let changed: Vec<Vec<bool>> = (0..rows)
        .into_par_iter()
        .map(|row| {
            let y0 = row * TILE;
            let y1 = (y0 + TILE).min(height);
            (0..cols)
                .map(|col| {
                    let Some(prev) = prev else { return true };
                    let x0 = (col * TILE) as usize * 3;
                    let x1 = (((col + 1) * TILE).min(width)) as usize * 3;
                    (y0..y1).any(|y| {
                        let o = y as usize * stride;
                        cur[o + x0..o + x1] != prev[o + x0..o + x1]
                    })
                })
                .collect()
        })
        .collect();

    // 2. Merge consecutive changed tiles in a row into runs.
    let mut runs = Vec::new();
    for (row, flags) in changed.iter().enumerate() {
        let mut col = 0;
        while col < cols as usize {
            if flags[col] {
                let start = col;
                while col < cols as usize && flags[col] {
                    col += 1;
                }
                runs.push((row as u32, start as u32, (col - start) as u32));
            } else {
                col += 1;
            }
        }
    }
    if runs.is_empty() {
        return Ok(false);
    }

    // 3. Encode runs in parallel.
    let encoded: Vec<EncodedRun> = runs
        .into_par_iter()
        .map(|(row, col0, ncols)| -> anyhow::Result<EncodedRun> {
            let x0 = col0 * TILE;
            let x1 = ((col0 + ncols) * TILE).min(width);
            let y0 = row * TILE;
            let y1 = (y0 + TILE).min(height);
            let (w, h) = (x1 - x0, y1 - y0);
            let mut pixels = Vec::with_capacity((w * h * 3) as usize);
            for y in y0..y1 {
                let o = y as usize * stride + x0 as usize * 3;
                pixels.extend_from_slice(&cur[o..o + w as usize * 3]);
            }
            let mut jpeg = Vec::with_capacity(pixels.len() / 8);
            Encoder::new(&mut jpeg, quality).encode(&pixels, w as u16, h as u16, ColorType::Rgb)?;
            Ok(EncodedRun { row, col0, cols: ncols, jpeg: Bytes::from(jpeg) })
        })
        .collect::<anyhow::Result<_>>()?;

    store.lock().unwrap().apply(encoded, captured_at);
    Ok(true)
}
