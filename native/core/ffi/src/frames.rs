//! Camera frames from the platform into the Monitor's video source.
//!
//! Android's `YUV_420_888` gives three planes with a row stride
//! each, and chroma with a pixel stride of 1 (planar) or 2 (interleaved, as
//! most phones do). [`fill_i420`] copies any of those layouts into an
//! [`I420Buffer`], checking every plane is long enough first.

use std::sync::Mutex;

use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_source::native::NativeVideoSource;

/// The running Monitor's video source. One Monitor runs per process.
pub(crate) static SOURCE: Mutex<Option<NativeVideoSource>> = Mutex::new(None);

/// A camera image as the platform hands it over.
pub(crate) struct Planes<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub y_row: usize,
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub uv_row: usize,
    pub uv_pixel: usize,
}

/// Bytes a plane must hold: the last pixel of the last row, plus one.
fn needed(rows: usize, cols: usize, row: usize, pixel: usize) -> Option<usize> {
    if rows == 0 || cols == 0 {
        return Some(0);
    }
    (rows - 1)
        .checked_mul(row)?
        .checked_add((cols - 1).checked_mul(pixel)?)?
        .checked_add(1)
}

/// Why a frame was dropped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// Zero or odd-sized, or a stride smaller than a row.
    Shape,
    /// A plane is shorter than its strides say.
    Short,
}

/// Copies `p` into a new I420 buffer.
pub(crate) fn fill_i420(p: &Planes<'_>) -> Result<I420Buffer, FrameError> {
    let (w, h) = (p.width as usize, p.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if w == 0 || h == 0 || p.y_row < w || p.uv_pixel == 0 || p.uv_row < (cw - 1) * p.uv_pixel + 1 {
        return Err(FrameError::Shape);
    }
    let short = |have: usize, want: Option<usize>| want.is_none_or(|n| have < n);
    if short(p.y.len(), needed(h, w, p.y_row, 1))
        || short(p.u.len(), needed(ch, cw, p.uv_row, p.uv_pixel))
        || short(p.v.len(), needed(ch, cw, p.uv_row, p.uv_pixel))
    {
        return Err(FrameError::Short);
    }
    let mut buf = I420Buffer::new(p.width, p.height);
    let (sy, su, sv) = buf.strides();
    let (sy, su, sv) = (sy as usize, su as usize, sv as usize);
    let (dy, du, dv) = buf.data_mut();
    for r in 0..h {
        dy[r * sy..r * sy + w].copy_from_slice(&p.y[r * p.y_row..r * p.y_row + w]);
    }
    for r in 0..ch {
        let src = r * p.uv_row;
        if p.uv_pixel == 1 {
            du[r * su..r * su + cw].copy_from_slice(&p.u[src..src + cw]);
            dv[r * sv..r * sv + cw].copy_from_slice(&p.v[src..src + cw]);
        } else {
            for c in 0..cw {
                du[r * su + c] = p.u[src + c * p.uv_pixel];
                dv[r * sv + c] = p.v[src + c * p.uv_pixel];
            }
        }
    }
    Ok(buf)
}

/// Degrees clockwise the image must turn to be upright.
pub(crate) fn rotation(degrees: i32) -> VideoRotation {
    match degrees.rem_euclid(360) {
        90 => VideoRotation::VideoRotation90,
        180 => VideoRotation::VideoRotation180,
        270 => VideoRotation::VideoRotation270,
        _ => VideoRotation::VideoRotation0,
    }
}

/// Hands a frame to the running Monitor. `false` when none is running or
/// the frame was malformed.
pub(crate) fn push(p: &Planes<'_>, degrees: i32) -> bool {
    let Ok(buf) = fill_i420(p) else { return false };
    let Ok(src) = SOURCE.lock() else { return false };
    let Some(src) = src.as_ref() else {
        return false;
    };
    src.capture_frame(&VideoFrame::new(rotation(degrees), buf));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planes<'a>(y: &'a [u8], u: &'a [u8], v: &'a [u8], uv_row: usize, px: usize) -> Planes<'a> {
        Planes {
            width: 4,
            height: 2,
            y,
            y_row: 4,
            u,
            v,
            uv_row,
            uv_pixel: px,
        }
    }

    #[test]
    fn interleaved_chroma_is_split_into_planes() {
        let y: Vec<u8> = (0..8).collect();
        // NV12-style: U and V share a buffer offset by one; the last V byte is missing,
        // exactly as CameraX hands it over.
        let uv = [10u8, 20, 11, 21];
        let buf = fill_i420(&planes(&y, &uv[..3], &uv[1..], 4, 2)).expect("fills");
        let (_, su, sv) = buf.strides();
        let (dy, du, dv) = buf.data();
        assert_eq!(&dy[..4], &[0, 1, 2, 3]);
        assert_eq!(&du[..2], &[10, 11]);
        assert_eq!(&dv[..2], &[20, 21]);
        assert!(su >= 2 && sv >= 2);
    }

    #[test]
    fn planar_chroma_is_copied() {
        let y = [7u8; 8];
        let buf = fill_i420(&planes(&y, &[1, 2], &[3, 4], 2, 1)).expect("fills");
        let (_, du, dv) = buf.data();
        assert_eq!((&du[..2], &dv[..2]), (&[1u8, 2][..], &[3u8, 4][..]));
    }

    #[test]
    fn short_or_misshapen_planes_are_refused() {
        let y = [0u8; 8];
        assert_eq!(
            fill_i420(&planes(&y[..7], &[1, 2], &[3, 4], 2, 1)).err(),
            Some(FrameError::Short)
        );
        assert_eq!(
            fill_i420(&planes(&y, &[1], &[3, 4], 2, 1)).err(),
            Some(FrameError::Short)
        );
        assert_eq!(
            fill_i420(&planes(&y, &[1, 2], &[3, 4], 1, 1)).err(),
            Some(FrameError::Shape)
        );
        assert_eq!(
            fill_i420(&planes(&y, &[1, 2], &[3, 4], 2, 0)).err(),
            Some(FrameError::Shape)
        );
        let mut p = planes(&y, &[1, 2], &[3, 4], 2, 1);
        p.y_row = usize::MAX;
        assert_eq!(
            fill_i420(&p).err(),
            Some(FrameError::Short),
            "overflow is a refusal"
        );
    }

    #[test]
    fn rotations() {
        assert_eq!(rotation(90), VideoRotation::VideoRotation90);
        assert_eq!(rotation(-90), VideoRotation::VideoRotation270);
        assert_eq!(rotation(45), VideoRotation::VideoRotation0);
    }
}
