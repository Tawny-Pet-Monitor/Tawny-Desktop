//! One Nostr relay connection's protocol (NIP-01 with NIP-42 AUTH), without
//! the socket. A port of `Relay` in `public/nostr/relay-client.js`.
//!
//! The relay is never trusted: every event handed up has had its id and
//! signature checked here and must match a filter of the subscription it
//! arrived on. The caller owns the WebSocket: it reports `on_open`,
//! `on_close` and each text frame, and sends what [`RelayOut::Send`] says.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::nostr::event::{self, Event, Template};

const PUBLISH_TIMEOUT_MS: u64 = 8_000;
const AUTH_WAIT_MS: u64 = 4_000;
/// Far above any real envelope (~11 KB); bigger frames are dropped unparsed.
pub const MAX_FRAME: usize = 128 * 1024;
const KIND_AUTH: u16 = 22_242;

/// A NIP-01 subscription filter (the parts Tawny uses).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Event kinds.
    pub kinds: Option<Vec<u16>>,
    /// Authors' public keys.
    pub authors: Option<Vec<String>>,
    /// Tag filters: `('p', [keys])` is `"#p": [keys]`.
    pub tags: Vec<(char, Vec<String>)>,
    /// Lower time bound, seconds.
    pub since: Option<u64>,
}

impl Filter {
    fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(k) = &self.kinds {
            o.insert("kinds".into(), json!(k));
        }
        if let Some(a) = &self.authors {
            o.insert("authors".into(), json!(a));
        }
        for (c, v) in &self.tags {
            o.insert(format!("#{c}"), json!(v));
        }
        if let Some(s) = self.since {
            o.insert("since".into(), json!(s));
        }
        Value::Object(o)
    }

    /// NIP-01 matching.
    #[must_use]
    pub fn matches(&self, ev: &Event) -> bool {
        if self.kinds.as_ref().is_some_and(|k| !k.contains(&ev.kind)) {
            return false;
        }
        if self
            .authors
            .as_ref()
            .is_some_and(|a| !a.contains(&ev.pubkey))
        {
            return false;
        }
        if self.since.is_some_and(|s| ev.created_at < s) {
            return false;
        }
        self.tags.iter().all(|(c, want)| {
            let name = c.to_string();
            ev.tags
                .iter()
                .any(|t| t.first() == Some(&name) && t.get(1).is_some_and(|v| want.contains(v)))
        })
    }
}

/// Where the connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// Not connected.
    Closed,
    /// Connected, not signed in.
    Open,
    /// An AUTH answer is in flight.
    Authenticating,
    /// Signed in with our key.
    Authenticated,
}

/// What the caller has to do or know.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayOut {
    /// Send this text frame on the socket.
    Send(String),
    /// A verified event on subscription `sub`, as the relay sent it.
    Event {
        /// Subscription id.
        sub: String,
        /// The event's JSON.
        raw: Value,
    },
    /// The relay's verdict on something we published (or a local timeout).
    Published {
        /// Event id.
        id: String,
        /// Whether the relay accepted it.
        ok: bool,
        /// The relay's message, or `timeout` / `not connected`.
        message: String,
    },
    /// The relay ended a subscription for good.
    SubClosed {
        /// Subscription id.
        sub: String,
        /// The relay's reason.
        why: String,
    },
    /// Signed in (NIP-42) as this public key.
    Authed(String),
    /// The state changed.
    State(ConnState),
    /// Something worth a diagnostics line.
    Log(String),
}

struct Sub {
    filters: Vec<Filter>,
    needs_auth: bool,
}

struct Pending {
    ev: Event,
    deadline: u64,
    retried: bool,
    auth: bool,
    /// Waiting for AUTH before being sent again (once).
    after_auth: Option<u64>,
}

/// One relay connection's protocol state.
pub struct RelayConn {
    auth_url: String,
    auth_key: Option<[u8; 32]>,
    state: ConnState,
    challenge: Option<String>,
    authed_as: Option<String>,
    auth_pending: bool,
    subs: BTreeMap<String, Sub>,
    pending: HashMap<String, Pending>,
    next_sub: u64,
}

fn frame(v: &Value) -> RelayOut {
    RelayOut::Send(v.to_string())
}

impl RelayConn {
    /// `auth_url` is the relay's own idea of its URL (signed into AUTH);
    /// `auth_key` answers AUTH challenges, or `None` to stay anonymous.
    #[must_use]
    pub fn new(auth_url: impl Into<String>, auth_key: Option<[u8; 32]>) -> Self {
        Self {
            auth_url: auth_url.into(),
            auth_key,
            state: ConnState::Closed,
            challenge: None,
            authed_as: None,
            auth_pending: false,
            subs: BTreeMap::new(),
            pending: HashMap::new(),
            next_sub: 0,
        }
    }

    /// The current state.
    #[must_use]
    pub const fn state(&self) -> ConnState {
        self.state
    }

    /// Whether the relay has asked us to sign in and we haven't yet.
    #[must_use]
    pub fn awaiting_auth(&self) -> bool {
        self.challenge.is_some() && self.authed_as.is_none()
    }

    /// The socket opened: re-sends every subscription.
    pub fn on_open(&mut self) -> Vec<RelayOut> {
        self.state = ConnState::Open;
        let mut out = vec![RelayOut::State(ConnState::Open)];
        for (id, s) in &self.subs {
            out.push(Self::req(id, &s.filters));
        }
        out
    }

    /// The socket closed. Anything in flight is reported as not sent.
    pub fn on_close(&mut self) -> Vec<RelayOut> {
        self.state = ConnState::Closed;
        self.challenge = None;
        self.authed_as = None;
        self.auth_pending = false;
        let mut out = vec![RelayOut::State(ConnState::Closed)];
        for (id, p) in self.pending.drain() {
            if !p.auth {
                out.push(RelayOut::Published {
                    id,
                    ok: false,
                    message: "not connected".into(),
                });
            }
        }
        out
    }

    fn req(id: &str, filters: &[Filter]) -> RelayOut {
        let mut arr = vec![json!("REQ"), json!(id)];
        arr.extend(filters.iter().map(Filter::to_json));
        frame(&Value::Array(arr))
    }

    /// Subscribes; returns the subscription id.
    pub fn subscribe(&mut self, filters: Vec<Filter>, out: &mut Vec<RelayOut>) -> String {
        self.next_sub += 1;
        let id = format!("t{}", self.next_sub);
        if self.state != ConnState::Closed {
            out.push(Self::req(&id, &filters));
        }
        self.subs.insert(
            id.clone(),
            Sub {
                filters,
                needs_auth: false,
            },
        );
        id
    }

    /// Ends a subscription.
    pub fn unsubscribe(&mut self, id: &str) -> Vec<RelayOut> {
        if self.subs.remove(id).is_some() && self.state != ConnState::Closed {
            return vec![frame(&json!(["CLOSE", id]))];
        }
        Vec::new()
    }

    /// Publishes an event; the verdict comes back as [`RelayOut::Published`].
    pub fn publish(&mut self, ev: &Event, now_ms: u64) -> Vec<RelayOut> {
        self.publish_inner(ev.clone(), now_ms, false)
    }

    fn publish_inner(&mut self, ev: Event, now_ms: u64, retried: bool) -> Vec<RelayOut> {
        if self.state == ConnState::Closed {
            return vec![RelayOut::Published {
                id: ev.id,
                ok: false,
                message: "not connected".into(),
            }];
        }
        let send = frame(&json!(["EVENT", ev]));
        self.pending.insert(
            ev.id.clone(),
            Pending {
                ev,
                deadline: now_ms + PUBLISH_TIMEOUT_MS,
                retried,
                auth: false,
                after_auth: None,
            },
        );
        vec![send]
    }

    fn authenticate(&mut self, now_ms: u64, out: &mut Vec<RelayOut>) {
        let (Some(key), Some(challenge)) = (self.auth_key, self.challenge.clone()) else {
            return;
        };
        if self.auth_pending {
            return;
        }
        let t = Template {
            kind: KIND_AUTH,
            tags: vec![
                vec!["relay".into(), self.auth_url.clone()],
                vec!["challenge".into(), challenge],
            ],
            content: String::new(),
            created_at: now_ms / 1000,
        };
        match event::sign(t, &key) {
            Ok(ev) => {
                self.auth_pending = true;
                self.state = ConnState::Authenticating;
                out.push(RelayOut::State(ConnState::Authenticating));
                out.push(frame(&json!(["AUTH", ev])));
                self.pending.insert(
                    ev.id.clone(),
                    Pending {
                        ev,
                        deadline: now_ms + PUBLISH_TIMEOUT_MS,
                        retried: false,
                        auth: true,
                        after_auth: None,
                    },
                );
            }
            Err(e) => out.push(RelayOut::Log(format!("could not sign AUTH: {e}"))),
        }
    }

    /// Re-sends what was waiting for AUTH (once each).
    fn release_after_auth(&mut self, now_ms: u64, out: &mut Vec<RelayOut>) {
        let waiting: Vec<Event> = self
            .pending
            .values()
            .filter(|p| p.after_auth.is_some())
            .map(|p| p.ev.clone())
            .collect();
        for ev in waiting {
            self.pending.remove(&ev.id);
            out.extend(self.publish_inner(ev, now_ms, true));
        }
    }

    /// Publish timeouts, and the AUTH wait running out.
    pub fn tick(&mut self, now_ms: u64) -> Vec<RelayOut> {
        let mut out = Vec::new();
        let auth_over = self
            .pending
            .values()
            .any(|p| p.after_auth.is_some_and(|d| d <= now_ms));
        if auth_over {
            self.release_after_auth(now_ms, &mut out);
        }
        let late: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.after_auth.is_none() && p.deadline <= now_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for id in late {
            if let Some(p) = self.pending.remove(&id) {
                if p.auth {
                    self.auth_pending = false;
                } else {
                    out.push(RelayOut::Published {
                        id,
                        ok: false,
                        message: "timeout".into(),
                    });
                }
            }
        }
        out
    }

    /// One text frame from the relay.
    pub fn on_text(&mut self, text: &str, now_ms: u64) -> Vec<RelayOut> {
        let mut out = Vec::new();
        if text.len() > MAX_FRAME {
            out.push(RelayOut::Log(
                "relay sent an oversized frame; dropped".into(),
            ));
            return out;
        }
        let Ok(Value::Array(msg)) = serde_json::from_str::<Value>(text) else {
            out.push(RelayOut::Log("relay sent non-JSON".into()));
            return out;
        };
        let kind = msg.first().and_then(Value::as_str).unwrap_or("");
        let a = msg.get(1);
        let b = msg.get(2);
        let c = msg.get(3).and_then(Value::as_str).unwrap_or("").to_owned();
        match kind {
            "AUTH" => {
                if let Some(ch) = a.and_then(Value::as_str) {
                    self.challenge = Some(ch.to_owned());
                    self.authenticate(now_ms, &mut out);
                }
            }
            "OK" => self.on_ok(
                a.and_then(Value::as_str).unwrap_or(""),
                b == Some(&Value::Bool(true)),
                c,
                now_ms,
                &mut out,
            ),
            "EVENT" => {
                let (Some(sub_id), Some(raw)) = (a.and_then(Value::as_str), b) else {
                    return out;
                };
                let Some(sub) = self.subs.get(sub_id) else {
                    return out;
                };
                let ev = match event::from_json(raw, true).and_then(|e| e.verify().map(|()| e)) {
                    Ok(e) => e,
                    Err(e) => {
                        out.push(RelayOut::Log(format!(
                            "relay delivered an invalid event ({e}); dropped"
                        )));
                        return out;
                    }
                };
                if !sub.filters.iter().any(|f| f.matches(&ev)) {
                    out.push(RelayOut::Log(
                        "relay delivered an event we did not ask for; dropped".into(),
                    ));
                    return out;
                }
                out.push(RelayOut::Event {
                    sub: sub_id.to_owned(),
                    raw: raw.clone(),
                });
            }
            "CLOSED" => {
                let Some(sub_id) = a.and_then(Value::as_str).map(str::to_owned) else {
                    return out;
                };
                let why = b.and_then(Value::as_str).unwrap_or("").to_owned();
                if !self.subs.contains_key(&sub_id) {
                    return out;
                }
                if why.starts_with("auth-required:") && self.auth_key.is_some() {
                    if self.authed_as.is_some() {
                        if let Some(s) = self.subs.get(&sub_id) {
                            out.push(Self::req(&sub_id, &s.filters));
                        }
                    } else {
                        if let Some(s) = self.subs.get_mut(&sub_id) {
                            s.needs_auth = true;
                        }
                        self.authenticate(now_ms, &mut out);
                    }
                } else {
                    self.subs.remove(&sub_id);
                    out.push(RelayOut::SubClosed { sub: sub_id, why });
                }
            }
            "NOTICE" => out.push(RelayOut::Log(format!(
                "relay notice: {}",
                a.and_then(Value::as_str).unwrap_or("")
            ))),
            _ => {}
        }
        out
    }

    fn on_ok(
        &mut self,
        id: &str,
        accepted: bool,
        message: String,
        now_ms: u64,
        out: &mut Vec<RelayOut>,
    ) {
        let Some(p) = self.pending.remove(id) else {
            return;
        };
        if p.auth {
            self.auth_pending = false;
            if accepted {
                let who = self
                    .auth_key
                    .and_then(|k| event::public_key(&k).ok())
                    .unwrap_or_default();
                self.authed_as = Some(who.clone());
                self.state = ConnState::Authenticated;
                out.push(RelayOut::State(ConnState::Authenticated));
                out.push(RelayOut::Authed(who));
                let resend: Vec<String> = self
                    .subs
                    .iter()
                    .filter(|(_, s)| s.needs_auth)
                    .map(|(k, _)| k.clone())
                    .collect();
                for sid in resend {
                    if let Some(s) = self.subs.get_mut(&sid) {
                        s.needs_auth = false;
                        out.push(Self::req(&sid, &s.filters));
                    }
                }
            } else {
                out.push(RelayOut::Log(format!("relay refused AUTH: {message}")));
                self.state = ConnState::Open;
                out.push(RelayOut::State(ConnState::Open));
            }
            self.release_after_auth(now_ms, out);
            return;
        }
        if !accepted
            && message.starts_with("auth-required:")
            && !p.retried
            && self.challenge.is_some()
        {
            // NIP-42 for a refused EVENT: sign in, then send it again once.
            let id = p.ev.id.clone();
            self.pending.insert(
                id,
                Pending {
                    after_auth: Some(now_ms + AUTH_WAIT_MS),
                    ..p
                },
            );
            self.authenticate(now_ms, out);
            return;
        }
        out.push(RelayOut::Published {
            id: id.to_owned(),
            ok: accepted,
            message,
        });
    }
}
