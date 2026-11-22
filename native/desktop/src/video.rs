//! A video track onto the screen: frames read on the engine's runtime,
//! turned upright into RGBA, and handed to the window one at a time. While
//! the window has not drawn the last frame yet, newer ones are dropped
//! rather than queued, so a slow window never falls behind the picture.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt as _;
use libwebrtc::video_frame::{VideoFormatType, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use libwebrtc::video_track::RtcVideoTrack;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::runtime::Runtime;

/// The largest frame drawn (as on Android): anything bigger is refused.
pub const MAX_PIXELS: usize = 4096 * 4096;

/// Where frames go, on the window's thread: the picture, or `None` when the
/// track ended.
pub trait Sink: Fn(Option<Image>) + Send + Sync + 'static {}
impl<T: Fn(Option<Image>) + Send + Sync + 'static> Sink for T {}

/// Shows `track` through `sink` until it ends or `stop` is set.
/// With `tap`, the frames also go to a clip being recorded.
pub fn pump(rt: &Runtime, track: RtcVideoTrack, stop: Arc<AtomicBool>, sink: impl Sink, tap: bool) {
    let sink = Arc::new(sink);
    let busy = Arc::new(AtomicBool::new(false));
    rt.spawn(async move {
        let mut stream = NativeVideoStream::new(track);
        let mut rgba = Vec::new();
        while let Some(frame) = stream.next().await {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // The window is still drawing the last one: skip this one.
            if busy.swap(true, Ordering::AcqRel) {
                continue;
            }
            let (w, h) = (
                frame.buffer.width() as usize,
                frame.buffer.height() as usize,
            );
            if w == 0 || h == 0 || w * h > MAX_PIXELS {
                busy.store(false, Ordering::Release);
                continue;
            }
            let (Ok(wi), Ok(hi), Ok(stride)) =
                (i32::try_from(w), i32::try_from(h), u32::try_from(w * 4))
            else {
                busy.store(false, Ordering::Release);
                continue;
            };
            rgba.resize(w * h * 4, 0);
            // libyuv's ABGR is R, G, B, A in memory.
            frame
                .buffer
                .to_argb(VideoFormatType::ABGR, &mut rgba, stride, wi, hi);
            let (pixels, ow, oh) = upright(&rgba, w, h, frame.rotation);
            if tap {
                crate::record::video(&pixels, ow, oh);
            }
            let (Ok(ow), Ok(oh)) = (u32::try_from(ow), u32::try_from(oh)) else {
                busy.store(false, Ordering::Release);
                continue;
            };
            let done = busy.clone();
            let to = sink.clone();
            let sent = slint::invoke_from_event_loop(move || {
                let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&pixels, ow, oh);
                (*to)(Some(Image::from_rgba8(buf)));
                done.store(false, Ordering::Release);
            });
            if sent.is_err() {
                break;
            }
        }
        let _ = slint::invoke_from_event_loop(move || (*sink)(None));
    });
}

/// The picture's mean brightness, 0..=255 (a dark frame vs a real one, for
/// the developer's end-to-end runs).
#[must_use]
pub fn mean_luma(rgba: &[u8]) -> u32 {
    let (px, _) = rgba.as_chunks::<4>();
    let n = px.len().max(1);
    let sum: u64 = px
        .iter()
        .map(|p| (u64::from(p[0]) * 3 + u64::from(p[1]) * 6 + u64::from(p[2])) / 10)
        .sum();
    u32::try_from(sum / n as u64).unwrap_or(255)
}

/// The frame turned the way it should be seen: (pixels, width, height).
fn upright(src: &[u8], w: usize, h: usize, rot: VideoRotation) -> (Vec<u8>, usize, usize) {
    let deg = match rot {
        VideoRotation::VideoRotation90 => 90,
        VideoRotation::VideoRotation180 => 180,
        VideoRotation::VideoRotation270 => 270,
        VideoRotation::VideoRotation0 => 0,
    };
    if deg == 0 {
        return (src.to_vec(), w, h);
    }
    let (ow, oh) = if deg == 180 { (w, h) } else { (h, w) };
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        for x in 0..w {
            // Where pixel (x, y) lands once turned `deg` clockwise.
            let (nx, ny) = match deg {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let s = (y * w + x) * 4;
            let d = (ny * ow + nx) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (out, ow, oh)
}

#[cfg(test)]
mod tests {
    use super::upright;
    use libwebrtc::video_frame::VideoRotation;

    /// A 2×1 picture: red, then green.
    const RG: [u8; 8] = [255, 0, 0, 255, 0, 255, 0, 255];

    #[test]
    fn a_quarter_turn_stands_the_picture_up() {
        let (out, w, h) = upright(&RG, 2, 1, VideoRotation::VideoRotation90);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[..4], &RG[..4], "red on top after a clockwise turn");
        assert_eq!(&out[4..], &RG[4..]);
    }

    #[test]
    fn a_half_turn_swaps_ends_and_none_keeps_it() {
        let (out, w, h) = upright(&RG, 2, 1, VideoRotation::VideoRotation180);
        assert_eq!((w, h), (2, 1));
        assert_eq!(&out[..4], &RG[4..]);
        let (same, ..) = upright(&RG, 2, 1, VideoRotation::VideoRotation0);
        assert_eq!(same, RG);
    }

    #[test]
    fn three_quarters_turns_the_other_way() {
        let (out, w, h) = upright(&RG, 2, 1, VideoRotation::VideoRotation270);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[..4], &RG[4..], "green on top after a counter-turn");
    }
}
