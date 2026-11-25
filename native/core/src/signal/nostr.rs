//! Tawny signalling over Nostr, as a state machine with no I/O.
//!
//! A port of `NostrSocket` in `public/nostr/transport.js`. To the call layer
//! it looks like the room relay: it says `welcome`, announces `peer-joined` and
//! `peer-left`, and delivers addressed frames (`offer`, `answer`, `ice`, …) with
//! `from` filled in. Underneath, every frame is a NIP-59 ephemeral gift wrap
//! (21059 → seal 13 → rumor 25050).
//!
//! Membership: a Handheld sends `join` with HMAC(channel key, its key |
//! Monitor key | time); the Monitor answers `here` and from then on accepts
//! only members' frames. A Handheld accepts only frames sealed by the
//! Monitor's key. Every message must be fresh (90 s past, 30 s future) and is
//! handled once.
//!
//! Presence: a Handheld sends `join` every 3 s until its Monitor answers, then
//! every 15 s. A member silent for 50 s is reported as `peer-left`; `leave`
//! says it at once. A Monitor coming up says `here` to the phones it already
//! knows.
//!
//! The caller owns the relays and the clock: it feeds in wraps with
//! [`Session::receive`], frames with [`Session::send`], and calls
//! [`Session::tick`] about once a second, then carries out the [`Output`]s.

use std::collections::{HashMap, HashSet, VecDeque};

use hmac::Mac;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::ids::hmac_for;
use crate::nostr::event::{self, Event, Template};
use crate::nostr::nip59::{self, KIND_EPHEMERAL_GIFT_WRAP};

/// The rumor kind of a Tawny signalling message.
pub const KIND_SIGNAL: u16 = 25_050;
/// The frames a room relay forwards (`rendezvous/protocol.js` RELAY).
pub const RELAYED: [&str; 12] = [
    "offer",
    "answer",
    "ice",
    "bye",
    "chime",
    "chime-ack",
    "talking",
    "cameras",
    "meta",
    "camera-control",
    "torch",
    "battery",
];
const MAX_SEEN: usize = 4000;
const HEARTBEAT_MS: u64 = 15_000;
const SEEK_MS: u64 = 3_000;
/// A Handheld that joined this recently doesn't join again when the Monitor
/// says `here` (that `here` is the answer to it).
const REJOIN_GAP_MS: u64 = 1_000;

/// How many Handheld keys a Monitor remembers.
pub const MAX_REMEMBERED: usize = 8;
const PEER_TIMEOUT_MS: u64 = 50_000;
const FRESH_PAST_S: i64 = 90;
const FRESH_FUTURE_S: i64 = 30;
const SEEN_TTL_MS: u64 = 5 * 60 * 1000;
const ICE_BATCH_MS: u64 = 150;
const WRAP_EXPIRY_S: u64 = 120;
const MAX_FRAMES: usize = 40;

/// Which end of the channel this device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The Monitor (the camera).
    Station,
    /// A Handheld (a phone watching).
    Viewer,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Viewer => "viewer",
        }
    }

    fn accepts(self, t: &str) -> bool {
        match self {
            Self::Station => matches!(t, "join" | "leave" | "sig"),
            Self::Viewer => matches!(t, "here" | "leave" | "sig"),
        }
    }
}

/// What the caller has to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Publish this wrap to every usable relay.
    Publish(Event),
    /// Hand this frame to the call layer (`welcome`, `peer-joined`,
    /// `peer-left`, or a relayed frame with `from`).
    Emit(Value),
    /// Persist the Monitor's list of known Handheld keys.
    Remember(Vec<String>),
    /// A message was refused (the code matches `transport.js`).
    Reject {
        /// Short code.
        code: &'static str,
        /// What was wrong.
        reason: String,
    },
    /// Something worth a diagnostics line.
    Log(String),
}

/// Why a session could not be made.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// The secret key is unusable.
    #[error("bad secret key")]
    Key,
    /// A Handheld was not given a valid Monitor public key.
    #[error("a Handheld needs the Monitor's public key")]
    NoStation,
    /// The system RNG failed.
    #[error("random number generator failed")]
    Random,
}

/// What a session is made from. Keys are stored by the caller.
pub struct Config {
    /// The pairing's channel key; it never leaves the device.
    pub channel_key: String,
    /// This device's role.
    pub role: Role,
    /// This device's Nostr secret key for the channel.
    pub secret: [u8; 32],
    /// Handheld: the Monitor's public key. Ignored on the Monitor.
    pub station_pub: String,
    /// Monitor: Handheld keys that have joined before.
    pub remembered: Vec<String>,
}

#[derive(Debug, Clone)]
struct Member {
    role: Role,
    last_seen: u64,
    session: String,
}

#[derive(Default)]
struct Seen {
    expiry: HashMap<String, u64>,
    order: VecDeque<String>,
}

impl Seen {
    fn has(&self, id: &str) -> bool {
        self.expiry.contains_key(id)
    }

    fn add(&mut self, id: &str, now: u64) {
        self.expiry.insert(id.to_owned(), now + SEEN_TTL_MS);
        self.order.push_back(id.to_owned());
        while self.expiry.len() > MAX_SEEN {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.expiry.remove(&old);
        }
    }

    fn sweep(&mut self, now: u64) {
        self.expiry.retain(|_, exp| *exp >= now);
        let live: HashSet<&String> = self.expiry.keys().collect();
        let keep: VecDeque<String> = self
            .order
            .iter()
            .filter(|k| live.contains(k))
            .cloned()
            .collect();
        self.order = keep;
    }
}

struct Outbox {
    frames: Vec<Value>,
    deadline: Option<u64>,
}

/// One device's end of a Nostr signalling channel.
pub struct Session {
    channel_key: String,
    role: Role,
    secret: [u8; 32],
    me: String,
    station_pub: String,
    instance: String,
    members: HashMap<String, Member>,
    outbox: HashMap<String, Outbox>,
    seen_wraps: Seen,
    seen_rumors: Seen,
    remembered: Vec<String>,
    open: bool,
    closed: bool,
    last_join: u64,
}

fn is_hex32(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh 8-byte random nonce in hex.
///
/// # Errors
/// [`SessionError::Random`] when the system RNG fails.
pub fn nonce() -> Result<String, SessionError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|_| SessionError::Random)?;
    Ok(hex::encode(b))
}

/// `joinProof()`: HMAC-SHA256(channel key, `"tawny-nostr-join-v1|" + handheld + "|" + station + "|" + at`) in hex.
#[must_use]
pub fn join_proof(channel_key: &str, handheld: &str, station: &str, at: u64) -> String {
    let mut mac = hmac_for(channel_key);
    mac.update(format!("tawny-nostr-join-v1|{handheld}|{station}|{at}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn proof_matches(channel_key: &str, handheld: &str, station: &str, at: u64, proof: &str) -> bool {
    let Ok(given) = hex::decode(proof) else {
        return false;
    };
    let mut mac = hmac_for(channel_key);
    mac.update(format!("tawny-nostr-join-v1|{handheld}|{station}|{at}").as_bytes());
    mac.verify_slice(&given).is_ok()
}

impl Session {
    /// Makes a session. Nothing is sent until [`Session::open`].
    ///
    /// # Errors
    /// [`SessionError`] for an unusable key, or a Handheld without the Monitor's key.
    pub fn new(cfg: Config) -> Result<Self, SessionError> {
        let me = event::public_key(&cfg.secret).map_err(|_| SessionError::Key)?;
        let station_pub = match cfg.role {
            Role::Station => me.clone(),
            Role::Viewer => {
                let s = cfg.station_pub.to_lowercase();
                if !is_hex32(&s) {
                    return Err(SessionError::NoStation);
                }
                s
            }
        };
        let mut remembered: Vec<String> =
            cfg.remembered.into_iter().filter(|k| is_hex32(k)).collect();
        if remembered.len() > MAX_REMEMBERED {
            remembered.drain(..remembered.len() - MAX_REMEMBERED);
        }
        Ok(Self {
            channel_key: cfg.channel_key,
            role: cfg.role,
            secret: cfg.secret,
            me,
            station_pub,
            instance: nonce()?,
            members: HashMap::new(),
            outbox: HashMap::new(),
            seen_wraps: Seen::default(),
            seen_rumors: Seen::default(),
            remembered,
            open: false,
            closed: false,
            last_join: 0,
        })
    }

    /// This device's public key on the channel.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.me
    }

    /// Call once a relay is usable: says `welcome`, and starts looking for
    /// the other end.
    pub fn open(&mut self, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        if self.open || self.closed {
            return out;
        }
        self.open = true;
        out.push(Output::Emit(
            json!({ "type": "welcome", "id": self.me, "peers": [] }),
        ));
        match self.role {
            Role::Viewer => self.join(now_ms, &mut out),
            Role::Station => {
                for pub_key in self.remembered.clone() {
                    let body = json!({ "s": self.instance });
                    self.wrap_to(&pub_key, "here", body, now_ms / 1000, &mut out);
                }
            }
        }
        out
    }

    /// Queues a frame from the call layer (it carries `to`). ICE is batched
    /// for up to 150 ms; anything else goes at once, behind what was queued.
    pub fn send(&mut self, frame: &Value, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.open || self.closed {
            return out;
        }
        let Some(obj) = frame.as_object() else {
            return out;
        };
        let Some(ty) = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| RELAYED.contains(t))
        else {
            return out;
        };
        let Some(to) = obj.get("to").and_then(Value::as_str).map(str::to_owned) else {
            return out;
        };
        if !self.members.contains_key(&to) {
            let short: String = to.chars().take(8).collect();
            out.push(Output::Log(format!(
                "nostr: dropped {ty} to non-member {short}"
            )));
            return out;
        }
        let is_ice = ty == "ice";
        let mut f: Map<String, Value> = obj.clone();
        f.remove("to");
        let box_ = self.outbox.entry(to.clone()).or_insert(Outbox {
            frames: Vec::new(),
            deadline: None,
        });
        box_.frames.push(Value::Object(f));
        if !is_ice || box_.frames.len() >= MAX_FRAMES {
            self.flush(&to, now_ms, &mut out);
        } else if box_.deadline.is_none() {
            box_.deadline = Some(now_ms + ICE_BATCH_MS);
        }
        out
    }

    /// Call about once a second (and when an ICE batch is due): presence,
    /// timeouts, batched ICE, expiry of replay memory.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.open || self.closed {
            return out;
        }
        let due: Vec<String> = self
            .outbox
            .iter()
            .filter(|(_, b)| b.deadline.is_some_and(|d| d <= now_ms))
            .map(|(k, _)| k.clone())
            .collect();
        for to in due {
            self.flush(&to, now_ms, &mut out);
        }
        if self.role == Role::Viewer {
            let found = self.members.contains_key(&self.station_pub);
            let every = if found { HEARTBEAT_MS } else { SEEK_MS };
            if now_ms.saturating_sub(self.last_join) >= every {
                self.join(now_ms, &mut out);
            }
        }
        let gone: Vec<String> = self
            .members
            .iter()
            .filter(|(_, m)| now_ms.saturating_sub(m.last_seen) > PEER_TIMEOUT_MS)
            .map(|(k, _)| k.clone())
            .collect();
        for pub_key in gone {
            if let Some(m) = self.members.remove(&pub_key) {
                let short: String = pub_key.chars().take(8).collect();
                out.push(Output::Log(format!(
                    "nostr: {} {short} went quiet",
                    m.role.as_str()
                )));
                out.push(Output::Emit(json!({ "type": "peer-left", "id": pub_key })));
            }
        }
        self.seen_wraps.sweep(now_ms);
        self.seen_rumors.sweep(now_ms);
        out
    }

    /// Says `leave` to every member and stops.
    pub fn close(&mut self, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        if self.closed {
            return out;
        }
        let members: Vec<String> = self.members.keys().cloned().collect();
        for m in members {
            self.wrap_to(&m, "leave", json!({}), now_ms / 1000, &mut out);
        }
        self.closed = true;
        self.open = false;
        out
    }

    fn join(&mut self, now_ms: u64, out: &mut Vec<Output>) {
        self.last_join = now_ms;
        let at = now_ms / 1000;
        let proof = join_proof(&self.channel_key, &self.me, &self.station_pub, at);
        let station = self.station_pub.clone();
        self.wrap_to(
            &station,
            "join",
            json!({ "proof": proof, "s": self.instance }),
            at,
            out,
        );
    }

    fn flush(&mut self, to: &str, now_ms: u64, out: &mut Vec<Output>) {
        loop {
            let Some(box_) = self.outbox.get_mut(to) else {
                return;
            };
            box_.deadline = None;
            if box_.frames.is_empty() {
                return;
            }
            let n = box_.frames.len().min(MAX_FRAMES);
            let frames: Vec<Value> = box_.frames.drain(..n).collect();
            self.wrap_to(to, "sig", json!({ "f": frames }), now_ms / 1000, out);
        }
    }

    fn wrap_to(&self, to: &str, ty: &str, mut body: Value, created_at: u64, out: &mut Vec<Output>) {
        // `n`: a nonce, so two identical frames in the same second don't share
        // a rumor id (the second would be dropped as a replay).
        let Ok(n) = nonce() else {
            out.push(Output::Log("nostr: random number generator failed".into()));
            return;
        };
        if let Some(o) = body.as_object_mut() {
            o.insert("n".into(), Value::String(n));
        }
        let t = Template {
            kind: KIND_SIGNAL,
            tags: vec![vec!["p".into(), to.into()], vec!["t".into(), ty.into()]],
            content: body.to_string(),
            created_at,
        };
        match nip59::gift_wrap(
            t,
            &self.secret,
            to,
            KIND_EPHEMERAL_GIFT_WRAP,
            created_at,
            Some(WRAP_EXPIRY_S),
        ) {
            Ok(w) => out.push(Output::Publish(w)),
            Err(e) => out.push(Output::Log(format!("nostr: could not wrap: {e}"))),
        }
    }

    fn reject(out: &mut Vec<Output>, code: &'static str, reason: impl Into<String>) {
        out.push(Output::Reject {
            code,
            reason: reason.into(),
        });
    }

    /// Handles a wrap from a relay. `relays` is how many relays this session
    /// is on: the same wrap twice from one relay is a replay, from two it's normal.
    #[expect(
        clippy::too_many_lines,
        reason = "one check after another, in transport.js's order"
    )]
    pub fn receive(&mut self, wrap: &Value, relays: usize, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        // Nothing before `open`: anything handled now could start a call
        // whose first frames `send` would drop (it sends nothing until open),
        // and the same peer saying `here` again after open would not start it
        // again. What arrives early is a Monitor's `here` from a moment ago
        // (fresh enough, but before our `join`); the `join` sent at `open`
        // gets a new one. Not marked seen, so a relay's later copy still counts.
        if self.closed || !self.open {
            return out;
        }
        let Some(wrap_id) = wrap.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return out;
        };
        if self.seen_wraps.has(&wrap_id) {
            if relays == 1 {
                Self::reject(
                    &mut out,
                    "replay",
                    "exact copy of a message already handled",
                );
            }
            return out;
        }
        self.seen_wraps.add(&wrap_id, now_ms);
        let (seal, rumor) = match nip59::unwrap(wrap, &self.secret) {
            Ok(x) => x,
            Err(e) => {
                Self::reject(&mut out, e.code(), e.to_string());
                return out;
            }
        };
        let from = seal.pubkey;
        if rumor.kind != KIND_SIGNAL {
            Self::reject(&mut out, "wrong-kind", format!("rumor kind {}", rumor.kind));
            return out;
        }
        if !rumor.tag_values("p").any(|p| p == self.me) {
            Self::reject(
                &mut out,
                "misaddressed",
                "sealed for us, addressed to someone else",
            );
            return out;
        }
        let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
        let age = now_secs - i64::try_from(rumor.created_at).unwrap_or(i64::MAX);
        if age > FRESH_PAST_S {
            Self::reject(&mut out, "stale", format!("written {age}s ago"));
            return out;
        }
        if age < -FRESH_FUTURE_S {
            Self::reject(&mut out, "future", "dated in the future");
            return out;
        }
        if self.seen_rumors.has(&rumor.id) {
            Self::reject(&mut out, "replay", "message already handled");
            return out;
        }
        let ty = rumor.tag_values("t").next().unwrap_or("").to_owned();
        // Our own relay test, come back through another relay: not for us here.
        if ty == "ping" && from == self.me {
            return out;
        }
        if !matches!(ty.as_str(), "join" | "here" | "leave" | "sig") {
            Self::reject(&mut out, "bad-type", format!("unknown type {ty}"));
            return out;
        }
        if !self.role.accepts(&ty) {
            Self::reject(
                &mut out,
                "bad-type",
                format!("{ty} is not something a {} is told", self.role.as_str()),
            );
            return out;
        }
        let body: Value = serde_json::from_str(&rumor.content).unwrap_or(Value::Null);
        let Some(body) = body.as_object() else {
            Self::reject(&mut out, "bad-body", "not an object");
            return out;
        };

        // Who may say this.
        match self.role {
            Role::Station if ty == "join" => {
                let Some(proof) = body
                    .get("proof")
                    .and_then(Value::as_str)
                    .filter(|p| is_hex32(p))
                else {
                    Self::reject(&mut out, "bad-proof", "join without a proof");
                    return out;
                };
                if !proof_matches(&self.channel_key, &from, &self.me, rumor.created_at, proof) {
                    Self::reject(
                        &mut out,
                        "bad-proof",
                        "join proof is wrong: sender does not hold this channel",
                    );
                    return out;
                }
            }
            Role::Station if !self.members.contains_key(&from) => {
                Self::reject(
                    &mut out,
                    "not-member",
                    format!("{ty} from a key that never joined"),
                );
                return out;
            }
            Role::Viewer if from != self.station_pub => {
                Self::reject(
                    &mut out,
                    "not-monitor",
                    format!("{ty} sealed by a key that is not this channel's Monitor"),
                );
                return out;
            }
            _ => {}
        }

        self.seen_rumors.add(&rumor.id, now_ms);
        let session: String = body
            .get("s")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(16).collect())
            .unwrap_or_default();
        let mut known = if let Some(m) = self.members.get_mut(&from) {
            m.last_seen = now_ms;
            Some(m.session.clone())
        } else {
            None
        };
        if let Some(prev) = &known {
            if (ty == "join" || ty == "here") && !session.is_empty() && *prev != session {
                // Same key, new session: it restarted. Say it left and came back.
                self.members.remove(&from);
                out.push(Output::Emit(json!({ "type": "peer-left", "id": from })));
                known = None;
            }
        }
        match ty.as_str() {
            "join" => {
                if known.is_none() {
                    self.remember(&from, &mut out);
                }
                self.members.insert(
                    from.clone(),
                    Member {
                        role: Role::Viewer,
                        last_seen: now_ms,
                        session,
                    },
                );
                let here = json!({ "s": self.instance });
                self.wrap_to(&from, "here", here, now_ms / 1000, &mut out);
                if known.is_none() {
                    out.push(Output::Emit(
                        json!({ "type": "peer-joined", "id": from, "role": "viewer" }),
                    ));
                }
            }
            "here" => {
                if known.is_none() {
                    self.members.insert(
                        from.clone(),
                        Member {
                            role: Role::Station,
                            last_seen: now_ms,
                            session,
                        },
                    );
                    // A Monitor newly here (it restarted, or came back after
                    // going quiet) doesn't know this phone yet. Join it now,
                    // before the call that `peer-joined` starts: otherwise
                    // the offer reaches it ahead of the next heartbeat's
                    // join, is refused as not-member, and the call waits on
                    // an answer that never comes. (A `here` that answers
                    // the join just sent needs none.)
                    if now_ms.saturating_sub(self.last_join) >= REJOIN_GAP_MS {
                        self.join(now_ms, &mut out);
                    }
                    out.push(Output::Emit(
                        json!({ "type": "peer-joined", "id": from, "role": "station" }),
                    ));
                }
            }
            "leave" => {
                if known.is_some() {
                    self.members.remove(&from);
                    out.push(Output::Emit(json!({ "type": "peer-left", "id": from })));
                }
            }
            "sig" => {
                let frames = body
                    .get("f")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                for f in frames.iter().take(MAX_FRAMES) {
                    let Some(o) = f.as_object() else { continue };
                    if !o
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|t| RELAYED.contains(&t))
                    {
                        continue;
                    }
                    let mut o = o.clone();
                    o.insert("from".into(), Value::String(from.clone()));
                    out.push(Output::Emit(Value::Object(o)));
                }
            }
            _ => {}
        }
        out
    }

    fn remember(&mut self, pub_key: &str, out: &mut Vec<Output>) {
        self.remembered.retain(|k| k != pub_key);
        self.remembered.push(pub_key.to_owned());
        if self.remembered.len() > MAX_REMEMBERED {
            self.remembered
                .drain(..self.remembered.len() - MAX_REMEMBERED);
        }
        out.push(Output::Remember(self.remembered.clone()));
    }
}
