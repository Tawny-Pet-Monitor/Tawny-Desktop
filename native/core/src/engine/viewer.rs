//! A whole Viewer (feature `engine`): the call layer's [`Viewer`] wired to
//! every way the pairing link says its Monitor can be reached.
//!
//! - The Monitor's LAN relay (`h=` in the link), when this phone is on the
//!   same Wi-Fi.
//! - Nostr (`np=`), when relays are configured.
//! - The internet rendezvous, when one is configured with a ticket.
//!
//! All of them are dialled at once. Each says where the Monitor is (a
//! `welcome` listing it, or `peer-joined`), under that transport's own id for
//! it, so the Viewer is handed exactly one of them: the LAN if it shows up
//! within [`LAN_GRACE`] of the first, else Nostr, else the rendezvous. If the
//! chosen way loses the Monitor, the call is hung up and placed again on
//! another way that still has it.
//!
//! As in `app.js` `showSas()`, the security code is shown only off the LAN.

use std::time::Duration;

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_track::RtcVideoTrack;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::monitor::EngineError;
use crate::call::media::{MediaConfig, Viewer, ViewerConfig, ViewerEvent};
use crate::net::nostr::{self as nostr_net, ChannelEvent, ChannelHandle};
use crate::net::room::{self as room_net, RoomEvent, RoomHandle};
use crate::relay::health::HealthBook;
use crate::signal::nostr::{Config as NostrConfig, Role};
use crate::signal::room::Kind;

/// How long a cloud way waits for the LAN before the call goes over it.
pub const LAN_GRACE: Duration = Duration::from_secs(2);

/// How a Viewer is set up: the stored pairing plus this phone's own keys.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// The channel key.
    pub channel_key: String,
    /// This phone's bond id with the Monitor (32 lower-case hex).
    pub pid: String,
    /// The pairing code, the first time in (the Monitor remembers `pid` after).
    pub code: Option<String>,
    /// A short device name for the Monitor's list.
    pub label: String,
    /// The person's name, if given.
    pub name: Option<String>,
    /// Talk-back microphone (pushed through [`Watch::audio_source`]).
    pub mic: bool,
    /// The Monitor's LAN relay, `ws://ip:port`.
    pub lan: Option<String>,
    /// The rendezvous, `wss://host`.
    pub rendezvous: Option<String>,
    /// The rendezvous admission ticket.
    pub ticket: Option<String>,
    /// The Monitor's Nostr public key.
    pub nostr_station: Option<String>,
    /// This phone's Nostr secret key for the channel.
    pub nostr_secret: Option<[u8; 32]>,
    /// Nostr relays to meet on.
    pub nostr_relays: Vec<String>,
    /// What each relay did last time.
    pub nostr_health: HealthBook,
    /// STUN/TURN.
    pub media: MediaConfig,
}

/// What a running Viewer tells its app.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// The Monitor's camera, for the platform's renderer.
    Video(RtcVideoTrack),
    /// The Monitor's sound, for the app to play.
    Audio(RtcAudioTrack),
    /// The call is being placed over this way: `lan`, `nostr` or `rendezvous`.
    Way(&'static str),
    /// The call connected.
    Connected,
    /// The security code (only off the LAN); `None`: show a warning.
    Sas(Option<String>),
    /// The Monitor turned this phone away (`expired`, `full`, …).
    Refused(String),
    /// The call ended (the Monitor left or the way dropped); another way
    /// may pick it up.
    Left,
    /// A control frame from the Monitor (`meta`, `battery`, `cameras`, …).
    Control(Value),
    /// A way came up or went down.
    Transport {
        /// `lan`, `nostr` or `rendezvous`.
        name: &'static str,
        /// Whether it is usable.
        up: bool,
    },
    /// Persist the relay health book.
    Health(HealthBook),
    /// A diagnostics line.
    Log(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    Lan,
    Nostr,
    Rendezvous,
}

impl Way {
    const fn name(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Nostr => "nostr",
            Self::Rendezvous => "rendezvous",
        }
    }
}

/// One transport's view of the room.
#[derive(Default)]
struct Seen {
    /// Our id on it.
    me: Option<String>,
    /// The Monitor's id on it, while it is there.
    station: Option<String>,
    /// When the Monitor was first seen there.
    since: Option<Instant>,
}

enum Cmd {
    Talking(bool),
    Control(Value),
    Close,
}

/// A running Viewer.
pub struct Watch {
    viewer: Viewer,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Watch {
    /// Starts dialling every configured way. Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`EngineError::NowhereToMeet`] when the pairing names no usable way,
    /// [`EngineError::Nostr`] when the Nostr keys are unusable.
    pub fn start(
        cfg: WatchConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<WatchEvent>), EngineError> {
        let nostr_on = cfg.nostr_station.is_some()
            && cfg.nostr_secret.is_some()
            && !cfg.nostr_relays.is_empty();
        let rv_on = cfg.rendezvous.is_some() && cfg.ticket.is_some();
        if cfg.lan.is_none() && !nostr_on && !rv_on {
            return Err(EngineError::NowhereToMeet);
        }
        let (lan, lan_rx) = match &cfg.lan {
            Some(base) => {
                let (h, rx) =
                    room_net::connect(Kind::Lan, base, false, &cfg.channel_key, cfg.ticket.clone());
                (Some(h), Some(rx))
            }
            None => (None, None),
        };
        let (nostr, nostr_rx) = match (nostr_on, cfg.nostr_secret, &cfg.nostr_station) {
            (true, Some(secret), Some(station)) => {
                let ncfg = NostrConfig {
                    channel_key: cfg.channel_key.clone(),
                    role: Role::Viewer,
                    secret,
                    station_pub: station.clone(),
                    remembered: Vec::new(),
                };
                let (h, rx) = nostr_net::start(ncfg, &cfg.nostr_relays, cfg.nostr_health.clone())?;
                (Some(h), Some(rx))
            }
            _ => (None, None),
        };
        let (rendezvous, rv_rx) = match (rv_on, &cfg.rendezvous) {
            (true, Some(base)) => {
                let (h, rx) = room_net::connect(
                    Kind::Rendezvous,
                    base,
                    false,
                    &cfg.channel_key,
                    cfg.ticket.clone(),
                );
                (Some(h), Some(rx))
            }
            _ => (None, None),
        };
        let (viewer, v_rx) = Viewer::start(ViewerConfig {
            channel_key: cfg.channel_key,
            pid: Some(cfg.pid),
            code: cfg.code,
            label: cfg.label,
            name: cfg.name,
            mic: cfg.mic,
            media: cfg.media,
        });
        let (ev, ev_rx) = mpsc::unbounded_channel();
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let router = Router {
            viewer: viewer.clone(),
            lan_expected: lan.is_some(),
            lan,
            nostr,
            rendezvous,
            seen: [Seen::default(), Seen::default(), Seen::default()],
            active: None,
            ev,
        };
        tokio::spawn(router.run(v_rx, lan_rx, nostr_rx, rv_rx, cmd_rx));
        Ok((Self { viewer, cmd }, ev_rx))
    }

    /// Where the platform pushes talk-back microphone frames.
    #[must_use]
    pub fn audio_source(&self) -> Option<NativeAudioSource> {
        self.viewer.audio_source()
    }

    /// Where the platform pushes this phone's camera for "My camera".
    #[must_use]
    pub fn video_source(&self) -> NativeVideoSource {
        self.viewer.video_source()
    }

    /// "My camera" on (this phone's camera goes to the Monitor) or off.
    pub fn set_camera(&self, on: bool) {
        self.viewer.set_camera(on);
    }

    /// Talk-back on (the user is holding Talk) or off.
    pub fn set_talking(&self, on: bool) {
        let _ = self.cmd.send(Cmd::Talking(on));
    }

    /// Sends a control frame to the Monitor (`torch`, `camera-control`,
    /// `chime`, …); `to` is filled in.
    pub fn send_control(&self, frame: Value) {
        let _ = self.cmd.send(Cmd::Control(frame));
    }

    /// Hangs up and stops every way.
    pub fn stop(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }
}

struct Router {
    viewer: Viewer,
    lan_expected: bool,
    lan: Option<RoomHandle>,
    nostr: Option<ChannelHandle>,
    rendezvous: Option<RoomHandle>,
    seen: [Seen; 3],
    active: Option<Way>,
    ev: mpsc::UnboundedSender<WatchEvent>,
}

const fn slot(w: Way) -> usize {
    match w {
        Way::Lan => 0,
        Way::Nostr => 1,
        Way::Rendezvous => 2,
    }
}

async fn recv<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

impl Router {
    async fn run(
        mut self,
        mut v: mpsc::UnboundedReceiver<ViewerEvent>,
        mut lan: Option<mpsc::UnboundedReceiver<RoomEvent>>,
        mut nostr: Option<mpsc::UnboundedReceiver<ChannelEvent>>,
        mut rv: Option<mpsc::UnboundedReceiver<RoomEvent>>,
        mut cmd: mpsc::UnboundedReceiver<Cmd>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                e = v.recv() => match e {
                    Some(e) => self.on_viewer(e),
                    None => break,
                },
                c = cmd.recv() => match c {
                    Some(Cmd::Talking(on)) => self.viewer.set_talking(on),
                    Some(Cmd::Control(f)) => self.control(f),
                    Some(Cmd::Close) | None => break,
                },
                Some(e) = recv(&mut lan) => self.on_room(Way::Lan, e),
                Some(e) = recv(&mut rv) => self.on_room(Way::Rendezvous, e),
                Some(e) = recv(&mut nostr) => self.on_nostr(e),
                _ = tick.tick() => self.choose(),
            }
        }
        self.viewer.close();
        for h in [&self.lan, &self.rendezvous].into_iter().flatten() {
            h.close();
        }
        if let Some(n) = &self.nostr {
            n.close();
        }
    }

    fn emit(&self, e: WatchEvent) {
        let _ = self.ev.send(e);
    }

    fn send(&self, way: Way, f: &Value) {
        match way {
            Way::Lan => self.lan.iter().for_each(|h| h.send(f.clone())),
            Way::Nostr => self.nostr.iter().for_each(|h| h.send(f.clone())),
            Way::Rendezvous => self.rendezvous.iter().for_each(|h| h.send(f.clone())),
        }
    }

    fn control(&self, mut f: Value) {
        let Some(way) = self.active else { return };
        let Some(station) = self.seen[slot(way)].station.clone() else {
            return;
        };
        if let Some(o) = f.as_object_mut() {
            o.insert("to".into(), json!(station));
            self.send(way, &f);
        }
    }

    fn on_frame(&mut self, way: Way, f: Value) {
        let s = &mut self.seen[slot(way)];
        match f["type"].as_str().unwrap_or("") {
            "welcome" => {
                s.me = f["id"].as_str().map(str::to_owned);
                let station = f["peers"]
                    .as_array()
                    .and_then(|p| p.iter().find(|x| x["role"] == "station"))
                    .and_then(|x| x["id"].as_str())
                    .map(str::to_owned);
                if station.is_some() {
                    s.station = station;
                    s.since.get_or_insert_with(Instant::now);
                }
            }
            "peer-joined" if f["role"] == "station" => {
                let id = f["id"].as_str().map(str::to_owned);
                let moved = id.is_some() && s.station.is_some() && s.station != id;
                s.station = id;
                s.since.get_or_insert_with(Instant::now);
                // A Monitor that came back under a new id: the old call is gone.
                if moved && self.active == Some(way) {
                    self.drop_active();
                }
            }
            "peer-left"
                if f["id"].as_str().is_some() && f["id"].as_str() == s.station.as_deref() =>
            {
                s.station = None;
                s.since = None;
                if self.active == Some(way) {
                    self.drop_active();
                }
            }
            _ if self.active == Some(way) => self.viewer.signal(f),
            _ => {}
        }
        self.choose();
    }

    /// The way the call was on lost the Monitor: hang up; [`Self::choose`]
    /// places it again on another way if one still has it.
    fn drop_active(&mut self) {
        let Some(way) = self.active.take() else {
            return;
        };
        let gone = self.seen[slot(way)].station.clone().unwrap_or_default();
        self.viewer
            .signal(json!({ "type": "peer-left", "id": gone, "from": gone }));
        self.emit(WatchEvent::Log(format!("{}: lost the Monitor", way.name())));
    }

    /// Picks the way the call goes over, once one has the Monitor.
    fn choose(&mut self) {
        if self.active.is_some() {
            return;
        }
        let has = |w: Way| self.seen[slot(w)].station.is_some();
        let pick = if has(Way::Lan) {
            Some(Way::Lan)
        } else {
            let first = [Way::Nostr, Way::Rendezvous]
                .into_iter()
                .filter_map(|w| self.seen[slot(w)].since)
                .min();
            let lan_may_come = self.lan_expected && first.is_some_and(|t| t.elapsed() < LAN_GRACE);
            if lan_may_come {
                None
            } else {
                [Way::Nostr, Way::Rendezvous].into_iter().find(|w| has(*w))
            }
        };
        let Some(way) = pick else { return };
        let s = &self.seen[slot(way)];
        let (Some(station), me) = (s.station.clone(), s.me.clone().unwrap_or_default()) else {
            return;
        };
        self.active = Some(way);
        self.emit(WatchEvent::Way(way.name()));
        self.viewer.signal(json!({
            "type": "welcome", "id": me, "peers": [{ "id": station, "role": "station" }]
        }));
    }

    fn on_viewer(&mut self, e: ViewerEvent) {
        match e {
            ViewerEvent::Signal(f) => {
                if let Some(way) = self.active {
                    self.send(way, &f);
                }
            }
            ViewerEvent::Video(t) => self.emit(WatchEvent::Video(t)),
            ViewerEvent::Audio(t) => self.emit(WatchEvent::Audio(t)),
            ViewerEvent::Connected => self.emit(WatchEvent::Connected),
            ViewerEvent::Sas(code) => {
                if self.active.is_some_and(|w| w != Way::Lan) {
                    self.emit(WatchEvent::Sas(code));
                }
            }
            ViewerEvent::Refused(r) => self.emit(WatchEvent::Refused(r)),
            ViewerEvent::Left => self.emit(WatchEvent::Left),
            ViewerEvent::Control(f) => self.emit(WatchEvent::Control(f)),
            ViewerEvent::Log(l) => self.emit(WatchEvent::Log(l)),
        }
    }

    fn lost(&mut self, way: Way) {
        let s = &mut self.seen[slot(way)];
        s.station = None;
        s.since = None;
        if self.active == Some(way) {
            self.drop_active();
        }
        self.choose();
    }

    fn on_room(&mut self, way: Way, e: RoomEvent) {
        let name = way.name();
        match e {
            RoomEvent::Frame(f) => self.on_frame(way, f),
            RoomEvent::Up => self.emit(WatchEvent::Transport { name, up: true }),
            RoomEvent::Down(why) => {
                self.emit(WatchEvent::Transport { name, up: false });
                if let Some(why) = why {
                    self.emit(WatchEvent::Log(format!(
                        "{name}: closed by the relay ({why:?})"
                    )));
                }
                self.lost(way);
            }
            RoomEvent::Log(l) => self.emit(WatchEvent::Log(format!("{name}: {l}"))),
        }
    }

    fn on_nostr(&mut self, e: ChannelEvent) {
        match e {
            ChannelEvent::Frame(f) => {
                if f["type"] == "welcome" {
                    self.emit(WatchEvent::Transport {
                        name: "nostr",
                        up: true,
                    });
                }
                self.on_frame(Way::Nostr, f);
            }
            ChannelEvent::Health(h) => self.emit(WatchEvent::Health(h)),
            ChannelEvent::Log(l) => self.emit(WatchEvent::Log(l)),
            ChannelEvent::Remember(_) | ChannelEvent::Relays(_) => {}
            ChannelEvent::Closed => {
                self.emit(WatchEvent::Transport {
                    name: "nostr",
                    up: false,
                });
                self.lost(Way::Nostr);
            }
        }
    }
}
