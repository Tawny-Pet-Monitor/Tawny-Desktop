//! The WebRTC peers (feature `media`): a Monitor ([`Station`]) answering up
//! to three Viewers, and a [`Viewer`] calling its Monitor.
//!
//! Both are transport-agnostic: frames from any signalling transport (Nostr,
//! the LAN relay, a rendezvous) go in with `signal()`, and frames to send come
//! out as `Signal` events carrying `to`. Each runs as one task that owns all
//! of its state; libwebrtc's callbacks only post messages to it, because they
//! run on libwebrtc's own threads where a panic would abort the process.
//!
//! Must be started inside a tokio runtime (libwebrtc's video source spawns a
//! task when it is made).
//!
//! Sound goes through the app, not libwebrtc's platform audio module: the app
//! captures the microphone and pushes 10 ms frames into an
//! [`AudioSource`](libwebrtc::audio_source::native::NativeAudioSource), and
//! plays the audio tracks it is handed. The platform module is shared by
//! every call and libwebrtc shuts it down when any one call ends, which
//! crashed the next call to start (seen on Android when a Viewer rejoined).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libwebrtc::MediaType;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::ice_candidate::IceCandidate;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::{
    AnswerOptions, OfferOptions, PeerConnection, PeerConnectionState,
};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection_factory::{IceServer, PeerConnectionFactory, RtcConfiguration};
use libwebrtc::rtp_parameters::DegradationPreference;
use libwebrtc::rtp_transceiver::{RtpTransceiver, RtpTransceiverDirection, RtpTransceiverInit};
use libwebrtc::session_description::{SdpType, SessionDescription};
use libwebrtc::video_source::VideoResolution;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_track::RtcVideoTrack;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::policy::{self, Gate, MAX_VIEWERS, PairGate};
use crate::sas;

const MAX_PENDING_ICE: usize = 64;

/// The sample rate the app captures at.
pub const AUDIO_RATE: u32 = 48_000;
/// Samples in one pushed frame: 10 ms, mono.
pub const AUDIO_FRAME: usize = 480;

/// A microphone source the app pushes 10 ms mono frames into. Echo
/// cancelling, noise suppression and gain are left to the platform's
/// voice-call capture path, which has the speaker signal to cancel against.
fn mic_source() -> NativeAudioSource {
    NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        AUDIO_RATE,
        1,
        0,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What both ends share.
#[derive(Debug, Clone, Default)]
pub struct MediaConfig {
    /// STUN and TURN servers.
    pub ice_servers: Vec<IceServer>,
}

fn rtc_config(m: &MediaConfig) -> RtcConfiguration {
    // RtcConfiguration is non-exhaustive: start from the default, then set.
    let mut c = RtcConfiguration::default();
    c.ice_servers.clone_from(&m.ice_servers);
    c
}

/// The offer as the Monitor should read it: the video slot `recvonly` while
/// "My camera" is off, `sendrecv` while it is on. Locally the slot always
/// goes both ways (libwebrtc offers no way to change a transceiver's
/// direction); this is what app.js's `tr.direction` change puts on the wire,
/// and what makes a browser Monitor mute and unmute its picture of us. An
/// answer of `sendonly` to our `sendrecv` is valid and simply means we only
/// receive.
fn camera_direction(sdp: &str, sending: bool) -> String {
    let mut video = false;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare.starts_with("m=") {
            video = bare.starts_with("m=video");
        }
        if video && !sending && bare == "a=sendrecv" {
            out.push_str(&line.replacen("a=sendrecv", "a=recvonly", 1));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The far end's description without the video-orientation header
/// extension (CVO, `urn:3gpp:video-orientation`), so it is never negotiated.
///
/// A phone's camera hands over frames sideways with "turn me 90°" attached.
/// With CVO agreed, libwebrtc sends them as they are and leaves the turn to the
/// receiver, and Safari ignores it: an iPhone watching a Monitor held upright
/// saw the room on its side (2026-11-25). Without it, libwebrtc turns the
/// pixels before encoding, so every receiver gets an upright picture.
///
/// The Monitor strips it from a Viewer's offer; a Viewer strips it from the
/// offers it sends (never from an answer: an answer agreeing CVO means the far
/// end already sends frames for the receiver to turn).
fn without_cvo(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|l| !(l.starts_with("a=extmap:") && l.contains("urn:3gpp:video-orientation")))
        .collect()
}

/// Closed calls, kept a while before they are dropped. libwebrtc goes on
/// calling a call's observer from its own threads after `close()` (the ICE
/// state settling), and the observer lives in the Rust `PeerConnection`:
/// dropping it straight away let such a callback run on freed memory (a
/// crash seen at the end of calls). Kept here, not in a task, so even a
/// runtime shutting down can't drop them early.
static RETIRED: Mutex<Vec<(Instant, Arc<PeerConnection>)>> = Mutex::new(Vec::new());
const RETIRE_FOR: Duration = Duration::from_secs(10);

/// Closes a call and parks it until libwebrtc is done with it.
fn retire(pc: Arc<PeerConnection>) {
    // Deaf first: closing fires a last state change tagged with the peer's
    // id, which would otherwise reach whichever call takes that id next (a
    // Viewer calling again) and end it.
    pc.on_connection_state_change(None);
    pc.on_ice_candidate(None);
    pc.on_track(None);
    pc.close();
    let now = Instant::now();
    let mut parked = RETIRED.lock().unwrap_or_else(PoisonError::into_inner);
    parked.retain(|(at, _)| now.duration_since(*at) < RETIRE_FOR);
    parked.push((now, pc));
}

/// The size "My camera" is sent at.
const VIEWER_CAM: (u32, u32) = (640, 480);

/// Offer options that leave every transceiver's direction alone. Never
/// `OfferOptions::default()`: the crate's default sets both legacy `offerToReceive`
/// flags to `Some(false)`, which strips receiving from every media line.
const OFFER: OfferOptions = OfferOptions {
    ice_restart: false,
    offer_to_receive_audio: None,
    offer_to_receive_video: None,
};

/// How long a Viewer's call may sit disconnected before the Viewer restarts
/// ICE (app.js's `iceKick`): a short drop usually heals by itself.
const ICE_KICK: Duration = Duration::from_secs(4);
/// How long an ICE restart gets to bring the call back before the next try.
const RESTART_WAIT: Duration = Duration::from_secs(10);
/// Answered ICE restarts that didn't bring the call back before the Viewer
/// gives the call up and places a new one.
const MAX_RESTARTS: u32 = 3;
/// Waits on an offer the Monitor hasn't answered (it is away: asleep, off
/// the Wi-Fi) before starting over with a new call. Only one offer is ever
/// out: piling them up had the Monitor answer a queue of stale ones when it
/// came back.
const MAX_UNANSWERED: u32 = 12;

/// Puts H.264 first (hardware encoders on most phones), keeping the rest.
fn prefer_h264(factory: &PeerConnectionFactory, t: &RtpTransceiver, receiving: bool) {
    let caps = if receiving {
        factory.get_rtp_receiver_capabilities(MediaType::Video)
    } else {
        factory.get_rtp_sender_capabilities(MediaType::Video)
    };
    let (mut first, rest): (Vec<_>, Vec<_>) = caps
        .codecs
        .into_iter()
        .partition(|c| c.mime_type.eq_ignore_ascii_case("video/H264"));
    first.extend(rest);
    let _ = t.set_codec_preferences(first);
}

fn candidate_json(c: &IceCandidate) -> Value {
    json!({ "candidate": c.candidate(), "sdpMid": c.sdp_mid(), "sdpMLineIndex": c.sdp_mline_index() })
}

fn parse_candidate(v: &Value) -> Option<IceCandidate> {
    let s = v["candidate"].as_str().filter(|s| !s.is_empty())?;
    let idx = i32::try_from(v["sdpMLineIndex"].as_i64().unwrap_or(0)).ok()?;
    IceCandidate::parse(v["sdpMid"].as_str().unwrap_or("0"), idx, s).ok()
}

/// The security code for a connected peer, the way `computeSas()` does it,
/// over the two certificates' fingerprints.
///
/// They are read from the two descriptions, not from the stats report:
/// libwebrtc's stats glue can end the whole program from a thread nothing
/// can catch on (seen as silent aborts on both ends of a call), and once
/// connected the descriptions name the same certificates (DTLS refuses a far
/// certificate that doesn't match its description's fingerprint).
fn security_code(pc: &PeerConnection, key: &str) -> Option<String> {
    let local = sas::sdp_fingerprint(&pc.current_local_description()?.to_string())?;
    let pair = sas::negotiated(&local, None, &pc.current_remote_description()?.to_string())?;
    Some(sas::code(key, &pair[0], &pair[1]))
}

/// A device name from a peer: printable ASCII only, short, only ever shown.
/// A name cut short loses its broken last word ("moto g 5G (2023)" must
/// not end in "5G (").
fn clean_label(s: &str) -> String {
    const MAX: usize = 20;
    let printable: String = s.chars().filter(|c| (' '..='~').contains(c)).collect();
    let whole = printable.trim();
    if whole.len() <= MAX {
        return whole.to_owned();
    }
    // Printable ASCII: one byte a character.
    let cut = &whole[..MAX];
    let kept = match cut.rfind(' ') {
        Some(space) if whole.as_bytes()[MAX] != b' ' => &cut[..space],
        _ => cut,
    };
    kept.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned()
}

/// A person's name from a peer: no control or bidi characters, 40 at most.
fn clean_name(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !('\u{202A}'..='\u{202E}').contains(c)
                && !('\u{2066}'..='\u{2069}').contains(c)
        })
        .collect::<String>()
        .trim()
        .chars()
        .take(40)
        .collect()
}

// =================================================================== Monitor

/// How a Monitor is set up.
#[derive(Debug, Clone)]
pub struct StationConfig {
    /// The channel key (for the security code).
    pub channel_key: String,
    /// The pairing gate (code on screen, known phones).
    pub gate: PairGate,
    /// The camera's frame size.
    pub width: u32,
    /// The camera's frame size.
    pub height: u32,
    /// Send the microphone (pushed through [`Station::audio_source`]).
    pub mic: bool,
    /// STUN/TURN.
    pub media: MediaConfig,
}

/// What a Monitor tells its app.
#[derive(Debug, Clone)]
pub enum StationEvent {
    /// Send this frame (it carries `to`) on the transport the peer came from.
    Signal(Value),
    /// Start (`true`) or stop the camera: it runs only while someone watches.
    Capture(bool),
    /// A Viewer was let in.
    Joined {
        /// Its signalling id.
        id: String,
        /// Its device name, if it gave one.
        label: String,
        /// Its person's name, if it gave one.
        name: String,
        /// Its bond id (stable across its reconnects), if it sent one: how a
        /// Monitor remembers that its security code was already checked.
        pid: Option<String>,
    },
    /// A Viewer's call connected.
    Connected {
        /// Its id.
        id: String,
    },
    /// The security code for a Viewer, or `None` when it can't be computed
    /// (fail closed: show a warning, not a code).
    Sas {
        /// Its id.
        id: String,
        /// The code.
        code: Option<String>,
    },
    /// A Viewer left or its call failed.
    Left {
        /// Its id.
        id: String,
    },
    /// A Viewer's talk-back sound, for the app to play.
    TalkAudio {
        /// Its id.
        id: String,
        /// The track.
        track: RtcAudioTrack,
    },
    /// A Viewer's own camera ("My camera"). The track comes with the call and
    /// carries frames only while that Viewer has its camera on.
    ViewerVideo {
        /// Its id.
        id: String,
        /// The track.
        track: RtcVideoTrack,
    },
    /// The gate changed (a phone was enrolled): persist it.
    Gate(PairGate),
    /// A control frame for the app (`talking`, `torch`, `camera-control`, `chime`, `meta`, …).
    Control(Value),
    /// A diagnostics line.
    Log(String),
}

enum StationMsg {
    Signal(Value),
    Gate(PairGate),
    Code(policy::PairCode),
    Dimmed(bool),
    Close,
    State(String, PeerConnectionState),
    Ice(String, IceCandidate),
    Audio(String, RtcAudioTrack),
    Video(String, RtcVideoTrack),
    Broadcast(Value),
    SendTo(String, Value),
    Disconnect(String),
}

/// Talks to a running Monitor.
#[derive(Clone)]
pub struct Station {
    tx: mpsc::UnboundedSender<StationMsg>,
    video: NativeVideoSource,
    audio: Option<NativeAudioSource>,
}

impl Station {
    /// Starts a Monitor. Push camera frames into [`Station::video_source`].
    #[must_use]
    pub fn start(cfg: StationConfig) -> (Self, mpsc::UnboundedReceiver<StationEvent>) {
        let factory = PeerConnectionFactory::default();
        let video = NativeVideoSource::new(
            VideoResolution {
                width: cfg.width,
                height: cfg.height,
            },
            false,
        );
        let video_track = factory.create_video_track("cam", video.clone());
        let audio = cfg.mic.then(mic_source);
        let mic = audio
            .as_ref()
            .map(|a| factory.create_audio_track("mic", a.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        let (ev, ev_rx) = mpsc::unbounded_channel();
        let actor = StationActor {
            factory,
            video_track,
            mic,
            cfg,
            viewers: HashMap::new(),
            ev,
            me: tx.clone(),
            dimmed: false,
            capturing: false,
        };
        tokio::spawn(actor.run(rx));
        (Self { tx, video, audio }, ev_rx)
    }

    /// Where the platform pushes camera frames.
    #[must_use]
    pub fn video_source(&self) -> NativeVideoSource {
        self.video.clone()
    }

    /// Where the platform pushes microphone frames ([`AUDIO_RATE`], mono,
    /// [`AUDIO_FRAME`] samples each), when the microphone is on.
    #[must_use]
    pub fn audio_source(&self) -> Option<NativeAudioSource> {
        self.audio.clone()
    }

    /// A frame from the transport (it carries `from`, or is `peer-left`).
    pub fn signal(&self, frame: Value) {
        let _ = self.tx.send(StationMsg::Signal(frame));
    }

    /// Replaces the gate (a new code on screen, or a re-pair).
    pub fn set_gate(&self, gate: PairGate) {
        let _ = self.tx.send(StationMsg::Gate(gate));
    }

    /// A new code on screen, leaving the phones already let in as they are
    /// (one enrolled a moment ago may not have reached the app's copy yet).
    pub fn set_code(&self, code: policy::PairCode) {
        let _ = self.tx.send(StationMsg::Code(code));
    }

    /// Dim mode: the Monitor's own screen is off; spend less.
    pub fn set_dimmed(&self, dimmed: bool) {
        let _ = self.tx.send(StationMsg::Dimmed(dimmed));
    }

    /// Sends a control frame (`chime-ack`, `torch`, …) to one Viewer.
    pub fn send_to(&self, id: &str, frame: Value) {
        let _ = self.tx.send(StationMsg::SendTo(id.to_owned(), frame));
    }

    /// Hangs up on one Viewer (the user tapped Disconnect on its security
    /// code). It is told `bye` and its call is closed.
    pub fn disconnect(&self, id: &str) {
        let _ = self.tx.send(StationMsg::Disconnect(id.to_owned()));
    }

    /// Sends a control frame (`battery`, `meta`, `cameras`, …) to every
    /// connected Viewer; `to` is filled in for each.
    pub fn broadcast(&self, frame: Value) {
        let _ = self.tx.send(StationMsg::Broadcast(frame));
    }

    /// Hangs up on everyone.
    pub fn close(&self) {
        let _ = self.tx.send(StationMsg::Close);
    }
}

struct ViewerPeer {
    pc: Arc<PeerConnection>,
    answered: bool,
    connected: bool,
    pending: Vec<IceCandidate>,
}

struct StationActor {
    factory: PeerConnectionFactory,
    video_track: RtcVideoTrack,
    mic: Option<RtcAudioTrack>,
    cfg: StationConfig,
    viewers: HashMap<String, ViewerPeer>,
    ev: mpsc::UnboundedSender<StationEvent>,
    me: mpsc::UnboundedSender<StationMsg>,
    dimmed: bool,
    capturing: bool,
}

/// A Monitor whose task is dropped (its runtime shutting down) parks its
/// calls rather than freeing them under libwebrtc (see [`retire`]).
impl Drop for StationActor {
    fn drop(&mut self) {
        for (_, v) in self.viewers.drain() {
            retire(v.pc);
        }
    }
}

impl StationActor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<StationMsg>) {
        while let Some(m) = rx.recv().await {
            match m {
                StationMsg::Signal(f) => self.on_signal(&f).await,
                StationMsg::Gate(g) => self.cfg.gate = g,
                StationMsg::Code(c) => self.cfg.gate.code = Some(c),
                StationMsg::Dimmed(d) => {
                    self.dimmed = d;
                    self.retune();
                }
                StationMsg::Close => break,
                StationMsg::State(id, s) => self.on_state(&id, s),
                StationMsg::Broadcast(f) => self.broadcast(&f),
                StationMsg::SendTo(id, f) => {
                    if let (true, Some(o)) = (self.viewers.contains_key(&id), f.as_object()) {
                        let mut frame = o.clone();
                        frame.insert("to".into(), json!(id));
                        self.emit(StationEvent::Signal(Value::Object(frame)));
                    }
                }
                StationMsg::Disconnect(id) => {
                    if self.viewers.contains_key(&id) {
                        self.emit(StationEvent::Signal(json!({ "type": "bye", "to": id })));
                        self.remove(&id);
                    }
                }
                StationMsg::Audio(id, track) => {
                    if self.viewers.contains_key(&id) {
                        self.emit(StationEvent::TalkAudio { id, track });
                    }
                }
                StationMsg::Video(id, track) => {
                    if self.viewers.contains_key(&id) {
                        self.emit(StationEvent::ViewerVideo { id, track });
                    }
                }
                StationMsg::Ice(id, c) => self.emit(StationEvent::Signal(
                    json!({ "type": "ice", "to": id, "candidate": candidate_json(&c) }),
                )),
            }
        }
        for (id, v) in self.viewers.drain() {
            retire(v.pc);
            let _ = self.ev.send(StationEvent::Left { id });
        }
        let _ = self.ev.send(StationEvent::Capture(false));
    }

    fn emit(&self, e: StationEvent) {
        let _ = self.ev.send(e);
    }

    fn broadcast(&self, f: &Value) {
        let Some(o) = f.as_object() else { return };
        for (id, v) in &self.viewers {
            if v.connected {
                let mut frame = o.clone();
                frame.insert("to".into(), json!(id));
                self.emit(StationEvent::Signal(Value::Object(frame)));
            }
        }
    }

    async fn on_signal(&mut self, f: &Value) {
        let ty = f["type"].as_str().unwrap_or("");
        let from = f["from"].as_str().unwrap_or("").to_owned();
        match ty {
            "offer" => self.on_offer(&from, f).await,
            "ice" => {
                let Some(c) = parse_candidate(&f["candidate"]) else {
                    return;
                };
                let Some(v) = self.viewers.get_mut(&from) else {
                    return;
                };
                if v.answered {
                    let pc = v.pc.clone();
                    let _ = pc.add_ice_candidate(c).await;
                } else if v.pending.len() < MAX_PENDING_ICE {
                    v.pending.push(c);
                }
            }
            "bye" => {
                if self.viewers.contains_key(&from) {
                    self.emit(StationEvent::Log(format!("call with {from}: hung up")));
                }
                self.remove(&from);
            }
            "peer-left" => self.remove(f["id"].as_str().unwrap_or("")),
            "" => {}
            _ if self.viewers.contains_key(&from) => self.emit(StationEvent::Control(f.clone())),
            _ => {}
        }
    }

    /// A call for a newly admitted Viewer, with the camera and microphone on
    /// it, its callbacks posting back to this actor.
    fn new_peer(&self, from: &str) -> Option<Arc<PeerConnection>> {
        let pc = self
            .factory
            .create_peer_connection(rtc_config(&self.cfg.media))
            .ok()?;
        let pc = Arc::new(pc);
        let (tx, id) = (self.me.clone(), from.to_owned());
        pc.on_connection_state_change(Some(Box::new(move |s| {
            let _ = tx.send(StationMsg::State(id.clone(), s));
        })));
        let (tx, id) = (self.me.clone(), from.to_owned());
        pc.on_ice_candidate(Some(Box::new(move |c| {
            let _ = tx.send(StationMsg::Ice(id.clone(), c));
        })));
        let (tx, id) = (self.me.clone(), from.to_owned());
        pc.on_track(Some(Box::new(move |ev| {
            let _ = tx.send(match ev.track {
                MediaStreamTrack::Audio(t) => StationMsg::Audio(id.clone(), t),
                MediaStreamTrack::Video(t) => StationMsg::Video(id.clone(), t),
            });
        })));
        // Tracks first, so applying the offer reuses their transceivers.
        let _ = pc.add_track(
            MediaStreamTrack::Video(self.video_track.clone()),
            &["tawny"],
        );
        if let Some(mic) = &self.mic {
            let _ = pc.add_track(MediaStreamTrack::Audio(mic.clone()), &["tawny"]);
        }
        Some(pc)
    }

    /// Whether an offer from a Viewer this Monitor already has is a new call
    /// rather than a renegotiation: a Viewer whose call died may place a new
    /// one under the same room id, on a new connection (a new certificate).
    /// Renegotiating the old call with it connects but leaves this end showing
    /// the old call's security code; it is taken as a new call instead.
    fn is_new_call(&self, from: &str, f: &Value) -> bool {
        let old = self
            .viewers
            .get(from)
            .and_then(|v| v.pc.current_remote_description())
            .and_then(|d| sas::sdp_fingerprint(&d.to_string()));
        let new = f["sdp"]["sdp"].as_str().and_then(sas::sdp_fingerprint);
        old.is_some() && new.is_some() && old != new
    }

    /// The pairing gate for a new call: its bond id, or the code it was
    /// paired with. A refused one is told why (`bye` / `expired`).
    fn admit(&mut self, from: &str, f: &Value) -> bool {
        let gate = self
            .cfg
            .gate
            .check(f["pid"].as_str(), f["pc"].as_str(), now_ms());
        if !gate.allowed() {
            self.emit(StationEvent::Log(format!(
                "offer refused: pairing code expired or never seen ({from})"
            )));
            self.emit(StationEvent::Signal(
                json!({ "type": "bye", "to": from, "reason": "expired" }),
            ));
            return false;
        }
        if gate == Gate::Enrolled {
            self.emit(StationEvent::Gate(self.cfg.gate.clone()));
        }
        true
    }

    async fn on_offer(&mut self, from: &str, f: &Value) {
        let mut known = self.viewers.contains_key(from);
        // A new call from a phone that has one: its seat is reused, and the
        // old call goes only once the new offer has passed the gate and
        // parsed (a refused one leaves the working call alone).
        let replacing = known && self.is_new_call(from, f);
        if replacing {
            known = false;
        }
        if !known && !replacing && self.viewers.len() >= MAX_VIEWERS {
            self.emit(StationEvent::Signal(
                json!({ "type": "bye", "to": from, "reason": "full", "max": MAX_VIEWERS }),
            ));
            return;
        }
        // The gate sits above anything that attaches the camera and mic. A
        // phone already let in this session (a renegotiation, an ICE restart)
        // passes.
        if !known && !self.admit(from, f) {
            return;
        }
        let Some(sdp) = f["sdp"]["sdp"].as_str() else {
            return;
        };
        let Ok(desc) = SessionDescription::parse(&without_cvo(sdp), SdpType::Offer) else {
            return;
        };
        if replacing {
            self.emit(StationEvent::Log(format!(
                "a new call from {from} replaces its old one"
            )));
            self.remove(from);
        }
        if !known {
            let Some(pc) = self.new_peer(from) else {
                return;
            };
            self.viewers.insert(
                from.to_owned(),
                ViewerPeer {
                    pc,
                    answered: false,
                    connected: false,
                    pending: Vec::new(),
                },
            );
            self.emit(StationEvent::Joined {
                id: from.to_owned(),
                label: clean_label(f["label"].as_str().unwrap_or("")),
                name: clean_name(f["name"].as_str().unwrap_or("")),
                pid: f["pid"]
                    .as_str()
                    .filter(|p| policy::is_bond_id(p))
                    .map(str::to_owned),
            });
        }
        let Some(pc) = self.viewers.get(from).map(|v| v.pc.clone()) else {
            return;
        };
        // A first offer that can't be answered gives its seat back (it
        // would never connect, and would count towards the Monitor's limit);
        // a later one (renegotiation) leaves the call it belongs to alone.
        let fresh = !known;
        if let Err(e) = pc.set_remote_description(desc).await {
            self.emit(StationEvent::Log(format!("setRemote: {e:?}")));
            if fresh {
                self.remove(from);
            }
            return;
        }
        for t in pc.transceivers() {
            if matches!(t.sender().track(), Some(MediaStreamTrack::Video(_))) {
                prefer_h264(&self.factory, &t, false);
            }
        }
        let Ok(answer) = pc.create_answer(AnswerOptions::default()).await else {
            if fresh {
                self.remove(from);
            }
            return;
        };
        let sdp = answer.to_string();
        if pc.set_local_description(answer).await.is_err() {
            if fresh {
                self.remove(from);
            }
            return;
        }
        let mut msg =
            json!({ "type": "answer", "to": from, "sdp": { "type": "answer", "sdp": sdp } });
        if let Some(sm) = sas::sign_sdp(&self.cfg.channel_key, "answer", &sdp) {
            msg["sm"] = json!(sm);
        }
        self.emit(StationEvent::Signal(msg));
        if let Some(v) = self.viewers.get_mut(from) {
            v.answered = true;
            for c in std::mem::take(&mut v.pending) {
                let _ = pc.add_ice_candidate(c).await;
            }
        }
        self.retune();
    }

    /// `tuneVideoSender()` for every Viewer.
    fn retune(&self) {
        let enc = policy::encoding(self.viewers.len(), self.dimmed);
        for v in self.viewers.values() {
            for s in v.pc.senders() {
                if !matches!(s.track(), Some(MediaStreamTrack::Video(_))) {
                    continue;
                }
                let mut p = s.parameters();
                if p.encodings.is_empty() {
                    continue;
                }
                p.encodings[0].max_bitrate = Some(enc.max_bitrate);
                p.encodings[0].max_framerate = Some(enc.max_framerate);
                p.encodings[0].scale_resolution_down_by = Some(enc.scale_down_by);
                p.set_degradation_preference(DegradationPreference::MaintainResolution);
                let _ = s.set_parameters(p);
            }
        }
    }

    fn on_state(&mut self, id: &str, s: PeerConnectionState) {
        match s {
            PeerConnectionState::Connected => {
                let Some(v) = self.viewers.get_mut(id) else {
                    return;
                };
                if v.connected {
                    return;
                }
                v.connected = true;
                let pc = v.pc.clone();
                self.emit(StationEvent::Connected { id: id.to_owned() });
                self.update_capture();
                let code = security_code(&pc, &self.cfg.channel_key);
                self.emit(StationEvent::Sas {
                    id: id.to_owned(),
                    code,
                });
            }
            PeerConnectionState::Failed | PeerConnectionState::Closed => {
                if self.viewers.contains_key(id) {
                    self.emit(StationEvent::Log(format!("call with {id}: {s:?}, let go")));
                }
                self.remove(id);
            }
            PeerConnectionState::Disconnected => {
                self.emit(StationEvent::Log(format!("call with {id}: Disconnected")));
            }
            _ => {}
        }
    }

    fn remove(&mut self, id: &str) {
        let Some(v) = self.viewers.remove(id) else {
            return;
        };
        retire(v.pc);
        self.emit(StationEvent::Left { id: id.to_owned() });
        self.update_capture();
        self.retune();
    }

    fn update_capture(&mut self) {
        let wanted = policy::capture_wanted(self.viewers.values().filter(|v| v.connected).count());
        if wanted != self.capturing {
            self.capturing = wanted;
            self.emit(StationEvent::Capture(wanted));
        }
    }
}

// ==================================================================== Viewer

/// How a Viewer is set up.
#[derive(Debug, Clone)]
pub struct ViewerConfig {
    /// The channel key (for the security code).
    pub channel_key: String,
    /// This phone's bond id with the Monitor (32 hex), once it has one.
    pub pid: Option<String>,
    /// The pairing code the user scanned, for the first time in.
    pub code: Option<String>,
    /// A short device name for the Monitor's list.
    pub label: String,
    /// The person's name, if they gave one.
    pub name: Option<String>,
    /// Send the microphone for talk-back, pushed through
    /// [`Viewer::audio_source`] (sent only while [`Viewer::set_talking`] is on).
    pub mic: bool,
    /// STUN/TURN.
    pub media: MediaConfig,
}

/// What a Viewer tells its app.
#[derive(Debug, Clone)]
pub enum ViewerEvent {
    /// Send this frame (it carries `to`).
    Signal(Value),
    /// The Monitor's video, for the platform's renderer.
    Video(RtcVideoTrack),
    /// The Monitor's sound, for the app to play.
    Audio(RtcAudioTrack),
    /// The call connected.
    Connected,
    /// The security code, or `None` when it can't be computed (fail closed).
    Sas(Option<String>),
    /// The Monitor turned us away (`expired`, `full`, …).
    Refused(String),
    /// The Monitor left or the call failed.
    Left,
    /// A control frame for the app (`meta`, `torch`, `battery`, `chime`, `cameras`, …).
    Control(Value),
    /// A diagnostics line.
    Log(String),
}

enum ViewerMsg {
    Signal(Value),
    Talking(bool),
    Camera(bool),
    Close,
    State(PeerConnectionState),
    Ice(IceCandidate),
    Track(MediaStreamTrack),
    /// A timer set by [`ViewerActor::kick_in`]; stale ones carry an old number.
    Kick(u64),
}

/// Talks to a running Viewer.
#[derive(Clone)]
pub struct Viewer {
    tx: mpsc::UnboundedSender<ViewerMsg>,
    audio: Option<NativeAudioSource>,
    video: NativeVideoSource,
}

impl Viewer {
    /// Starts a Viewer; it calls the Monitor as soon as the transport shows one.
    #[must_use]
    pub fn start(cfg: ViewerConfig) -> (Self, mpsc::UnboundedReceiver<ViewerEvent>) {
        let factory = PeerConnectionFactory::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let (ev, ev_rx) = mpsc::unbounded_channel();
        let audio = cfg.mic.then(mic_source);
        let mic = audio.as_ref().map(|a| {
            let t = factory.create_audio_track("talk", a.clone());
            t.set_enabled(false); // only while the user is pressing Talk
            t
        });
        let video = NativeVideoSource::new(
            VideoResolution {
                width: VIEWER_CAM.0,
                height: VIEWER_CAM.1,
            },
            false,
        );
        let cam = factory.create_video_track("mycam", video.clone());
        // What the slot carries while the camera is off: a track whose source
        // is never fed, so nothing goes out. (libwebrtc's `set_track(None)`
        // dereferences the missing track and crashes.)
        let idle_source = NativeVideoSource::new(
            VideoResolution {
                width: VIEWER_CAM.0,
                height: VIEWER_CAM.1,
            },
            false,
        );
        let idle = factory.create_video_track("mycam-off", idle_source);
        let actor = ViewerActor {
            factory,
            cfg,
            ev,
            me: tx.clone(),
            station: None,
            pc: None,
            mic,
            my: MyCamera {
                track: cam,
                idle,
                slot: None,
                on: false,
                offered: false,
            },
            answered: false,
            connected: false,
            pending: Vec::new(),
            kick: 0,
            restarts: 0,
            unanswered: 0,
        };
        tokio::spawn(actor.run(rx));
        (Self { tx, audio, video }, ev_rx)
    }

    /// Where the platform pushes this phone's camera frames for "My camera"
    /// (they go out only while [`Viewer::set_camera`] is on).
    #[must_use]
    pub fn video_source(&self) -> NativeVideoSource {
        self.video.clone()
    }

    /// "My camera": send this phone's camera to the Monitor, or stop.
    pub fn set_camera(&self, on: bool) {
        let _ = self.tx.send(ViewerMsg::Camera(on));
    }

    /// Where the platform pushes microphone frames for talk-back
    /// ([`AUDIO_RATE`], mono, [`AUDIO_FRAME`] samples each).
    #[must_use]
    pub fn audio_source(&self) -> Option<NativeAudioSource> {
        self.audio.clone()
    }

    /// A frame from the transport.
    pub fn signal(&self, frame: Value) {
        let _ = self.tx.send(ViewerMsg::Signal(frame));
    }

    /// Talk-back on or off (the mic track is only enabled while talking).
    pub fn set_talking(&self, on: bool) {
        let _ = self.tx.send(ViewerMsg::Talking(on));
    }

    /// Hangs up.
    pub fn close(&self) {
        let _ = self.tx.send(ViewerMsg::Close);
    }
}

/// "My camera": this phone's camera and the call's video slot.
struct MyCamera {
    track: RtcVideoTrack,
    /// What the slot carries while the camera is off (see `Viewer::start`).
    idle: RtcVideoTrack,
    /// The call's video slot: the Monitor's picture in, this camera out.
    slot: Option<RtpTransceiver>,
    /// Whether the camera is going out.
    on: bool,
    /// What the last offer told the Monitor.
    offered: bool,
}

struct ViewerActor {
    factory: PeerConnectionFactory,
    cfg: ViewerConfig,
    ev: mpsc::UnboundedSender<ViewerEvent>,
    me: mpsc::UnboundedSender<ViewerMsg>,
    station: Option<String>,
    pc: Option<Arc<PeerConnection>>,
    mic: Option<RtcAudioTrack>,
    my: MyCamera,
    answered: bool,
    connected: bool,
    pending: Vec<IceCandidate>,
    /// The current kick timer's number (a newer one, or a reconnect, voids
    /// the ones before).
    kick: u64,
    /// ICE restarts tried since the call was last up.
    restarts: u32,
    /// Kicks that found the last offer still unanswered.
    unanswered: u32,
}

/// As for the Monitor: a dropped Viewer parks its call.
impl Drop for ViewerActor {
    fn drop(&mut self) {
        if let Some(pc) = self.pc.take() {
            retire(pc);
        }
    }
}

impl ViewerActor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ViewerMsg>) {
        while let Some(m) = rx.recv().await {
            match m {
                ViewerMsg::Signal(f) => self.on_signal(&f).await,
                ViewerMsg::Talking(on) => {
                    if let Some(m) = &self.mic {
                        m.set_enabled(on);
                    }
                    if let Some(to) = &self.station {
                        let _ = self.ev.send(ViewerEvent::Signal(
                            json!({ "type": "talking", "to": to, "on": on }),
                        ));
                    }
                }
                ViewerMsg::Camera(on) => {
                    self.my.on = on;
                    self.attach_camera();
                    self.reoffer().await;
                }
                ViewerMsg::Close => break,
                ViewerMsg::State(s) => self.on_state(s),
                ViewerMsg::Kick(n) => self.on_kick(n).await,
                ViewerMsg::Ice(c) => {
                    if let Some(to) = &self.station {
                        let _ = self.ev.send(ViewerEvent::Signal(
                            json!({ "type": "ice", "to": to, "candidate": candidate_json(&c) }),
                        ));
                    }
                }
                ViewerMsg::Track(MediaStreamTrack::Video(t)) => {
                    let _ = self.ev.send(ViewerEvent::Video(t));
                }
                ViewerMsg::Track(MediaStreamTrack::Audio(t)) => {
                    let _ = self.ev.send(ViewerEvent::Audio(t));
                }
            }
        }
        if let Some(pc) = self.pc.take() {
            retire(pc);
        }
    }

    async fn on_signal(&mut self, f: &Value) {
        let ty = f["type"].as_str().unwrap_or("");
        match ty {
            "welcome" => {
                let station = f["peers"]
                    .as_array()
                    .and_then(|p| p.iter().find(|x| x["role"] == "station"))
                    .and_then(|x| x["id"].as_str());
                if let Some(id) = station.map(str::to_owned) {
                    self.call(&id).await;
                }
            }
            "peer-joined" if f["role"] == "station" => {
                if let Some(id) = f["id"].as_str().map(str::to_owned) {
                    self.hang_up();
                    self.call(&id).await;
                }
            }
            "peer-left" if f["id"].as_str() == self.station.as_deref() => {
                self.hang_up();
                let _ = self.ev.send(ViewerEvent::Left);
            }
            _ if f["from"].as_str() != self.station.as_deref() => {}
            "answer" => {
                let (Some(pc), Some(sdp)) = (self.pc.clone(), f["sdp"]["sdp"].as_str()) else {
                    return;
                };
                let Ok(desc) = SessionDescription::parse(sdp, SdpType::Answer) else {
                    return;
                };
                if let Err(e) = pc.set_remote_description(desc).await {
                    let _ = self.ev.send(ViewerEvent::Log(format!("setRemote: {e:?}")));
                    return;
                }
                self.answered = true;
                for c in std::mem::take(&mut self.pending) {
                    let _ = pc.add_ice_candidate(c).await;
                }
                // The camera was switched while this answer was on its way.
                self.reoffer().await;
            }
            "ice" => {
                let Some(c) = parse_candidate(&f["candidate"]) else {
                    return;
                };
                match (&self.pc, self.answered) {
                    (Some(pc), true) => {
                        let _ = pc.add_ice_candidate(c).await;
                    }
                    _ if self.pending.len() < MAX_PENDING_ICE => self.pending.push(c),
                    _ => {}
                }
            }
            "bye" => {
                let reason = f["reason"].as_str().unwrap_or("").to_owned();
                self.hang_up();
                let _ = self.ev.send(ViewerEvent::Refused(reason));
            }
            _ => {
                let _ = self.ev.send(ViewerEvent::Control(f.clone()));
            }
        }
    }

    /// Puts this camera on the call's video slot, or takes it off. No new
    /// offer is needed: the slot was offered both ways from the start.
    fn attach_camera(&self) {
        if let Some(t) = &self.my.slot {
            let track = if self.my.on {
                &self.my.track
            } else {
                &self.my.idle
            };
            if let Err(e) = t
                .sender()
                .set_track(Some(MediaStreamTrack::Video(track.clone())))
            {
                let _ = self.ev.send(ViewerEvent::Log(format!("my camera: {e:?}")));
            }
        }
    }

    fn hang_up(&mut self) {
        self.my.slot = None;
        if let Some(pc) = self.pc.take() {
            retire(pc);
        }
        self.answered = false;
        self.connected = false;
        self.pending.clear();
        // The next call starts with its own repairs: none of this one's
        // counts, and no timer of this one's left to fire on it.
        self.restarts = 0;
        self.unanswered = 0;
        self.kick = self.kick.wrapping_add(1);
    }

    async fn call(&mut self, station: &str) {
        if self.pc.is_some() {
            return;
        }
        self.station = Some(station.to_owned());
        let Ok(pc) = self
            .factory
            .create_peer_connection(rtc_config(&self.cfg.media))
        else {
            return;
        };
        let pc = Arc::new(pc);
        let tx = self.me.clone();
        pc.on_connection_state_change(Some(Box::new(move |s| {
            let _ = tx.send(ViewerMsg::State(s));
        })));
        let tx = self.me.clone();
        pc.on_ice_candidate(Some(Box::new(move |c| {
            let _ = tx.send(ViewerMsg::Ice(c));
        })));
        let tx = self.me.clone();
        pc.on_track(Some(Box::new(move |ev| {
            let _ = tx.send(ViewerMsg::Track(ev.track));
        })));
        if let Some(m) = &self.mic {
            let _ = pc.add_track(MediaStreamTrack::Audio(m.clone()), &["talk"]);
        } else {
            let init = RtpTransceiverInit {
                direction: RtpTransceiverDirection::RecvOnly,
                stream_ids: vec![],
                send_encodings: vec![],
            };
            let _ = pc.add_transceiver_for_media(MediaType::Audio, init);
        }
        // Both ways: the Monitor's picture comes in on it, and "My camera"
        // goes out on it when switched on (nothing is sent until then).
        let init = RtpTransceiverInit {
            direction: RtpTransceiverDirection::SendRecv,
            stream_ids: vec!["mycam".to_owned()],
            send_encodings: vec![],
        };
        if let Ok(t) = pc.add_transceiver_for_media(MediaType::Video, init) {
            // The receiver's order decides what the Monitor sends: H.264 first
            // is what gets its hardware encoder used.
            prefer_h264(&self.factory, &t, true);
            self.my.slot = Some(t);
            if self.my.on {
                self.attach_camera();
            }
        }
        let Ok(offer) = pc.create_offer(OFFER).await else {
            return;
        };
        let sdp = without_cvo(&camera_direction(&offer.to_string(), self.my.on));
        if pc.set_local_description(offer).await.is_err() {
            return;
        }
        self.my.offered = self.my.on;
        self.pc = Some(pc);
        let msg = self.offer_msg(station, &sdp);
        let _ = self.ev.send(ViewerEvent::Signal(msg));
    }

    fn offer_msg(&self, station: &str, sdp: &str) -> Value {
        let mut msg = json!({ "type": "offer", "to": station, "sdp": { "type": "offer", "sdp": sdp }, "label": clean_label(&self.cfg.label) });
        if let Some(sm) = sas::sign_sdp(&self.cfg.channel_key, "offer", sdp) {
            msg["sm"] = json!(sm);
        }
        if let Some(pid) = self.cfg.pid.as_ref().filter(|p| policy::is_bond_id(p)) {
            msg["pid"] = json!(pid);
        }
        if let Some(code) = &self.cfg.code {
            msg["pc"] = json!(code);
        }
        if let Some(name) = &self.cfg.name {
            msg["name"] = json!(clean_name(name));
        }
        msg
    }

    /// "My camera" changed on a call that is up: offer again, so the Monitor
    /// hears the slot's new direction (its picture of us goes on or off).
    async fn reoffer(&mut self) {
        let (Some(pc), Some(station)) = (self.pc.clone(), self.station.clone()) else {
            return;
        };
        if !self.answered || self.my.offered == self.my.on {
            return;
        }
        let Ok(offer) = pc.create_offer(OFFER).await else {
            return;
        };
        let sdp = without_cvo(&camera_direction(&offer.to_string(), self.my.on));
        if let Err(e) = pc.set_local_description(offer).await {
            let _ = self
                .ev
                .send(ViewerEvent::Log(format!("my camera offer: {e:?}")));
            return;
        }
        self.my.offered = self.my.on;
        self.answered = false;
        let msg = self.offer_msg(&station, &sdp);
        let _ = self.ev.send(ViewerEvent::Signal(msg));
    }

    fn on_state(&mut self, s: PeerConnectionState) {
        match s {
            PeerConnectionState::Connected => {
                // Up (again): any restart in hand is done.
                self.kick = self.kick.wrapping_add(1);
                self.restarts = 0;
                self.unanswered = 0;
                if self.connected {
                    return;
                }
                self.connected = true;
                let _ = self.ev.send(ViewerEvent::Connected);
                let Some(pc) = self.pc.clone() else { return };
                let code = security_code(&pc, &self.cfg.channel_key);
                let _ = self.ev.send(ViewerEvent::Sas(code));
            }
            // As app.js: the Viewer drives the repair. The Monitor may still
            // be in the room (its signalling never dropped), so nothing else
            // would ever call again; hanging up here left a Viewer on
            // "Reconnecting" for good.
            PeerConnectionState::Disconnected => self.kick_in(ICE_KICK),
            PeerConnectionState::Failed => self.kick_in(Duration::ZERO),
            _ => {}
        }
    }

    /// Looks at the call again after `after`, unless it comes back first.
    fn kick_in(&mut self, after: Duration) {
        self.kick = self.kick.wrapping_add(1);
        let n = self.kick;
        let tx = self.me.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(ViewerMsg::Kick(n));
        });
    }

    /// The call is still down: restart ICE on it, and after
    /// [`MAX_RESTARTS`] place a new call to the same Monitor.
    async fn on_kick(&mut self, n: u64) {
        if n != self.kick {
            return;
        }
        let Some(station) = self.station.clone() else {
            return;
        };
        let Some(pc) = self.pc.clone() else {
            // The last new call couldn't even be set up: try again.
            self.call(&station).await;
            self.kick_in(RESTART_WAIT);
            return;
        };
        let state = pc.connection_state();
        if state == PeerConnectionState::Connected {
            return;
        }
        if !self.answered && self.unanswered < MAX_UNANSWERED {
            // The Monitor hasn't answered the last offer: it is away. It
            // answers that one when it is back; another would only queue up.
            self.unanswered += 1;
            self.kick_in(RESTART_WAIT);
            return;
        }
        let stuck = !matches!(
            state,
            PeerConnectionState::Disconnected | PeerConnectionState::Failed
        );
        if stuck && self.restarts < MAX_RESTARTS {
            // Still connecting (a slow network, a new call): give it time,
            // as many waits as a broken call gets restarts.
            self.restarts += 1;
            self.kick_in(RESTART_WAIT);
            return;
        }
        if self.restarts >= MAX_RESTARTS || self.unanswered >= MAX_UNANSWERED || stuck {
            let _ = self.ev.send(ViewerEvent::Log(format!(
                "the call didn't come back ({state:?}): calling the Monitor again"
            )));
            self.hang_up();
            let _ = self.ev.send(ViewerEvent::Left);
            self.call(&station).await;
            self.kick_in(RESTART_WAIT);
            return;
        }
        self.restarts += 1;
        let _ = self.ev.send(ViewerEvent::Log(format!(
            "call {:?}: ICE restart {}",
            pc.connection_state(),
            self.restarts
        )));
        let restart = OfferOptions {
            ice_restart: true,
            ..OFFER
        };
        // A restart that can't be made now is tried again, never dropped:
        // nothing else would look at a Failed call again.
        let Ok(offer) = pc.create_offer(restart).await else {
            self.kick_in(RESTART_WAIT);
            return;
        };
        let sdp = without_cvo(&camera_direction(&offer.to_string(), self.my.on));
        if pc.set_local_description(offer).await.is_err() {
            self.kick_in(RESTART_WAIT);
            return;
        }
        self.my.offered = self.my.on;
        self.answered = false;
        let msg = self.offer_msg(&station, &sdp);
        let _ = self.ev.send(ViewerEvent::Signal(msg));
        self.kick_in(RESTART_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::{camera_direction, clean_label, without_cvo};

    #[test]
    fn a_long_device_name_is_cut_at_a_word() {
        assert_eq!(
            clean_label("Motorola moto g 5G (2023)"),
            "Motorola moto g 5G"
        );
        assert_eq!(clean_label("  Pixel 8  "), "Pixel 8");
        assert_eq!(
            clean_label("Samsung SM-S918B\u{202E}x"),
            "Samsung SM-S918Bx"
        );
        assert_eq!(
            clean_label("Abcdefghijklmnopqrstuvwxyz"),
            "Abcdefghijklmnopqrst"
        );
        assert_eq!(
            clean_label("Twelve chars twelve chars"),
            "Twelve chars twelve"
        );
    }

    const SDP: &str = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n\
                       m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\na=sendrecv\r\n";

    #[test]
    fn camera_off_says_recvonly_on_the_video_slot_only() {
        let off = camera_direction(SDP, false);
        assert_eq!(
            off,
            "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\na=recvonly\r\n"
        );
        assert_eq!(camera_direction(SDP, true), SDP, "camera on: unchanged");
    }

    #[test]
    fn the_orientation_extension_is_left_out() {
        let offer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     a=extmap:3 urn:3gpp:video-orientation\r\n\
                     a=extmap:4 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
                     a=sendrecv\r\n";
        assert_eq!(
            without_cvo(offer),
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
             a=extmap:4 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
             a=sendrecv\r\n"
        );
        assert_eq!(without_cvo(SDP), SDP, "nothing else changes");
    }
}
