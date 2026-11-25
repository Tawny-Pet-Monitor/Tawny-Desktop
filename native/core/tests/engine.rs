//! A whole Monitor (LAN relay + Station) and a Viewer that dials it like a
//! phone on the same Wi-Fi: real sockets, real frames, the same code.
#![cfg(feature = "engine")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use tawny_core::call::media::{MediaConfig, Viewer, ViewerConfig, ViewerEvent};
use tawny_core::call::policy::{PAIR_MIN_LEFT_MS, PAIR_TTL_MS, PairCode, PairGate};
use tawny_core::engine::monitor::{Monitor, MonitorConfig, MonitorEvent};
use tawny_core::link::{self, Anchors};
use tawny_core::net::room::{self, RoomEvent};
use tawny_core::relay::health::HealthBook;
use tawny_core::sas;
use tawny_core::signal::room::Kind;

const KEY: &str = "Zx9_-abcDEF0123456789";
type R = Result<(), Box<dyn std::error::Error>>;

fn config() -> MonitorConfig {
    MonitorConfig {
        channel_key: KEY.into(),
        room_name: "Pet camera".into(),
        ticket: Some("TICKET1234abcd".into()),
        nostr_secret: None,
        nostr_relays: vec![],
        nostr_health: HealthBook::default(),
        nostr_known: vec![],
        rendezvous: None,
        rendezvous_fallback: None,
        lan_bind: Some(
            "127.0.0.1:0"
                .parse()
                .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
        ),
        lan_ip: Some("10.0.0.5".into()),
        gate: PairGate::default(),
        mic: false,
        width: 320,
        height: 240,
        media: MediaConfig::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_on_the_wifi_watches_the_monitor() -> R {
    let (monitor, mut m_rx) = Monitor::start(config()).await?;
    let port = monitor.lan_port().ok_or("no LAN relay")?;

    // The QR the Monitor shows, read back the way a phone would.
    let raw = monitor.pair_link()?;
    let pairing = link::parse(&raw, Anchors::default())?;
    assert_eq!(pairing.key, KEY);
    assert_eq!(
        pairing.signal.as_deref(),
        Some(format!("ws://10.0.0.5:{port}").as_str())
    );
    let code = pairing.code.clone().ok_or("no code in the link")?;

    // Camera frames while asked for.
    let src = monitor.video_source();
    tokio::spawn(async move {
        loop {
            let b = I420Buffer::new(320, 240);
            src.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, b));
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
    });

    // The phone: a Viewer on the Monitor's LAN relay.
    let (viewer, mut v_rx) = Viewer::start(ViewerConfig {
        channel_key: KEY.into(),
        pid: Some("0123456789abcdef0123456789abcdef".into()),
        code: Some(code),
        label: "Test phone".into(),
        name: None,
        mic: false,
        media: MediaConfig::default(),
    });
    let (wire, mut w_rx) = room::connect(
        Kind::Lan,
        &format!("ws://127.0.0.1:{port}"),
        false,
        KEY,
        pairing.token.clone(),
    );

    let decoded = Arc::new(AtomicU32::new(0));
    let (mut joined, mut capture, mut connected, mut m_sas, mut v_sas) =
        (false, false, false, None, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while (!connected || v_sas.is_none() || decoded.load(Ordering::Relaxed) < 10)
        && tokio::time::Instant::now() < deadline
    {
        tokio::select! {
            Some(e) = w_rx.recv() => if let RoomEvent::Frame(f) = e { viewer.signal(f) },
            Some(e) = v_rx.recv() => match e {
                ViewerEvent::Signal(f) => wire.send(f),
                ViewerEvent::Sas(c) => v_sas = Some(c),
                ViewerEvent::Video(t) => {
                    let d = decoded.clone();
                    tokio::spawn(async move { let mut s = NativeVideoStream::new(t); while s.next().await.is_some() { d.fetch_add(1, Ordering::Relaxed); } });
                }
                ViewerEvent::Refused(r) => return Err(format!("refused: {r}").into()),
                _ => {}
            },
            Some(e) = m_rx.recv() => match e {
                MonitorEvent::Joined { label, .. } => { assert_eq!(label, "Test phone"); joined = true; }
                MonitorEvent::Capture(on) => capture = on,
                MonitorEvent::Connected { .. } => connected = true,
                MonitorEvent::Sas { code, .. } => m_sas = Some(code),
                _ => {}
            },
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    assert!(joined, "the Monitor let the phone in");
    assert!(capture, "the camera was asked for once the phone connected");
    assert!(connected, "the call connected");
    // The LAN path has the Monitor as its own relay: no code to compare, as in
    // app.js. (The Viewer still works one out; its app shows it only off the LAN.)
    assert_eq!(m_sas, None, "no security code on the LAN path");
    assert!(
        matches!(v_sas, Some(Some(_))),
        "the certificates were readable"
    );
    assert!(
        decoded.load(Ordering::Relaxed) >= 10,
        "the phone decoded the camera"
    );

    // The phone hangs up: the Monitor stops wanting the camera.
    viewer.close();
    wire.close();
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = m_rx.recv().await {
            if matches!(e, MonitorEvent::Capture(false)) {
                return true;
            }
        }
        false
    })
    .await?;
    assert!(stopped, "the camera was released");
    monitor.stop();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_with_nowhere_to_meet_refuses_to_start() {
    let mut cfg = config();
    cfg.lan_bind = None;
    assert!(Monitor::start(cfg).await.is_err());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

async fn link_code_with_saved(left_ms: u64) -> Result<String, Box<dyn std::error::Error>> {
    let mut cfg = config();
    cfg.gate.code = Some(PairCode {
        code: "SAVEDcode00".into(),
        expires_ms: now_ms() + left_ms,
    });
    let (monitor, _rx) = Monitor::start(cfg).await?;
    let code = link::parse(&monitor.pair_link()?, Anchors::default())?
        .code
        .ok_or("no code")?;
    monitor.stop();
    Ok(code)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_code_about_to_lapse_is_never_put_in_the_qr() -> R {
    // Seen on a phone: a code saved by the last run had 7 s left, the QR
    // showed it, and the Viewer that scanned it was refused.
    assert_ne!(link_code_with_saved(7_000).await?, "SAVEDcode00");
    assert_ne!(
        link_code_with_saved(PAIR_MIN_LEFT_MS - 1_000).await?,
        "SAVEDcode00"
    );
    assert_eq!(
        link_code_with_saved(PAIR_TTL_MS - 60_000).await?,
        "SAVEDcode00",
        "a code with time left stays"
    );
    Ok(())
}

/// Someone else on the Wi-Fi can pass the LAN relay's challenge on to a real
/// phone and carry its frames, but can't sign an offer naming their own
/// certificate. An offer with no signature, or one under another key, never
/// reaches the camera, even with a good pairing code and a known phone's id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsigned_offer_is_not_let_in_on_the_lan() -> R {
    for forge in [false, true] {
        let (monitor, mut m_rx) = Monitor::start(config()).await?;
        let port = monitor.lan_port().ok_or("no LAN relay")?;
        let pairing = link::parse(&monitor.pair_link()?, Anchors::default())?;
        let (viewer, mut v_rx) = Viewer::start(ViewerConfig {
            channel_key: KEY.into(),
            pid: Some("0123456789abcdef0123456789abcdef".into()),
            code: pairing.code.clone(),
            label: "Stranger".into(),
            name: None,
            mic: false,
            media: MediaConfig::default(),
        });
        let (wire, mut w_rx) = room::connect(
            Kind::Lan,
            &format!("ws://127.0.0.1:{port}"),
            false,
            KEY,
            pairing.token.clone(),
        );
        let (mut tampered, mut refused, mut joined, mut kicked) = (false, false, false, false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline && !(refused && tampered && kicked) {
            tokio::select! {
                Some(e) = w_rx.recv() => match e {
                    RoomEvent::Frame(f) => viewer.signal(f),
                    RoomEvent::Down(_) => kicked = true,
                    _ => {}
                },
                Some(e) = v_rx.recv() => if let ViewerEvent::Signal(mut f) = e {
                    if f["type"] == "offer" {
                        let sdp = f["sdp"]["sdp"].as_str().unwrap_or("").to_owned();
                        let o = f.as_object_mut().ok_or("not an object")?;
                        if forge {
                            let sm = sas::sign_sdp("someone else's key", "offer", &sdp)
                                .ok_or("no certificate in the offer")?;
                            o.insert("sm".into(), sm.into());
                        } else {
                            o.remove("sm");
                        }
                        tampered = true;
                    }
                    wire.send(f);
                },
                Some(e) = m_rx.recv() => match e {
                    MonitorEvent::Joined { .. } | MonitorEvent::Capture(true) => joined = true,
                    MonitorEvent::Log(l) if l.starts_with("offer refused") => refused = true,
                    _ => {}
                },
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        assert!(tampered, "the offer was sent");
        assert!(refused, "the Monitor said why it refused (forged: {forge})");
        assert!(!joined, "the camera stayed off (forged: {forge})");
        assert!(kicked, "its socket was closed, so an older phone moves on");
        viewer.close();
        wire.close();
        monitor.stop();
    }
    Ok(())
}
