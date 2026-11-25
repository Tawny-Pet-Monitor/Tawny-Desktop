//! Android: export libwebrtc's `Java_livekit_org_webrtc_*` JNI entry points
//! from this library. The classes in libwebrtc.jar bind to them at run time;
//! without this the linker hides them and the first factory call aborts.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        if let Err(e) = webrtc_sys_build::configure_jni_symbols() {
            println!("cargo:warning=JNI symbol export failed: {e:#}");
            std::process::exit(1);
        }
    }
}
