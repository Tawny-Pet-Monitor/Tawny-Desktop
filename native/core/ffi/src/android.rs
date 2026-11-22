//! The calls that skip the generated bindings: handing libwebrtc the app's
//! Android `Context`, camera frames read straight out of the camera's
//! buffers, the sound going in and out every 10 ms, and the `Surface` a
//! Viewer draws the Monitor's camera on.
//! Kotlin side: `com.tawny.core.NativeBridge`.

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JIntArray, JObject, JShortArray};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jlong};

use libwebrtc::video_frame::{BoxVideoFrame, VideoFormatType};
use ndk_sys as ndk;

use crate::frames::{self, Planes};
use crate::video::{self, Screen};

/// The app's JVM, kept by [`Java_com_tawny_core_NativeBridge_init`].
static VM: std::sync::OnceLock<jni::JavaVM> = std::sync::OnceLock::new();

/// Attaches the calling thread to the JVM until it ends (see
/// `on_thread_start` in `lib.rs` for why). Does nothing before `init`.
pub(crate) fn attach_thread() {
    if let Some(vm) = VM.get() {
        // A thread that can't attach still runs; it just isn't protected.
        let _ = vm.attach_current_thread_permanently();
    }
}

/// `NativeBridge.init(context)`: call once, before the first Monitor starts.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_init(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    context: JObject<'_>,
) -> jboolean {
    let Ok(vm) = env.get_java_vm() else {
        return JNI_FALSE;
    };
    let ok = libwebrtc::android::initialize_android_context(&vm, &context);
    if let Ok(own) = env.get_java_vm() {
        // Only the first init's is kept; it's the same JVM every time.
        let _ = VM.set(own);
    }
    if ok { JNI_TRUE } else { JNI_FALSE }
}

/// The bytes of a direct `ByteBuffer`, borrowed for this call only.
fn bytes<'a>(env: &JNIEnv<'_>, buf: &'a JByteBuffer<'_>) -> Option<&'a [u8]> {
    let ptr = env.get_direct_buffer_address(buf).ok()?;
    let len = env.get_direct_buffer_capacity(buf).ok()?;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: a direct buffer's address and capacity describe memory the JVM
    // keeps alive while the buffer object is reachable. The caller holds it
    // (and the ImageProxy behind it) open until this call returns, and we
    // only read, within `len`, for the lifetime of `buf`.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// `NativeBridge.pushFrame(...)`: one `YUV_420_888` camera image. The
/// buffers must be direct (the camera's are) and stay open until it returns.
/// Returns whether a Monitor took the frame.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_pushFrame(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    width: jint,
    height: jint,
    y: JByteBuffer<'_>,
    y_row: jint,
    u: JByteBuffer<'_>,
    v: JByteBuffer<'_>,
    uv_row: jint,
    uv_pixel: jint,
    rotation: jint,
) -> jboolean {
    let dims = (
        u32::try_from(width),
        u32::try_from(height),
        usize::try_from(y_row),
        usize::try_from(uv_row),
        usize::try_from(uv_pixel),
    );
    let (Ok(width), Ok(height), Ok(y_row), Ok(uv_row), Ok(uv_pixel)) = dims else {
        return JNI_FALSE;
    };
    let (Some(y), Some(u), Some(v)) = (bytes(&env, &y), bytes(&env, &u), bytes(&env, &v)) else {
        return JNI_FALSE;
    };
    let planes = Planes {
        width,
        height,
        y,
        y_row,
        u,
        v,
        uv_row,
        uv_pixel,
    };
    if frames::push(&planes, rotation) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

/// `NativeBridge.pushMic(samples)`: 10 ms of microphone sound, 480 samples
/// at 48 kHz mono. Returns whether a Monitor took it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_pushMic(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    samples: JShortArray<'_>,
) -> jboolean {
    let mut buf = [0i16; tawny_core::call::media::AUDIO_FRAME];
    if env.get_array_length(&samples).ok() != i32::try_from(buf.len()).ok()
        || env.get_short_array_region(&samples, 0, &mut buf).is_err()
    {
        return JNI_FALSE;
    }
    match crate::runtime() {
        Ok(rt) if crate::sound::push_mic(rt, &buf) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

/// `NativeBridge.readSpeaker(out)`: fills `out` with every incoming track,
/// mixed (silence where there is none). Returns how many samples had sound.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_readSpeaker(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    out: JShortArray<'_>,
) -> jint {
    let Ok(len) = env.get_array_length(&out) else {
        return 0;
    };
    let mut buf = vec![0i16; usize::try_from(len).unwrap_or(0).min(48_000)];
    let n = crate::sound::SPEAKER.read(&mut buf);
    if env.set_short_array_region(&out, 0, &buf).is_err() {
        return 0;
    }
    jint::try_from(n).unwrap_or(0)
}

/// `NativeBridge.recordFrames(on)`: keep the latest frame for a recording
/// (true) or stop (false).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_recordFrames(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    on: jboolean,
) {
    crate::record::set(on != JNI_FALSE);
}

/// `NativeBridge.readFrame(dst, after, dims)`: the latest frame, upright and
/// packed I420 (Y, U, V), into the direct buffer `dst` if it is newer than
/// frame number `after`; its width and height into `dims`. Returns its
/// number, or -1 when there is no newer frame (or `dst` is too small).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_readFrame(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    dst: JByteBuffer<'_>,
    after: jlong,
    dims: JIntArray<'_>,
) -> jlong {
    let (Ok(ptr), Ok(len)) = (
        env.get_direct_buffer_address(&dst),
        env.get_direct_buffer_capacity(&dst),
    ) else {
        return -1;
    };
    if ptr.is_null() {
        return -1;
    }
    // SAFETY: a direct buffer's address and capacity describe memory the JVM
    // keeps alive while `dst` is reachable, which it is for this call; the
    // caller gives each buffer to one reader at a time.
    let out = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
    let Some((w, h, seq)) = crate::record::take(out, u64::try_from(after).unwrap_or(0)) else {
        return -1;
    };
    let wh = [
        jint::try_from(w).unwrap_or(0),
        jint::try_from(h).unwrap_or(0),
    ];
    if env.set_int_array_region(&dims, 0, &wh).is_err() {
        return -1;
    }
    jlong::try_from(seq).unwrap_or(-1)
}

/// A `Surface` to draw on, held as its native window.
struct Window(*mut ndk::ANativeWindow, Vec<u8>);

// SAFETY: an ANativeWindow is reference-counted and safe to use from any
// thread; this one is only ever touched under `video::SCREEN`'s lock.
unsafe impl Send for Window {}

impl Screen for Window {
    fn show(&mut self, frame: &BoxVideoFrame) -> Result<(), &'static str> {
        let (w, h) = (
            frame.buffer.width() as usize,
            frame.buffer.height() as usize,
        );
        if w * h > video::MAX_PIXELS {
            return Err("frame too large");
        }
        let deg = video::degrees(frame.rotation);
        let (ow, oh) = if deg % 180 == 0 { (w, h) } else { (h, w) };
        let (Ok(owi), Ok(ohi), Ok(wi), Ok(hi)) = (
            i32::try_from(ow),
            i32::try_from(oh),
            i32::try_from(w),
            i32::try_from(h),
        ) else {
            return Err("frame too large");
        };
        if deg != 0 {
            // libyuv's ABGR is R, G, B, A in memory: Android's RGBA_8888.
            self.1.resize(w * h * 4, 0);
            frame.buffer.to_argb(
                VideoFormatType::ABGR,
                &mut self.1,
                u32::try_from(w * 4).unwrap_or(0),
                wi,
                hi,
            );
        }
        // SAFETY: `self.0` is a live window (acquired in `setSurface`, released
        // only after it is taken out of `SCREEN`). The locked buffer is written
        // only within the `stride * height` pixels of 4 bytes the lock
        // reported, and it is unlocked before returning.
        unsafe {
            let fmt = i32::try_from(ndk::ANativeWindow_LegacyFormat::WINDOW_FORMAT_RGBA_8888.0)
                .unwrap_or(1);
            ndk::ANativeWindow_setBuffersGeometry(self.0, owi, ohi, fmt);
            let mut buf: ndk::ANativeWindow_Buffer = std::mem::zeroed();
            if ndk::ANativeWindow_lock(self.0, &raw mut buf, std::ptr::null_mut()) != 0 {
                return Err("window lock failed");
            }
            let fits =
                buf.width == owi && buf.height == ohi && !buf.bits.is_null() && buf.stride >= owi;
            if fits {
                let stride = usize::try_from(buf.stride).unwrap_or(0);
                let dst = std::slice::from_raw_parts_mut(buf.bits.cast::<u8>(), stride * oh * 4);
                if deg == 0 {
                    frame.buffer.to_argb(
                        VideoFormatType::ABGR,
                        dst,
                        u32::try_from(stride * 4).unwrap_or(0),
                        owi,
                        ohi,
                    );
                } else {
                    rotate_rgba(&self.1, w, h, deg, dst, stride);
                }
                video::LAST_LEVEL.store(level(dst), std::sync::atomic::Ordering::Relaxed);
            }
            ndk::ANativeWindow_unlockAndPost(self.0);
            if fits {
                Ok(())
            } else {
                Err("window buffer is another size")
            }
        }
    }
}

/// The average of every 997th byte of an RGBA picture's colour channels.
fn level(rgba: &[u8]) -> u32 {
    let (mut sum, mut n) = (0u64, 0u64);
    for (i, b) in rgba.iter().enumerate().step_by(997) {
        if i % 4 != 3 {
            sum += u64::from(*b);
            n += 1;
        }
    }
    u32::try_from(sum.checked_div(n).unwrap_or(0)).unwrap_or(0)
}

/// Copies a `w`×`h` RGBA picture into `dst` (`stride` pixels a row) turned
/// `deg` degrees clockwise.
fn rotate_rgba(src: &[u8], w: usize, h: usize, deg: u32, dst: &mut [u8], stride: usize) {
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match deg {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let s = (y * w + x) * 4;
            let d = (dy * stride + dx) * 4;
            if let (Some(from), Some(to)) = (src.get(s..s + 4), dst.get_mut(d..d + 4)) {
                to.copy_from_slice(from);
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the reference taken by `ANativeWindow_fromSurface`, released once.
        unsafe { ndk::ANativeWindow_release(self.0) };
    }
}

/// `NativeBridge.setSurface(surface)`: draw the Monitor's camera on this
/// `Surface` from now on; `null` to stop (call it from `surfaceDestroyed`,
/// before the surface goes: it waits for a frame being drawn to finish).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tawny_core_NativeBridge_setSurface(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    surface: JObject<'_>,
) {
    let window = if surface.is_null() {
        None
    } else {
        // SAFETY: a valid JNIEnv for this thread and a live Surface reference,
        // both from the JVM for the duration of this call.
        let w = unsafe {
            ndk::ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast())
        };
        (!w.is_null()).then(|| Box::new(Window(w, Vec::new())) as Box<dyn Screen>)
    };
    if let Ok(mut s) = video::SCREEN.lock() {
        *s = window;
    }
}
