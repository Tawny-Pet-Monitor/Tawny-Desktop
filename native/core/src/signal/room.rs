//! The client side of a room relay, without the socket: the Monitor's LAN
//! relay ([`super::lan`]) or an internet rendezvous (the Cloudflare Worker,
//! the Docker box's `server.js`).
//!
//! Both speak the same room protocol: `welcome`, `peer-joined`, `peer-left`,
//! and addressed frames that come back with `from`. They differ only in how a
//! socket is let in:
//!
//! - **LAN** (`ws://`, a shared Wi-Fi): the relay sends `challenge {n}` and we
//!   answer `hello {r: HMAC(channel key, "tawny-lan-v1|" + n)}`. The raw
//!   ticket must never go out here: anyone sniffing would get the room id
//!   (it is in the URL) plus the ticket, which is exactly what the internet
//!   rendezvous admits a Viewer on.
//! - **Rendezvous** (`wss://`): the first frame is `hello {t}` (the ticket,
//!   inside TLS); a Monitor adds `hashT` (so the relay knows which ticket to
//!   accept) and `a` (proof of holding the key, so only it can re-key the room).

use serde_json::{Value, json};

use crate::ids::{auth_proof, lan_response, room_id, ticket_hash};

/// Which kind of relay this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The Monitor's relay on the home network.
    Lan,
    /// An internet rendezvous.
    Rendezvous,
}

/// What the caller has to do or know.
#[derive(Debug, Clone, PartialEq)]
pub enum RoomOut {
    /// Send this text frame.
    Send(String),
    /// Hand this frame to the call layer.
    Emit(Value),
}

/// Why a relay closed us, as `app.js` reads the close codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 4003: one Monitor and three Viewers already.
    Full,
    /// 4004: another Monitor holds this channel.
    MonitorRunning,
    /// 4005: the relay is busy, or we were replaced by the owner.
    Busy,
    /// 4006: too many sockets from this device.
    TooMany,
    /// 4008: the pairing has expired, the key or proof is wrong, or no ticket.
    Admission,
    /// 4010: the Monitor is offline (no ticket registered for this room).
    MonitorOffline,
    /// Anything else: the network, a restart. Worth retrying.
    Other,
}

impl Refusal {
    /// Reads a WebSocket close code.
    #[must_use]
    pub const fn from_code(code: u16) -> Self {
        match code {
            4003 => Self::Full,
            4004 => Self::MonitorRunning,
            4005 => Self::Busy,
            4006 => Self::TooMany,
            4008 => Self::Admission,
            4010 => Self::MonitorOffline,
            _ => Self::Other,
        }
    }

    /// Whether dialling again could help without anything changing.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::MonitorOffline | Self::Other)
    }
}

/// One connection to a room relay.
pub struct RoomClient {
    kind: Kind,
    station: bool,
    key: String,
    ticket: Option<String>,
    admitted: bool,
}

impl RoomClient {
    /// A client for the channel with `key`; `ticket` is the pairing's relay ticket.
    #[must_use]
    pub fn new(kind: Kind, station: bool, key: impl Into<String>, ticket: Option<String>) -> Self {
        Self {
            kind,
            station,
            key: key.into(),
            ticket,
            admitted: false,
        }
    }

    /// The URL path and query to dial on `base` (`ws://ip:port` or `wss://host`).
    #[must_use]
    pub fn url(&self, base: &str) -> String {
        let role = if self.station { "station" } else { "viewer" };
        format!(
            "{}/ws?room={}&role={role}",
            base.trim_end_matches('/'),
            room_id(&self.key)
        )
    }

    /// Forgets admission, for a fresh connection.
    pub fn reset(&mut self) {
        self.admitted = false;
    }

    /// Whether the relay has let us in (`welcome` seen).
    #[must_use]
    pub const fn admitted(&self) -> bool {
        self.admitted
    }

    /// The socket opened.
    #[must_use]
    pub fn on_open(&self) -> Vec<RoomOut> {
        if self.kind == Kind::Lan {
            return Vec::new(); // wait for the challenge; nothing secret goes out
        }
        let mut hello = json!({ "type": "hello" });
        if let Some(t) = &self.ticket {
            hello["t"] = json!(t);
            if self.station {
                hello["hashT"] = json!(ticket_hash(t));
            }
        }
        if self.station {
            hello["a"] = json!(auth_proof(&self.key));
        }
        vec![RoomOut::Send(hello.to_string())]
    }

    /// A text frame from the relay.
    pub fn on_text(&mut self, text: &str) -> Vec<RoomOut> {
        let Ok(m) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        if m["type"] == "challenge" {
            if self.kind != Kind::Lan {
                return Vec::new(); // only the LAN relay challenges
            }
            let nonce = m["n"].as_str().unwrap_or("");
            if nonce.len() != 32
                || !nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Vec::new();
            }
            return vec![RoomOut::Send(
                json!({ "type": "hello", "r": lan_response(&self.key, nonce) }).to_string(),
            )];
        }
        if m["type"] == "welcome" {
            self.admitted = true;
        }
        if m.is_object() {
            vec![RoomOut::Emit(m)]
        } else {
            Vec::new()
        }
    }

    /// A frame from the call layer (it carries `to`).
    #[must_use]
    pub fn send(&self, frame: &Value) -> Vec<RoomOut> {
        if !self.admitted {
            return Vec::new();
        }
        vec![RoomOut::Send(frame.to_string())]
    }
}
