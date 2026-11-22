//! Compiles the Slint screens in `ui/`, with their translations from `lang/`
//! (`lang/<language>/LC_MESSAGES/tawny-desktop.po`) built in.
//!
//! Also builds `csrc/rec.c`, the recorder's way to the VP8 and Opus encoders
//! inside libwebrtc, against the headers vendored from that build. On
//! Windows, the program gets its icon and version details.
fn main() -> Result<(), slint_build::CompileError> {
    println!("cargo:rerun-if-changed=lang");
    println!("cargo:rerun-if-changed=csrc");
    cc::Build::new()
        .file("csrc/rec.c")
        .include("csrc/vendor")
        .include("csrc/vendor/opus")
        .warnings(true)
        .compile("tawny_rec");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/windows/tawny.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/windows/tawny.ico")
            .set("ProductName", "Tawny")
            .set("FileDescription", "Tawny")
            .set("CompanyName", "Tawny Pet Monitor")
            .set("LegalCopyright", "MIT");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon on the program: {e}");
        }
    }
    let config = slint_build::CompilerConfiguration::new().with_bundled_translations("lang");
    slint_build::compile_with_config("ui/app.slint", config)
}
