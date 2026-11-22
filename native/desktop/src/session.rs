//! A job, once picked, on the Tawny core engine: the Monitor shows its
//! pairing code and lets Viewers in; the Viewer dials its Monitor.
//!
//! The engine runs on a tokio runtime; its events come back to the window's
//! thread through `invoke_from_event_loop`, where the host (one per window,
//! kept in a thread-local) updates the screens and the store.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use libwebrtc::peer_connection_factory::IceServer;
use serde_json::{Value, json};
use slint::{ComponentHandle, Image, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use tawny_core::call::media::MediaConfig;
use tawny_core::call::policy::PairGate;
use tawny_core::engine::monitor::{Monitor, MonitorConfig, MonitorEvent};
use tawny_core::engine::viewer::{Watch, WatchConfig, WatchEvent};
use tawny_core::link::{self, Anchors, Pairing};
use tawny_core::relay::health::HealthBook;
use tokio::runtime::Runtime;

use crate::app::shown_name;
use crate::store::{Channel, Role, Store};
use crate::{AppWindow, Live};

/// The build's Nostr relays, when the user lists none of their own.
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.primal.net",
    "wss://relay.nostr.net",
    "wss://relay.snort.social",
];
/// The build's rendezvous, for when neither the LAN nor Nostr reaches.
pub const DEFAULT_RENDEZVOUS: &str = "wss://tawny-rendezvous.tawny1.workers.dev";
/// The same, as the link parser's anchors name it.
const RENDEZVOUS_HOST: &str = "tawny-rendezvous.tawny1.workers.dev";
/// How this device is named in the Monitor's list.
const DEVICE: &str = if cfg!(windows) {
    "Windows PC"
} else {
    "Linux PC"
};
/// STUN, as every Tawny build.
const STUN: &[&str] = &[
    "stun:stun.cloudflare.com:3478",
    "stun:stun.l.google.com:19302",
];
/// How often the pairing code on screen is checked for running low.
const CODE_CHECK: Duration = Duration::from_secs(20);

/// A Viewer's camera is on the Monitor's stage (its own camera is in the
/// corner meanwhile).
static FEATURED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The window's host; only ever touched on the window's thread.
    static HOST: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
}

/// A Viewer let in to this Monitor.
#[derive(Debug, Clone)]
struct Guest {
    label: String,
    name: String,
    pid: Option<String>,
    connected: bool,
}

/// A running Monitor and what the screens need of it.
struct MonitorRun {
    monitor: Monitor,
    channel_key: String,
    /// The room's name, as Viewers see it.
    room: String,
    guests: BTreeMap<String, Guest>,
    /// The Viewer whose safety code is on screen.
    sas_for: Option<String>,
    link: String,
    _code_timer: Timer,
    /// The camera, while the engine wants frames.
    capture: Option<crate::camera::Capture>,
    /// Which of the computer's cameras.
    camera: u32,
    /// When the last chime played (they are spaced at least 300 ms).
    chimed: Option<std::time::Instant>,
    /// The room's sound, to the Viewers.
    mic: Option<crate::audio::Mic>,
}

/// A running Viewer.
struct WatchRun {
    watch: Watch,
    channel_key: String,
    /// Stops the picture's frame pump.
    stop: Arc<AtomicBool>,
    /// Talk-back, passed on while Talk is held.
    mic: Option<crate::audio::Mic>,
    /// "My camera", while it is on.
    camera: Option<crate::camera::Capture>,
    /// When a newcomer was last announced: a burst of joins is one sound.
    announced: Cell<Option<std::time::Instant>>,
}

/// What is running.
enum Run {
    Monitor(Box<MonitorRun>),
    Watch(Box<WatchRun>),
}

impl Run {
    fn stop(self) {
        match self {
            Self::Monitor(m) => m.monitor.stop(),
            Self::Watch(w) => {
                w.stop.store(true, Ordering::Relaxed);
                w.watch.stop();
            }
        }
    }
}

/// The window's engine host.
pub struct Host {
    rt: Runtime,
    store: Rc<RefCell<Store>>,
    ui: Weak<AppWindow>,
    run: RefCell<Option<Run>>,
    /// A pairing link read and waiting on "Join now".
    pending: RefCell<Option<Pairing>>,
    /// Every far end's sound, and the speaker playing it while a job runs.
    mix: Arc<crate::audio::Mix>,
    speaker: RefCell<Option<crate::audio::Speaker>>,
    /// The scanner's camera, while its sheet is open.
    scanner: RefCell<Option<crate::camera::Capture>>,
}

impl Host {
    /// Makes the host for a window and wires the call's keys.
    ///
    /// # Errors
    /// When the runtime can't start.
    pub fn attach(ui: &AppWindow, store: Rc<RefCell<Store>>) -> Result<Rc<Self>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tawny-engine")
            .enable_all()
            .build()
            .context("starting the engine's runtime")?;
        let host = Rc::new(Self {
            rt,
            store,
            ui: ui.as_weak(),
            run: RefCell::new(None),
            pending: RefCell::new(None),
            mix: Arc::default(),
            speaker: RefCell::new(None),
            scanner: RefCell::new(None),
        });
        HOST.with(|h| *h.borrow_mut() = Some(host.clone()));
        wire(ui);
        Ok(host)
    }

    /// The engine's runtime, for other work off the window's thread.
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// Opens the speaker, if it isn't already (a word on screen if none).
    fn open_speaker(&self) {
        if self.speaker.borrow().is_some() {
            return;
        }
        match crate::audio::Speaker::open(self.mix.clone()) {
            Ok(s) => *self.speaker.borrow_mut() = Some(s),
            Err(e) => eprintln!("tawny: {e:#}"),
        }
    }

    /// Opens the microphone into a call's source.
    fn open_mic(
        &self,
        src: libwebrtc::audio_source::native::NativeAudioSource,
        open: bool,
    ) -> Option<crate::audio::Mic> {
        match crate::audio::Mic::open(&self.rt, src, open) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("tawny: {e:#}");
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_toast(
                        crate::tr::t("No microphone found: the call goes on without one.").into(),
                    );
                }
                None
            }
        }
    }

    /// Stops whatever runs and goes back to the list.
    fn stop(&self, ui: &AppWindow) {
        // A clip being made is saved, as the web page's is on hang-up.
        finish_clip(ui);
        if let Some(run) = self.run.borrow_mut().take() {
            run.stop();
        }
        *self.speaker.borrow_mut() = None;
        let live = ui.global::<Live>();
        live.set_has_video(false);
        live.set_has_pip(false);
        live.set_camera_on(false);
        FEATURED.store(false, Ordering::Relaxed);
        live.set_talking(false);
        live.set_talkflag(false);
        live.set_note_title(SharedString::new());
        live.set_chimes_open(false);
        live.set_dimmed(false);
        live.set_sas_code(SharedString::new());
        live.set_peers(0);
        live.set_coach_step(-1);
        ui.set_screen("channels".into());
    }

    fn start_monitor(&self, ui: &AppWindow, store: &mut Store, ch: &Channel) -> Result<()> {
        let secret = parse_secret(&store.station_nostr(&ch.key))?;
        // The rendezvous admits on a ticket the Monitor mints and keeps (the
        // web app's tawny.token), and hands to Viewers in the link.
        store
            .keys_mut(&ch.key)
            .ticket
            .get_or_insert_with(crate::store::new_channel_key);
        let keys = store.keys_mut(&ch.key).clone();
        let gate: PairGate = keys
            .gate
            .as_deref()
            .and_then(|g| serde_json::from_str(g).ok())
            .unwrap_or_default();
        let settings = &store.data.settings;
        let only = settings.relays_only && !settings.relays.is_empty();
        let cfg = MonitorConfig {
            channel_key: ch.key.clone(),
            room_name: shown_name(&ch.name),
            ticket: keys.ticket.clone(),
            nostr_secret: Some(secret),
            nostr_relays: relays(store),
            nostr_health: HealthBook::default(),
            nostr_known: keys.known,
            rendezvous: (!only).then(|| DEFAULT_RENDEZVOUS.to_owned()),
            lan_bind: (!only).then(|| SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
            lan_ip: if only { None } else { lan_ip() },
            gate,
            mic: true,
            width: 1280,
            height: 720,
            media: media(),
        };
        let (monitor, mut rx) = self
            .rt
            .block_on(Monitor::start(cfg))
            .map_err(|e| anyhow!("{e}"))?;
        let link = monitor.pair_link().map_err(|e| anyhow!("{e}"))?;
        show_link(ui, &link);
        self.open_speaker();
        let muted = ui.global::<Live>().get_mic_muted();
        let mic = monitor
            .audio_source()
            .and_then(|src| self.open_mic(src, !muted));

        // Events, back on the window's thread.
        self.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                let _ = slint::invoke_from_event_loop(move || {
                    HOST.with(|h| {
                        if let Some(host) = h.borrow().clone() {
                            host.on_monitor(ev);
                        }
                    });
                });
            }
        });

        // A code left on screen is renewed before it runs low.
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, CODE_CHECK, || {
            HOST.with(|h| {
                if let Some(host) = h.borrow().clone() {
                    host.renew_code();
                }
            });
        });

        *self.run.borrow_mut() = Some(Run::Monitor(Box::new(MonitorRun {
            monitor,
            channel_key: ch.key.clone(),
            room: shown_name(&ch.name),
            guests: BTreeMap::new(),
            sas_for: None,
            link,
            _code_timer: timer,
            capture: None,
            camera: 0,
            chimed: None,
            mic,
        })));
        let live = ui.global::<Live>();
        live.set_role("station".into());
        live.set_connecting(true);
        live.set_dot("on".into());
        live.set_status(crate::tr::t("Waiting").into());
        live.set_peers(0);
        ui.set_pair_status(crate::tr::t("Waiting for the Viewer").into());
        ui.set_screen("watch-pair".into());
        Ok(())
    }

    fn renew_code(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let mut run = self.run.borrow_mut();
        let Some(Run::Monitor(m)) = run.as_mut() else {
            return;
        };
        if let Ok(link) = m.monitor.pair_link()
            && link != m.link
        {
            show_link(&ui, &link);
            m.link = link;
        }
    }

    fn on_monitor(&self, ev: MonitorEvent) {
        let Some(ui) = self.ui.upgrade() else { return };
        let live = ui.global::<Live>();
        let mut run = self.run.borrow_mut();
        let Some(Run::Monitor(m)) = run.as_mut() else {
            return;
        };
        match ev {
            MonitorEvent::Joined {
                id,
                label,
                name,
                pid,
            } => {
                m.guests.insert(
                    id,
                    Guest {
                        label,
                        name,
                        pid,
                        connected: false,
                    },
                );
            }
            MonitorEvent::Connected { id } => self.viewer_connected(&ui, &live, m, &id),
            MonitorEvent::Sas { id, code } => {
                eprintln!("tawny: STATE sas {}", code.as_deref().unwrap_or("-"));
                let pid = m.guests.get(&id).and_then(|g| g.pid.clone());
                let checked = pid.is_some_and(|p| {
                    self.store
                        .borrow()
                        .data
                        .keys
                        .get(&m.channel_key)
                        .is_some_and(|k| k.viewer_sas_ok.contains(&p))
                });
                if !checked {
                    let who = m.guests.get(&id).map(Guest::display).unwrap_or_default();
                    live.set_sas_warn(code.is_none());
                    live.set_sas_note(sas_note(&who, code.is_none()).into());
                    live.set_sas_code(code.unwrap_or_else(|| "—".into()).into());
                    m.sas_for = Some(id);
                }
            }
            MonitorEvent::Left { id } => {
                self.mix.forget(&id);
                m.guests.remove(&id);
                if m.sas_for.as_deref() == Some(id.as_str()) {
                    m.sas_for = None;
                    live.set_sas_code(SharedString::new());
                }
                if !m.guests.values().any(|g| g.connected) {
                    live.set_dot("on".into());
                    live.set_status(crate::tr::t("Waiting").into());
                }
                let me = self.store.borrow().data.settings.my_name.clone();
                send_roster(m, &me);
            }
            MonitorEvent::Gate(gate) => {
                let json = serde_json::to_string(&gate).ok();
                self.persist(|s| s.keys_mut(&m.channel_key).gate = json);
            }
            MonitorEvent::NostrKnown(known) => {
                self.persist(|s| s.keys_mut(&m.channel_key).known = known);
            }
            MonitorEvent::Control(v) => self.monitor_control(&live, m, &v),
            MonitorEvent::Log(text) => eprintln!("tawny: {text}"),
            MonitorEvent::Transport { name, up } => {
                eprintln!("tawny: {name} {}", if up { "up" } else { "down" });
            }
            MonitorEvent::Capture(on) => {
                m.capture = None;
                if on {
                    m.capture = open_camera(&m.monitor, m.camera, &ui);
                } else {
                    live.set_has_video(false);
                }
            }
            MonitorEvent::TalkAudio { id, track } => {
                crate::audio::listen(&self.rt, self.mix.clone(), id, track, false);
            }
            // A Viewer's own camera and the relay book: still to come.
            MonitorEvent::ViewerVideo { id: _, track } => {
                // A Viewer's own camera takes the stage while it sends;
                // this Monitor's camera moves to the corner.
                FEATURED.store(true, Ordering::Relaxed);
                crate::video::pump(
                    &self.rt,
                    track,
                    Arc::new(AtomicBool::new(false)),
                    show_featured,
                    false,
                );
            }
            MonitorEvent::Health(_) => {}
        }
        let watching: Vec<&Guest> = m.guests.values().filter(|g| g.connected).collect();
        live.set_peers(i32::try_from(watching.len()).unwrap_or(i32::MAX));
        // One phone watching: its own short name instead of a count's worth.
        live.set_peer_label(match watching.as_slice() {
            [one] => one.display().into(),
            _ => SharedString::new(),
        });
        let me = self.store.borrow().data.settings.my_name.clone();
        ui.set_who(who_model(&roster(m, &me, None)));
    }

    /// A Viewer's call came up: it is greeted, everyone gets the list, and
    /// the Monitor goes on air.
    fn viewer_connected(&self, ui: &AppWindow, live: &Live<'_>, m: &mut MonitorRun, id: &str) {
        eprintln!("tawny: STATE viewer {id} connected");
        if let Some(g) = m.guests.get_mut(id) {
            g.connected = true;
        }
        // A phone that just connected gets what the others have: no
        // light on a computer, the room's name and where to find this
        // Monitor on the relays next time; then everyone's list.
        m.monitor.send_to(
            id,
            json!({ "type": "torch", "on": false, "supported": false, "facing": "user" }),
        );
        let mut meta = json!({ "type": "meta", "petName": m.room });
        if let Some(np) = m.monitor.nostr_public_key() {
            meta["np"] = json!(np);
        }
        m.monitor.send_to(id, meta);
        let me = self.store.borrow().data.settings.my_name.clone();
        send_roster(m, &me);
        // A Viewer is watching: the Monitor goes on air.
        live.set_connecting(false);
        live.set_dot("live".into());
        live.set_status(crate::tr::t("On air").into());
        if ui.get_screen() == "watch-pair" {
            ui.set_screen("live".into());
            self.coach_if_new(ui, Role::Station);
        }
    }

    /// A frame from a Viewer: talking, or a chime to ring.
    fn monitor_control(&self, live: &Live<'_>, m: &mut MonitorRun, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("meta") => {
                let from = v.get("from").and_then(Value::as_str).unwrap_or_default();
                if let Some(name) = v.get("name").and_then(Value::as_str)
                    && let Some(g) = m.guests.get_mut(from)
                {
                    g.name = crate::store::clip(name.trim(), crate::store::MY_NAME_MAX);
                    let me = self.store.borrow().data.settings.my_name.clone();
                    send_roster(m, &me);
                }
            }
            Some("torch") => {
                // A computer has no light to turn on: say so.
                let from = v.get("from").and_then(Value::as_str).unwrap_or_default();
                m.monitor.send_to(
                    from,
                    json!({ "type": "torch", "on": false, "supported": false, "facing": "user" }),
                );
            }
            Some("talking") => {
                let on = v.get("on").and_then(Value::as_bool) == Some(true);
                live.set_talkflag(on);
            }
            Some("chime") => {
                let from = v.get("from").and_then(Value::as_str).unwrap_or_default();
                let sound = v.get("sound").and_then(Value::as_str).unwrap_or_default();
                // Spaced out, and a dropped one gets no ack: the Viewer's
                // "played" must not claim otherwise.
                let spaced = m
                    .chimed
                    .is_none_or(|t| t.elapsed() > Duration::from_millis(300));
                if spaced
                    && crate::chime::label(sound).is_some()
                    && let Some(clip) = crate::chime::samples(sound)
                {
                    m.chimed = Some(std::time::Instant::now());
                    crate::audio::play_clip(&self.mix, &clip);
                    eprintln!("tawny: STATE chime {sound} played");
                    m.monitor
                        .send_to(from, json!({ "type": "chime-ack", "sound": sound }));
                }
            }
            _ => {}
        }
    }

    /// The safety code checked: this Viewer is not asked again.
    fn sas_ok(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.global::<Live>().set_sas_code(SharedString::new());
        let viewer_key = match self.run.borrow().as_ref() {
            Some(Run::Watch(w)) => Some(w.channel_key.clone()),
            _ => None,
        };
        if let Some(key) = viewer_key {
            self.persist(|s| s.keys_mut(&key).sas_ok = true);
            return;
        }
        let mut run = self.run.borrow_mut();
        let Some(Run::Monitor(m)) = run.as_mut() else {
            return;
        };
        let pid = m
            .sas_for
            .take()
            .and_then(|id| m.guests.get(&id).and_then(|g| g.pid.clone()));
        if let Some(pid) = pid {
            let key = m.channel_key.clone();
            drop(run);
            self.persist(|s| {
                let k = s.keys_mut(&key);
                if !k.viewer_sas_ok.contains(&pid) {
                    k.viewer_sas_ok.push(pid);
                }
            });
        }
    }

    /// The safety code didn't match: that Viewer is let go.
    fn sas_no(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.global::<Live>().set_sas_code(SharedString::new());
        let mut run = self.run.borrow_mut();
        match run.as_mut() {
            Some(Run::Monitor(m)) => {
                if let Some(id) = m.sas_for.take() {
                    m.monitor.disconnect(&id);
                }
            }
            Some(Run::Watch(_)) => {
                drop(run);
                self.stop(&ui);
            }
            None => {}
        }
    }

    /// A pairing link pasted, scanned or opened: checked, then "Join now?".
    pub fn read_link(&self, ui: &AppWindow, raw: &str) {
        let anchors = Anchors {
            preferred: None,
            build: Some(RENDEZVOUS_HOST),
            has_rendezvous: true,
        };
        match link::parse(raw, anchors) {
            Ok(p) if p.is_expired(now_ms()) => {
                ui.set_toast(
                    crate::tr::t("That pairing code has expired. Ask the Monitor for a fresh one.")
                        .into(),
                );
            }
            Ok(p) => {
                ui.set_join_name(shown_name(&p.name).into());
                ui.set_join_note(SharedString::new());
                *self.pending.borrow_mut() = Some(p);
                ui.set_sheet(SharedString::new());
                ui.set_screen("join".into());
            }
            Err(e) => {
                eprintln!("tawny: link: {e}");
                ui.set_toast(crate::tr::t("That isn’t a Tawny pairing link.").into());
            }
        }
    }

    /// Joins the link just read, as "Join now" does.
    pub fn join_now(&self, ui: &AppWindow) {
        self.join(ui);
    }

    /// "Join now": the channel is kept, with what the link said, and dialled.
    fn join(&self, ui: &AppWindow) {
        let Some(p) = self.pending.borrow_mut().take() else {
            return;
        };
        let ch = {
            let mut s = self.store.borrow_mut();
            let ch = s.add_paired(&p.name, &p.key);
            s.set_role(&ch.id, Role::Viewer);
            s.data.settings.imported = true;
            let k = s.keys_mut(&p.key);
            if p.nostr_station.is_some() {
                k.monitor_nostr.clone_from(&p.nostr_station);
            }
            if p.signal.is_some() {
                k.lan.clone_from(&p.signal);
            }
            k.ticket.clone_from(&p.token);
            k.code.clone_from(&p.code);
            if let Err(e) = s.save() {
                eprintln!("tawny: saving: {e:#}");
            }
            ch
        };
        crate::app::refresh(ui, &self.store.borrow());
        crate::app::Sessions::start(self, ui, &mut self.store.borrow_mut(), &ch, Role::Viewer);
    }

    fn start_viewer(&self, ui: &AppWindow, store: &mut Store, ch: &Channel) -> Result<()> {
        let secret = parse_secret(&store.viewer_nostr(&ch.key))?;
        let pid = store.bond(&ch.key);
        let keys = store.keys_mut(&ch.key).clone();
        if keys.monitor_nostr.is_none() && keys.lan.is_none() {
            return Err(anyhow!("no pairing for this channel"));
        }
        let only = store.data.settings.relays_only && !store.data.settings.relays.is_empty();
        let name = store.data.settings.my_name.clone();
        let cfg = WatchConfig {
            channel_key: ch.key.clone(),
            pid,
            code: keys.code.clone(),
            label: DEVICE.into(),
            name: (!name.is_empty()).then_some(name),
            mic: true,
            lan: if only { None } else { keys.lan.clone() },
            // Only with the ticket the pairing link carried (as on Android).
            rendezvous: (!only && keys.ticket.is_some()).then(|| DEFAULT_RENDEZVOUS.to_owned()),
            ticket: keys.ticket.clone(),
            nostr_station: keys.monitor_nostr.clone(),
            nostr_secret: Some(secret),
            nostr_relays: relays(store),
            nostr_health: HealthBook::default(),
            media: media(),
        };
        let (watch, mut rx) = {
            let _rt = self.rt.enter();
            Watch::start(cfg).map_err(|e| anyhow!("{e}"))?
        };
        self.open_speaker();
        let mic = watch
            .audio_source()
            .and_then(|src| self.open_mic(src, false));
        self.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                let _ = slint::invoke_from_event_loop(move || {
                    HOST.with(|h| {
                        if let Some(host) = h.borrow().clone() {
                            host.on_watch(ev);
                        }
                    });
                });
            }
        });
        *self.run.borrow_mut() = Some(Run::Watch(Box::new(WatchRun {
            watch,
            channel_key: ch.key.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            mic,
            camera: None,
            announced: Cell::new(None),
        })));
        let live = ui.global::<Live>();
        live.set_role("viewer".into());
        live.set_connecting(true);
        live.set_has_video(false);
        live.set_dot("idle".into());
        live.set_status(crate::tr::t("Connecting").into());
        live.set_peers(0);
        live.set_battery(-1);
        // Until the Monitor sends its list, it may be an older one.
        ui.set_who(ModelRc::new(VecModel::from(Vec::<crate::WhoEntry>::new())));
        ui.set_who_old_monitor(true);
        live.set_lenses(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_screen("live".into());
        Ok(())
    }

    fn on_watch(&self, ev: WatchEvent) {
        let Some(ui) = self.ui.upgrade() else { return };
        let live = ui.global::<Live>();
        let run = self.run.borrow();
        let Some(Run::Watch(w)) = run.as_ref() else {
            return;
        };
        match ev {
            WatchEvent::Video(track) => {
                crate::video::pump(&self.rt, track, w.stop.clone(), show_far_picture, true);
            }
            WatchEvent::Way(way) => {
                eprintln!("tawny: calling over {way}");
            }
            WatchEvent::Connected => {
                eprintln!("tawny: STATE connected");
                live.set_connecting(false);
                live.set_dot("live".into());
                live.set_status(crate::tr::t("Live").into());
                live.set_note_title(SharedString::new());
                // Let in: the code has done its job.
                let key = w.channel_key.clone();
                drop(run);
                self.persist(|s| s.keys_mut(&key).code = None);
                self.coach_if_new(&ui, Role::Viewer);
            }
            WatchEvent::Sas(code) => {
                eprintln!("tawny: STATE sas {}", code.as_deref().unwrap_or("-"));
                let checked = self
                    .store
                    .borrow()
                    .data
                    .keys
                    .get(&w.channel_key)
                    .is_some_and(|k| k.sas_ok);
                if !checked {
                    live.set_sas_warn(code.is_none());
                    live.set_sas_note(sas_note("", code.is_none()).into());
                    live.set_sas_code(code.unwrap_or_else(|| "—".into()).into());
                }
            }
            WatchEvent::Refused(why) => {
                drop(run);
                self.stop(&ui);
                ui.set_toast(refused_line(&why).into());
            }
            WatchEvent::Left => {
                live.set_dot("warn".into());
                live.set_status(crate::tr::t("Reconnecting").into());
            }
            WatchEvent::Control(v) => {
                // Another phone started watching, and the Monitor announces
                // viewers: the same sound it plays, once for a burst.
                let joined = v.get("type").and_then(Value::as_str) == Some("meta")
                    && v.get("joined").and_then(Value::as_bool) == Some(true);
                let spaced = w
                    .announced
                    .get()
                    .is_none_or(|t| t.elapsed() > Duration::from_millis(1500));
                if joined
                    && spaced
                    && let Some(clip) = crate::chime::samples("viewer")
                {
                    w.announced.set(Some(std::time::Instant::now()));
                    crate::audio::play_clip(&self.mix, &clip);
                    eprintln!("tawny: STATE joined sound");
                }
                on_monitor_control(&ui, &live, &v);
            }
            WatchEvent::Log(text) => eprintln!("tawny: {text}"),
            WatchEvent::Transport { name, up } => {
                eprintln!("tawny: {name} {}", if up { "up" } else { "down" });
            }
            WatchEvent::Audio(track) => {
                crate::audio::listen(&self.rt, self.mix.clone(), "monitor".into(), track, true);
            }
            WatchEvent::Health(_) => {}
        }
    }

    /// Sends a control frame to the Monitor (Viewer only).
    fn control(&self, frame: Value) {
        if let Some(Run::Watch(w)) = self.run.borrow().as_ref() {
            w.watch.send_control(frame);
        }
    }

    /// Flip: the next of the computer's cameras that streams.
    fn flip_camera(&self, ui: &AppWindow) {
        let mut run = self.run.borrow_mut();
        let Some(Run::Monitor(m)) = run.as_mut() else {
            return;
        };
        // Some cameras show up twice (a picture node and a metadata
        // one): try the next ones in turn, skipping any that won't
        // stream, and stay put if none does.
        let n = crate::camera::count();
        if m.capture.is_none() {
            // Off: the next one is tried when the camera comes on.
            m.camera = (m.camera + 1) % n.max(1);
            return;
        }
        let was = m.capture.take();
        let source = m.monitor.video_source();
        let next = (1..n).map(|step| (m.camera + step) % n).find_map(|i| {
            crate::camera::Capture::start(i, source.clone(), show_own_picture)
                .ok()
                .map(|c| (i, c))
        });
        if let Some((i, c)) = next {
            drop(was);
            m.camera = i;
            m.capture = Some(c);
        } else {
            m.capture = was;
            ui.set_toast(crate::tr::t("This computer has one camera.").into());
        }
    }

    fn coach_if_new(&self, ui: &AppWindow, role: Role) {
        let seen = self
            .store
            .borrow()
            .data
            .settings
            .coach_seen
            .iter()
            .any(|r| r == role.as_str());
        if !seen {
            ui.global::<Live>().set_coach_step(0);
        }
    }

    fn persist(&self, f: impl FnOnce(&mut Store)) {
        let mut s = self.store.borrow_mut();
        f(&mut s);
        if let Err(e) = s.save() {
            eprintln!("tawny: saving: {e:#}");
        }
    }
}

impl Guest {
    fn display(&self) -> String {
        if self.name.is_empty() {
            self.label.clone()
        } else {
            self.name.clone()
        }
    }
}

impl crate::app::Sessions for Host {
    fn renamed(&self, name: &str) {
        match self.run.borrow().as_ref() {
            Some(Run::Watch(w)) => w
                .watch
                .send_control(json!({ "type": "meta", "name": name })),
            Some(Run::Monitor(m)) => send_roster(m, name),
            None => {}
        }
    }

    fn start(&self, ui: &AppWindow, store: &mut Store, channel: &Channel, role: Role) {
        if let Some(run) = self.run.borrow_mut().take() {
            run.stop();
        }
        let live = ui.global::<Live>();
        live.set_channel(shown_name(&channel.name).into());
        live.set_coach_step(-1);
        match role {
            Role::Station => {
                if let Err(e) = self.start_monitor(ui, store, channel) {
                    eprintln!("tawny: monitor: {e:#}");
                    ui.set_toast(
                        crate::tr::t("Couldn’t start the Monitor. Check the network.").into(),
                    );
                    ui.set_screen("channels".into());
                }
            }
            Role::Viewer => {
                if let Err(e) = self.start_viewer(ui, store, channel) {
                    eprintln!("tawny: viewer: {e:#}");
                    ui.set_toast(
                        crate::tr::t("This channel isn’t paired yet. Scan its Monitor’s code.")
                            .into(),
                    );
                    ui.set_screen("channels".into());
                    ui.set_sheet("scanner".into());
                }
            }
        }
    }
}

/// A copy of Tawny started later handed over a pairing link (or only asked
/// to be shown): the window comes forward, and a link opens "Join now".
pub fn link_from_another_copy(line: &str) {
    host(|h| {
        let Some(ui) = h.ui.upgrade() else { return };
        let _ = ui.show();
        ui.window().set_minimized(false);
        if line.starts_with("tawny://") {
            h.read_link(&ui, line);
        }
    });
}

/// Runs `f` on the window's host, if there is one.
fn host(f: impl FnOnce(&Host)) {
    HOST.with(|h| {
        if let Some(host) = h.borrow().clone() {
            f(&host);
        }
    });
}

/// The call's keys that reach the engine.
fn wire(ui: &AppWindow) {
    let live = ui.global::<Live>();
    let w = ui.as_weak();
    ui.on_sheet_changed(move |sheet| {
        crate::tip::sheet_changed(&sheet);
        let Some(ui) = w.upgrade() else { return };
        host(|h| {
            let mut sc = h.scanner.borrow_mut();
            if sheet != "scanner" {
                *sc = None;
            } else if sc.is_none() {
                ui.set_scan_has_preview(false);
                match crate::camera::Capture::scanner(0, show_scan_picture, scanned) {
                    Ok(c) => *sc = Some(c),
                    Err(e) => {
                        eprintln!("tawny: {e:#}");
                        ui.set_scan_hint(
                            crate::tr::t("No camera to scan with. Paste the pairing link instead.")
                                .into(),
                        );
                    }
                }
            }
        });
    });
    let w = ui.as_weak();
    live.on_peers_clicked(move || {
        if let Some(ui) = w.upgrade() {
            ui.set_sheet("who".into());
        }
    });
    live.on_sas_ok(|| host(Host::sas_ok));
    live.on_sas_no(|| host(Host::sas_no));
    let w = ui.as_weak();
    live.on_stop(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| h.stop(&ui));
        }
    });
    let w = ui.as_weak();
    live.on_leave(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| h.stop(&ui));
        }
    });
    let w = ui.as_weak();
    ui.on_pair_later(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| h.stop(&ui));
        }
    });
    let w = ui.as_weak();
    ui.on_pair_start(move || {
        if let Some(ui) = w.upgrade() {
            ui.set_screen("live".into());
            host(|h| h.coach_if_new(&ui, Role::Station));
        }
    });
    wire_monitor(ui);
    wire_viewer(ui);
}

/// The Monitor's keys, and pairing (copy, paste, join).
fn wire_monitor(ui: &AppWindow) {
    let live = ui.global::<Live>();
    let w = ui.as_weak();
    live.on_mic_mute(move || {
        if let Some(ui) = w.upgrade() {
            let live = ui.global::<Live>();
            let muted = !live.get_mic_muted();
            live.set_mic_muted(muted);
            host(|h| {
                if let Some(Run::Monitor(m)) = h.run.borrow().as_ref()
                    && let Some(mic) = &m.mic
                {
                    mic.open.store(!muted, Ordering::Relaxed);
                }
            });
        }
    });
    let w = ui.as_weak();
    live.on_dim(move || {
        if let Some(ui) = w.upgrade() {
            ui.global::<Live>().set_dimmed(true);
            host(|h| {
                if let Some(Run::Monitor(m)) = h.run.borrow().as_ref() {
                    m.monitor.set_dimmed(true);
                }
            });
        }
    });
    let w = ui.as_weak();
    live.on_flip(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| h.flip_camera(&ui));
        }
    });
    let w = ui.as_weak();
    live.on_wake(move || {
        if let Some(ui) = w.upgrade() {
            ui.global::<Live>().set_dimmed(false);
            host(|h| {
                if let Some(Run::Monitor(m)) = h.run.borrow().as_ref() {
                    m.monitor.set_dimmed(false);
                }
            });
        }
    });
    let w = ui.as_weak();
    ui.on_pair_copy(move || {
        if let Some(ui) = w.upgrade() {
            copy_link(&ui);
            ui.set_pair_copied(true);
        }
    });
    let w = ui.as_weak();
    ui.on_pair_sheet_copy(move || {
        if let Some(ui) = w.upgrade() {
            copy_link(&ui);
        }
    });
    let w = ui.as_weak();
    ui.on_scan_paste(move || {
        if let Some(ui) = w.upgrade() {
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text());
            match text {
                Ok(t) => host(|h| h.read_link(&ui, &t)),
                Err(_) => ui.set_toast(
                    crate::tr::t("Copy the pairing link first, then paste it here.").into(),
                ),
            }
        }
    });
    let w = ui.as_weak();
    ui.on_join_go(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| h.join(&ui));
        }
    });
    let w = ui.as_weak();
    ui.on_join_cancel(move || {
        if let Some(ui) = w.upgrade() {
            host(|h| *h.pending.borrow_mut() = None);
            ui.set_screen("channels".into());
        }
    });
}

/// The Viewer's keys.
fn wire_viewer(ui: &AppWindow) {
    let live = ui.global::<Live>();
    let w = ui.as_weak();
    live.on_talk(move |on| {
        if let Some(ui) = w.upgrade() {
            ui.global::<Live>().set_talking(on);
            host(|h| {
                if let Some(Run::Watch(r)) = h.run.borrow().as_ref() {
                    r.watch.set_talking(on);
                    if let Some(mic) = &r.mic {
                        mic.open.store(on, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let w = ui.as_weak();
    live.on_chime(move |sound| {
        if let Some(ui) = w.upgrade() {
            ui.global::<Live>().set_chimes_open(false);
            host(|h| h.control(json!({ "type": "chime", "sound": sound.as_str() })));
        }
    });
    let w = ui.as_weak();
    live.on_torch(move || {
        if let Some(ui) = w.upgrade() {
            let live = ui.global::<Live>();
            if live.get_torch_unavailable() {
                ui.set_toast(
                    crate::tr::t("The Monitor’s camera has no light on this side.").into(),
                );
                return;
            }
            let on = !live.get_torch_on();
            host(|h| h.control(json!({ "type": "torch", "on": on })));
        }
    });
    let w = ui.as_weak();
    live.on_pick_lens(move |i| {
        if let Some(ui) = w.upgrade() {
            ui.global::<Live>().set_lens(i);
            host(|h| h.control(json!({ "type": "camera-control", "lens": i })));
        }
    });
    wire_viewer_media(ui);
}

/// The Viewer's camera, snapshot and record keys.
fn wire_viewer_media(ui: &AppWindow) {
    let live = ui.global::<Live>();
    let w = ui.as_weak();
    live.on_camera(move || {
        let Some(ui) = w.upgrade() else { return };
        host(|h| {
            let mut run = h.run.borrow_mut();
            let Some(Run::Watch(r)) = run.as_mut() else {
                return;
            };
            let live = ui.global::<Live>();
            if r.camera.take().is_some() {
                r.watch.set_camera(false);
                live.set_camera_on(false);
                live.set_has_pip(false);
                return;
            }
            match crate::camera::Capture::start(0, r.watch.video_source(), show_own_picture) {
                Ok(c) => {
                    r.camera = Some(c);
                    r.watch.set_camera(true);
                    live.set_camera_on(true);
                }
                Err(e) => {
                    eprintln!("tawny: {e:#}");
                    ui.set_toast(
                        crate::tr::t("Couldn’t open the camera. Is another app using it?").into(),
                    );
                }
            }
        });
    });
    let w = ui.as_weak();
    live.on_snapshot(move || {
        if let Some(ui) = w.upgrade() {
            let live = ui.global::<Live>();
            if !live.get_has_video() {
                ui.set_toast(crate::tr::t("Nothing to capture yet.").into());
                return;
            }
            match crate::snapshot::save(&live.get_video()) {
                Ok(path) => {
                    eprintln!("tawny: snapshot {}", path.display());
                    ui.set_toast(crate::tr::t("Snapshot saved to Pictures").into());
                }
                Err(e) => {
                    eprintln!("tawny: snapshot: {e:#}");
                    ui.set_toast(crate::tr::t("Couldn’t save the snapshot.").into());
                }
            }
        }
    });
    let w = ui.as_weak();
    live.on_record(move || {
        if let Some(ui) = w.upgrade() {
            if CLIP.with(|c| c.borrow().is_some()) {
                finish_clip(&ui);
            } else {
                start_clip(&ui);
            }
        }
    });
    wire_viewer_sound(ui);
}

/// A clip in the making, and the timer that counts it down.
struct ClipRun {
    rec: crate::record::Recording,
    _tick: slint::Timer,
}

thread_local! {
    static CLIP: RefCell<Option<ClipRun>> = const { RefCell::new(None) };
}

/// Record pressed: a clip of what is on the stage, up to 20 s.
fn start_clip(ui: &AppWindow) {
    let live = ui.global::<Live>();
    if !live.get_has_video() {
        ui.set_toast(crate::tr::t("Nothing to capture yet.").into());
        return;
    }
    let rec = match crate::record::start() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tawny: record: {e:#}");
            ui.set_toast(crate::tr::t("Couldn’t record a video.").into());
            return;
        }
    };
    let started = std::time::Instant::now();
    live.set_recording(true);
    live.set_record_left(countdown(crate::record::MAX).into());
    ui.set_toast(crate::tr::t("Recording — up to 20s").into());
    let tick = slint::Timer::default();
    let w = ui.as_weak();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(ui) = w.upgrade() else { return };
            let left = crate::record::MAX.saturating_sub(started.elapsed());
            ui.global::<Live>().set_record_left(countdown(left).into());
            if left.is_zero() {
                // Not from inside this timer's own call: it goes with the clip.
                let w = ui.as_weak();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        finish_clip(&ui);
                    }
                });
            }
        },
    );
    CLIP.with(|c| *c.borrow_mut() = Some(ClipRun { rec, _tick: tick }));
}

/// "0:20" for what is left.
fn countdown(left: Duration) -> String {
    format!("0:{:02}", left.as_millis().div_ceil(1000).min(59))
}

/// Ends the clip, if one is being made, and saves it off the window's thread.
fn finish_clip(ui: &AppWindow) {
    let Some(run) = CLIP.with(|c| c.borrow_mut().take()) else {
        return;
    };
    ui.global::<Live>().set_recording(false);
    let ClipRun { rec, .. } = run;
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let saved = rec.finish();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let line = match saved {
                Ok(path) => {
                    eprintln!("tawny: STATE clip {}", path.display());
                    "Video saved to Videos"
                }
                Err(e) if e.to_string() == "nothing to capture" => "Nothing to capture yet.",
                Err(e) => {
                    eprintln!("tawny: record: {e:#}");
                    "Couldn’t save the video."
                }
            };
            ui.set_toast(crate::tr::t(line).into());
        });
    });
}

/// The Viewer's Mute.
fn wire_viewer_sound(ui: &AppWindow) {
    let live = ui.global::<Live>();
    let w = ui.as_weak();
    live.on_listen_mute(move || {
        if let Some(ui) = w.upgrade() {
            let live = ui.global::<Live>();
            let muted = !live.get_listen_muted();
            live.set_listen_muted(muted);
            host(|h| h.mix.muted.store(muted, Ordering::Relaxed));
        }
    });
}

/// The camera, or a word on screen when there is none.
fn open_camera(monitor: &Monitor, index: u32, ui: &AppWindow) -> Option<crate::camera::Capture> {
    match crate::camera::Capture::start(index, monitor.video_source(), show_own_picture) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("tawny: {e:#}");
            ui.set_toast(crate::tr::t("Couldn’t open the camera. Is another app using it?").into());
            None
        }
    }
}

/// The scanner's camera, into its sheet.
fn show_scan_picture(img: Image) {
    host(|h| {
        if let Some(ui) = h.ui.upgrade() {
            ui.set_scan_preview(img);
            ui.set_scan_has_preview(true);
        }
    });
}

/// A code was read: off to "Join now", if it is a pairing link.
fn scanned(text: &str) {
    host(|h| {
        *h.scanner.borrow_mut() = None;
        if let Some(ui) = h.ui.upgrade() {
            h.read_link(&ui, text);
        }
    });
}

/// The Monitor's own camera: its stage, or the corner while a Viewer's
/// camera has the stage. On a Viewer, its own camera in the corner.
fn show_own_picture(img: Image) {
    host(|h| {
        let Some(ui) = h.ui.upgrade() else { return };
        let live = ui.global::<Live>();
        if live.get_role() == "station" && !FEATURED.load(Ordering::Relaxed) {
            live.set_video(img);
            live.set_has_video(true);
            live.set_has_pip(false);
        } else {
            live.set_pip(img);
            live.set_has_pip(true);
        }
    });
}

/// A Viewer's camera on the Monitor's stage; when it stops, the stage goes
/// back to this camera.
fn show_featured(img: Option<Image>) {
    host(|h| {
        let Some(ui) = h.ui.upgrade() else { return };
        let live = ui.global::<Live>();
        if let Some(img) = img {
            if std::env::var_os("TAWNY_DEV").is_some() {
                let luma = img
                    .to_rgba8()
                    .map_or(0, |b| crate::video::mean_luma(b.as_bytes()));
                eprintln!("tawny: STATE featured viewer camera luma {luma}");
            }
            live.set_video(img);
            live.set_has_video(true);
        } else {
            FEATURED.store(false, Ordering::Relaxed);
            live.set_has_pip(false);
        }
    });
}

/// The Monitor's picture, onto the call's stage.
fn show_far_picture(img: Option<Image>) {
    HOST.with(|h| {
        let Some(host) = h.borrow().clone() else {
            return;
        };
        let Some(ui) = host.ui.upgrade() else { return };
        let live = ui.global::<Live>();
        match img {
            Some(img) => {
                if std::env::var_os("TAWNY_DEV").is_some() {
                    let size = img.size();
                    let luma = img
                        .to_rgba8()
                        .map_or(0, |b| crate::video::mean_luma(b.as_bytes()));
                    eprintln!(
                        "tawny: STATE video {}x{} luma {luma}",
                        size.width, size.height
                    );
                }
                live.set_video(img);
                live.set_has_video(true);
                live.set_connecting(false);
            }
            None => live.set_has_video(false),
        }
    });
}

/// What the Monitor says about itself, onto the call's rail and keys.
fn on_monitor_control(ui: &AppWindow, live: &Live<'_>, v: &Value) {
    match v.get("type").and_then(Value::as_str) {
        Some("chime-ack") => {
            if let Some(name) = v
                .get("sound")
                .and_then(Value::as_str)
                .and_then(crate::chime::label)
            {
                let said = if crate::tr::spanish() {
                    format!("{name} sonó en el monitor")
                } else {
                    format!("{name} played on the Monitor")
                };
                ui.set_toast(said.into());
            }
        }
        Some("battery") => {
            let level = v.get("level").and_then(Value::as_f64).unwrap_or(-1.0);
            if (0.0..=1.0).contains(&level) {
                #[expect(clippy::cast_possible_truncation, reason = "0..=100")]
                live.set_battery((level * 100.0).round() as i32);
            }
            live.set_charging(v.get("charging").and_then(Value::as_bool) == Some(true));
        }
        Some("torch") => {
            live.set_torch_on(v.get("on").and_then(Value::as_bool) == Some(true));
            live.set_torch_unavailable(v.get("supported").and_then(Value::as_bool) == Some(false));
        }
        Some("cameras") => {
            let names: Vec<SharedString> = v
                .get("list")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|c| c.get("label").and_then(Value::as_str))
                        .map(SharedString::from)
                        .collect()
                })
                .unwrap_or_default();
            live.set_lenses(ModelRc::new(VecModel::from(names)));
        }
        Some("meta") => {
            if let Some(roster) = v.get("roster").and_then(Value::as_array) {
                let viewers = roster
                    .iter()
                    .filter(|e| e.get("role").and_then(Value::as_str) == Some("viewer"))
                    .count();
                live.set_peers(i32::try_from(viewers).unwrap_or(i32::MAX));
                ui.set_who(who_model(roster));
                ui.set_who_old_monitor(false);
                if std::env::var_os("TAWNY_DEV").is_some() {
                    eprintln!("tawny: STATE roster {}", Value::Array(roster.clone()));
                }
            }
            if v.get("paused").and_then(Value::as_bool) == Some(true) {
                live.set_dot("warn".into());
                live.set_status(crate::tr::t("Monitor paused").into());
                live.set_note_title(crate::tr::t("The monitor is paused").into());
                live.set_note_body(
                    crate::tr::t("Its screen turned off or the app moved to the background. The picture comes back on its own when the monitor phone is woken.").into(),
                );
            } else if v.get("paused").and_then(Value::as_bool) == Some(false) {
                live.set_dot("live".into());
                live.set_status(crate::tr::t("Live").into());
                live.set_note_title(SharedString::new());
            }
        }
        _ => {}
    }
}

/// Why the Monitor turned this Viewer away, in words.
fn refused_line(why: &str) -> &'static str {
    match why {
        "expired" | "code" => {
            crate::tr::t("That pairing code has expired. Ask the Monitor for a fresh one.")
        }
        "full" => crate::tr::t("The Monitor already has as many viewers as it takes."),
        _ => crate::tr::t("The Monitor didn’t let this device in. Pair it again."),
    }
}

/// Copies the pairing link on screen.
fn copy_link(ui: &AppWindow) {
    let link = ui.get_pair_link().to_string();
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(link)) {
        Ok(()) => ui.set_toast(crate::tr::t("Link copied").into()),
        Err(_) => ui.set_toast(crate::tr::t("Couldn’t copy the link.").into()),
    }
}

/// Puts a pairing link on the screen: the QR and the words.
fn show_link(ui: &AppWindow, link: &str) {
    ui.set_pair_link(link.into());
    if let Some(img) = crate::qr::image(link) {
        ui.set_pair_qr(img);
    }
    ui.set_pair_copied(false);
}

/// Who is on this camera, as one Viewer should see it (app.js rosterFor):
/// this Monitor first, then every phone watching, that Viewer marked `you`.
fn roster(m: &MonitorRun, me: &str, for_id: Option<&str>) -> Vec<Value> {
    let mut list = vec![json!({ "role": "station", "name": me, "device": DEVICE })];
    for (id, g) in m.guests.iter().filter(|(_, g)| g.connected) {
        let mut v = json!({
            "role": "viewer",
            "name": g.name,
            "device": g.label.chars().take(20).collect::<String>(),
        });
        if Some(id.as_str()) == for_id {
            v["you"] = json!(true);
        }
        list.push(v);
    }
    list
}

/// Sends every connected Viewer its copy of the list.
fn send_roster(m: &MonitorRun, me: &str) {
    for id in m
        .guests
        .iter()
        .filter(|(_, g)| g.connected)
        .map(|(id, _)| id)
    {
        let list = roster(m, me, Some(id));
        m.monitor
            .send_to(id, json!({ "type": "meta", "roster": list }));
    }
}

/// A roster as the who's-here sheet lists it (`you`: this device on the
/// Monitor's own copy is the station entry).
fn who_model(list: &[Value]) -> ModelRc<crate::WhoEntry> {
    let rows: Vec<crate::WhoEntry> = list
        .iter()
        .take(13)
        .map(|e| {
            let text = |k: &str| {
                e.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let (name, device, role) = (
                crate::store::clip(&text("name"), crate::store::MY_NAME_MAX),
                text("device").chars().take(20).collect::<String>(),
                text("role"),
            );
            let initial = name
                .chars()
                .chain(device.chars())
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            crate::WhoEntry {
                name: name.into(),
                device: device.into(),
                role: if role == "station" {
                    "station"
                } else {
                    "viewer"
                }
                .into(),
                you: e.get("you").and_then(Value::as_bool) == Some(true),
                initial: initial.into(),
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// "Check this code matches the one on Sam's phone."
fn sas_note(who: &str, warn: bool) -> String {
    if warn {
        crate::tr::t(
            "This connection couldn’t be checked. Disconnect unless you just paired this phone.",
        )
        .into()
    } else if who.is_empty() {
        crate::tr::t("Check this code matches the one on the other phone:").into()
    } else if crate::tr::spanish() {
        format!("Revisa que este código coincida con el de {who}:")
    } else {
        format!("Check this code matches the one on {who}:")
    }
}

/// The relays to meet on: the user's own, or the build's.
fn relays(store: &Store) -> Vec<String> {
    if store.data.settings.relays.is_empty() {
        DEFAULT_RELAYS.iter().map(|r| (*r).to_owned()).collect()
    } else {
        store.data.settings.relays.clone()
    }
}

fn media() -> MediaConfig {
    MediaConfig {
        ice_servers: vec![IceServer {
            urls: STUN.iter().map(|s| (*s).to_owned()).collect(),
            username: String::new(),
            password: String::new(),
        }],
    }
}

/// This computer's address on the local network, as a phone on the same
/// Wi-Fi dials it. Nothing is sent: connecting a UDP socket only picks the
/// route.
fn lan_ip() -> Option<String> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

/// A 32-byte secret from its hex.
fn parse_secret(hex: &str) -> Result<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(anyhow!("a stored key has the wrong length"));
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).map_err(|_| anyhow!("a stored key isn't hex"))?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| anyhow!("a stored key isn't hex"))?;
    }
    Ok(out)
}

/// Milliseconds since the epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{parse_secret, refused_line, sas_note};

    #[test]
    fn refusals_read_as_sentences() {
        assert!(refused_line("expired").contains("expired"));
        assert!(refused_line("full").contains("as many"));
        assert!(refused_line("anything").contains("Pair it again"));
    }

    #[test]
    fn secrets_parse_from_hex() {
        let hex = "00".repeat(31) + "ff";
        let got = parse_secret(&hex).expect("parses");
        assert_eq!(got[31], 0xff);
        assert!(parse_secret("abc").is_err());
        assert!(parse_secret(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn the_safety_note_names_the_phone() {
        assert!(sas_note("Sam", false).contains("on Sam:"));
        assert!(sas_note("", false).contains("other phone"));
        assert!(sas_note("Sam", true).contains("couldn’t be checked"));
    }
}
