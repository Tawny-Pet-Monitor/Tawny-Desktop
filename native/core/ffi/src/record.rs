//! The picture for a recording: while one runs, the latest decoded frame is
//! kept upright as packed I420, for the app's encoder to read at its own
//! pace (`NativeBridge.readFrame` on Android).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use libwebrtc::video_frame::{BoxVideoFrame, VideoBuffer};

use crate::video;

/// The latest frame, upright.
#[cfg_attr(
    not(target_os = "android"),
    expect(dead_code, reason = "read through NativeBridge on Android")
)]
pub(crate) struct Still {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Y, then U, then V, each plane packed (no row padding).
    pub(crate) data: Vec<u8>,
    /// Counts frames, so a reader can tell a new one from the last it took.
    pub(crate) seq: u64,
}

static ON: AtomicBool = AtomicBool::new(false);
static LATEST: Mutex<Option<Still>> = Mutex::new(None);

/// Starts (true) or stops keeping frames. Stopping drops the one kept.
#[cfg_attr(
    not(target_os = "android"),
    expect(dead_code, reason = "called through NativeBridge on Android")
)]
pub(crate) fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
    if !on {
        *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Called for every frame played: kept only while a recording runs.
pub(crate) fn offer(frame: &BoxVideoFrame) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    if frame.buffer.width() as usize * frame.buffer.height() as usize > video::MAX_PIXELS {
        return;
    }
    let i420 = frame.buffer.to_i420();
    let (width, height) = (i420.width() as usize, i420.height() as usize);
    let (luma_stride, u_stride, v_stride) = i420.strides();
    let (luma, u_plane, v_plane) = i420.data();
    let deg = video::degrees(frame.rotation);
    let mut guard = LATEST.lock().unwrap_or_else(PoisonError::into_inner);
    let seq = guard.as_ref().map_or(1, |s| s.seq + 1);
    let mut data = guard.take().map(|s| s.data).unwrap_or_default();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    data.resize(width * height + 2 * cw * ch, 0);
    let (out_luma, rest) = data.split_at_mut(width * height);
    let (out_u, out_v) = rest.split_at_mut(cw * ch);
    rotate_plane(luma, width, height, luma_stride as usize, deg, out_luma);
    rotate_plane(u_plane, cw, ch, u_stride as usize, deg, out_u);
    rotate_plane(v_plane, cw, ch, v_stride as usize, deg, out_v);
    let (width, height) = if deg % 180 == 0 {
        (width, height)
    } else {
        (height, width)
    };
    *guard = Some(Still {
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        data,
        seq,
    });
}

/// Copies the latest frame into `dst` if it is newer than `after` and fits.
/// Returns its width, height and number.
#[cfg_attr(
    not(target_os = "android"),
    expect(dead_code, reason = "called through NativeBridge on Android")
)]
pub(crate) fn take(dst: &mut [u8], after: u64) -> Option<(u32, u32, u64)> {
    let guard = LATEST.lock().unwrap_or_else(PoisonError::into_inner);
    let s = guard.as_ref()?;
    if s.seq <= after || dst.len() < s.data.len() {
        return None;
    }
    dst[..s.data.len()].copy_from_slice(&s.data);
    Some((s.width, s.height, s.seq))
}

/// Copies a `w`×`h` plane (`stride` bytes a row) into `dst`, packed, turned
/// `deg` degrees clockwise.
pub(crate) fn rotate_plane(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    deg: u32,
    dst: &mut [u8],
) {
    if deg == 0 {
        for (row, out) in dst.chunks_exact_mut(w).take(h).enumerate() {
            if let Some(line) = src.get(row * stride..row * stride + w) {
                out.copy_from_slice(line);
            }
        }
        return;
    }
    // Width of the turned plane, in pixels.
    let ow = if deg == 180 { w } else { h };
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match deg {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            if let (Some(p), Some(q)) = (src.get(y * stride + x), dst.get_mut(dy * ow + dx)) {
                *q = *p;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rotate_plane;

    // 3×2, padded to a stride of 4:
    //   a b c
    //   d e f
    const SRC: [u8; 8] = [b'a', b'b', b'c', 0, b'd', b'e', b'f', 0];

    fn turned(deg: u32) -> Vec<u8> {
        let mut out = vec![0; 6];
        rotate_plane(&SRC, 3, 2, 4, deg, &mut out);
        out
    }

    #[test]
    fn upright_drops_the_padding() {
        assert_eq!(turned(0), b"abcdef");
    }

    #[test]
    fn quarter_turns_go_clockwise() {
        // 90°: d a / e b / f c
        assert_eq!(turned(90), b"daebfc");
        // 270°: c f / b e / a d
        assert_eq!(turned(270), b"cfbead");
    }

    #[test]
    fn half_turn_reverses() {
        assert_eq!(turned(180), b"fedcba");
    }
}
