//! The Monitor's signalling relay on the home network, without the socket.
//!
//! A port of `SignalServer` in the Android shell's `LocalWeb.kt`. Media never
//! flows through here. The room id travels in the WebSocket URL over plain
//! `ws://`, so knowing it proves nothing: every socket must answer a
//! per-connection nonce with `HMAC-SHA256(channel key, "tawny-lan-v1|" + nonce)`
//! before it is put in a room, counted, or announced to anyone. The key never
//! goes on the wire, and a captured answer is useless on the next connection.
//!
//! The caller owns the listening socket. It reports each connection with
//! [`LanRelay::on_connect`] (with the request's query string and the remote
//! address), each text frame, each close, and calls [`LanRelay::tick`] every
//! couple of seconds; then it carries out the [`LanOut`]s.

use std::collections::HashMap;

use hmac::Mac;
use serde_json::{Value, json};

use crate::ids::hmac_for;

/// The addressed frames this relay forwards (the same list as every relay).
pub use super::nostr::RELAYED;

const MAX_ROOMS: usize = 32;
const MAX_PER_IP: usize = 8;
/// One Monitor and up to three Viewers.
pub const MAX_PER_ROOM: usize = 4;
const MAX_PENDING: usize = 64;
const ADMIT_TIMEOUT_MS: u64 = 5_000;
/// Signalling frames are a few KB; anything larger is an attack.
pub const MAX_MSG: usize = 64 * 1024;
/// The close code for a socket the Monitor turned away for an unsigned offer.
/// Not one `app.js` treats as final, so an older phone tries again and then
/// goes on to Nostr or the rendezvous, where its call gets a code.
pub const CLOSE_UNSIGNED: u16 = 4012;

/// A connection, as the caller numbers them.
pub type ConnId = u64;

/// What the caller has to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanOut {
    /// Send this text frame on connection `0`.
    Send(ConnId, String),
    /// Close connection `0` with this code and reason.
    Close(ConnId, u16, &'static str),
}

struct Pending {
    nonce: String,
    room: String,
    station: bool,
    ip: String,
    since: u64,
}

struct Member {
    id: String,
    room: String,
    station: bool,
    ip: String,
}

/// The relay's state.
pub struct LanRelay {
    key: String,
    pending: HashMap<ConnId, Pending>,
    members: HashMap<ConnId, Member>,
    /// room → (peer id → connection)
    rooms: HashMap<String, HashMap<String, ConnId>>,
    per_ip: HashMap<String, usize>,
}

fn is_room(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn random_hex(bytes: usize) -> Option<String> {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).ok()?;
    Some(hex::encode(b))
}

impl LanRelay {
    /// A relay for the channel with this key.
    #[must_use]
    pub fn new(channel_key: impl Into<String>) -> Self {
        Self {
            key: channel_key.into(),
            pending: HashMap::new(),
            members: HashMap::new(),
            rooms: HashMap::new(),
            per_ip: HashMap::new(),
        }
    }

    /// Changes the key (after a re-pair). Sockets already admitted stay.
    pub fn set_key(&mut self, channel_key: impl Into<String>) {
        self.key = channel_key.into();
    }

    /// How many peers are in rooms.
    #[must_use]
    pub fn members(&self) -> usize {
        self.members.len()
    }

    /// A new WebSocket. `query` is the request's query string
    /// (`room=…&role=…`), `ip` the remote address.
    pub fn on_connect(&mut self, conn: ConnId, query: &str, ip: &str, now_ms: u64) -> Vec<LanOut> {
        let params: HashMap<&str, &str> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        // Unanswered challenges count too, or the cap is sidestepped by never answering.
        let in_flight = self.per_ip.get(ip).copied().unwrap_or(0)
            + self.pending.values().filter(|p| p.ip == ip).count();
        if in_flight >= MAX_PER_IP {
            return vec![LanOut::Close(conn, 4006, "too many")];
        }
        if self.pending.len() >= MAX_PENDING {
            return vec![LanOut::Close(conn, 4005, "busy")];
        }
        let Some(room) = params.get("room").filter(|r| is_room(r)) else {
            return vec![LanOut::Close(conn, 4000, "no room")];
        };
        if self.rooms.len() >= MAX_ROOMS && !self.rooms.contains_key(*room) {
            return vec![LanOut::Close(conn, 4005, "busy")];
        }
        let Some(nonce) = random_hex(16) else {
            return vec![LanOut::Close(conn, 4005, "busy")];
        };
        let station = params.get("role") == Some(&"station");
        let out = vec![LanOut::Send(
            conn,
            json!({ "type": "challenge", "n": nonce }).to_string(),
        )];
        self.pending.insert(
            conn,
            Pending {
                nonce,
                room: (*room).to_owned(),
                station,
                ip: ip.to_owned(),
                since: now_ms,
            },
        );
        out
    }

    fn proof_ok(&self, nonce: &str, proof: &str) -> bool {
        let Ok(given) = hex::decode(proof) else {
            return false;
        };
        let mut mac = hmac_for(&self.key);
        mac.update(format!("tawny-lan-v1|{nonce}").as_bytes());
        mac.verify_slice(&given).is_ok() // constant time
    }

    /// A text frame.
    pub fn on_text(&mut self, conn: ConnId, text: &str) -> Vec<LanOut> {
        if text.len() > MAX_MSG {
            return vec![LanOut::Close(conn, 4009, "message too large")];
        }
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        // Still unadmitted: the only frame accepted is the challenge answer.
        if self.pending.contains_key(&conn) {
            if msg["type"] != "hello" {
                return Vec::new();
            }
            let proof = msg["r"].as_str().unwrap_or("");
            let well_formed = proof.len() == 64 && proof.bytes().all(|b| b.is_ascii_hexdigit());
            let Some(p) = self.pending.remove(&conn) else {
                return Vec::new();
            };
            if !well_formed || !self.proof_ok(&p.nonce, proof) {
                return vec![LanOut::Close(conn, 4008, "bad proof")];
            }
            return self.admit(conn, p);
        }
        let Some(me) = self.members.get(&conn) else {
            return Vec::new();
        };
        // The Monitor's own socket turning a phone away (see `CLOSE_UNSIGNED`).
        // Only the station may, and only someone in its room.
        if msg["type"] == "kick" {
            let target = msg["to"]
                .as_str()
                .and_then(|to| self.rooms.get(&me.room)?.get(to))
                .copied()
                .filter(|t| me.station && *t != conn);
            return target
                .map(|t| vec![LanOut::Close(t, CLOSE_UNSIGNED, "unsigned offer")])
                .unwrap_or_default();
        }
        if !msg["type"].as_str().is_some_and(|t| RELAYED.contains(&t)) {
            return Vec::new();
        }
        let Some(to) = msg["to"].as_str().filter(|t| !t.is_empty()) else {
            return Vec::new();
        };
        let Some(&target) = self.rooms.get(&me.room).and_then(|r| r.get(to)) else {
            return Vec::new();
        };
        if target == conn {
            return Vec::new();
        }
        let mut msg = msg;
        msg["from"] = Value::String(me.id.clone());
        vec![LanOut::Send(target, msg.to_string())]
    }

    fn admit(&mut self, conn: ConnId, p: Pending) -> Vec<LanOut> {
        let peers = self.rooms.entry(p.room.clone()).or_default();
        if peers.len() >= MAX_PER_ROOM {
            return vec![LanOut::Close(conn, 4003, "channel full")];
        }
        let station_running = peers
            .values()
            .any(|c| self.members.get(c).is_some_and(|m| m.station));
        if p.station && station_running {
            if peers.is_empty() {
                self.rooms.remove(&p.room);
            }
            return vec![LanOut::Close(conn, 4004, "monitor already running")];
        }
        let Some(id) = random_hex(6) else {
            return vec![LanOut::Close(conn, 4005, "busy")];
        };
        let role = if p.station { "station" } else { "viewer" };
        let mut out = Vec::new();
        let peers = self.rooms.entry(p.room.clone()).or_default();
        let others: Vec<Value> = peers
            .iter()
            .map(|(pid, c)| json!({ "id": pid, "role": if self.members.get(c).is_some_and(|m| m.station) { "station" } else { "viewer" } }))
            .collect();
        out.push(LanOut::Send(
            conn,
            json!({ "type": "welcome", "id": id, "role": role, "peers": others }).to_string(),
        ));
        for c in peers.values() {
            out.push(LanOut::Send(
                *c,
                json!({ "type": "peer-joined", "id": id, "role": role }).to_string(),
            ));
        }
        peers.insert(id.clone(), conn);
        *self.per_ip.entry(p.ip.clone()).or_insert(0) += 1;
        self.members.insert(
            conn,
            Member {
                id,
                room: p.room,
                station: p.station,
                ip: p.ip,
            },
        );
        out
    }

    /// A connection closed.
    pub fn on_close(&mut self, conn: ConnId) -> Vec<LanOut> {
        self.pending.remove(&conn);
        let Some(m) = self.members.remove(&conn) else {
            return Vec::new();
        };
        if let Some(n) = self.per_ip.get_mut(&m.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_ip.remove(&m.ip);
            }
        }
        let Some(peers) = self.rooms.get_mut(&m.room) else {
            return Vec::new();
        };
        peers.remove(&m.id);
        if peers.is_empty() {
            self.rooms.remove(&m.room);
            return Vec::new();
        }
        peers
            .values()
            .map(|c| LanOut::Send(*c, json!({ "type": "peer-left", "id": m.id }).to_string()))
            .collect()
    }

    /// Closes sockets that were challenged and never answered (5 s).
    pub fn tick(&mut self, now_ms: u64) -> Vec<LanOut> {
        let stale: Vec<ConnId> = self
            .pending
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.since) > ADMIT_TIMEOUT_MS)
            .map(|(c, _)| *c)
            .collect();
        stale
            .into_iter()
            .map(|c| {
                self.pending.remove(&c);
                LanOut::Close(c, 4008, "no proof")
            })
            .collect()
    }
}
