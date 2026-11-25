//! Headless screenshots: every screen, light and dark, through Slint's
//! software renderer into PNGs. No display, no GPU: runs on the build box.

use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError};

use slint::{ModelRc, VecModel};

use crate::{AppWindow, Channel, Live, RelayCheck, Theme, Tip, TipAmount, WhoEntry, draw_stills};

const W: u32 = 1280;
const H: u32 = 800;

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
}

/// The screens to shoot: the web page's id, and the name of the shot.
const SCREENS: &[(&str, &str)] = &[
    ("welcome", "welcome"),
    ("setup-role", "setup-role"),
    ("watch-pair", "watch-pair"),
    ("channels", "channels-list"),
    ("channels", "channels-empty"),
    ("role", "role"),
    ("join", "join"),
    ("live", "live-station"),
    ("live", "live-viewer"),
    ("live", "live-station-on"),
    ("live", "live-viewer-on"),
    ("live", "live-viewer-sas"),
    ("live", "live-viewer-chimes"),
    ("live", "live-viewer-paused"),
    ("live", "live-station-talk"),
    ("live", "live-station-dimmed"),
    ("live", "live-viewer-lenses"),
    ("live", "live-viewer-coach"),
    ("live", "live-viewer-zoomed"),
    ("live", "live-station-coach"),
    ("channels", "sheet-row-menu"),
    ("channels", "sheet-del-confirm"),
    ("channels", "sheet-editor"),
    ("channels", "sheet-who"),
    ("channels", "sheet-myname"),
    ("channels", "sheet-pair"),
    ("channels", "sheet-scanner"),
    ("channels", "sheet-servers"),
    ("channels", "sheet-only"),
    ("channels", "sheet-whatsnew"),
    ("channels", "sheet-tip"),
    ("channels", "sheet-tip-qr"),
    ("channels", "sheet-tip-other"),
    ("channels", "sheet-tip-error"),
    ("channels", "sheet-tip-thanks"),
];

/// The tip page as the reference stages it (fixtures, nothing fetched).
fn stage_tip(ui: &AppWindow, name: &str) {
    use crate::tip::{amount_at, fmt_sats, fmt_usd};
    let tip = ui.global::<Tip>();
    let page = match name {
        "sheet-tip-qr" | "sheet-tip-other" | "sheet-tip-error" => "pick",
        "sheet-tip-thanks" => "thanks",
        _ => "hub",
    };
    tip.set_page(page.into());
    let price = Some(107_500.0);
    let pills: Vec<TipAmount> = (0..3)
        .map(|i| {
            let a = amount_at(i, price);
            TipAmount {
                big: fmt_usd(a.usd.unwrap_or_default()).into(),
                small: fmt_sats(a.sats, false).into(),
            }
        })
        .collect();
    tip.set_amounts(ModelRc::new(VecModel::from(pills)));
    let (pick, state) = match name {
        "sheet-tip-other" => (3, "empty"),
        "sheet-tip-error" => (1, "error"),
        _ => (0, "qr"),
    };
    tip.set_pick(pick);
    tip.set_state(state.into());
    let invoice = format!(
        "lnbc18600n1p5tawnyqpp5{}cqzzsxqyz5vq",
        "qqqsyqcyq5rqwzqfqypqhq4kgj7vzx3r8m2d5w9c6n0t".repeat(6)
    );
    if let Some(img) =
        crate::tip::qr_image(&format!("lightning:{invoice}").to_ascii_uppercase(), 224)
    {
        tip.set_qr(img);
    }
    tip.set_code(format!("{}…{}", &invoice[..20], &invoice[invoice.len() - 8..]).into());
    tip.set_waiting(name == "sheet-tip-qr");
    tip.set_big_amount("$5".into());
    tip.set_big_sats(fmt_sats(4651, false).into());
}

/// Sample contents for the sheets that show data.
fn fill_sheets(ui: &AppWindow) {
    let who = vec![
        WhoEntry {
            name: "Biscuit cam".into(),
            device: "Pixel 6".into(),
            role: "station".into(),
            you: false,
            initial: "B".into(),
        },
        WhoEntry {
            name: "Sam".into(),
            device: "Desktop".into(),
            role: "viewer".into(),
            you: true,
            initial: "S".into(),
        },
        WhoEntry {
            name: "".into(),
            device: "iPhone".into(),
            role: "viewer".into(),
            you: false,
            initial: "I".into(),
        },
    ];
    ui.set_who(ModelRc::new(VecModel::from(who)));
    ui.set_pair_channel("Biscuit".into());
    ui.set_pair_expiry(
        "This code works for another 9 minutes. A fresh one appears here when it runs out.".into(),
    );
    let checks = vec![
        RelayCheck {
            url: "wss://relay.example.org".into(),
            state: "ok".into(),
        },
        RelayCheck {
            url: "wss://nostr.home.lan".into(),
            state: "ok".into(),
        },
    ];
    ui.set_relays("wss://relay.example.org\nwss://nostr.home.lan".into());
    ui.set_only_checks(ModelRc::new(VecModel::from(checks)));
    ui.set_only_verdict("Both relays answered and passed a sealed message back.".into());
    ui.set_only_passed(true);
    ui.set_whatsnew_version("1.1.0".into());
    ui.set_whatsnew_notes(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("A faster, native Tawny for the desktop"),
        slint::SharedString::from("Pair over Nostr with your own relays"),
        slint::SharedString::from("See who is watching, and give yourself a name"),
    ])));
    ui.set_whatsnew_changelog(true);
}

/// Puts the call in the state a shot is named for, as the reference stages it
/// on the web page (tools/desktop-ref.mjs).
fn stage_live(ui: &AppWindow, name: &str) {
    let live = ui.global::<Live>();
    let station = name.starts_with("live-station");
    live.set_role(if station { "station" } else { "viewer" }.into());
    let on = name.ends_with("-on") || name.matches('-').count() > 1;
    live.set_connecting(!on);
    live.set_dot(if on { "live" } else { "idle" }.into());
    live.set_status(
        match (on, station) {
            (false, _) => "Connecting",
            (true, true) => "On air",
            (true, false) => "Live",
        }
        .into(),
    );
    live.set_level(if on { 9.0 / 14.0 } else { 0.0 });
    live.set_peers(if on { 2 } else { 0 });
    live.set_announce(on && station);
    live.set_channel(if on && !station { "Biscuit" } else { "" }.into());
    live.set_battery(if on && !station { 64 } else { -1 });
    live.set_sas_code(if name.ends_with("-sas") { "4821" } else { "" }.into());
    let chimes = name.ends_with("-chimes");
    live.set_chimes_open(chimes);
    live.set_torch_on(chimes);
    let paused = name.ends_with("-paused");
    if paused {
        live.set_dot("warn".into());
        live.set_status("Monitor paused".into());
        live.set_note_title("The monitor is paused".into());
        live.set_note_body(
            "Its screen turned off or the app moved to the background. The picture comes \
             back on its own when the monitor phone is woken."
                .into(),
        );
    } else {
        live.set_note_title("".into());
        live.set_note_body("".into());
    }
    live.set_dimmed(name.ends_with("-dimmed"));
    // The walkthrough's second step: a key in the row.
    live.set_coach_step(if name.ends_with("-coach") { 1 } else { -1 });
    let lenses = name.ends_with("-lenses");
    let names: Vec<slint::SharedString> = if lenses {
        vec!["0.5×".into(), "1×".into(), "3×".into()]
    } else {
        Vec::new()
    };
    live.set_lenses(ModelRc::new(VecModel::from(names)));
    live.set_lens(1);
    live.set_zoom_text(if lenses { "2.0×" } else { "" }.into());
    let talk = name.ends_with("-talk");
    live.set_talkflag(talk);
    live.set_mic_muted(talk);
    // The Viewer's own zoom: a picture whose quarters can be told apart,
    // zoomed 2.5x at a point in its upper-left quarter.
    let zoomed = name.ends_with("-zoomed");
    live.set_has_video(zoomed);
    live.set_zoom(1.0);
    live.set_zoom_x(0.0);
    live.set_zoom_y(0.0);
    if zoomed {
        live.set_video(quarters());
        let f = crate::zoom::Frame {
            stage_w: 1280.0,
            stage_h: 708.0,
            video_w: 640.0,
            video_h: 360.0,
        };
        let v = crate::zoom::around(crate::zoom::View::WHOLE, 2.5, 380.0, 220.0, f);
        live.set_zoom(v.zoom);
        live.set_zoom_x(v.x);
        live.set_zoom_y(v.y);
        live.set_zoom_text(crate::zoom::label(v).into());
    }
}

/// A 640x360 test picture: four coloured quarters inside a white frame.
fn quarters() -> slint::Image {
    let (width, height) = (640u32, 360u32);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    for (n, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let n = u32::try_from(n).unwrap_or(0);
        let (col, row) = (n % width, n / width);
        let frame = col < 6 || row < 6 || col >= width - 6 || row >= height - 6;
        let (r, g, b) = match (frame, col < width / 2, row < height / 2) {
            (true, _, _) => (255, 255, 255),
            (false, true, true) => (214, 72, 102),
            (false, false, true) => (240, 190, 120),
            (false, true, false) => (90, 140, 200),
            (false, false, false) => (120, 180, 110),
        };
        *px = slint::Rgba8Pixel { r, g, b, a: 255 };
    }
    slint::Image::from_rgba8(buf)
}

/// Two channels like the reference's: one this device monitors, one it views.
fn sample_channels() -> Vec<Channel> {
    vec![
        Channel {
            id: "c1".into(),
            name: "Biscuit".into(),
            role: "station".into(),
            key: "L_F7wC".into(),
        },
        Channel {
            id: "c2".into(),
            name: "Living room".into(),
            role: "viewer".into(),
            key: "Zx9_ab".into(),
        },
    ]
}

/// A pairing link like a real one, for the pairing screens.
const SAMPLE_LINK: &str = "tawny://pair?k=L_F7wC98BnTz78KPFl-bRg&n=Pet%20camera&h=192.168.1.20:8820\
                           &c=fpCEeBfXe2g&e=1795275874&np=79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

/// Renders every screen in both themes into `dir`.
pub fn all(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless {
        window: window.clone(),
    }))
    .map_err(|e| anyhow!("platform: {e}"))?;
    let ui = AppWindow::new()?;
    window.set_size(PhysicalSize::new(W, H));
    ui.show()?;
    ui.set_pair_link(SAMPLE_LINK.into());
    ui.set_role_name("Living room".into());
    fill_sheets(&ui);
    if let Some(img) = crate::qr::image(SAMPLE_LINK) {
        ui.set_pair_qr(img);
    }
    // Settled pictures: no transition caught half way.
    ui.global::<Theme>().set_still(true);
    for theme in ["light", "dark"] {
        ui.global::<Theme>().set_mode(theme.into());
        for (screen, name) in SCREENS {
            let sheet = name.strip_prefix("sheet-").unwrap_or_default();
            ui.set_sheet(
                if sheet.starts_with("tip") {
                    "tip"
                } else {
                    sheet
                }
                .into(),
            );
            stage_tip(&ui, name);
            let list = if *name == "channels-empty" || name.starts_with("sheet-") {
                Vec::new()
            } else {
                sample_channels()
            };
            ui.set_channels(ModelRc::new(VecModel::from(list)));
            stage_live(&ui, name);
            ui.set_screen((*screen).into());
            // Two passes: the first lays the screen out, the second draws the
            // pictures at the size they ended up.
            render(&window)?;
            draw_stills(&ui);
            let px = render(&window)?;
            let file = if name.starts_with("sheet-") {
                dir.join(format!("{theme}-{name}.png"))
            } else {
                dir.join(format!("{theme}-screen-{name}.png"))
            };
            px.save_png(&file)
                .with_context(|| format!("writing {}", file.display()))?;
            println!("{}", file.display());
        }
    }
    Ok(())
}

/// One frame of the window, as a pixmap.
fn render(window: &MinimalSoftwareWindow) -> Result<tiny_skia::Pixmap> {
    slint::platform::update_timers_and_animations();
    let mut buf = vec![PremultipliedRgbaColor::default(); (W * H) as usize];
    window.request_redraw();
    window.draw_if_needed(|r| {
        r.render(&mut buf, W as usize);
    });
    let mut px = tiny_skia::Pixmap::new(W, H).context("pixmap")?;
    for (dst, src) in px.pixels_mut().iter_mut().zip(&buf) {
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(src.red, src.green, src.blue, src.alpha)
            .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
    }
    Ok(px)
}
