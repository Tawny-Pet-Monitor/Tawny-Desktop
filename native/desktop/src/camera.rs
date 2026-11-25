//! The computer's camera into the Monitor: V4L2 on Linux, Media Foundation
//! on Windows (through `nokhwa`). Frames come as YUYV, NV12 or MJPEG,
//! whichever the camera gives at about 720p; each is turned into I420 for
//! the call, and every few is also handed to the window as the Monitor's own
//! preview.
//!
//! Capture runs on a thread of its own and stops when its flag is cleared.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::{I420Buffer, VideoBuffer as _, VideoFrame, VideoRotation};
use libwebrtc::video_source::native::NativeVideoSource;
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Every how many frames the preview is refreshed.
const PREVIEW_EVERY: u32 = 3;

/// Where the preview goes, on the window's thread.
pub type Preview = fn(Image);

/// A running capture.
pub struct Capture {
    run: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens camera number `index` and feeds `source` until stopped.
    ///
    /// # Errors
    /// When there is no such camera or it won't stream.
    pub fn start(index: u32, source: NativeVideoSource, preview: Preview) -> Result<Self> {
        let run = Arc::new(AtomicBool::new(true));
        let flag = run.clone();
        // The camera is opened on its own thread (it is not Send on every
        // platform); the first result comes back before start returns.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        if std::env::var_os("TAWNY_FAKE_CAMERA").is_some_and(|v| v == "1") {
            let thread = std::thread::Builder::new()
                .name("tawny-camera".into())
                .spawn(move || test_pattern(&flag, &source, preview))
                .context("starting the camera thread")?;
            return Ok(Self {
                run,
                thread: Some(thread),
            });
        }
        let thread = std::thread::Builder::new()
            .name("tawny-camera".into())
            .spawn(move || {
                let mut cam = match open(index) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                feed(&mut cam, &flag, &source, preview);
                let _ = cam.stop_stream();
            })
            .context("starting the camera thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("the camera thread stopped"))??;
        Ok(Self {
            run,
            thread: Some(thread),
        })
    }
}

impl Capture {
    /// Opens camera number `index` to read a pairing code: every frame to
    /// `preview`, and the first QR code's text to `found`, after which it
    /// stops on its own.
    ///
    /// # Errors
    /// When there is no such camera or it won't stream.
    pub fn scanner(index: u32, preview: Preview, found: fn(&str)) -> Result<Self> {
        let run = Arc::new(AtomicBool::new(true));
        let flag = run.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("tawny-scanner".into())
            .spawn(move || {
                let mut cam = match open(index) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                read_codes(&mut cam, &flag, preview, found);
                let _ = cam.stop_stream();
            })
            .context("starting the scanner thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("the scanner thread stopped"))??;
        Ok(Self {
            run,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.run.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens camera `index`, reads `frames` frames through the call's
/// conversion, and says what came: format, size, frames a second and mean
/// brightness. Nothing is kept. (A developer's check.)
///
/// # Errors
/// When the camera won't open or no frame converts.
pub fn check(index: u32, frames: u32) -> Result<String> {
    let mut cam = open(index)?;
    let fmt = cam.camera_format();
    let started = std::time::Instant::now();
    let mut good = 0u32;
    let mut luma = 0u64;
    for _ in 0..frames {
        let Ok(frame) = cam.frame() else { continue };
        let res = frame.resolution();
        if let Some(i420) = to_i420(
            frame.source_frame_format(),
            frame.buffer(),
            res.width(),
            res.height(),
        ) {
            good += 1;
            let (y, _, _) = i420.data();
            let n = (res.width() * res.height()) as usize;
            luma = y.iter().take(n).map(|v| u64::from(*v)).sum::<u64>() / n.max(1) as u64;
        }
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let _ = cam.stop_stream();
    if good == 0 {
        return Err(anyhow!("no frame converted ({fmt})"));
    }
    Ok(format!(
        "{fmt}: {good}/{frames} frames converted, {:.1} a second, mean brightness {luma}",
        f64::from(good) / secs
    ))
}

/// How many cameras this computer has.
#[must_use]
pub fn count() -> u32 {
    nokhwa::query(ApiBackend::Auto).map_or(0, |l| u32::try_from(l.len()).unwrap_or(0))
}

/// Opens a camera at about 720p: MJPEG, else YUYV, else whatever it offers.
fn open(index: u32) -> Result<Camera> {
    let near =
        |f| RequestedFormatType::Closest(CameraFormat::new(Resolution::new(1280, 720), f, 30));
    let tries = [
        near(FrameFormat::MJPEG),
        near(FrameFormat::YUYV),
        near(FrameFormat::NV12),
        RequestedFormatType::AbsoluteHighestFrameRate,
    ];
    let mut last = anyhow!("no camera");
    for t in tries {
        let want = RequestedFormat::new::<RgbFormat>(t);
        match Camera::new(CameraIndex::Index(index), want) {
            Ok(mut cam) => match cam.open_stream() {
                Ok(()) => return Ok(cam),
                Err(e) => last = anyhow!("camera: {e}"),
            },
            Err(e) => last = anyhow!("camera: {e}"),
        }
    }
    Err(last)
}

/// A moving test picture instead of a camera (`TAWNY_FAKE_CAMERA=1`, for
/// end-to-end runs on a computer without one): bars that slide, 15 a second.
fn test_pattern(run: &AtomicBool, source: &NativeVideoSource, preview: Preview) {
    let (w, h) = (640u32, 360u32);
    let busy = Arc::new(AtomicBool::new(false));
    let mut t = 0usize;
    while run.load(Ordering::Relaxed) {
        let mut buf = I420Buffer::new(w, h);
        let (sy, su, sv) = buf.strides();
        let (sy, su, sv) = (sy as usize, su as usize, sv as usize);
        let (dy, du, dv) = buf.data_mut();
        for row in 0..h as usize {
            for col in 0..w as usize {
                let bar = ((col + t * 4) / 80) % 2;
                dy[row * sy + col] = if bar == 0 { 200 } else { 60 };
            }
        }
        for row in 0..(h / 2) as usize {
            du[row * su..row * su + (w / 2) as usize].fill(90);
            dv[row * sv..row * sv + (w / 2) as usize].fill(170);
        }
        if t.is_multiple_of(PREVIEW_EVERY as usize) && !busy.swap(true, Ordering::AcqRel) {
            show(&buf, preview, busy.clone());
        }
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
        t = t.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(66));
    }
}

fn feed(cam: &mut Camera, run: &AtomicBool, source: &NativeVideoSource, preview: Preview) {
    let busy = Arc::new(AtomicBool::new(false));
    let mut n = 0u32;
    while run.load(Ordering::Relaxed) {
        let Ok(frame) = cam.frame() else { continue };
        let res = frame.resolution();
        let (w, h) = (res.width(), res.height());
        let Some(i420) = to_i420(frame.source_frame_format(), frame.buffer(), w, h) else {
            continue;
        };
        n = n.wrapping_add(1);
        if n.is_multiple_of(PREVIEW_EVERY) && !busy.swap(true, Ordering::AcqRel) {
            show(&i420, preview, busy.clone());
        }
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, i420));
    }
}

fn read_codes(cam: &mut Camera, run: &AtomicBool, preview: Preview, found: fn(&str)) {
    let busy = Arc::new(AtomicBool::new(false));
    let mut n = 0u32;
    while run.load(Ordering::Relaxed) {
        let Ok(frame) = cam.frame() else { continue };
        let res = frame.resolution();
        let Some(i420) = to_i420(
            frame.source_frame_format(),
            frame.buffer(),
            res.width(),
            res.height(),
        ) else {
            continue;
        };
        n = n.wrapping_add(1);
        if !busy.swap(true, Ordering::AcqRel) {
            show(&i420, preview, busy.clone());
        }
        if n.is_multiple_of(4)
            && let Some(text) = qr_text(&i420)
        {
            let _ = slint::invoke_from_event_loop(move || found(&text));
            return;
        }
    }
}

/// The text of a QR code in the picture, if there is one.
fn qr_text(i420: &I420Buffer) -> Option<String> {
    let (w, h) = (i420.width() as usize, i420.height() as usize);
    let (sy, _, _) = i420.strides();
    let (luma, _, _) = i420.data();
    let sy = sy as usize;
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| luma[y * sy + x]);
    img.detect_grids()
        .into_iter()
        .find_map(|g| g.decode().ok().map(|(_, text)| text))
}

/// One camera frame as I420, or `None` if it doesn't add up.
fn to_i420(format: FrameFormat, data: &[u8], w: u32, h: u32) -> Option<I420Buffer> {
    if w < 2 || h < 2 || w % 2 == 1 || h % 2 == 1 {
        return None;
    }
    let (wu, hu) = (w as usize, h as usize);
    match format {
        FrameFormat::YUYV => {
            if data.len() < wu * hu * 2 {
                return None;
            }
            let mut buf = I420Buffer::new(w, h);
            yuyv_into(&mut buf, data, wu, hu);
            Some(buf)
        }
        FrameFormat::NV12 => {
            let (y, uv) = data.split_at_checked(wu * hu)?;
            if uv.len() < wu * hu / 2 {
                return None;
            }
            let mut buf = I420Buffer::new(w, h);
            let (sy, su, sv) = buf.strides();
            let (dy, du, dv) = buf.data_mut();
            yuv_helper::nv12_to_i420(
                y,
                w,
                uv,
                w,
                dy,
                sy,
                du,
                su,
                dv,
                sv,
                i32::try_from(w).ok()?,
                i32::try_from(h).ok()?,
            );
            Some(buf)
        }
        FrameFormat::MJPEG => {
            let opts = zune_jpeg::zune_core::options::DecoderOptions::default()
                .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA);
            let mut dec = zune_jpeg::JpegDecoder::new_with_options(data, opts);
            let rgba = dec.decode().ok()?;
            let (dw, dh) = dec.dimensions()?;
            if (dw, dh) != (wu, hu) || rgba.len() < wu * hu * 4 {
                return None;
            }
            let mut buf = I420Buffer::new(w, h);
            let (sy, su, sv) = buf.strides();
            let (dy, du, dv) = buf.data_mut();
            // libyuv's ABGR is R, G, B, A in memory.
            yuv_helper::abgr_to_i420(
                &rgba,
                w * 4,
                dy,
                sy,
                du,
                su,
                dv,
                sv,
                i32::try_from(w).ok()?,
                i32::try_from(h).ok()?,
            );
            Some(buf)
        }
        _ => None,
    }
}

/// YUYV (Y0 U Y1 V per pixel pair) into I420, chroma from the even rows.
fn yuyv_into(buf: &mut I420Buffer, src: &[u8], w: usize, h: usize) {
    let (sy, su, sv) = buf.strides();
    let (sy, su, sv) = (sy as usize, su as usize, sv as usize);
    let (dy, du, dv) = buf.data_mut();
    for row in 0..h {
        let line = &src[row * w * 2..(row + 1) * w * 2];
        let (pairs, _) = line.as_chunks::<4>();
        for (i, px) in pairs.iter().enumerate() {
            dy[row * sy + i * 2] = px[0];
            dy[row * sy + i * 2 + 1] = px[2];
            if row % 2 == 0 {
                du[(row / 2) * su + i] = px[1];
                dv[(row / 2) * sv + i] = px[3];
            }
        }
    }
}

/// A preview of the frame onto the window.
fn show(i420: &I420Buffer, preview: Preview, done: Arc<AtomicBool>) {
    let (w, h) = (i420.width(), i420.height());
    let Ok(len) = usize::try_from(u64::from(w) * u64::from(h) * 4) else {
        done.store(false, Ordering::Release);
        return;
    };
    let mut rgba = vec![0u8; len];
    let (sy, su, sv) = i420.strides();
    let (luma, cb, cr) = i420.data();
    let (Ok(wi), Ok(hi)) = (i32::try_from(w), i32::try_from(h)) else {
        done.store(false, Ordering::Release);
        return;
    };
    yuv_helper::i420_to_abgr(luma, sy, cb, su, cr, sv, &mut rgba, w * 4, wi, hi);
    let sent = slint::invoke_from_event_loop(move || {
        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
        preview(Image::from_rgba8(buf));
        done.store(false, Ordering::Release);
    });
    if sent.is_err() {
        // The window is gone; nothing will clear the flag, and nothing needs to.
    }
}

#[cfg(test)]
mod tests {
    use super::{qr_text, yuyv_into};
    use libwebrtc::video_frame::I420Buffer;

    #[test]
    fn a_pairing_code_on_screen_reads_back() {
        let link = "tawny://pair?k=L_F7wC98BnTz78KPFl-bRg&n=Biscuit&c=fpCEeBfXe2g&e=1795275874";
        let code =
            qrcode::QrCode::with_error_correction_level(link, qrcode::EcLevel::M).expect("encodes");
        let n = code.width();
        // Four pixels a module, four modules of quiet white round it.
        let side = (n + 8) * 4;
        let side = side + side % 2;
        let px = u32::try_from(side).expect("fits");
        let mut buf = I420Buffer::new(px, px);
        let (sy, _, _) = buf.strides();
        let (y, _, _) = buf.data_mut();
        let colors = code.to_colors();
        for row in 0..side {
            for col in 0..side {
                let (mr, mc) = ((row / 4).wrapping_sub(4), (col / 4).wrapping_sub(4));
                let dark = mr < n && mc < n && colors[mr * n + mc] == qrcode::Color::Dark;
                y[row * sy as usize + col] = if dark { 20 } else { 235 };
            }
        }
        assert_eq!(qr_text(&buf).as_deref(), Some(link));
    }

    #[test]
    fn yuyv_splits_into_planes() {
        // 2×2: two rows of (Y0=10 U=20 Y1=30 V=40) and (Y0=50 U=60 Y1=70 V=80).
        let src = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut buf = I420Buffer::new(2, 2);
        yuyv_into(&mut buf, &src, 2, 2);
        let (sy, _, _) = buf.strides();
        let (y, u, v) = buf.data();
        assert_eq!([y[0], y[1]], [10, 30]);
        assert_eq!([y[sy as usize], y[sy as usize + 1]], [50, 70]);
        assert_eq!((u[0], v[0]), (20, 40), "chroma from the even row");
    }
}
