//! OpenGL that is really the CPU.
//!
//! Without a usable GPU driver, OpenGL falls back to Mesa's llvmpipe (or
//! Windows' "GDI Generic"): the CPU emulating a GPU, a frame at a time. On b1
//! that drew the call through llvmpipe on ~20 threads, about 3 cores for one
//! picture. Slint's own software renderer draws the same screens for a
//! fraction of that.
//!
//! The renderer is picked before the window opens and can't change after, so
//! the first frame reads which OpenGL it got. A software one is noted in the
//! data folder; for a week of starts Tawny uses the software renderer, then
//! tries OpenGL again; for the rest of this run the picture is redrawn less
//! often. `SLINT_BACKEND`, when set, always wins.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

/// The note left in the data folder.
const MARK: &str = "software-gl";

/// This run draws through a software OpenGL.
static SOFTWARE_GL: AtomicBool = AtomicBool::new(false);

/// How long a software OpenGL is believed before OpenGL is tried again.
const RECHECK: Duration = Duration::from_hours(7 * 24);

/// How often the picture is redrawn on a software OpenGL: 10 a second.
const SLOW_FRAME: Duration = Duration::from_millis(100);

/// Names a software OpenGL gives as its renderer.
pub fn is_software(renderer: &str) -> bool {
    let r = renderer.to_ascii_lowercase();
    [
        "llvmpipe",
        "softpipe",
        "swiftshader",
        "gdi generic",
        "basic render",
    ]
    .iter()
    .any(|s| r.contains(s))
}

fn mark(dir: &Path) -> PathBuf {
    dir.join(MARK)
}

/// Picks Slint's software renderer when an earlier run found a software
/// OpenGL. Call before the first window. Does nothing if `SLINT_BACKEND` is
/// set or nothing was noted.
pub fn choose_renderer(dir: &Path) {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let mark = mark(dir);
    let Ok(age) = std::fs::metadata(&mark)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().unwrap_or_default())
    else {
        return;
    };
    // Kept a week, then OpenGL gets another try (and is read again on its
    // first frame): a driver installed since, or one run over a remote
    // desktop, doesn't leave Tawny on the CPU for good.
    if age > RECHECK {
        let _ = std::fs::remove_file(&mark);
        return;
    }
    match slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
    {
        Ok(()) => eprintln!("tawny: no GPU OpenGL last time; drawing with the software renderer"),
        Err(e) => eprintln!("tawny: the software renderer is not available: {e}"),
    }
}

/// Reads, on the first frame, which OpenGL the window got; a software one is
/// noted for next time and slows the picture for this run.
pub fn watch(ui: &AppWindow, dir: PathBuf) {
    let result = ui.window().set_rendering_notifier(move |state, api| {
        if !matches!(state, RenderingState::RenderingSetup) {
            return;
        }
        let GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
            return;
        };
        let Some(name) = gl_renderer(get_proc_address) else {
            return;
        };
        if is_software(&name) {
            SOFTWARE_GL.store(true, Ordering::Relaxed);
            eprintln!(
                "tawny: OpenGL is {name} (the CPU): the software renderer from the next start"
            );
            if let Err(e) = std::fs::write(mark(&dir), format!("{name}\n")) {
                eprintln!("tawny: noting it: {e}");
            }
        }
    });
    if let Err(e) = result {
        // The software renderer itself has no notifier: nothing to watch.
        if std::env::var_os("TAWNY_DEV").is_some() {
            eprintln!("tawny: rendering notifier: {e}");
        }
    }
}

/// Whether a new picture should be drawn now. On a software OpenGL, at most
/// one every [`SLOW_FRAME`]; otherwise always.
pub fn draw_now(last: &mut Option<Instant>) -> bool {
    if !SOFTWARE_GL.load(Ordering::Relaxed) {
        return true;
    }
    let now = Instant::now();
    if last.is_some_and(|t| now.duration_since(t) < SLOW_FRAME) {
        return false;
    }
    *last = Some(now);
    true
}

/// `glGetString(GL_RENDERER)` of the context Slint is setting up.
#[expect(
    unsafe_code,
    reason = "reading one string from the OpenGL context: the only way to tell a software one"
)]
fn gl_renderer(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Option<String> {
    const GL_RENDERER: u32 = 0x1F01;
    let p = get_proc_address(c"glGetString");
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is non-null and is the context's `glGetString`, whose C
    // signature is `const GLubyte *glGetString(GLenum name)`; Slint calls
    // this notifier with that context current.
    let get_string = unsafe {
        std::mem::transmute::<*const c_void, extern "system" fn(u32) -> *const c_char>(p)
    };
    let s = get_string(GL_RENDERER);
    if s.is_null() {
        return None;
    }
    // SAFETY: OpenGL returns a static, NUL-terminated string for a valid
    // name, and `s` was checked for null.
    Some(unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::is_software;

    #[test]
    fn software_opengls_are_told_from_gpus() {
        assert!(is_software("llvmpipe (LLVM 18.1.3, 256 bits)"));
        assert!(is_software("GDI Generic"));
        assert!(is_software("Google SwiftShader"));
        assert!(!is_software("Mesa Intel(R) Arc(tm) Graphics (MTL)"));
        assert!(!is_software("NVIDIA GeForce RTX 4070/PCIe/SSE2"));
    }
}
