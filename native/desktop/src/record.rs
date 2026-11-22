//! Record: a short clip of the Monitor's picture and sound (web/app.js's
//! #btn-video), capped at 20 s, saved as `WebM` (VP8 + Opus) in the user's
//! Videos folder. The far picture and sound are tapped as they arrive and
//! handed to a worker thread through a bounded channel: a slow encoder drops
//! frames, it never holds the call up.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use libwebrtc::native::yuv_helper;

use crate::webm::{Clip, Frame, Track};

/// The longest clip.
pub const MAX: Duration = Duration::from_secs(20);
/// Frames closer together than this are skipped (about 30 a second).
const FRAME_GAP: Duration = Duration::from_millis(30);
/// Video bitrate, kbit/s.
const KBPS: u32 = 1500;
/// The largest encoded video frame accepted.
const MAX_PACKET: usize = 4 << 20;
/// 20 ms of 48 kHz mono: one Opus packet.
const OPUS_FRAME: usize = 960;
/// The largest picture recorded (the call's own cap).
const MAX_PIXELS: usize = crate::video::MAX_PIXELS;

enum Input {
    Video {
        rgba: Vec<u8>,
        w: usize,
        h: usize,
        at: Instant,
    },
    Audio(Vec<i16>),
}

/// Where the taps send, while a clip is being made, and when the last
/// picture went.
struct Tap {
    tx: SyncSender<Input>,
    last_frame: Option<Instant>,
}

static TAP: Mutex<Option<Tap>> = Mutex::new(None);

/// A clip in the making; [`Recording::finish`] saves it.
pub struct Recording {
    worker: JoinHandle<Result<PathBuf>>,
}

/// Starts a clip of the far picture and sound.
///
/// # Errors
/// When one is already being made or the worker won't start.
pub fn start() -> Result<Recording> {
    let mut tap = TAP.lock().unwrap_or_else(PoisonError::into_inner);
    if tap.is_some() {
        bail!("already recording");
    }
    // About two seconds of frames and sound in flight at most.
    let (tx, rx) = sync_channel(96);
    let worker = std::thread::Builder::new()
        .name("tawny-record".into())
        .spawn(move || work(&rx))
        .context("starting the recorder")?;
    *tap = Some(Tap {
        tx,
        last_frame: None,
    });
    Ok(Recording { worker })
}

impl Recording {
    /// Ends the clip and saves it: its file, once written.
    ///
    /// # Errors
    /// When nothing was captured or the file can't be written.
    pub fn finish(self) -> Result<PathBuf> {
        // Dropping the sender is the worker's signal to wrap up.
        TAP.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.worker
            .join()
            .map_err(|_| anyhow::anyhow!("the recorder stopped"))?
    }
}

/// A far picture, upright RGBA (as drawn): kept if a clip is being made.
pub fn video(rgba: &[u8], w: usize, h: usize) {
    let mut tap = TAP.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(t) = tap.as_mut() else { return };
    let now = Instant::now();
    if t.last_frame.is_some_and(|l| now - l < FRAME_GAP) {
        return;
    }
    if w * h == 0 || w * h > MAX_PIXELS || rgba.len() != w * h * 4 {
        return;
    }
    let sent = t.tx.try_send(Input::Video {
        rgba: rgba.to_vec(),
        w,
        h,
        at: now,
    });
    if !matches!(sent, Err(TrySendError::Full(_))) {
        t.last_frame = Some(now);
    }
}

/// Far sound, 48 kHz mono: kept if a clip is being made.
pub fn audio(samples: &[i16]) {
    let tap = TAP.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(t) = tap.as_ref() {
        let _ = t.tx.try_send(Input::Audio(samples.to_vec()));
    }
}

/// The worker: encodes what comes until the taps close or the time is up,
/// then writes the file.
fn work(rx: &Receiver<Input>) -> Result<PathBuf> {
    let started = Instant::now();
    let mut video: Option<(codec::Vp8, usize, usize)> = None;
    let mut opus = codec::Opus::new().context("starting the sound encoder")?;
    let mut frames: Vec<Frame> = Vec::new();
    let mut pcm: Vec<i16> = Vec::with_capacity(OPUS_FRAME * 4);
    let mut sound_ms = 0u64;
    let mut last_ms: Option<u64> = None;
    let mut i420 = Vec::new();
    let mut packet = vec![0u8; MAX_PACKET];
    // Sound counts from its first sample, so a late start doesn't drift.
    let mut sound_from: Option<u64> = None;
    loop {
        let left = MAX.saturating_sub(started.elapsed());
        let input = match rx.recv_timeout(left.max(Duration::from_millis(1))) {
            Ok(i) => i,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < MAX => continue,
            Err(_) => break,
        };
        match input {
            Input::Video { rgba, w, h, at } => {
                let ms = u64::try_from(at.saturating_duration_since(started).as_millis())
                    .unwrap_or(u64::MAX);
                // Even sides (I420 halves them), cropped at the far edge.
                let (ew, eh) = (w & !1, h & !1);
                if video.is_none() {
                    let enc =
                        codec::Vp8::new(ew, eh, KBPS).context("starting the video encoder")?;
                    video = Some((enc, ew, eh));
                }
                let Some((enc, vw, vh)) = video.as_mut() else {
                    continue;
                };
                // The picture changed size mid-clip: kept to the first one.
                if (ew, eh) != (*vw, *vh) {
                    continue;
                }
                to_i420(&rgba, w, ew, eh, &mut i420);
                let dur = last_ms.map_or(33, |l| ms.saturating_sub(l).clamp(1, 1000));
                let first = last_ms.is_none();
                last_ms = Some(ms);
                if enc.encode(Some(&i420), ms, dur, first).is_ok() {
                    drain(enc, &mut packet, &mut frames);
                }
            }
            Input::Audio(s) => {
                if sound_from.is_none() {
                    sound_from = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(0));
                    sound_ms = sound_from.unwrap_or(0);
                }
                pcm.extend_from_slice(&s);
                let mut at = 0;
                while pcm.len() - at >= OPUS_FRAME {
                    if let Ok(p) = opus.encode(&pcm[at..at + OPUS_FRAME]) {
                        frames.push(Frame {
                            track: Track::Audio,
                            ms: sound_ms,
                            key: true,
                            data: p,
                        });
                    }
                    sound_ms += 20;
                    at += OPUS_FRAME;
                }
                pcm.drain(..at);
            }
        }
    }
    let Some((mut enc, w, h)) = video else {
        bail!("nothing to capture");
    };
    if enc.encode(None, last_ms.unwrap_or(0), 1, false).is_ok() {
        drain(&mut enc, &mut packet, &mut frames);
    }
    if !frames.iter().any(|f| f.track == Track::Video) {
        bail!("nothing to capture");
    }
    let end = frames.iter().map(|f| f.ms).max().unwrap_or(0);
    let clip = Clip {
        width: u32::try_from(w).unwrap_or(0),
        height: u32::try_from(h).unwrap_or(0),
        pre_skip: opus.lookahead(),
        duration_ms: end + 33,
    };
    let file = crate::webm::write(&clip, &mut frames);
    let dir = crate::snapshot::user_folder("XDG_VIDEOS_DIR", "Videos")?.join("Tawny");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!(
        "tawny-{}.webm",
        crate::snapshot::stamp(crate::snapshot::now_secs())
    ));
    std::fs::write(&path, file).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Every frame the encoder has ready, onto the clip.
fn drain(enc: &mut codec::Vp8, buf: &mut [u8], frames: &mut Vec<Frame>) {
    while let Some((len, ms, key)) = enc.take_frame(buf) {
        frames.push(Frame {
            track: Track::Video,
            ms,
            key,
            data: buf[..len].to_vec(),
        });
    }
}

/// RGBA (`stride_px` wide) to I420 of `width`×`height` (both even), planes
/// back to back.
fn to_i420(rgba: &[u8], stride_px: usize, width: usize, height: usize, out: &mut Vec<u8>) {
    let (half_w, half_h) = (width / 2, height / 2);
    out.resize(width * height + 2 * half_w * half_h, 0);
    let (luma, chroma) = out.split_at_mut(width * height);
    let (cb, cr) = chroma.split_at_mut(half_w * half_h);
    let (Ok(src_stride), Ok(luma_stride), Ok(chroma_stride), Ok(wi), Ok(hi)) = (
        u32::try_from(stride_px * 4),
        u32::try_from(width),
        u32::try_from(half_w),
        i32::try_from(width),
        i32::try_from(height),
    ) else {
        return;
    };
    // libyuv's ABGR is R, G, B, A in memory, as the picture is.
    yuv_helper::abgr_to_i420(
        rgba,
        src_stride,
        luma,
        luma_stride,
        cb,
        chroma_stride,
        cr,
        chroma_stride,
        wi,
        hi,
    );
}

/// The encoders in libwebrtc, through csrc/rec.c.
mod codec {
    #![expect(
        unsafe_code,
        reason = "the encoders are C; every call passes only sizes and buffers rec.c checks"
    )]

    use std::ffi::c_void;
    use std::ptr::NonNull;

    use anyhow::{Result, bail};

    unsafe extern "C" {
        fn tawny_vp8_new(w: u32, h: u32, kbps: u32) -> *mut c_void;
        fn tawny_vp8_encode(
            e: *mut c_void,
            i420: *const u8,
            len: usize,
            pts_ms: i64,
            dur_ms: u32,
            force_key: i32,
        ) -> i32;
        fn tawny_vp8_next(
            e: *mut c_void,
            out: *mut u8,
            cap: usize,
            len: *mut usize,
            pts_ms: *mut i64,
            key: *mut i32,
        ) -> i32;
        fn tawny_vp8_free(e: *mut c_void);
        fn tawny_opus_new(lookahead: *mut i32) -> *mut c_void;
        fn tawny_opus_encode(
            o: *mut c_void,
            pcm: *const i16,
            samples: usize,
            out: *mut u8,
            cap: i32,
        ) -> i32;
        fn tawny_opus_free(o: *mut c_void);
    }

    /// A VP8 encoder (one thread uses it at a time).
    pub struct Vp8(NonNull<c_void>);

    // SAFETY: libvpx's encoder context has no thread affinity; it is only
    // ever used from one thread at a time (it isn't Sync).
    unsafe impl Send for Vp8 {}

    impl Vp8 {
        pub fn new(w: usize, h: usize, kbps: u32) -> Result<Self> {
            let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
                bail!("picture too large");
            };
            // SAFETY: plain numbers; rec.c checks them and returns NULL if unusable.
            let e = unsafe { tawny_vp8_new(w, h, kbps) };
            NonNull::new(e)
                .map(Self)
                .ok_or_else(|| anyhow::anyhow!("VP8 refused {w}x{h}"))
        }

        /// One I420 frame (or a flush with `None`).
        pub fn encode(&mut self, i420: Option<&[u8]>, ms: u64, dur: u64, key: bool) -> Result<()> {
            let (ptr, len) = i420.map_or((std::ptr::null(), 0), |b| (b.as_ptr(), b.len()));
            let pts = i64::try_from(ms).unwrap_or(i64::MAX);
            let dur = u32::try_from(dur).unwrap_or(1000);
            // SAFETY: the pointer and length describe a live slice (or are
            // null/0 for a flush); rec.c checks the length against the size
            // the encoder was made for, and libvpx only reads it during the call.
            let r =
                unsafe { tawny_vp8_encode(self.0.as_ptr(), ptr, len, pts, dur, i32::from(key)) };
            if r == 0 {
                Ok(())
            } else {
                bail!("VP8 encode failed")
            }
        }

        /// The next encoded frame into `buf`: (length, ms, keyframe).
        pub fn take_frame(&mut self, buf: &mut [u8]) -> Option<(usize, u64, bool)> {
            let (mut len, mut pts, mut key) = (0usize, 0i64, 0i32);
            // SAFETY: `buf` is writable for `buf.len()` bytes and rec.c never
            // writes past `cap`; the out-parameters are live locals.
            let r = unsafe {
                tawny_vp8_next(
                    self.0.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut len,
                    &raw mut pts,
                    &raw mut key,
                )
            };
            (r == 1 && len <= buf.len()).then(|| (len, u64::try_from(pts).unwrap_or(0), key != 0))
        }
    }

    impl Drop for Vp8 {
        fn drop(&mut self) {
            // SAFETY: made by tawny_vp8_new, freed once, here.
            unsafe { tawny_vp8_free(self.0.as_ptr()) }
        }
    }

    /// An Opus encoder, 48 kHz mono, 20 ms packets.
    pub struct Opus {
        enc: NonNull<c_void>,
        lookahead: i32,
    }

    // SAFETY: as Vp8: no thread affinity, one thread at a time.
    unsafe impl Send for Opus {}

    impl Opus {
        pub fn new() -> Result<Self> {
            let mut lookahead = 0i32;
            // SAFETY: the out-parameter is a live local.
            let e = unsafe { tawny_opus_new(&raw mut lookahead) };
            NonNull::new(e)
                .map(|enc| Self { enc, lookahead })
                .ok_or_else(|| anyhow::anyhow!("Opus refused"))
        }

        /// The stream's pre-skip, in samples.
        pub fn lookahead(&self) -> u16 {
            u16::try_from(self.lookahead).unwrap_or(312)
        }

        /// Exactly 960 samples to one packet.
        pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
            let mut out = vec![0u8; 1500];
            let cap = i32::try_from(out.len()).unwrap_or(0);
            // SAFETY: `pcm` and `out` are live slices of the lengths passed;
            // rec.c refuses anything but 960 samples, and libopus writes at
            // most `cap` bytes.
            let n = unsafe {
                tawny_opus_encode(
                    self.enc.as_ptr(),
                    pcm.as_ptr(),
                    pcm.len(),
                    out.as_mut_ptr(),
                    cap,
                )
            };
            let Ok(n) = usize::try_from(n) else {
                bail!("Opus encode failed")
            };
            out.truncate(n);
            Ok(out)
        }
    }

    impl Drop for Opus {
        fn drop(&mut self) {
            // SAFETY: made by tawny_opus_new, freed once, here.
            unsafe { tawny_opus_free(self.enc.as_ptr()) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::codec::{Opus, Vp8};

    /// The encoders libwebrtc carries answer through rec.c.
    #[test]
    fn the_encoders_make_frames() {
        let (w, h) = (64usize, 48usize);
        let mut enc = Vp8::new(w, h, 500).expect("vp8");
        let mut buf = vec![0u8; 1 << 20];
        let mut got = Vec::new();
        for i in 0..10u64 {
            let frame: Vec<u8> = (0..w * h * 3 / 2)
                .map(|p| u8::try_from((p as u64 + i * 7) % 251).expect("small"))
                .collect();
            enc.encode(Some(&frame), i * 40, 40, i == 0)
                .expect("encodes");
            while let Some(f) = enc.take_frame(&mut buf) {
                got.push(f);
            }
        }
        assert_ne!(got.len(), 0, "frames came out");
        assert!(got[0].2, "the first frame is a keyframe");
        assert!(
            enc.encode(Some(&[0u8; 10]), 400, 40, false).is_err(),
            "wrong size refused"
        );
        assert!(Vp8::new(63, 48, 500).is_err(), "odd sides refused");

        let mut opus = Opus::new().expect("opus");
        let tone: Vec<i16> = (0..960)
            .map(|i| if (i / 50) % 2 == 0 { 3000 } else { -3000 })
            .collect();
        let p = opus.encode(&tone).expect("encodes");
        assert!(!p.is_empty() && p.len() < 1500);
        assert!(opus.encode(&tone[..480]).is_err(), "only 20 ms at a time");
        assert!(opus.lookahead() > 0);
    }
}
