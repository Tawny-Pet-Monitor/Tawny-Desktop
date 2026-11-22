//! Kotlin and Swift bindings for tawny-core.
//!
//! The app gets a [`MonitorHandle`] (a running Monitor), a
//! [`MonitorListener`] it implements to hear what happens, and a few
//! helpers to mint secrets and read pairing links. On Android, camera frames
//! skip the generated bindings: a direct JNI call
//! (`com.tawny.core.NativeBridge.pushFrame`) reads the camera's planes in
//! place, with no copy on the Kotlin side. Elsewhere,
//! [`MonitorHandle::push_frame`] takes them as byte arrays.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use libwebrtc::peer_connection_factory::IceServer;
use tawny_core::call::media::MediaConfig;
use tawny_core::call::policy::PairGate;
use tawny_core::engine::monitor::{Monitor, MonitorConfig, MonitorEvent};
use tawny_core::ids::{Secret, SecretKind};
use tawny_core::link::{self, Anchors};
use tawny_core::nostr::event;
use tawny_core::relay::health::HealthBook;
use tokio::runtime::Runtime;

#[cfg(target_os = "android")]
mod android;
mod frames;
mod record;
mod sound;
mod video;
mod watch;

pub use watch::{WatchHandle, WatchListener, WatchSettings};

uniffi::setup_scaffolding!();

/// What went wrong, with a message for the diagnostics screen.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum TawnyError {
    /// The Monitor could not start.
    #[error("{0}")]
    Start(String),
    /// A setting was malformed.
    #[error("{0}")]
    BadSetting(String),
    /// A pairing link was refused.
    #[error("{0}")]
    Link(String),
    /// The system's random number generator failed.
    #[error("{0}")]
    Random(String),
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub(crate) fn runtime() -> Result<&'static Runtime, TawnyError> {
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("tawny-core")
        .on_thread_start(on_thread_start)
        .enable_all()
        .build()
        .map_err(|e| TawnyError::Start(format!("no async runtime: {e}")))?;
    // Another thread may have won the race; theirs is kept and ours dropped.
    let _ = RUNTIME.set(rt);
    RUNTIME
        .get()
        .ok_or_else(|| TawnyError::Start("no async runtime".into()))
}

/// Every runtime thread (workers and the blocking pool) starts attached to
/// the JVM for good, on Android. Two libraries would otherwise take turns:
/// JNA attaches a thread for a listener callback and detaches it afterwards,
/// while libwebrtc keeps the thread's `JNIEnv` in its own thread-local slot
/// once it has attached it (to release a hardware decoder's frame), and
/// aborts the process when it finds that slot set on a detached thread.
/// Attached up front, both see a JVM thread and leave it alone.
pub(crate) fn on_thread_start() {
    #[cfg(target_os = "android")]
    android::attach_thread();
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A STUN or TURN server.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IceServerSpec {
    /// `stun:` or `turn:` URLs.
    pub urls: Vec<String>,
    /// TURN user name (empty for STUN).
    pub username: String,
    /// TURN password (empty for STUN).
    pub password: String,
}

/// How to run a Monitor. The app stores all of it between runs; the JSON
/// fields are whatever [`MonitorListener`] last handed over (empty for none).
#[derive(Debug, Clone, uniffi::Record)]
pub struct MonitorSettings {
    /// The channel key.
    pub channel_key: String,
    /// The room's name, shown to Viewers.
    pub room_name: String,
    /// The rendezvous admission ticket.
    pub ticket: Option<String>,
    /// This Monitor's Nostr secret key, 64 hex characters.
    pub nostr_secret: Option<String>,
    /// Nostr relays (`wss://…`); empty for none.
    pub nostr_relays: Vec<String>,
    /// The relay health book, as JSON.
    pub nostr_health: String,
    /// Handheld Nostr keys that have joined before.
    pub nostr_known: Vec<String>,
    /// The internet rendezvous (`wss://host`), if any.
    pub rendezvous: Option<String>,
    /// The LAN relay's port (0 picks one); `None` for no LAN relay.
    pub lan_port: Option<u16>,
    /// This device's address on the Wi-Fi, for the pairing link.
    pub lan_ip: Option<String>,
    /// The pairing gate, as JSON.
    pub gate: String,
    /// Send the microphone.
    pub mic: bool,
    /// Camera frame width.
    pub width: u32,
    /// Camera frame height.
    pub height: u32,
    /// STUN and TURN servers.
    pub ice_servers: Vec<IceServerSpec>,
}

/// What a running Monitor tells the app. Calls arrive on one background
/// thread, in order; return quickly.
#[uniffi::export(with_foreign)]
pub trait MonitorListener: Send + Sync {
    /// Start (`true`) or stop the camera. It is wanted only while a Viewer is connected.
    fn on_capture(&self, on: bool);
    /// A Viewer was let in: its id, device, person's name, and its bond id
    /// when it sent one (stable across its reconnects).
    fn on_joined(&self, id: String, label: String, name: String, pid: Option<String>);
    /// A Viewer's call connected.
    fn on_connected(&self, id: String);
    /// The security code for a Viewer; `None` means show a warning instead.
    fn on_security_code(&self, id: String, code: Option<String>);
    /// A Viewer left.
    fn on_left(&self, id: String);
    /// Viewer `id`'s own camera came on the screen (its upright size) or
    /// went off (`0, 0`). Draw it by attaching a surface, as a Viewer does.
    fn on_viewer_video(&self, id: String, width: u32, height: u32);
    /// Store this as the pairing gate (`MonitorSettings::gate`).
    fn on_gate(&self, json: String);
    /// Store these as the known Handheld keys (`MonitorSettings::nostr_known`).
    fn on_nostr_known(&self, keys: Vec<String>);
    /// Store this as the relay health book (`MonitorSettings::nostr_health`).
    fn on_relay_health(&self, json: String);
    /// A control frame from a Viewer (`talking`, `torch`, `camera-control`, …), as JSON.
    fn on_control(&self, json: String);
    /// A way in came up or went down: `lan`, `nostr` or `rendezvous`.
    fn on_transport(&self, name: String, up: bool);
    /// A diagnostics line.
    fn on_log(&self, line: String);
}

fn deliver(rt: &Runtime, l: &Arc<dyn MonitorListener>, e: MonitorEvent) {
    match e {
        MonitorEvent::TalkAudio { id, track } => sound::listen(rt, id, track),
        MonitorEvent::ViewerVideo { id, track } => {
            let l = Arc::clone(l);
            let who = id.clone();
            video::feature(
                rt,
                id,
                track,
                Arc::new(move |w, h| l.on_viewer_video(who.clone(), w, h)),
            );
        }
        MonitorEvent::Capture(on) => l.on_capture(on),
        MonitorEvent::Joined {
            id,
            label,
            name,
            pid,
        } => l.on_joined(id, label, name, pid),
        MonitorEvent::Connected { id } => l.on_connected(id),
        MonitorEvent::Sas { id, code } => l.on_security_code(id, code),
        MonitorEvent::Left { id } => {
            sound::SPEAKER.forget(&id);
            l.on_left(id);
        }
        MonitorEvent::Gate(g) => {
            if let Ok(j) = serde_json::to_string(&g) {
                l.on_gate(j);
            }
        }
        MonitorEvent::NostrKnown(k) => l.on_nostr_known(k),
        MonitorEvent::Health(h) => {
            if let Ok(j) = serde_json::to_string(&h) {
                l.on_relay_health(j);
            }
        }
        MonitorEvent::Control(v) => l.on_control(v.to_string()),
        MonitorEvent::Transport { name, up } => l.on_transport(name.to_owned(), up),
        MonitorEvent::Log(s) => l.on_log(s),
    }
}

pub(crate) fn parse_json<T: serde::de::DeserializeOwned + Default>(
    what: &str,
    s: &str,
) -> Result<T, TawnyError> {
    if s.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(s).map_err(|e| TawnyError::BadSetting(format!("{what}: {e}")))
}

pub(crate) fn secret_bytes(hex_key: &str) -> Result<[u8; 32], TawnyError> {
    let bad = || TawnyError::BadSetting("the Nostr secret is not 64 hex characters".into());
    let v = hex::decode(hex_key).map_err(|_| bad())?;
    <[u8; 32]>::try_from(v.as_slice()).map_err(|_| bad())
}

fn config(s: MonitorSettings) -> Result<MonitorConfig, TawnyError> {
    let gate: PairGate = parse_json("gate", &s.gate)?;
    let nostr_health: HealthBook = parse_json("relay health", &s.nostr_health)?;
    let nostr_secret = s.nostr_secret.as_deref().map(secret_bytes).transpose()?;
    if s.width == 0 || s.height == 0 {
        return Err(TawnyError::BadSetting("the camera size is zero".into()));
    }
    Ok(MonitorConfig {
        channel_key: s.channel_key,
        room_name: s.room_name,
        ticket: s.ticket,
        nostr_secret,
        nostr_relays: s.nostr_relays,
        nostr_health,
        nostr_known: s.nostr_known,
        rendezvous: s.rendezvous,
        lan_bind: s
            .lan_port
            .map(|p| SocketAddr::from((Ipv4Addr::UNSPECIFIED, p))),
        lan_ip: s.lan_ip,
        gate,
        mic: s.mic,
        width: s.width,
        height: s.height,
        media: MediaConfig {
            ice_servers: s
                .ice_servers
                .into_iter()
                .map(|i| IceServer {
                    urls: i.urls,
                    username: i.username,
                    password: i.password,
                })
                .collect(),
        },
    })
}

/// The pairing code on screen.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairCodeInfo {
    /// The code.
    pub code: String,
    /// When it stops working, epoch milliseconds.
    pub expires_ms: u64,
}

/// One camera image in `YUV_420_888` layout: three planes, each with its
/// row stride, chroma with a pixel stride of 1 (planar) or 2 (interleaved).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The luma plane.
    pub y: Vec<u8>,
    /// Bytes from one luma row to the next.
    pub y_row: u32,
    /// The U plane.
    pub u: Vec<u8>,
    /// The V plane.
    pub v: Vec<u8>,
    /// Bytes from one chroma row to the next.
    pub uv_row: u32,
    /// Bytes from one chroma sample to the next in a row.
    pub uv_pixel: u32,
    /// Degrees clockwise to turn the image upright (0, 90, 180, 270).
    pub rotation: i32,
}

/// A running Monitor. Dropping it stops everything.
#[derive(uniffi::Object)]
pub struct MonitorHandle {
    monitor: Monitor,
}

#[uniffi::export]
impl MonitorHandle {
    /// Starts a Monitor. Blocks until its transports are listening (well
    /// under a second); call it off the main thread.
    ///
    /// # Errors
    /// [`TawnyError::BadSetting`] or [`TawnyError::Start`].
    #[uniffi::constructor]
    pub fn start(
        settings: MonitorSettings,
        listener: Arc<dyn MonitorListener>,
    ) -> Result<Arc<Self>, TawnyError> {
        let cfg = config(settings)?;
        let rt = runtime()?;
        let (monitor, mut rx) = rt
            .block_on(Monitor::start(cfg))
            .map_err(|e| TawnyError::Start(e.to_string()))?;
        if let Ok(mut s) = frames::SOURCE.lock() {
            *s = Some(monitor.video_source());
        }
        if let Ok(mut m) = sound::MIC.lock() {
            *m = monitor.audio_source();
        }
        // Calls into the app happen on a thread of their own, so a slow
        // listener never stalls the engine's.
        let pump = std::thread::Builder::new()
            .name("tawny-events".into())
            .spawn(move || {
                crate::on_thread_start();
                while let Some(e) = rx.blocking_recv() {
                    deliver(rt, &listener, e);
                }
            });
        if let Err(e) = pump {
            monitor.stop();
            return Err(TawnyError::Start(format!("no event thread: {e}")));
        }
        Ok(Arc::new(Self { monitor }))
    }

    /// The pairing link for the QR code, with the current code (a fresh one
    /// if none is on screen).
    ///
    /// # Errors
    /// [`TawnyError::Random`].
    pub fn pair_link(&self) -> Result<String, TawnyError> {
        self.monitor
            .pair_link()
            .map_err(|e| TawnyError::Random(e.to_string()))
    }

    /// Puts a new ten-minute code on screen.
    ///
    /// # Errors
    /// [`TawnyError::Random`].
    pub fn new_code(&self) -> Result<PairCodeInfo, TawnyError> {
        self.monitor
            .new_code()
            .map(|c| PairCodeInfo {
                code: c.code,
                expires_ms: c.expires_ms,
            })
            .map_err(|e| TawnyError::Random(e.to_string()))
    }

    /// Puts the app's own code on screen (the one in the QR it draws); a
    /// phone presenting it before `expires_ms` is let in.
    pub fn set_pair_code(&self, code: &str, expires_ms: u64) {
        self.monitor.set_code(code, expires_ms);
    }

    /// Sends a control frame (`battery`, `meta`, …) to every connected
    /// Viewer, as JSON.
    ///
    /// # Errors
    /// [`TawnyError::BadSetting`] when it isn't a JSON object with a `type`.
    pub fn broadcast(&self, json: &str) -> Result<(), TawnyError> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| TawnyError::BadSetting(format!("broadcast: {e}")))?;
        if !v["type"].is_string() {
            return Err(TawnyError::BadSetting("broadcast: no type".into()));
        }
        self.monitor.broadcast(v);
        Ok(())
    }

    /// Sends a control frame to one Viewer, as JSON.
    ///
    /// # Errors
    /// [`TawnyError::BadSetting`] when it isn't a JSON object with a `type`.
    pub fn send_to(&self, id: &str, json: &str) -> Result<(), TawnyError> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| TawnyError::BadSetting(format!("control: {e}")))?;
        if !v["type"].is_string() {
            return Err(TawnyError::BadSetting("control: no type".into()));
        }
        self.monitor.send_to(id, v);
        Ok(())
    }

    /// Hangs up on one Viewer (it is told `bye`).
    pub fn disconnect(&self, id: &str) {
        self.monitor.disconnect(id);
    }

    /// Dim mode: nobody is looking at this screen, so send a lighter picture.
    pub fn set_dimmed(&self, dimmed: bool) {
        self.monitor.set_dimmed(dimmed);
    }

    /// The LAN relay's port, when it runs.
    #[must_use]
    pub fn lan_port(&self) -> Option<u16> {
        self.monitor.lan_port()
    }

    /// This Monitor's Nostr public key, when Nostr runs.
    #[must_use]
    pub fn nostr_public_key(&self) -> Option<String> {
        self.monitor.nostr_public_key().map(str::to_owned)
    }

    /// Hands over one camera image. Returns whether it was taken (`false`:
    /// this Monitor was stopped, or the planes don't match their strides).
    #[must_use]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the generated bindings hand over owned values"
    )]
    pub fn push_frame(&self, frame: Frame) -> bool {
        frames::push(
            &frames::Planes {
                width: frame.width,
                height: frame.height,
                y: &frame.y,
                y_row: frame.y_row as usize,
                u: &frame.u,
                v: &frame.v,
                uv_row: frame.uv_row as usize,
                uv_pixel: frame.uv_pixel as usize,
            },
            frame.rotation,
        )
    }

    /// Hands over 10 ms of microphone sound: 480 samples, 48 kHz mono.
    /// Returns whether it was taken (`false`: stopped, the microphone is
    /// off, or the wrong length).
    #[must_use]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the generated bindings hand over owned values"
    )]
    pub fn push_mic(&self, samples: Vec<i16>) -> bool {
        runtime().is_ok_and(|rt| sound::push_mic(rt, &samples))
    }

    /// The Viewers' talk-back, mixed, for the app's player: fills `len`
    /// samples (48 kHz mono), silence where nobody is talking.
    #[must_use]
    pub fn read_speaker(&self, len: u32) -> Vec<i16> {
        let mut out = vec![0; len.min(48_000) as usize];
        sound::SPEAKER.read(&mut out);
        out
    }

    /// Hangs up on everyone and stops listening.
    pub fn stop(&self) {
        if let Ok(mut s) = frames::SOURCE.lock() {
            *s = None;
        }
        if let Ok(mut m) = sound::MIC.lock() {
            *m = None;
        }
        sound::SPEAKER.clear();
        self.monitor.stop();
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A new channel key.
///
/// # Errors
/// [`TawnyError::Random`].
#[uniffi::export]
pub fn new_channel_key() -> Result<String, TawnyError> {
    mint(SecretKind::ChannelKey)
}

/// A new rendezvous admission ticket.
///
/// # Errors
/// [`TawnyError::Random`].
#[uniffi::export]
pub fn new_ticket() -> Result<String, TawnyError> {
    mint(SecretKind::Ticket)
}

fn mint(kind: SecretKind) -> Result<String, TawnyError> {
    Secret::new(kind)
        .map(|s| s.expose().to_owned())
        .map_err(|e| TawnyError::Random(e.to_string()))
}

/// A new Nostr secret key, 64 hex characters. Keep it on this device.
///
/// # Errors
/// [`TawnyError::Random`].
#[uniffi::export]
pub fn new_nostr_secret() -> Result<String, TawnyError> {
    // A random 32 bytes is a valid key except with probability ~2^-128;
    // try again rather than ever hand out an invalid one.
    for _ in 0..8 {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| TawnyError::Random(e.to_string()))?;
        if event::public_key(&b).is_ok() {
            return Ok(hex::encode(b));
        }
    }
    Err(TawnyError::Random("no valid key after 8 tries".into()))
}

/// A new bond id: how this phone is known to a Monitor once paired (32 hex).
///
/// # Errors
/// [`TawnyError::Random`].
#[uniffi::export]
pub fn new_bond_id() -> Result<String, TawnyError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| TawnyError::Random(e.to_string()))?;
    Ok(hex::encode(b))
}

/// The Nostr public key for a secret from [`new_nostr_secret`].
///
/// # Errors
/// [`TawnyError::BadSetting`].
#[uniffi::export]
pub fn nostr_public_key(secret: &str) -> Result<String, TawnyError> {
    event::public_key(&secret_bytes(secret)?).map_err(|e| TawnyError::BadSetting(e.to_string()))
}

/// What a pairing link says, once read and vetted.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairingInfo {
    /// `ws://<lan-ip>:<port>`, or `None` for a relay-only pairing.
    pub signal: Option<String>,
    /// The channel key.
    pub key: String,
    /// The room's name, cleaned for display.
    pub name: String,
    /// The rendezvous admission ticket.
    pub token: Option<String>,
    /// The pairing code.
    pub code: Option<String>,
    /// When the code lapses, epoch milliseconds; 0 for no deadline.
    pub expires_at: i64,
    /// Whether the code is stale on any plausible clock.
    pub expired: bool,
    /// The rendezvous the Monitor is on, if the link named one.
    pub relay: Option<String>,
    /// Whether `relay` may be saved without asking.
    pub relay_trusted: bool,
    /// The Monitor's Nostr public key.
    pub nostr_station: Option<String>,
}

/// Reads a scanned or tapped pairing link. `preferred` is the rendezvous the
/// user set and `build` the one this build ships with, each `host[:port]`.
///
/// # Errors
/// [`TawnyError::Link`], with the reason.
#[uniffi::export]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated bindings hand over owned values"
)]
pub fn parse_pair_link(
    raw: &str,
    preferred: Option<String>,
    build: Option<String>,
    has_rendezvous: bool,
) -> Result<PairingInfo, TawnyError> {
    let p = link::parse(
        raw,
        Anchors {
            preferred: preferred.as_deref(),
            build: build.as_deref(),
            has_rendezvous,
        },
    )
    .map_err(|e| TawnyError::Link(e.to_string()))?;
    let expired = p.is_expired(i64::try_from(now_ms()).unwrap_or(i64::MAX));
    Ok(PairingInfo {
        signal: p.signal,
        key: p.key,
        name: p.name,
        token: p.token,
        code: p.code,
        expires_at: p.expires_at,
        expired,
        relay: p.relay,
        relay_trusted: p.relay_trusted,
        nostr_station: p.nostr_station,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> MonitorSettings {
        MonitorSettings {
            channel_key: "Zx9_-abcDEF0123456789".into(),
            room_name: "Den".into(),
            ticket: None,
            nostr_secret: None,
            nostr_relays: vec![],
            nostr_health: String::new(),
            nostr_known: vec![],
            rendezvous: None,
            lan_port: Some(0),
            lan_ip: Some("192.168.1.20".into()),
            gate: String::new(),
            mic: false,
            width: 320,
            height: 240,
            ice_servers: vec![],
        }
    }

    #[test]
    fn minted_secrets_are_usable() {
        let s = new_nostr_secret().expect("mints");
        assert_eq!(s.len(), 64);
        assert_eq!(nostr_public_key(&s).expect("derives").len(), 64);
        assert!(nostr_public_key("zz").is_err());
        assert!(new_channel_key().expect("mints").len() >= 20);
    }

    #[test]
    fn bad_settings_are_named() {
        let mut s = settings();
        s.gate = "{not json".into();
        let Err(TawnyError::BadSetting(m)) = config(s) else {
            panic!("accepted a broken gate")
        };
        assert!(m.starts_with("gate"), "{m}");
        let mut s = settings();
        s.nostr_secret = Some("abc".into());
        assert!(matches!(config(s), Err(TawnyError::BadSetting(_))));
        let mut s = settings();
        s.width = 0;
        assert!(matches!(config(s), Err(TawnyError::BadSetting(_))));
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl MonitorListener for Recorder {
        fn on_capture(&self, on: bool) {
            self.push(format!("capture {on}"));
        }
        fn on_joined(&self, id: String, _: String, _: String, _: Option<String>) {
            self.push(format!("joined {id}"));
        }
        fn on_connected(&self, _: String) {}
        fn on_security_code(&self, _: String, _: Option<String>) {}
        fn on_left(&self, _: String) {}
        fn on_viewer_video(&self, _: String, _: u32, _: u32) {}
        fn on_gate(&self, json: String) {
            self.push(format!("gate {json}"));
        }
        fn on_nostr_known(&self, _: Vec<String>) {}
        fn on_relay_health(&self, _: String) {}
        fn on_control(&self, _: String) {}
        fn on_transport(&self, name: String, up: bool) {
            self.push(format!("{name} {up}"));
        }
        fn on_log(&self, _: String) {}
    }
    impl Recorder {
        fn push(&self, s: String) {
            if let Ok(mut v) = self.0.lock() {
                v.push(s);
            }
        }
    }

    #[test]
    fn a_monitor_starts_hands_out_a_link_that_reads_back_and_stops() {
        let rec = Arc::new(Recorder::default());
        let m = MonitorHandle::start(settings(), rec.clone()).expect("starts");
        let port = m.lan_port().expect("LAN relay is up");
        let link = m.pair_link().expect("link");
        let p = parse_pair_link(&link, None, None, false).expect("reads back");
        assert_eq!(p.key, "Zx9_-abcDEF0123456789");
        assert_eq!(p.name, "Den");
        assert_eq!(p.signal, Some(format!("ws://192.168.1.20:{port}")));
        assert!(p.code.is_some() && !p.expired);
        // The LAN station joins its own relay; the listener hears it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if rec
                .0
                .lock()
                .is_ok_and(|v| v.iter().any(|s| s == "lan true"))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(rec.0.lock().expect("lock").iter().any(|s| s == "lan true"));
        let frame = Frame {
            width: 4,
            height: 2,
            y: vec![0; 8],
            y_row: 4,
            u: vec![128; 2],
            v: vec![128; 2],
            uv_row: 2,
            uv_pixel: 1,
            rotation: 0,
        };
        assert!(
            m.push_frame(frame.clone()),
            "a running Monitor takes frames"
        );
        assert!(
            !m.push_frame(Frame {
                y: vec![0; 3],
                ..frame.clone()
            }),
            "a short plane is dropped"
        );
        m.stop();
        assert!(!m.push_frame(frame), "a stopped one doesn't");
    }

    #[test]
    fn nowhere_to_meet_is_a_start_error() {
        let mut s = settings();
        s.lan_port = None;
        let Err(TawnyError::Start(_)) = MonitorHandle::start(s, Arc::new(Recorder::default()))
        else {
            panic!("started with no way in")
        };
    }

    #[test]
    fn something_else_is_not_a_pairing_link() {
        assert!(matches!(
            parse_pair_link("https://example.com/", None, None, false),
            Err(TawnyError::Link(_))
        ));
    }
}
