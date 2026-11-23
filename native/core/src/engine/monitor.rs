//! A whole Monitor (feature `engine`): the call layer's [`Station`] wired to
//! every way a Viewer can reach it.
//!
//! - Its own LAN relay ([`LanServer`]), which it also joins as the station,
//!   like the Android shell's page does with `LocalWeb`.
//! - Nostr ([`crate::net::nostr`]), when relays are configured.
//! - An internet rendezvous ([`crate::net::room`]), when one is configured.
//!
//! Every frame that arrives records which transport its sender came in on,
//! and the Station's answers go back the same way. The camera is wanted only
//! while a Viewer is connected ([`MonitorEvent::Capture`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_track::RtcVideoTrack;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::call::media::{MediaConfig, Station, StationConfig, StationEvent};
use crate::call::policy::{PAIR_MIN_LEFT_MS, PairCode, PairGate};
use crate::link::{LinkParts, build_app_link};
use crate::net::nostr::{self as nostr_net, ChannelEvent, ChannelHandle};
use crate::net::room::{self as room_net, LanServer, RoomEvent, RoomHandle};
use crate::nostr::event;
use crate::relay::health::HealthBook;
use crate::sas::{self, Signed};
use crate::signal::nostr::{Config as NostrConfig, Role};
use crate::signal::room::Kind;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// How a Monitor is set up. Everything here is stored by the app.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// The channel key.
    pub channel_key: String,
    /// The room's name, as shown to Viewers.
    pub room_name: String,
    /// The rendezvous admission ticket.
    pub ticket: Option<String>,
    /// This Monitor's Nostr secret key for the channel (kept on the Monitor).
    pub nostr_secret: Option<[u8; 32]>,
    /// Nostr relays to meet on; empty for none.
    pub nostr_relays: Vec<String>,
    /// What each relay did last time.
    pub nostr_health: HealthBook,
    /// Handheld Nostr keys that have joined before.
    pub nostr_known: Vec<String>,
    /// The rendezvous (`wss://host`), if any.
    pub rendezvous: Option<String>,
    /// Where the LAN relay listens (port 0 picks one); `None` for no LAN relay.
    pub lan_bind: Option<SocketAddr>,
    /// The address phones on the Wi-Fi dial, for the pairing link (`h=`).
    pub lan_ip: Option<String>,
    /// The pairing gate (known phones; a code if one is on screen).
    pub gate: PairGate,
    /// Send the microphone.
    pub mic: bool,
    /// The camera's frame size.
    pub width: u32,
    /// The camera's frame size.
    pub height: u32,
    /// STUN/TURN.
    pub media: MediaConfig,
}

/// What a Monitor tells its app.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// Start (`true`) or stop the camera.
    Capture(bool),
    /// A Viewer was let in.
    Joined {
        /// Its id.
        id: String,
        /// Its device name.
        label: String,
        /// Its person's name.
        name: String,
        /// Its bond id, if it sent one.
        pid: Option<String>,
    },
    /// A Viewer's call connected.
    Connected {
        /// Its id.
        id: String,
    },
    /// The security code for a Viewer (`None`: show a warning).
    Sas {
        /// Its id.
        id: String,
        /// The code.
        code: Option<String>,
    },
    /// A Viewer left.
    Left {
        /// Its id.
        id: String,
    },
    /// A Viewer's talk-back sound, for the app to play.
    TalkAudio {
        /// Its id.
        id: String,
        /// The track (read it with `NativeAudioStream`).
        track: RtcAudioTrack,
    },
    /// A Viewer's own camera ("My camera"): frames arrive only while it is on.
    ViewerVideo {
        /// Its id.
        id: String,
        /// The track (read it with `NativeVideoStream`).
        track: RtcVideoTrack,
    },
    /// Persist the pairing gate.
    Gate(PairGate),
    /// Persist the known Handheld Nostr keys.
    NostrKnown(Vec<String>),
    /// Persist the relay health book.
    Health(HealthBook),
    /// A control frame from a Viewer (`talking`, `torch`, `camera-control`, …).
    Control(Value),
    /// A transport came up or went down.
    Transport {
        /// `lan`, `nostr` or `rendezvous`.
        name: &'static str,
        /// Whether it is usable.
        up: bool,
    },
    /// A diagnostics line.
    Log(String),
}

/// Why a Monitor could not start.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The LAN relay could not listen.
    #[error("the LAN relay could not start: {0}")]
    Lan(#[from] std::io::Error),
    /// The Nostr session could not be made.
    #[error("Nostr: {0}")]
    Nostr(#[from] crate::signal::nostr::SessionError),
    /// No way for a Viewer to reach this Monitor.
    #[error("no LAN relay, no Nostr relays and no rendezvous: nowhere to meet")]
    NowhereToMeet,
    /// The system RNG failed.
    #[error(transparent)]
    Random(#[from] crate::ids::RandomError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Lan,
    Nostr,
    Rendezvous,
}

/// A running Monitor.
pub struct Monitor {
    station: Station,
    gate: Arc<Mutex<PairGate>>,
    lan: Option<LanServer>,
    lan_room: Option<RoomHandle>,
    nostr: Option<ChannelHandle>,
    rendezvous: Option<RoomHandle>,
    nostr_pub: Option<String>,
    cfg: MonitorConfig,
}

impl Monitor {
    /// Starts everything. Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`EngineError`] when a transport can't start or none is configured.
    pub async fn start(
        cfg: MonitorConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<MonitorEvent>), EngineError> {
        let nostr_on = cfg.nostr_secret.is_some() && !cfg.nostr_relays.is_empty();
        if cfg.lan_bind.is_none() && !nostr_on && cfg.rendezvous.is_none() {
            return Err(EngineError::NowhereToMeet);
        }
        let (station, st_rx) = Station::start(StationConfig {
            channel_key: cfg.channel_key.clone(),
            gate: cfg.gate.clone(),
            width: cfg.width,
            height: cfg.height,
            mic: cfg.mic,
            media: cfg.media.clone(),
        });
        let (ev, ev_rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Mutex::new(cfg.gate.clone()));

        let (lan, lan_room, lan_rx) = match cfg.lan_bind {
            Some(addr) => {
                let server = LanServer::start(addr, &cfg.channel_key).await?;
                let base = format!("ws://127.0.0.1:{}", server.port);
                let (h, rx) =
                    room_net::connect(Kind::Lan, &base, true, &cfg.channel_key, cfg.ticket.clone());
                (Some(server), Some(h), Some(rx))
            }
            None => (None, None, None),
        };
        let (nostr, nostr_rx, nostr_pub) = match (cfg.nostr_secret, nostr_on) {
            (Some(secret), true) => {
                let pub_key = event::public_key(&secret).ok();
                let ncfg = NostrConfig {
                    channel_key: cfg.channel_key.clone(),
                    role: Role::Station,
                    secret,
                    station_pub: String::new(),
                    remembered: cfg.nostr_known.clone(),
                };
                let (h, rx) = nostr_net::start(ncfg, &cfg.nostr_relays, cfg.nostr_health.clone())?;
                (Some(h), Some(rx), pub_key)
            }
            _ => (None, None, None),
        };
        let (rendezvous, rv_rx) = match &cfg.rendezvous {
            Some(base) => {
                let (h, rx) = room_net::connect(
                    Kind::Rendezvous,
                    base,
                    true,
                    &cfg.channel_key,
                    cfg.ticket.clone(),
                );
                (Some(h), Some(rx))
            }
            None => (None, None),
        };

        let router = Router {
            station: station.clone(),
            key: cfg.channel_key.clone(),
            gate: gate.clone(),
            routes: HashMap::new(),
            lan: lan_room.clone(),
            nostr: nostr.clone(),
            rendezvous: rendezvous.clone(),
            ev,
        };
        tokio::spawn(router.run(st_rx, lan_rx, nostr_rx, rv_rx));
        Ok((
            Self {
                station,
                gate,
                lan,
                lan_room,
                nostr,
                rendezvous,
                nostr_pub,
                cfg,
            },
            ev_rx,
        ))
    }

    /// Where the platform pushes camera frames.
    #[must_use]
    pub fn video_source(&self) -> NativeVideoSource {
        self.station.video_source()
    }

    /// Where the platform pushes microphone frames, when the microphone is
    /// on (see [`crate::call::media::AUDIO_FRAME`]).
    #[must_use]
    pub fn audio_source(&self) -> Option<NativeAudioSource> {
        self.station.audio_source()
    }

    /// The LAN relay's port, when it runs.
    #[must_use]
    pub fn lan_port(&self) -> Option<u16> {
        self.lan.as_ref().map(|l| l.port)
    }

    /// This Monitor's Nostr public key (`np` in the link), when Nostr runs.
    #[must_use]
    pub fn nostr_public_key(&self) -> Option<&str> {
        self.nostr_pub.as_deref()
    }

    /// Puts a new ten-minute code on screen.
    ///
    /// # Errors
    /// [`EngineError::Random`] if the RNG fails.
    pub fn new_code(&self) -> Result<PairCode, EngineError> {
        let mut g = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let code = g.new_code(now_ms())?.clone();
        self.station.set_code(code.clone());
        Ok(code)
    }

    /// The pairing link for the QR, with the current code, or a new one if
    /// there is none or it has under [`PAIR_MIN_LEFT_MS`] left (a code saved
    /// from an earlier run can be seconds from lapsing).
    ///
    /// # Errors
    /// [`EngineError::Random`] if the RNG fails.
    pub fn pair_link(&self) -> Result<String, EngineError> {
        let live = self.gate.lock().ok().and_then(|g| {
            g.code
                .clone()
                .filter(|c| c.expires_ms > now_ms().saturating_add(PAIR_MIN_LEFT_MS))
        });
        let code = match live {
            Some(c) => c,
            None => self.new_code()?,
        };
        let lan = match (&self.cfg.lan_ip, self.lan_port()) {
            (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
            _ => None,
        };
        Ok(build_app_link(&LinkParts {
            key: &self.cfg.channel_key,
            name: &self.cfg.room_name,
            token: self.cfg.ticket.as_deref(),
            code: &code.code,
            expires_s: i64::try_from(code.expires_ms / 1000).unwrap_or(i64::MAX),
            nostr_station: self.nostr_pub.as_deref(),
            lan: lan.as_deref(),
        }))
    }

    /// Puts this code on screen (the app minted it and shows it in its own
    /// QR); a phone presenting it before `expires_ms` is let in.
    pub fn set_code(&self, code: &str, expires_ms: u64) {
        let mut g = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let code = PairCode {
            code: code.to_owned(),
            expires_ms,
        };
        g.code = Some(code.clone());
        self.station.set_code(code);
    }

    /// Sends a control frame (`battery`, `meta`, …) to every connected Viewer.
    pub fn broadcast(&self, frame: Value) {
        self.station.broadcast(frame);
    }

    /// Sends a control frame to one Viewer.
    pub fn send_to(&self, id: &str, frame: Value) {
        self.station.send_to(id, frame);
    }

    /// Hangs up on one Viewer.
    pub fn disconnect(&self, id: &str) {
        self.station.disconnect(id);
    }

    /// Dim mode: the Monitor's own screen is off; spend less.
    pub fn set_dimmed(&self, dimmed: bool) {
        self.station.set_dimmed(dimmed);
    }

    /// Hangs up on everyone and stops every transport.
    pub fn stop(&self) {
        self.station.close();
        if let Some(h) = &self.lan_room {
            h.close();
        }
        if let Some(l) = &self.lan {
            l.stop();
        }
        if let Some(n) = &self.nostr {
            n.close();
        }
        if let Some(r) = &self.rendezvous {
            r.close();
        }
    }
}

struct Router {
    station: Station,
    /// The channel key, for the signatures on offers.
    key: String,
    gate: Arc<Mutex<PairGate>>,
    routes: HashMap<String, Route>,
    lan: Option<RoomHandle>,
    nostr: Option<ChannelHandle>,
    rendezvous: Option<RoomHandle>,
    ev: mpsc::UnboundedSender<MonitorEvent>,
}

async fn recv_room(rx: &mut Option<mpsc::UnboundedReceiver<RoomEvent>>) -> Option<RoomEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_nostr(
    rx: &mut Option<mpsc::UnboundedReceiver<ChannelEvent>>,
) -> Option<ChannelEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

impl Router {
    async fn run(
        mut self,
        mut st: mpsc::UnboundedReceiver<StationEvent>,
        mut lan: Option<mpsc::UnboundedReceiver<RoomEvent>>,
        mut nostr: Option<mpsc::UnboundedReceiver<ChannelEvent>>,
        mut rv: Option<mpsc::UnboundedReceiver<RoomEvent>>,
    ) {
        loop {
            tokio::select! {
                e = st.recv() => match e {
                    Some(e) => self.on_station(e),
                    None => return,
                },
                Some(e) = recv_room(&mut lan) => self.on_room(Route::Lan, e),
                Some(e) = recv_room(&mut rv) => self.on_room(Route::Rendezvous, e),
                Some(e) = recv_nostr(&mut nostr) => self.on_nostr(e),
            }
        }
    }

    fn emit(&self, e: MonitorEvent) {
        let _ = self.ev.send(e);
    }

    fn on_frame(&mut self, route: Route, f: Value) {
        match f["type"].as_str().unwrap_or("") {
            "welcome" => {}
            "peer-joined" => {
                if let Some(id) = f["id"].as_str() {
                    self.routes.insert(id.to_owned(), route);
                }
            }
            "peer-left" => {
                if let Some(id) = f["id"].as_str() {
                    if self.routes.get(id) == Some(&route) {
                        self.routes.remove(id);
                        self.station.signal(f);
                    }
                }
            }
            ty => {
                let Some(from) = f["from"].as_str() else {
                    return;
                };
                if ty == "offer" && !self.offer_ok(route, from, &f) {
                    return;
                }
                self.routes.insert(from.to_owned(), route);
                self.station.signal(f);
            }
        }
    }

    /// Whether an offer may reach the Station. On the LAN relay it must be
    /// signed with the channel key: there no code is shown, and the relay's
    /// own challenge proves nothing about who is at the far end of a socket
    /// (anyone on the Wi-Fi can pass it between two Tawnys), so the signature
    /// over the certificate is the only thing that keeps a stranger from
    /// answering in a Viewer's place. Elsewhere an unsigned offer is an older
    /// Tawny and is let through, its code shown; a forged one never is.
    fn offer_ok(&self, route: Route, from: &str, f: &Value) -> bool {
        let why = match sas::check_frame(&self.key, f) {
            Signed::Yes => return true,
            Signed::No if route != Route::Lan => return true,
            Signed::No => "unsigned on the LAN",
            Signed::Forged => "its signature doesn't match the channel key",
        };
        self.emit(MonitorEvent::Log(format!(
            "offer refused from {from}: {why}"
        )));
        // An older phone would sit on the open socket waiting for an answer:
        // closing it sends it on to a way where its call gets a code.
        if route == Route::Lan {
            self.lan
                .iter()
                .for_each(|h| h.send(json!({ "type": "kick", "to": from })));
        }
        false
    }

    fn on_room(&mut self, route: Route, e: RoomEvent) {
        let name = if route == Route::Lan {
            "lan"
        } else {
            "rendezvous"
        };
        match e {
            RoomEvent::Frame(f) => self.on_frame(route, f),
            RoomEvent::Up => self.emit(MonitorEvent::Transport { name, up: true }),
            RoomEvent::Down(why) => {
                self.emit(MonitorEvent::Transport { name, up: false });
                if let Some(why) = why {
                    self.emit(MonitorEvent::Log(format!(
                        "{name}: closed by the relay ({why:?})"
                    )));
                }
            }
            RoomEvent::Log(l) => self.emit(MonitorEvent::Log(format!("{name}: {l}"))),
        }
    }

    fn on_nostr(&mut self, e: ChannelEvent) {
        match e {
            ChannelEvent::Frame(f) => {
                if f["type"] == "welcome" {
                    self.emit(MonitorEvent::Transport {
                        name: "nostr",
                        up: true,
                    });
                }
                self.on_frame(Route::Nostr, f);
            }
            ChannelEvent::Remember(k) => self.emit(MonitorEvent::NostrKnown(k)),
            ChannelEvent::Health(h) => self.emit(MonitorEvent::Health(h)),
            ChannelEvent::Log(l) => self.emit(MonitorEvent::Log(l)),
            ChannelEvent::Relays(_) => {}
            ChannelEvent::Closed => self.emit(MonitorEvent::Transport {
                name: "nostr",
                up: false,
            }),
        }
    }

    fn on_station(&mut self, e: StationEvent) {
        match e {
            StationEvent::Signal(f) => {
                let Some(to) = f["to"].as_str() else { return };
                match self.routes.get(to) {
                    Some(Route::Lan) => self.lan.iter().for_each(|h| h.send(f.clone())),
                    Some(Route::Nostr) => self.nostr.iter().for_each(|h| h.send(f.clone())),
                    Some(Route::Rendezvous) => {
                        self.rendezvous.iter().for_each(|h| h.send(f.clone()));
                    }
                    None => self.emit(MonitorEvent::Log(format!(
                        "no route to {to}; frame dropped"
                    ))),
                }
            }
            StationEvent::Gate(g) => {
                if let Ok(mut mine) = self.gate.lock() {
                    // Keep the code on screen; take the newly enrolled phones.
                    mine.known.clone_from(&g.known);
                }
                self.emit(MonitorEvent::Gate(g));
            }
            StationEvent::Capture(on) => self.emit(MonitorEvent::Capture(on)),
            StationEvent::Joined {
                id,
                label,
                name,
                pid,
            } => {
                self.emit(MonitorEvent::Joined {
                    id,
                    label,
                    name,
                    pid,
                });
            }
            StationEvent::Connected { id } => self.emit(MonitorEvent::Connected { id }),
            // As in app.js `showSas()`: only paths through someone else's
            // server get a code. On the LAN the Monitor is its own relay and
            // every offer it takes there is signed with the channel key
            // ([`Router::offer_ok`]), so there is nothing a code would add.
            StationEvent::Sas { id, code } => {
                if self.routes.get(&id) != Some(&Route::Lan) {
                    self.emit(MonitorEvent::Sas { id, code });
                }
            }
            StationEvent::Left { id } => {
                self.routes.remove(&id);
                self.emit(MonitorEvent::Left { id });
            }
            StationEvent::Control(f) => self.emit(MonitorEvent::Control(f)),
            StationEvent::TalkAudio { id, track } => {
                self.emit(MonitorEvent::TalkAudio { id, track });
            }
            StationEvent::ViewerVideo { id, track } => {
                self.emit(MonitorEvent::ViewerVideo { id, track });
            }
            StationEvent::Log(l) => self.emit(MonitorEvent::Log(l)),
        }
    }
}
