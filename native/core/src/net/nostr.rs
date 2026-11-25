//! Runs a [`Session`] over real Nostr relays (tokio + `WebSocket`s).
//!
//! Each relay gets a socket task that reconnects with backoff (1 s doubling
//! to 10 s: this is a live call's signalling, not a feed reader). On every
//! connect the relay is tested: this device sends itself one sealed envelope
//! the size of a real answer through that relay alone, and the relay is used
//! only once it comes back. A relay that refuses, or accepts and never
//! delivers, is closed for the session and recorded in the [`HealthBook`].
//! The session opens as soon as one relay passes, once every relay has
//! failed, or after 6 s.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::nostr::event::Template;
use crate::nostr::nip59::{self, KIND_EPHEMERAL_GIFT_WRAP};
use crate::relay::conn::{ConnState, Filter, RelayConn, RelayOut};
use crate::relay::health::{HealthBook, Verdict};
use crate::signal::nostr::{Config, KIND_SIGNAL, Output, Session, SessionError};

const OPEN_WAIT_MS: u64 = 6_000;
const PROBE_DELAY_MS: u64 = 300;
const PROBE_AUTH_WAIT_MS: u64 = 3_000;
const PROBE_TIMEOUT_MS: u64 = 5_000;
const PROBE_PAD: usize = 6_000;
const FLOOD_WINDOW_MS: u64 = 10_000;
const FLOOD_MAX: u32 = 300;
const TICK: Duration = Duration::from_millis(100);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A refusal that says nothing about the relay's rules (the network, a busy
/// relay, a duplicate), so it isn't held against it.
fn transient(message: &str) -> bool {
    message == "timeout"
        || message == "not connected"
        || ["rate-limited:", "duplicate:", "error:"]
            .iter()
            .any(|p| message.starts_with(p))
}

/// A relay's standing this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayReport {
    /// The relay URL.
    pub url: String,
    /// `testing`, `ok`, `refused`, `no-echo`, `unreachable` or `skipped`.
    pub verdict: &'static str,
    /// Why, when it failed.
    pub why: String,
    /// The test's round trip, milliseconds.
    pub ms: Option<u64>,
}

/// What the channel tells its owner.
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// A frame for the call layer (`welcome`, `peer-joined`, `offer`, …).
    Frame(Value),
    /// Relay verdicts changed.
    Relays(Vec<RelayReport>),
    /// Persist the Monitor's list of known Handheld keys.
    Remember(Vec<String>),
    /// Persist the relay health book.
    Health(HealthBook),
    /// A diagnostics line.
    Log(String),
    /// The channel has shut down.
    Closed,
}

enum Cmd {
    Send(Value),
    Close,
}

/// Talks to a running channel.
#[derive(Clone)]
pub struct ChannelHandle(mpsc::UnboundedSender<Cmd>);

impl ChannelHandle {
    /// Sends a frame (it carries `to`) to a member.
    pub fn send(&self, frame: Value) {
        let _ = self.0.send(Cmd::Send(frame));
    }

    /// Says `leave` to every member and shuts down.
    pub fn close(&self) {
        let _ = self.0.send(Cmd::Close);
    }
}

enum Io {
    Open(usize),
    Text(usize, String),
    Down(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum V {
    Testing,
    Ok,
    Refused,
    NoEcho,
    Unreachable,
}

impl V {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Testing => "testing",
            Self::Ok => "ok",
            Self::Refused => "refused",
            Self::NoEcho => "no-echo",
            Self::Unreachable => "unreachable",
        }
    }

    const fn usable(self) -> bool {
        !matches!(self, Self::Refused | Self::NoEcho)
    }
}

struct Slot {
    url: String,
    conn: RelayConn,
    writer: mpsc::UnboundedSender<String>,
    stop: tokio::sync::watch::Sender<bool>,
    verdict: V,
    why: String,
    ms: Option<u64>,
    probe_at: Option<u64>,
    probe_id: Option<String>,
    probe_sent: u64,
    flood_start: u64,
    flood_n: u32,
}

/// Starts a channel on `relays`. Returns a handle and the event stream.
///
/// # Errors
/// [`SessionError`] when the session can't be made (bad key, no Monitor key).
pub fn start(
    cfg: Config,
    relays: &[String],
    health: HealthBook,
) -> Result<(ChannelHandle, mpsc::UnboundedReceiver<ChannelEvent>), SessionError> {
    // rustls needs a crypto provider; ring builds everywhere (Android included).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let secret = cfg.secret;
    let session = Session::new(cfg)?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (io_tx, io_rx) = mpsc::unbounded_channel();
    let now = now_ms();
    let (use_, skipped) = health.pick(relays, now);
    for (url, why) in &skipped {
        let _ = ev_tx.send(ChannelEvent::Log(format!(
            "nostr: skipping {url}, it refused last time ({why})"
        )));
    }
    let me = session.public_key().to_owned();
    let mut slots = Vec::new();
    for (i, url) in use_.iter().enumerate() {
        let (writer, wr_rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(socket(i, url.clone(), wr_rx, io_tx.clone(), stop_rx));
        let mut conn = RelayConn::new(url.clone(), Some(secret));
        let mut ignore = Vec::new();
        conn.subscribe(
            vec![Filter {
                kinds: Some(vec![KIND_EPHEMERAL_GIFT_WRAP]),
                tags: vec![('p', vec![me.clone()])],
                ..Filter::default()
            }],
            &mut ignore,
        );
        slots.push(Slot {
            url: url.clone(),
            conn,
            writer,
            stop,
            verdict: V::Testing,
            why: String::new(),
            ms: None,
            probe_at: None,
            probe_id: None,
            probe_sent: 0,
            flood_start: 0,
            flood_n: 0,
        });
    }
    let skipped = skipped
        .into_iter()
        .map(|(url, why)| RelayReport {
            url,
            verdict: "skipped",
            why,
            ms: None,
        })
        .collect();
    let run = Run {
        session,
        secret,
        me,
        slots,
        skipped,
        health,
        ev: ev_tx,
        started: now,
        opened: false,
    };
    tokio::spawn(run.main(cmd_rx, io_rx));
    Ok((ChannelHandle(cmd_tx), ev_rx))
}

async fn socket(
    i: usize,
    url: String,
    mut out: mpsc::UnboundedReceiver<String>,
    io: mpsc::UnboundedSender<Io>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let connected = tokio::time::timeout(
            Duration::from_secs(8),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await;
        if let Ok(Ok((ws, _))) = connected {
            backoff = Duration::from_secs(1);
            let (mut sink, mut stream) = ws.split();
            let _ = io.send(Io::Open(i));
            loop {
                tokio::select! {
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(t))) => { let _ = io.send(Io::Text(i, t.to_string())); }
                        Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                    frame = out.recv() => match frame {
                        Some(f) => { if sink.send(Message::text(f)).await.is_err() { break; } }
                        None => return,
                    },
                    _ = stop.changed() => {
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
        }
        let _ = io.send(Io::Down(i));
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

struct Run {
    session: Session,
    secret: [u8; 32],
    me: String,
    slots: Vec<Slot>,
    skipped: Vec<RelayReport>,
    health: HealthBook,
    ev: mpsc::UnboundedSender<ChannelEvent>,
    started: u64,
    opened: bool,
}

impl Run {
    async fn main(
        mut self,
        mut cmd: mpsc::UnboundedReceiver<Cmd>,
        mut io: mpsc::UnboundedReceiver<Io>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                c = cmd.recv() => match c {
                    Some(Cmd::Send(f)) => { let out = self.session.send(&f, now_ms()); self.session_out(out); }
                    Some(Cmd::Close) | None => break,
                },
                Some(x) = io.recv() => self.on_io(x),
                _ = tick.tick() => self.on_tick(),
            }
        }
        let out = self.session.close(now_ms());
        self.session_out(out);
        tokio::time::sleep(Duration::from_millis(300)).await; // let the leaves go out
        for s in &self.slots {
            let _ = s.stop.send(true);
        }
        let _ = self.ev.send(ChannelEvent::Closed);
    }

    fn emit(&self, e: ChannelEvent) {
        let _ = self.ev.send(e);
    }

    fn report(&self) -> Vec<RelayReport> {
        self.slots
            .iter()
            .map(|s| RelayReport {
                url: s.url.clone(),
                verdict: s.verdict.as_str(),
                why: s.why.clone(),
                ms: s.ms,
            })
            .chain(self.skipped.iter().cloned())
            .collect()
    }

    fn relay_out(&mut self, i: usize, outs: Vec<RelayOut>) {
        for o in outs {
            match o {
                RelayOut::Send(f) => {
                    let _ = self.slots[i].writer.send(f);
                }
                RelayOut::Event { raw, .. } => self.on_event(i, &raw),
                RelayOut::Published { id, ok, message } => self.on_published(i, &id, ok, &message),
                RelayOut::SubClosed { why, .. } => {
                    self.judge(i, V::Refused, &format!("subscription closed: {why}"), None);
                }
                RelayOut::Log(m) => self.emit(ChannelEvent::Log(format!(
                    "nostr {}: {m}",
                    self.slots[i].url
                ))),
                RelayOut::Authed(_) | RelayOut::State(_) => {}
            }
        }
    }

    fn session_out(&mut self, outs: Vec<Output>) {
        for o in outs {
            match o {
                Output::Publish(ev) => {
                    let now = now_ms();
                    for i in 0..self.slots.len() {
                        if self.slots[i].verdict.usable()
                            && self.slots[i].conn.state() != ConnState::Closed
                        {
                            let r = self.slots[i].conn.publish(&ev, now);
                            self.relay_out(i, r);
                        }
                    }
                }
                Output::Emit(v) => self.emit(ChannelEvent::Frame(v)),
                Output::Remember(l) => self.emit(ChannelEvent::Remember(l)),
                Output::Reject { code, reason } => self.emit(ChannelEvent::Log(format!(
                    "nostr: refused {code}: {reason}"
                ))),
                Output::Log(m) => self.emit(ChannelEvent::Log(m)),
            }
        }
    }

    fn on_io(&mut self, x: Io) {
        let now = now_ms();
        match x {
            Io::Open(i) => {
                if self.slots[i].verdict == V::Unreachable {
                    self.slots[i].verdict = V::Testing; // it connected after all
                }
                let r = self.slots[i].conn.on_open();
                self.relay_out(i, r);
                if self.slots[i].verdict == V::Testing && self.slots[i].probe_id.is_none() {
                    self.slots[i].probe_at = Some(now + PROBE_DELAY_MS);
                }
            }
            Io::Text(i, t) => {
                let r = self.slots[i].conn.on_text(&t, now);
                self.relay_out(i, r);
            }
            Io::Down(i) => {
                let r = self.slots[i].conn.on_close();
                self.relay_out(i, r);
                self.slots[i].probe_id = None;
                if self.slots[i].verdict == V::Testing {
                    self.slots[i].verdict = V::Unreachable;
                    self.slots[i].why = "cannot connect".into();
                    self.emit(ChannelEvent::Relays(self.report()));
                }
            }
        }
    }

    fn on_event(&mut self, i: usize, raw: &Value) {
        let now = now_ms();
        let id = raw.get("id").and_then(Value::as_str).unwrap_or("");
        if self.slots[i].probe_id.as_deref() == Some(id) {
            self.slots[i].probe_id = None;
            let ms = now.saturating_sub(self.slots[i].probe_sent);
            self.judge(i, V::Ok, "", Some(ms));
            return;
        }
        // Flooding a known key costs a signature check and an ECDH per
        // envelope; past a sane rate a relay's extra envelopes go unread.
        let s = &mut self.slots[i];
        if now.saturating_sub(s.flood_start) > FLOOD_WINDOW_MS {
            s.flood_start = now;
            s.flood_n = 0;
        }
        s.flood_n += 1;
        if s.flood_n > FLOOD_MAX {
            if s.flood_n == FLOOD_MAX + 1 {
                let url = s.url.clone();
                self.emit(ChannelEvent::Log(format!(
                    "nostr: {url} is flooding us; dropping the rest for now"
                )));
            }
            return;
        }
        let relays = self.slots.iter().filter(|s| s.verdict.usable()).count();
        let out = self.session.receive(raw, relays, now);
        self.session_out(out);
    }

    fn on_published(&mut self, i: usize, id: &str, ok: bool, message: &str) {
        if ok {
            return;
        }
        if self.slots[i].probe_id.as_deref() == Some(id) {
            if transient(message) {
                // The network, not the relay's rules: test again on the next connect.
                self.slots[i].probe_id = None;
                message.clone_into(&mut self.slots[i].why);
            } else {
                self.judge(i, V::Refused, message, None);
            }
            return;
        }
        let url = self.slots[i].url.clone();
        self.emit(ChannelEvent::Log(format!(
            "nostr: {url} refused an envelope: {message}"
        )));
        if !transient(message) && !message.starts_with("auth-required:") {
            self.judge(i, V::Refused, message, None);
        }
    }

    fn judge(&mut self, i: usize, v: V, why: &str, ms: Option<u64>) {
        let s = &mut self.slots[i];
        if s.verdict == v
            || matches!(s.verdict, V::Refused | V::NoEcho)
            || (s.verdict != V::Testing && v == V::Ok)
        {
            return;
        }
        s.verdict = v;
        why.clone_into(&mut s.why);
        s.ms = ms;
        let url = s.url.clone();
        let hv = match v {
            V::Ok => Some(Verdict::Ok),
            V::Refused => Some(Verdict::Refused),
            V::NoEcho => Some(Verdict::NoEcho),
            V::Unreachable => Some(Verdict::Unreachable),
            V::Testing => None,
        };
        if let Some(hv) = hv {
            self.health.record(&url, hv, why, ms, now_ms());
            self.emit(ChannelEvent::Health(self.health.clone()));
        }
        if v == V::Ok {
            self.emit(ChannelEvent::Log(format!(
                "nostr: {url} works ({} ms round trip)",
                ms.unwrap_or(0)
            )));
        } else {
            self.emit(ChannelEvent::Log(format!("nostr: not using {url}: {why}")));
            let _ = self.slots[i].stop.send(true);
        }
        self.emit(ChannelEvent::Relays(self.report()));
    }

    fn probe(&mut self, i: usize, now: u64) {
        if self.slots[i].conn.awaiting_auth()
            && now < self.slots[i].probe_at.unwrap_or(0) + PROBE_AUTH_WAIT_MS
        {
            return; // give the AUTH a moment first
        }
        self.slots[i].probe_at = None;
        let Ok(nonce) = crate::signal::nostr::nonce() else {
            return;
        };
        let content = json!({ "n": nonce, "pad": "x".repeat(PROBE_PAD) }).to_string();
        let ping = Template {
            kind: KIND_SIGNAL,
            tags: vec![
                vec!["p".into(), self.me.clone()],
                vec!["t".into(), "ping".into()],
            ],
            content,
            created_at: now / 1000,
        };
        match nip59::gift_wrap(
            ping,
            &self.secret,
            &self.me,
            KIND_EPHEMERAL_GIFT_WRAP,
            now / 1000,
            Some(120),
        ) {
            Ok(wrap) => {
                self.slots[i].probe_id = Some(wrap.id.clone());
                self.slots[i].probe_sent = now;
                let r = self.slots[i].conn.publish(&wrap, now);
                self.relay_out(i, r);
            }
            Err(e) => self.emit(ChannelEvent::Log(format!(
                "nostr: could not build the relay test: {e}"
            ))),
        }
    }

    fn on_tick(&mut self) {
        let now = now_ms();
        for i in 0..self.slots.len() {
            if self.slots[i].probe_at.is_some_and(|t| t <= now)
                && self.slots[i].conn.state() != ConnState::Closed
            {
                self.probe(i, now);
            }
            if self.slots[i].verdict == V::Testing
                && self.slots[i].probe_id.is_some()
                && now.saturating_sub(self.slots[i].probe_sent) > PROBE_TIMEOUT_MS
            {
                self.judge(
                    i,
                    V::NoEcho,
                    "accepted the test message but never delivered it",
                    None,
                );
            }
            let r = self.slots[i].conn.tick(now);
            self.relay_out(i, r);
        }
        if !self.opened {
            let any_ok = self.slots.iter().any(|s| s.verdict == V::Ok);
            let all_done = self.slots.iter().all(|s| s.verdict != V::Testing);
            if any_ok || all_done || now.saturating_sub(self.started) >= OPEN_WAIT_MS {
                self.opened = true;
                let n = self.slots.len();
                self.emit(ChannelEvent::Log(format!(
                    "nostr open as {} on {n} relay(s)",
                    &self.me[..12.min(self.me.len())]
                )));
                let out = self.session.open(now);
                self.session_out(out);
            }
        }
        let out = self.session.tick(now);
        self.session_out(out);
    }
}
