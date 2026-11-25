//! The Monitor's camera on a Viewer's screen.
//!
//! A task reads the incoming track and hands each frame to the [`Screen`]
//! the app has attached (on Android, a `Surface`; see `android.rs`). With no
//! screen attached (the app is in the background) frames are dropped after
//! decoding, which costs little and keeps the call warm.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use libwebrtc::video_frame::{BoxVideoFrame, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use libwebrtc::video_track::RtcVideoTrack;
use tokio::runtime::Runtime;

/// Somewhere to draw frames.
pub(crate) trait Screen: Send {
    /// Draws one frame; `Err` says why it wasn't.
    ///
    /// # Errors
    /// A short reason, for the diagnostics log.
    fn show(&mut self, frame: &BoxVideoFrame) -> Result<(), &'static str>;
}

/// The average brightness (0-255) of the last frame a screen drew, sampled:
/// tells a black picture from a picture that never reached the screen.
pub(crate) static LAST_LEVEL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The average of every 101st byte of a frame's Y plane (16 is black in
/// video range): what the decoder handed over, before any conversion.
fn luma(frame: &BoxVideoFrame) -> u32 {
    let i420 = frame.buffer.to_i420();
    let (y, _, _) = i420.data();
    let (sum, n) = y
        .iter()
        .step_by(101)
        .fold((0u64, 0u64), |(s, n), b| (s + u64::from(*b), n + 1));
    u32::try_from(sum.checked_div(n).unwrap_or(0)).unwrap_or(0)
}

/// The largest frame drawn or recorded. The size is the far end's to choose,
/// and drawing turned or recording copies it, so without a cap a paired phone
/// could make this one allocate gigabytes. 4096² covers every camera we meet.
pub(crate) const MAX_PIXELS: usize = 4096 * 4096;

/// The attached screen, if any.
pub(crate) static SCREEN: Mutex<Option<Box<dyn Screen>>> = Mutex::new(None);

/// Degrees clockwise the frame must turn to be upright.
pub(crate) const fn degrees(r: VideoRotation) -> u32 {
    match r {
        VideoRotation::VideoRotation0 => 0,
        VideoRotation::VideoRotation90 => 90,
        VideoRotation::VideoRotation180 => 180,
        VideoRotation::VideoRotation270 => 270,
    }
}

/// On a Monitor: the Viewer whose camera is on the screen ("My camera"). One
/// at a time: the first to send frames has it until it stops.
static FEATURED: Mutex<Option<String>> = Mutex::new(None);

/// How long a Viewer's camera may send nothing before it counts as off.
const QUIET: std::time::Duration = std::time::Duration::from_millis(1500);

/// On a Monitor: shows Viewer `id`'s camera when it sends frames, telling
/// `on_size` its upright size when it comes on screen (and changes), and
/// `0, 0` when it goes off. Ends with the track.
pub(crate) fn feature(
    rt: &Runtime,
    id: String,
    track: RtcVideoTrack,
    on_size: Arc<dyn Fn(u32, u32) + Send + Sync>,
) {
    rt.spawn(async move {
        let mut stream = NativeVideoStream::new(track);
        let mut last = (0, 0);
        let featured = |id: &str| FEATURED.lock().is_ok_and(|f| f.as_deref() == Some(id));
        let release = |id: &str| {
            if let Ok(mut f) = FEATURED.lock() {
                if f.as_deref() == Some(id) {
                    *f = None;
                    return true;
                }
            }
            false
        };
        loop {
            match tokio::time::timeout(QUIET, stream.next()).await {
                Ok(Some(frame)) => {
                    let claimed = FEATURED.lock().is_ok_and(|mut f| {
                        if f.is_none() {
                            *f = Some(id.clone());
                        }
                        f.as_deref() == Some(id.as_str())
                    });
                    if !claimed {
                        continue;
                    }
                    let (w, h) = (frame.buffer.width(), frame.buffer.height());
                    let upright = if degrees(frame.rotation) % 180 == 0 {
                        (w, h)
                    } else {
                        (h, w)
                    };
                    if upright != last {
                        last = upright;
                        on_size(upright.0, upright.1);
                    }
                    if let Ok(mut s) = SCREEN.lock() {
                        if let Some(screen) = s.as_mut() {
                            let _ = screen.show(&frame);
                        }
                    }
                }
                Ok(None) => {
                    if release(&id) {
                        on_size(0, 0);
                    }
                    break;
                }
                Err(_) => {
                    if featured(&id) && release(&id) {
                        last = (0, 0);
                        on_size(0, 0);
                    }
                }
            }
        }
    });
}

/// Plays `track` on the attached screen, telling `on_size` the upright size
/// whenever it changes, and `on_log` how it is going (first frame, then
/// every 30 s). Ends with the track.
pub(crate) fn play(
    rt: &Runtime,
    track: RtcVideoTrack,
    on_size: Arc<dyn Fn(u32, u32) + Send + Sync>,
    on_log: Arc<dyn Fn(String) + Send + Sync>,
) {
    rt.spawn(async move {
        let mut stream = NativeVideoStream::new(track);
        let mut last = (0, 0);
        let (mut got, mut drawn, mut why) = (0u64, 0u64, "");
        let mut said = std::time::Instant::now();
        while let Some(frame) = stream.next().await {
            got += 1;
            crate::record::offer(&frame);
            let (w, h) = (frame.buffer.width(), frame.buffer.height());
            let upright = if degrees(frame.rotation) % 180 == 0 {
                (w, h)
            } else {
                (h, w)
            };
            if upright != last {
                last = upright;
                on_size(upright.0, upright.1);
            }
            match SCREEN.lock() {
                Ok(mut s) => match s.as_mut() {
                    Some(screen) => match screen.show(&frame) {
                        Ok(()) => drawn += 1,
                        Err(e) => why = e,
                    },
                    None => why = "no screen attached",
                },
                Err(_) => why = "screen lock poisoned",
            }
            if got == 1 || said.elapsed() >= std::time::Duration::from_secs(30) {
                said = std::time::Instant::now();
                let miss = if why.is_empty() {
                    String::new()
                } else {
                    format!(" (last miss: {why})")
                };
                on_log(format!(
                    "video {w}x{h} rot {} {:?}: {got} frames, {drawn} drawn, \
                     luma {}, level {}{miss}",
                    degrees(frame.rotation),
                    frame.buffer.buffer_type(),
                    luma(&frame),
                    LAST_LEVEL.load(std::sync::atomic::Ordering::Relaxed)
                ));
            }
        }
    });
}
