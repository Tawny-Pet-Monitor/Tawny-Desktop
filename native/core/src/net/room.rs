//! Room relays over real sockets (feature `net`): a reconnecting client for
//! the LAN relay and the internet rendezvous, and the Monitor's own LAN relay
//! server. The protocol work is in [`crate::signal::room`] and
//! [`crate::signal::lan`]; this only moves frames.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::signal::lan::{ConnId, LanOut, LanRelay, MAX_MSG};
use crate::signal::room::{Kind, Refusal, RoomClient, RoomOut};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What a room connection tells its owner.
#[derive(Debug, Clone)]
pub enum RoomEvent {
    /// A frame for the call layer (`welcome`, `peer-joined`, `offer`, …).
    Frame(Value),
    /// The relay let us in.
    Up,
    /// The socket dropped; it will be dialled again unless the refusal says not to.
    Down(Option<Refusal>),
    /// A diagnostics line.
    Log(String),
}

/// Talks to a running room connection.
#[derive(Clone)]
pub struct RoomHandle {
    out: mpsc::UnboundedSender<Value>,
    stop: watch::Sender<bool>,
}

impl RoomHandle {
    /// Sends a frame (it carries `to`).
    pub fn send(&self, frame: Value) {
        let _ = self.out.send(frame);
    }

    /// Hangs up for good.
    pub fn close(&self) {
        let _ = self.stop.send(true);
    }
}

/// Dials a room relay at `base` (`ws://ip:port` or `wss://host`) and keeps
/// dialling (1 s doubling to 10 s) until closed or refused for a reason that
/// retrying can't fix. Must run inside a tokio runtime.
#[must_use]
pub fn connect(
    kind: Kind,
    base: &str,
    station: bool,
    key: &str,
    ticket: Option<String>,
) -> (RoomHandle, mpsc::UnboundedReceiver<RoomEvent>) {
    // rustls needs a crypto provider for wss://; ring builds everywhere.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(false);
    let client = RoomClient::new(kind, station, key, ticket);
    let url = client.url(base);
    tokio::spawn(dial_loop(client, url, out_rx, ev_tx, stop_rx));
    (RoomHandle { out: out_tx, stop }, ev_rx)
}

async fn dial_loop(
    mut client: RoomClient,
    url: String,
    mut out: mpsc::UnboundedReceiver<Value>,
    ev: mpsc::UnboundedSender<RoomEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let mut refusal = None;
        if let Ok(Ok((ws, _))) = tokio::time::timeout(
            Duration::from_secs(8),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        {
            backoff = Duration::from_secs(1);
            let (mut sink, mut stream) = ws.split();
            let mut admitted = false;
            for o in client.on_open() {
                if let RoomOut::Send(s) = o {
                    let _ = sink.send(Message::text(s)).await;
                }
            }
            loop {
                tokio::select! {
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            for o in client.on_text(&t) {
                                match o {
                                    RoomOut::Send(s) => { let _ = sink.send(Message::text(s)).await; }
                                    RoomOut::Emit(v) => { let _ = ev.send(RoomEvent::Frame(v)); }
                                }
                            }
                            if client.admitted() && !admitted {
                                admitted = true;
                                let _ = ev.send(RoomEvent::Up);
                            }
                        }
                        Some(Ok(Message::Close(Some(f)))) => { refusal = Some(Refusal::from_code(u16::from(f.code))); break; }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                    f = out.recv() => match f {
                        Some(f) => for o in client.send(&f) {
                            if let RoomOut::Send(s) = o { if sink.send(Message::text(s)).await.is_err() { break; } }
                        },
                        None => return,
                    },
                    _ = stop.changed() => { let _ = sink.send(Message::Close(None)).await; return; }
                }
            }
            client.reset();
        }
        let _ = ev.send(RoomEvent::Down(refusal));
        if refusal.is_some_and(|r| !r.retryable()) {
            let _ = ev.send(RoomEvent::Log(format!(
                "room relay refused us for good: {refusal:?}"
            )));
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// The Monitor's LAN relay, running.
pub struct LanServer {
    /// The port it listens on.
    pub port: u16,
    relay: Arc<Mutex<LanRelay>>,
    stop: watch::Sender<bool>,
}

impl LanServer {
    /// Starts listening on `addr` (port 0 picks a free one) for the channel
    /// with `key`. Must run inside a tokio runtime.
    ///
    /// # Errors
    /// The bind error.
    pub async fn start(addr: SocketAddr, key: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let port = listener.local_addr()?.port();
        let relay = Arc::new(Mutex::new(LanRelay::new(key)));
        let socks: Socks = Arc::default();
        let (stop, stop_rx) = watch::channel(false);
        tokio::spawn(sweep(relay.clone(), socks.clone(), stop_rx.clone()));
        tokio::spawn(accept_loop(listener, relay.clone(), socks, stop_rx));
        Ok(Self { port, relay, stop })
    }

    /// Changes the channel key (after a re-pair).
    pub fn set_key(&self, key: &str) {
        if let Ok(mut r) = self.relay.lock() {
            r.set_key(key);
        }
    }

    /// Stops listening (open sockets close as their tasks see it).
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

type Socks = Arc<Mutex<HashMap<ConnId, mpsc::UnboundedSender<Message>>>>;

fn apply(socks: &Socks, out: Vec<LanOut>) {
    let Ok(s) = socks.lock() else { return };
    for o in out {
        match o {
            LanOut::Send(c, t) => {
                if let Some(tx) = s.get(&c) {
                    let _ = tx.send(Message::text(t));
                }
            }
            LanOut::Close(c, code, why) => {
                if let Some(tx) = s.get(&c) {
                    let _ = tx.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(code),
                        reason: why.into(),
                    })));
                }
            }
        }
    }
}

async fn sweep(relay: Arc<Mutex<LanRelay>>, socks: Socks, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = stop.changed() => return,
        }
        let out = relay
            .lock()
            .map(|mut r| r.tick(now_ms()))
            .unwrap_or_default();
        apply(&socks, out);
    }
}

async fn accept_loop(
    listener: TcpListener,
    relay: Arc<Mutex<LanRelay>>,
    socks: Socks,
    mut stop: watch::Receiver<bool>,
) {
    let mut next: ConnId = 0;
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = stop.changed() => return,
        };
        let Ok((tcp, addr)) = accepted else { continue };
        next += 1;
        tokio::spawn(serve_socket(
            next,
            tcp,
            addr,
            relay.clone(),
            socks.clone(),
            stop.clone(),
        ));
    }
}

#[expect(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback returns its own large error type"
)]
async fn serve_socket(
    conn: ConnId,
    tcp: tokio::net::TcpStream,
    addr: SocketAddr,
    relay: Arc<Mutex<LanRelay>>,
    socks: Socks,
    mut stop: watch::Receiver<bool>,
) {
    let mut query = String::new();
    let cb = |req: &Request, resp: Response| {
        req.uri().query().unwrap_or("").clone_into(&mut query);
        Ok(resp)
    };
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MSG))
        .max_frame_size(Some(MAX_MSG));
    let Ok(ws) = tokio_tungstenite::accept_hdr_async_with_config(tcp, cb, Some(config)).await
    else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    if let Ok(mut s) = socks.lock() {
        s.insert(conn, tx);
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });
    let ip = addr.ip().to_string();
    let out = relay
        .lock()
        .map(|mut r| r.on_connect(conn, &query, &ip, now_ms()))
        .unwrap_or_default();
    apply(&socks, out);
    loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let out = relay.lock().map(|mut r| r.on_text(conn, &t)).unwrap_or_default();
                    apply(&socks, out);
                }
                Some(Ok(_)) => {}
                _ => break,
            },
            _ = stop.changed() => break,
        }
    }
    let out = relay
        .lock()
        .map(|mut r| r.on_close(conn))
        .unwrap_or_default();
    if let Ok(mut s) = socks.lock() {
        s.remove(&conn);
    }
    apply(&socks, out);
    writer.abort();
}
