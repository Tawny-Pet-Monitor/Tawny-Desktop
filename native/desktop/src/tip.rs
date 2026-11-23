//! Tip in Bitcoin, over Lightning (web/tip.js, its "web" mode). The page
//! offers $2 / $5 / $10 or a typed amount; Tawny asks the Lightning
//! Address's LNURL-pay service (Strike) for an invoice of exactly that and
//! shows it as a QR, for any wallet to pay. Tawny never holds a key, a
//! balance or a satoshi.
//!
//! Knowing the tip landed: the invoice is asked for as a Nostr zap (NIP-57)
//! under a throwaway key made for this one tip. Once it's paid, Strike
//! publishes a receipt signed with its own key to the relays named in the
//! request; the page listens for a receipt carrying exactly this invoice,
//! checks its id and signature, and turns into a thank-you. Nothing is sent
//! anywhere but the tip service, the price API and those relays.
//!
//! One thing more than the web page: an invoice whose own amount isn't the
//! one asked for is refused, so a wrong answer can't put a different price
//! in front of the person tipping.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak,
};
use tawny_core::nostr::event::{Event, Template, public_key, sign};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::ui::{AppWindow, Tip, TipAmount};

/// Where tips go, on the web for anyone without a wallet.
pub const LN_WEB: &str = "https://strike.me/@loustrikes";
/// Buy a coffee instead.
pub const KOFI: &str = "https://ko-fi.com/tawnyone";
const LNURLP: &str = "https://strike.me/.well-known/lnurlp/loustrikes";
const PRICE: &str = "https://api.coinbase.com/v2/prices/BTC-USD/spot";
/// The relays the receipt is published to and heard on (as tip.js).
const RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.snort.social",
];
/// How long a receipt is waited for.
const LISTEN: Duration = Duration::from_mins(20);
const USD: [f64; 3] = [2.0, 5.0, 10.0];
/// The pills when the price can't be had: sats outright.
const SATS_IF_OFFLINE: [u64; 3] = [2000, 5000, 10000];
/// The largest answer read from the tip service or the price API.
const MAX_BODY: usize = 64 * 1024;
/// The longest invoice shown (a QR still reads at this length).
const MAX_INVOICE: usize = 2000;
/// The "Other" pill.
const OTHER: usize = 3;

/// A tip: dollars (when the price is known) and what it is in sats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Amount {
    pub usd: Option<f64>,
    pub sats: u64,
}

/// What proves the tip landed: the receipt's signer, our tag, the invoice.
#[derive(Debug, Clone)]
pub struct Watch {
    invoice: String,
    tag: String,
    signer: String,
}

/// An invoice, and when it was asked for as a zap, how to hear it paid.
#[derive(Debug)]
pub struct Invoice {
    pub pr: String,
    pub watch: Option<Watch>,
}

// ---- amounts ----------------------------------------------------------------

/// Dollars in sats at `price` dollars a bitcoin; at least one.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 1..=1e12 first"
)]
pub fn to_sats(usd: f64, price: f64) -> u64 {
    (usd / price * 1e8).round().clamp(1.0, 1e12) as u64
}

/// Pill `i` (0..3): dollars in sats with a price, set sats without.
#[must_use]
pub fn amount_at(i: usize, price: Option<f64>) -> Amount {
    match price {
        Some(p) => Amount {
            usd: Some(USD[i]),
            sats: to_sats(USD[i], p),
        },
        None => Amount {
            usd: None,
            sats: SATS_IF_OFFLINE[i],
        },
    }
}

/// What was typed into "Other": dollars with a price, sats without.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked positive and below 1e12 first"
)]
pub fn other_amount(text: &str, price: Option<f64>) -> Option<Amount> {
    let v: f64 = text.trim().replace(',', ".").parse().ok()?;
    let v = (v * 100.0).round() / 100.0;
    if !(v > 0.0 && v < 1e12) {
        return None;
    }
    Some(match price {
        Some(p) => Amount {
            usd: Some(v),
            sats: to_sats(v, p),
        },
        None => Amount {
            usd: None,
            sats: (v.round() as u64).max(1),
        },
    })
}

/// "1,860 sats" ("1860 sats", "10.000 sats" in Spanish, as the browser does).
#[must_use]
pub fn fmt_sats(n: u64, spanish: bool) -> String {
    let digits = n.to_string();
    let (sep, min) = if spanish { ('.', 5) } else { (',', 4) };
    if digits.len() < min {
        return format!("{digits} sats");
    }
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(d);
    }
    format!("{out} sats")
}

/// "$5", "$3.50".
#[must_use]
pub fn fmt_usd(u: f64) -> String {
    if u.fract() == 0.0 {
        format!("${u:.0}")
    } else {
        format!("${u:.2}")
    }
}

/// An amount's big line and small line, as a pill and the thank-you show it.
fn lines(a: Amount) -> (String, String) {
    let es = crate::tr::spanish();
    match a.usd {
        Some(u) => (fmt_usd(u), fmt_sats(a.sats, es)),
        None => (fmt_sats(a.sats, es), String::new()),
    }
}

// ---- the tip service ----------------------------------------------------------

fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(3))
        .https_only(true)
        .build()
        .context("starting the web client")
}

async fn get_json(client: &Client, url: &str) -> Result<Value> {
    let mut resp = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        ensure!(body.len() + chunk.len() <= MAX_BODY, "answer too long");
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Dollars a bitcoin, or `None`.
async fn price(client: &Client) -> Option<f64> {
    let v = get_json(client, PRICE).await.ok()?;
    let p: f64 = v["data"]["amount"].as_str()?.parse().ok()?;
    (p.is_finite() && p > 0.0).then_some(p)
}

/// An invoice's amount in millisats, from its human-readable part
/// (`lnbc<amount><multiplier>1…`); `None` if it names none.
#[must_use]
pub fn invoice_msats(pr: &str) -> Option<u64> {
    let lower = pr.to_ascii_lowercase();
    let hrp = &lower[..lower.rfind('1')?];
    let rest = hrp.strip_prefix("lnbc")?;
    let mult = rest.chars().last()?;
    let digits = &rest[..rest.len() - mult.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    // One bitcoin is 1e11 millisats.
    match mult {
        'm' => n.checked_mul(100_000_000),
        'u' => n.checked_mul(100_000),
        'n' => n.checked_mul(100),
        'p' => n.is_multiple_of(10).then_some(n / 10),
        _ => None,
    }
}

/// The invoice in an LNURL answer, if it is one for `msats`.
fn bolt11(v: &Value, msats: u64) -> Option<String> {
    let pr = v["pr"].as_str()?;
    let ok = pr.len() <= MAX_INVOICE
        && pr.get(..4).is_some_and(|h| h.eq_ignore_ascii_case("lnbc"))
        && pr.bytes().all(|b| b.is_ascii_alphanumeric())
        && invoice_msats(pr) == Some(msats);
    ok.then(|| pr.to_owned())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A throwaway key for one tip's zap request.
fn throwaway() -> Result<([u8; 32], String)> {
    for _ in 0..8 {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| anyhow!("random: {e}"))?;
        if let Ok(pk) = public_key(&secret) {
            return Ok((secret, pk));
        }
    }
    Err(anyhow!("no usable key"))
}

/// An invoice for `sats`, asked for as a zap when the service takes them.
///
/// # Errors
/// When the service can't be reached or answers with no usable invoice.
pub async fn invoice_for(client: &Client, sats: u64) -> Result<Invoice> {
    let pay = get_json(client, LNURLP).await?;
    let msats = sats.checked_mul(1000).context("too much")?;
    let callback = pay["callback"]
        .as_str()
        .filter(|c| c.starts_with("https://"))
        .context("no callback")?;
    let min = pay["minSendable"].as_f64().unwrap_or(1000.0);
    let max = pay["maxSendable"].as_f64().unwrap_or(f64::MAX);
    #[expect(clippy::cast_precision_loss, reason = "a range check")]
    let m = msats as f64;
    ensure!(m >= min && m <= max, "amount out of range");
    let mut base = Url::parse(callback)?;
    base.query_pairs_mut()
        .append_pair("amount", &msats.to_string());
    if pay["allowsNostr"] == true
        && let Some(signer) = pay["nostrPubkey"].as_str().filter(|k| is_hex64(k))
    {
        let (secret, tag) = throwaway()?;
        let mut relays = vec!["relays".to_owned()];
        relays.extend(RELAYS.iter().map(|r| (*r).to_owned()));
        let zap = sign(
            Template {
                kind: 9734,
                tags: vec![
                    relays,
                    vec!["amount".into(), msats.to_string()],
                    vec!["p".into(), tag.clone()],
                ],
                content: "A tip for Tawny ⚡".into(),
                created_at: now_secs(),
            },
            &secret,
        )?;
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("nostr", &serde_json::to_string(&zap)?);
        // A refused zap falls back to a plain invoice, as the page does.
        if let Ok(v) = get_json(client, url.as_str()).await
            && let Some(pr) = bolt11(&v, msats)
        {
            return Ok(Invoice {
                watch: Some(Watch {
                    invoice: pr.clone(),
                    tag,
                    signer: signer.to_ascii_lowercase(),
                }),
                pr,
            });
        }
    }
    let v = get_json(client, base.as_str()).await?;
    let pr = bolt11(&v, msats).with_context(|| {
        v["reason"]
            .as_str()
            .unwrap_or("no invoice")
            .chars()
            .take(120)
            .collect::<String>()
    })?;
    Ok(Invoice { pr, watch: None })
}

/// The paid receipt for this invoice: right kind, right signer, our tag, our
/// invoice, and a valid id and signature.
#[must_use]
pub fn is_receipt(ev: &Value, watch: &Watch) -> bool {
    let Ok(ev) = serde_json::from_value::<Event>(ev.clone()) else {
        return false;
    };
    ev.kind == 9735
        && ev.sig.is_some()
        && ev.pubkey.eq_ignore_ascii_case(&watch.signer)
        && ev.tag_values("p").next() == Some(watch.tag.as_str())
        && ev
            .tag_values("bolt11")
            .next()
            .is_some_and(|b| b.eq_ignore_ascii_case(&watch.invoice))
        && ev.verify().is_ok()
}

/// Ends the relay listeners with the listen that started them.
struct Listeners(Vec<JoinHandle<()>>);

impl Drop for Listeners {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

/// Whether the receipt for `watch` shows up on any relay within the time.
async fn listen(watch: Watch) -> bool {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let since = now_secs().saturating_sub(120);
    let _listeners = Listeners(
        RELAYS
            .iter()
            .map(|url| tokio::spawn(hear(url, watch.clone(), since, tx.clone())))
            .collect(),
    );
    drop(tx);
    tokio::time::timeout(LISTEN, rx.recv())
        .await
        .is_ok_and(|got| got.is_some())
}

/// One relay, asked for receipts; back on after a drop (5 s … 60 s).
async fn hear(url: &'static str, watch: Watch, since: u64, tx: tokio::sync::mpsc::Sender<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let req =
        json!(["REQ", "tip", { "kinds": [9735], "#p": [watch.tag], "since": since }]).to_string();
    let mut drops = 0u32;
    loop {
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(url).await
            && ws.send(Message::text(req.clone())).await.is_ok()
        {
            if std::env::var_os("TAWNY_DEV").is_some() {
                eprintln!("tawny: tip: listening on {url}");
            }
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                if text.len() > MAX_BODY {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v[0] == "EVENT"
                    && is_receipt(&v[2], &watch)
                {
                    let _ = tx.send(()).await;
                    return;
                }
            }
        }
        drops += 1;
        let wait = (5u64 << (drops - 1).min(4)).min(60);
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// The developer's check of the real service (`TAWNY_DEV=1 tawny
/// --tip-check`): the price, then a zap invoice for 1000 sats, and a minute
/// on the relays (the receipt never comes: nothing is paid). Prints what it
/// got; keeps nothing.
///
/// # Errors
/// When the service gives no usable invoice.
pub fn check() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = client()?;
        match price(&client).await {
            Some(p) => println!(
                "price ${p:.0} a bitcoin; $5 is {}",
                fmt_sats(to_sats(5.0, p), false)
            ),
            None => println!("price: none (the pills fall back to sats)"),
        }
        let inv = invoice_for(&client, 1000).await?;
        println!(
            "invoice {}… ({} chars) for {:?} msats; receipt {}",
            inv.pr.get(..24).unwrap_or_default(),
            inv.pr.len(),
            invoice_msats(&inv.pr),
            if inv.watch.is_some() {
                "listened for (zap)"
            } else {
                "not available (plain invoice)"
            }
        );
        if let Some(w) = inv.watch {
            let paid = tokio::time::timeout(Duration::from_secs(60), listen(w)).await;
            println!(
                "a minute on the relays: {}",
                if paid.is_ok() {
                    "a receipt?!"
                } else {
                    "no receipt, as expected"
                }
            );
        }
        Ok(())
    })
}

/// Whether an error is this computer being offline (not the service).
fn offline(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<reqwest::Error>())
        .any(reqwest::Error::is_connect)
}

// ---- the QR -------------------------------------------------------------------

/// The invoice as a QR that looks like Tawny's (tip.js `qrSvg`): round dots
/// in warm ink on white, soft pupils in the three finder eyes, and a patch in
/// the middle left clear for the bolt. The eyes' outer rings stay square:
/// rounded ones (as the web page draws them) stopped the code reading back
/// in testing, even slightly rounded, and a code that won't scan is a tip
/// that never comes. Level M, a four-module
/// quiet zone, dots filling 92% of their cell. Drawn `px` pixels across,
/// the size it is shown at, so the screen shows it without rescaling.
#[must_use]
pub fn qr_image(text: &str, px: u32) -> Option<Image> {
    let pm = qr_pixmap(text, px)?;
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(pm.data(), pm.width(), pm.height());
    Some(Image::from_rgba8_premultiplied(buf))
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "module counts are small (at most 177)"
)]
fn qr_pixmap(text: &str, px: u32) -> Option<Pixmap> {
    use qrcode::{Color as Module, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    let n = code.width();
    let dark = code.to_colors();
    let q = 4usize;
    let size = n + 2 * q;
    let px = px.clamp(64, 4096);
    let cell = px as f32 / size as f32;
    let mut pm = Pixmap::new(px, px)?;
    pm.fill(Color::WHITE);
    let mut ink = Paint::default();
    ink.set_color_rgba8(0x2b, 0x24, 0x20, 0xff);
    ink.anti_alias = true;
    let eyes = [(0, 0), (0, n - 7), (n - 7, 0)];
    let in_eye = |r: usize, c: usize| {
        eyes.iter()
            .any(|&(er, ec)| r >= er && r < er + 7 && c >= ec && c < ec + 7)
    };
    let hole = ((n as f32 * 0.16).ceil() as usize) | 1;
    let h0 = (n - hole) / 2;
    let in_hole = |r: usize, c: usize| r >= h0 && r < h0 + hole && c >= h0 && c < h0 + hole;
    let mut dots = PathBuilder::new();
    for r in 0..n {
        for c in 0..n {
            if dark[r * n + c] != Module::Dark || in_eye(r, c) || in_hole(r, c) {
                continue;
            }
            let x = (c + q) as f32 + 0.04;
            let y = (r + q) as f32 + 0.04;
            round_rect(
                &mut dots,
                x * cell,
                y * cell,
                0.92 * cell,
                0.92 * cell,
                0.28 * cell,
            );
        }
    }
    if let Some(path) = dots.finish() {
        pm.fill_path(&path, &ink, FillRule::Winding, Transform::identity(), None);
    }
    for (er, ec) in eyes {
        let (x, y) = ((ec + q) as f32, (er + q) as f32);
        let mut ring = PathBuilder::new();
        round_rect(
            &mut ring,
            (x + 0.5) * cell,
            (y + 0.5) * cell,
            6.0 * cell,
            6.0 * cell,
            0.0,
        );
        if let Some(path) = ring.finish() {
            let stroke = Stroke {
                width: cell,
                ..Stroke::default()
            };
            pm.stroke_path(&path, &ink, &stroke, Transform::identity(), None);
        }
        let mut pupil = PathBuilder::new();
        round_rect(
            &mut pupil,
            (x + 2.0) * cell,
            (y + 2.0) * cell,
            3.0 * cell,
            3.0 * cell,
            0.95 * cell,
        );
        if let Some(path) = pupil.finish() {
            pm.fill_path(&path, &ink, FillRule::Winding, Transform::identity(), None);
        }
    }
    Some(pm)
}

/// The QR's size on screen, in logical pixels (the plate's inside).
const QR_SIZE: f32 = 224.0;

fn round_rect(path: &mut PathBuilder, left: f32, top: f32, width: f32, height: f32, radius: f32) {
    let rad = radius.min(width / 2.0).min(height / 2.0);
    let (right, bottom) = (left + width, top + height);
    path.move_to(left + rad, top);
    path.line_to(right - rad, top);
    path.quad_to(right, top, right, top + rad);
    path.line_to(right, bottom - rad);
    path.quad_to(right, bottom, right - rad, bottom);
    path.line_to(left + rad, bottom);
    path.quad_to(left, bottom, left, bottom - rad);
    path.line_to(left, top + rad);
    path.quad_to(left, top, left + rad, top);
    path.close();
}

// ---- the page -------------------------------------------------------------------

/// The page's state, on the window's thread.
struct State {
    ui: Weak<AppWindow>,
    rt: Handle,
    client: Option<Client>,
    price: Option<f64>,
    asked_price: bool,
    /// Bumped whenever what is on the page changes; late answers for an
    /// older one are dropped.
    ticket: u64,
    listener: Option<JoinHandle<()>>,
    invoice: String,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Hops asked of the owlet on the plate (one per amount step), taken by
    /// the scene driver.
    static FLUTTER: Cell<u8> = const { Cell::new(0) };
    /// The tip landed: the thank-you's trio cheers.
    static CHEER: Cell<bool> = const { Cell::new(false) };
}

/// Hops asked of the owlet since last asked (0: none).
pub fn take_flutter() -> u8 {
    FLUTTER.with(|f| f.replace(0))
}

/// Whether the thank-you's trio should cheer now.
pub fn take_cheer() -> bool {
    CHEER.with(|c| c.replace(false))
}

fn with(f: impl FnOnce(&mut State, &AppWindow)) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        let Some(ui) = st.ui.upgrade() else { return };
        f(st, &ui);
    });
}

/// Opens a page in the browser (a fixed https address, never one from afar).
pub fn open_web(url: &'static str) {
    #[cfg(target_os = "windows")]
    let child = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let child = std::process::Command::new("xdg-open").arg(url).spawn();
    match child {
        // Waited for off the window's thread, so it leaves no zombie.
        Ok(mut c) => {
            std::thread::spawn(move || c.wait());
        }
        Err(e) => eprintln!("tawny: opening {url}: {e}"),
    }
}

/// Wires the Support sheet's Lightning page.
pub fn attach(ui: &AppWindow, rt: Handle) {
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ui: ui.as_weak(),
            rt,
            client: client().map_err(|e| eprintln!("tawny: {e:#}")).ok(),
            price: None,
            asked_price: false,
            ticket: 0,
            listener: None,
            invoice: String::new(),
        });
    });
    ui.on_open_kofi(|| open_web(KOFI));
    ui.on_open_lightning(|| with(show_lightning));
    let tip = ui.global::<Tip>();
    tip.on_choose(|i| {
        with(|st, ui| match usize::try_from(i) {
            Ok(OTHER) => {
                stop(st, ui);
                let tip = ui.global::<Tip>();
                tip.set_pick(i);
                tip.set_other_made(SharedString::new());
                tip.set_state("empty".into());
            }
            Ok(i) if i < OTHER => choose_pill(st, ui, i),
            _ => {}
        });
    });
    tip.on_other(|text| {
        with(|st, ui| {
            let amount = other_amount(&text, st.price);
            choose(st, ui, OTHER, amount);
        });
    });
    tip.on_retry(|| {
        with(|st, ui| {
            let pick = usize::try_from(ui.global::<Tip>().get_pick()).unwrap_or(0);
            if pick < OTHER {
                choose_pill(st, ui, pick);
            } else {
                ui.global::<Tip>().set_state("empty".into());
            }
        });
    });
    tip.on_copy(|| {
        with(|st, ui| {
            let tip = ui.global::<Tip>();
            let ok = arboard::Clipboard::new()
                .and_then(|mut c| c.set_text(st.invoice.clone()))
                .is_ok();
            tip.set_copied(ok);
            tip.set_copy_failed(!ok);
            let w = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(1800), move || {
                if let Some(ui) = w.upgrade() {
                    ui.global::<Tip>().set_copied(false);
                    ui.global::<Tip>().set_copy_failed(false);
                }
            });
        });
    });
    tip.on_strike(|| open_web(LN_WEB));
    tip.on_back(|| {
        with(|st, ui| {
            stop(st, ui);
            ui.global::<Tip>().set_page("hub".into());
        });
    });
    tip.on_close(|| {
        with(|st, ui| {
            stop(st, ui);
            ui.set_sheet(SharedString::new());
        });
    });
}

/// The sheet changed: away from Support, nothing is left listening, and it
/// opens on its hub next time.
pub fn sheet_changed(sheet: &str) {
    with(|st, ui| {
        if sheet != "tip" {
            stop(st, ui);
        }
        ui.global::<Tip>().set_page("hub".into());
    });
}

fn stop(st: &mut State, ui: &AppWindow) {
    st.ticket += 1;
    if let Some(l) = st.listener.take() {
        l.abort();
    }
    ui.global::<Tip>().set_waiting(false);
}

fn draw_amounts(st: &State, ui: &AppWindow) {
    let pills: Vec<TipAmount> = (0..OTHER)
        .map(|i| {
            let (big, small) = lines(amount_at(i, st.price));
            TipAmount {
                big: big.into(),
                small: small.into(),
            }
        })
        .collect();
    let tip = ui.global::<Tip>();
    tip.set_amounts(ModelRc::new(VecModel::from(pills)));
    tip.set_currency(if st.price.is_some() { "$" } else { "sats" }.into());
}

fn show_lightning(st: &mut State, ui: &AppWindow) {
    stop(st, ui);
    let tip = ui.global::<Tip>();
    tip.set_page("pick".into());
    tip.set_pick(0);
    tip.set_other_made(SharedString::new());
    tip.set_offline(false);
    tip.set_state("loading".into());
    draw_amounts(st, ui);
    if st.price.is_some() || st.asked_price {
        choose_pill(st, ui, 0);
        return;
    }
    // The price first, so the pills say dollars; then the $2 code.
    st.asked_price = true;
    let Some(client) = st.client.clone() else {
        choose_pill(st, ui, 0);
        return;
    };
    let mine = st.ticket;
    st.rt.spawn(async move {
        let p = price(&client).await;
        let _ = slint::invoke_from_event_loop(move || {
            with(|st, ui| {
                st.price = p;
                draw_amounts(st, ui);
                if st.ticket == mine && ui.global::<Tip>().get_page() == "pick" {
                    choose_pill(st, ui, 0);
                }
            });
        });
    });
}

/// Pill `i` (0..3) picked: a code for its amount.
fn choose_pill(st: &mut State, ui: &AppWindow, i: usize) {
    let amount = amount_at(i, st.price);
    choose(st, ui, i, Some(amount));
}

/// Pill `i` picked with `amount` (`None`: "Other" with nothing usable
/// typed): a code for it.
fn choose(st: &mut State, ui: &AppWindow, i: usize, amount: Option<Amount>) {
    stop(st, ui);
    let tip = ui.global::<Tip>();
    tip.set_pick(i32::try_from(i).unwrap_or(0));
    tip.set_other_made(SharedString::new());
    tip.set_offline(false);
    // The owlet on the plate answers the amount: one hop a step.
    FLUTTER.with(|f| f.set(u8::try_from(i + 1).unwrap_or(1)));
    let Some(amount) = amount else {
        tip.set_state("empty".into());
        return;
    };
    let Some(client) = st.client.clone() else {
        tip.set_state("error".into());
        return;
    };
    tip.set_state("loading".into());
    let mine = st.ticket;
    st.rt.spawn(async move {
        let got = invoice_for(&client, amount.sats).await;
        let _ = slint::invoke_from_event_loop(move || {
            with(|st, ui| {
                if st.ticket == mine {
                    show_invoice(st, ui, i, amount, got);
                }
            });
        });
    });
}

fn show_invoice(st: &mut State, ui: &AppWindow, i: usize, amount: Amount, got: Result<Invoice>) {
    let tip = ui.global::<Tip>();
    let inv = match got {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("tawny: tip: invoice failed: {e:#}");
            tip.set_offline(offline(&e));
            tip.set_state("error".into());
            return;
        }
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a few hundred pixels"
    )]
    let px = (QR_SIZE * ui.window().scale_factor()).round() as u32;
    let Some(qr) = qr_image(&format!("lightning:{}", inv.pr).to_ascii_uppercase(), px) else {
        tip.set_state("error".into());
        return;
    };
    tip.set_qr(qr);
    let short = match (
        inv.pr.get(..20),
        inv.pr.get(inv.pr.len().saturating_sub(8)..),
    ) {
        (Some(a), Some(b)) => format!("{a}…{b}"),
        _ => inv.pr.clone(),
    };
    tip.set_code(short.into());
    if i == OTHER {
        tip.set_other_made(lines(amount).0.into());
    }
    st.invoice = inv.pr;
    tip.set_state("qr".into());
    if let Some(watch) = inv.watch {
        tip.set_waiting(true);
        let mine = st.ticket;
        st.listener = Some(st.rt.spawn(async move {
            if listen(watch).await {
                let _ = slint::invoke_from_event_loop(move || {
                    with(|st, ui| {
                        if st.ticket == mine {
                            landed(st, ui, amount);
                        }
                    });
                });
            }
        }));
    }
}

fn landed(st: &mut State, ui: &AppWindow, amount: Amount) {
    st.listener = None;
    let tip = ui.global::<Tip>();
    tip.set_waiting(false);
    let (big, small) = lines(amount);
    tip.set_big_amount(big.into());
    tip.set_big_sats(small.into());
    tip.set_page("thanks".into());
    CHEER.with(|c| c.set(true));
    eprintln!("tawny: tip landed");
    bell();
}

/// A soft bell as it lands: the Monitor's own chime, kept quiet, on a
/// speaker opened for it alone.
fn bell() {
    let Some(clip) = crate::chime::samples("bell") else {
        return;
    };
    let quiet: Vec<i16> = clip
        .iter()
        .map(|s| i16::try_from(i32::from(*s) * 45 / 100).unwrap_or(0))
        .collect();
    let mix = std::sync::Arc::<crate::audio::Mix>::default();
    let Ok(speaker) = crate::audio::Speaker::open(mix.clone()) else {
        return;
    };
    crate::audio::play_clip(&mix, &quiet);
    slint::Timer::single_shot(Duration::from_secs(4), move || drop(speaker));
}

#[cfg(test)]
mod tests {
    use super::{
        Amount, Watch, amount_at, fmt_sats, fmt_usd, invoice_msats, is_receipt, other_amount,
        qr_pixmap,
    };
    use serde_json::{Value, json};
    use tawny_core::nostr::event::{Template, public_key, sign};

    #[test]
    fn pills_say_dollars_with_a_price_and_sats_without() {
        let a = amount_at(1, Some(100_000.0));
        assert_eq!(
            a,
            Amount {
                usd: Some(5.0),
                sats: 5000
            }
        );
        assert_eq!(
            amount_at(0, None),
            Amount {
                usd: None,
                sats: 2000
            }
        );
    }

    #[test]
    fn other_reads_what_was_typed() {
        assert_eq!(
            other_amount("3,50", Some(100_000.0)).map(|a| a.sats),
            Some(3500)
        );
        assert_eq!(other_amount(" 5000 ", None).map(|a| a.sats), Some(5000));
        assert_eq!(other_amount("0", None), None);
        assert_eq!(other_amount("-2", None), None);
        assert_eq!(other_amount("lots", None), None);
        assert_eq!(other_amount("NaN", None), None);
        assert_eq!(other_amount("1e30", None), None);
    }

    #[test]
    fn amounts_read_as_the_page_shows_them() {
        assert_eq!(fmt_sats(1860, false), "1,860 sats");
        assert_eq!(fmt_sats(940, false), "940 sats");
        assert_eq!(fmt_sats(1_234_567, false), "1,234,567 sats");
        assert_eq!(fmt_sats(4651, true), "4651 sats");
        assert_eq!(fmt_sats(10_000, true), "10.000 sats");
        assert_eq!(fmt_usd(5.0), "$5");
        assert_eq!(fmt_usd(3.5), "$3.50");
    }

    #[test]
    fn an_invoice_names_its_amount() {
        assert_eq!(invoice_msats("lnbc46530n1p5qqq"), Some(4_653_000));
        assert_eq!(invoice_msats("LNBC20U1PXYZ"), Some(2_000_000));
        assert_eq!(invoice_msats("lnbc2500p1abc"), Some(250));
        assert_eq!(invoice_msats("lnbc2501p1abc"), None);
        assert_eq!(invoice_msats("lnbc1pvjluez"), None, "no amount");
        assert_eq!(invoice_msats("lntb20u1abc"), None, "testnet");
    }

    #[test]
    fn only_the_signed_receipt_for_this_invoice_counts() {
        let strike = [7u8; 32];
        let signer = public_key(&strike).expect("key");
        let watch = Watch {
            invoice: "lnbc20u1abc".into(),
            tag: "ab".repeat(32),
            signer: signer.clone(),
        };
        let receipt = |kind, p: &str, bolt: &str| {
            let ev = sign(
                Template {
                    kind,
                    tags: vec![
                        vec!["p".into(), p.into()],
                        vec!["bolt11".into(), bolt.into()],
                    ],
                    content: String::new(),
                    created_at: 1_800_000_000,
                },
                &strike,
            )
            .expect("signs");
            serde_json::to_value(ev).expect("json")
        };
        let tag = watch.tag.clone();
        assert!(is_receipt(&receipt(9735, &tag, "LNBC20U1ABC"), &watch));
        assert!(
            !is_receipt(&receipt(9734, &tag, "lnbc20u1abc"), &watch),
            "kind"
        );
        assert!(
            !is_receipt(&receipt(9735, &"cd".repeat(32), "lnbc20u1abc"), &watch),
            "tag"
        );
        assert!(
            !is_receipt(&receipt(9735, &tag, "lnbc50u1abc"), &watch),
            "invoice"
        );
        let mut forged = receipt(9735, &tag, "lnbc20u1abc");
        forged["content"] = json!("changed");
        assert!(!is_receipt(&forged, &watch), "id");
        let mut unsigned = receipt(9735, &tag, "lnbc20u1abc");
        unsigned.as_object_mut().expect("object").remove("sig");
        assert!(!is_receipt(&unsigned, &watch), "signature");
        let other = Watch {
            signer: public_key(&[9u8; 32]).expect("key"),
            ..watch
        };
        assert!(
            !is_receipt(&receipt(9735, &tag, "lnbc20u1abc"), &other),
            "signer"
        );
    }

    /// The drawn code reads back. Checked from 2x the size it is shown at
    /// (rqrr, unlike a phone's scanner, misses codes at ~3 px a module even
    /// when they are plain), across sizes so no scale is lucky.
    #[test]
    fn the_qr_reads_back_at_its_size() {
        let pr = format!(
            "LIGHTNING:LNBC46530N1P5{}CQZZSXQYZ5VQ",
            "QQQSYQCYQ5RQWZQFQYPQHQ4KGJ7VZX3R8M2D5W9C6N0T".repeat(7)
        );
        for px in [448u32, 560, 672, 784, 896] {
            let pm = qr_pixmap(&pr, px).expect("encodes");
            let w = usize::try_from(pm.width()).expect("fits");
            let data = pm.data().to_vec();
            let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, w, |x, y| {
                let p = &data[(y * w + x) * 4..];
                u8::try_from((u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2])) / 3).unwrap_or(0)
            });
            let grids = img.detect_grids();
            assert_eq!(grids.len(), 1, "one code at {px}px");
            let (_, text) = grids[0].decode().expect("decodes");
            assert_eq!(text, pr, "at {px}px");
        }
    }

    /// A receipt a real wallet published (the Android app's `TipZapTest` uses
    /// the same one): taken as it is, refused for another invoice, another
    /// signer, or a changed body.
    #[test]
    fn a_real_receipt_is_accepted_and_forgeries_are_not() {
        let ev: Value =
            serde_json::from_str(include_str!("../tests/fixtures/zap-receipt.json")).expect("json");
        let tag = |n: &str| -> String {
            ev["tags"]
                .as_array()
                .and_then(|t| t.iter().find(|t| t[0] == n))
                .and_then(|t| t[1].as_str())
                .expect("tag")
                .to_owned()
        };
        let watch = Watch {
            invoice: tag("bolt11"),
            tag: tag("p"),
            signer: ev["pubkey"].as_str().expect("pubkey").to_owned(),
        };
        assert!(is_receipt(&ev, &watch), "the real receipt");
        let other = Watch {
            invoice: "lnbc1other".into(),
            ..watch.clone()
        };
        assert!(!is_receipt(&ev, &other), "another invoice");
        let stranger = Watch {
            signer: "00".repeat(32),
            ..watch.clone()
        };
        assert!(!is_receipt(&ev, &stranger), "another signer");
        let mut changed = ev.clone();
        changed["content"] = Value::from("changed");
        assert!(!is_receipt(&changed, &watch), "a changed body");
    }
}
