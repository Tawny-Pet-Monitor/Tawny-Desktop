//! A whole Viewer ([`Watch`]) calling a whole Monitor over real sockets: on
//! the Wi-Fi, over Nostr only, and with both (the LAN wins).
#![cfg(feature = "engine")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use tawny_core::call::media::MediaConfig;
use tawny_core::call::policy::PairGate;
use tawny_core::engine::monitor::{Monitor, MonitorConfig, MonitorEvent};
use tawny_core::engine::viewer::{Watch, WatchConfig, WatchEvent};
use tawny_core::link::{self, Anchors, Pairing};
use tawny_core::relay::health::HealthBook;
use tokio::sync::mpsc::UnboundedReceiver;

mod common;

const KEY: &str = "Zx9_-abcDEF0123456789";
const STATION_SK: [u8; 32] = [0x61; 32];
const PHONE_SK: [u8; 32] = [0x62; 32];
const PID: &str = "0123456789abcdef0123456789abcdef";
type R = Result<(), Box<dyn std::error::Error>>;

fn monitor_config(lan: bool, relays: Vec<String>) -> MonitorConfig {
    MonitorConfig {
        channel_key: KEY.into(),
        room_name: "Den".into(),
        ticket: None,
        nostr_secret: (!relays.is_empty()).then_some(STATION_SK),
        nostr_relays: relays,
        nostr_health: HealthBook::default(),
        nostr_known: vec![],
        rendezvous: None,
        rendezvous_fallback: None,
        lan_bind: lan.then(|| std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
        // A private address, as a phone would advertise; tests dial loopback.
        lan_ip: lan.then(|| "10.0.0.5".to_owned()),
        gate: PairGate::default(),
        mic: false,
        width: 320,
        height: 240,
        media: MediaConfig::default(),
    }
}

fn watch_config(p: &Pairing, relays: Vec<String>) -> WatchConfig {
    WatchConfig {
        channel_key: p.key.clone(),
        pid: PID.into(),
        code: p.code.clone(),
        label: "Test phone".into(),
        name: None,
        mic: false,
        lan: p.signal.clone(),
        rendezvous: None,
        rendezvous_fallback: None,
        ticket: None,
        nostr_station: p.nostr_station.clone(),
        nostr_secret: Some(PHONE_SK),
        nostr_relays: relays,
        nostr_health: HealthBook::default(),
        media: MediaConfig::default(),
    }
}

/// The QR read back as a phone would, with the LAN address pointed at
/// loopback (the link must carry a private address; the test dials 127.0.0.1).
fn scan(m: &Monitor) -> Result<Pairing, Box<dyn std::error::Error>> {
    let mut p = link::parse(&m.pair_link()?, Anchors::default())?;
    if let (Some(_), Some(port)) = (&p.signal, m.lan_port()) {
        p.signal = Some(format!("ws://127.0.0.1:{port}"));
    }
    Ok(p)
}

fn camera(m: &Monitor) {
    let src = m.video_source();
    tokio::spawn(async move {
        loop {
            src.capture_frame(&VideoFrame::new(
                VideoRotation::VideoRotation0,
                I420Buffer::new(320, 240),
            ));
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
    });
}

#[derive(Debug, Default)]
#[expect(
    clippy::option_option,
    reason = "not heard yet, versus heard with no code"
)]
struct Seen {
    way: Option<&'static str>,
    connected: bool,
    watch_sas: Option<Option<String>>,
    monitor_sas: Option<Option<String>>,
    refused: Option<String>,
    /// The Monitor's gate as it last reported it (phones it enrolled).
    gate: Option<PairGate>,
}

/// Runs both ends until the phone has decoded 10 frames (or 30 s pass).
async fn watch_until_frames(
    m_rx: &mut UnboundedReceiver<MonitorEvent>,
    w_rx: &mut UnboundedReceiver<WatchEvent>,
    want_sas: bool,
) -> Seen {
    let decoded = Arc::new(AtomicU32::new(0));
    let mut seen = Seen::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline
        && (decoded.load(Ordering::Relaxed) < 10
            || !seen.connected
            || (want_sas && (seen.watch_sas.is_none() || seen.monitor_sas.is_none())))
    {
        tokio::select! {
            Some(e) = w_rx.recv() => match e {
                WatchEvent::Way(w) => seen.way = Some(w),
                WatchEvent::Connected => seen.connected = true,
                WatchEvent::Sas(c) => seen.watch_sas = Some(c),
                WatchEvent::Refused(r) => { seen.refused = Some(r); return seen; }
                WatchEvent::Video(t) => {
                    let d = decoded.clone();
                    tokio::spawn(async move {
                        let mut s = NativeVideoStream::new(t);
                        while s.next().await.is_some() { d.fetch_add(1, Ordering::Relaxed); }
                    });
                }
                _ => {}
            },
            Some(e) = m_rx.recv() => match e {
                MonitorEvent::Sas { code, .. } => seen.monitor_sas = Some(code),
                MonitorEvent::Gate(g) => seen.gate = Some(g),
                _ => {}
            },
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    assert!(
        decoded.load(Ordering::Relaxed) >= 10,
        "the phone decoded the camera: {seen:?}"
    );
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_wifi_the_call_goes_over_the_lan_with_no_code_to_compare() -> R {
    let (monitor, mut m_rx) = Monitor::start(monitor_config(true, vec![])).await?;
    camera(&monitor);
    let p = scan(&monitor)?;
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![]))?;
    let seen = watch_until_frames(&mut m_rx, &mut w_rx, false).await;
    assert_eq!(seen.way, Some("lan"));
    assert!(seen.connected);
    assert_eq!(seen.watch_sas, None, "no code on the LAN");
    watch.stop();
    monitor.stop();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn off_the_wifi_the_call_goes_over_nostr_and_both_ends_show_one_code() -> R {
    let relay = common::relay(false).await?;
    let (monitor, mut m_rx) = Monitor::start(monitor_config(false, vec![relay.clone()])).await?;
    camera(&monitor);
    let p = scan(&monitor)?;
    assert!(p.signal.is_none() && p.nostr_station.is_some());
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![relay]))?;
    let seen = watch_until_frames(&mut m_rx, &mut w_rx, true).await;
    assert_eq!(seen.way, Some("nostr"));
    let (Some(Some(a)), Some(Some(b))) = (&seen.watch_sas, &seen.monitor_sas) else {
        return Err(format!("no security code: {seen:?}").into());
    };
    assert_eq!(a, b, "the same code on both phones");
    watch.stop();
    monitor.stop();
    Ok(())
}

/// The Monitor app restarts (the same keys, the phones it knows): the phone
/// that was watching hangs up the dead call and calls the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_monitor_that_restarts_is_called_again() -> R {
    let relay = common::relay(false).await?;
    let (first, mut m_rx) = Monitor::start(monitor_config(false, vec![relay.clone()])).await?;
    camera(&first);
    let p = scan(&first)?;
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![relay.clone()]))?;
    let seen = watch_until_frames(&mut m_rx, &mut w_rx, false).await;
    let gate = seen.gate.ok_or("the phone was never enrolled")?;
    first.stop();
    let mut cfg = monitor_config(false, vec![relay]);
    cfg.gate = gate;
    let (second, mut m_rx) = Monitor::start(cfg).await?;
    camera(&second);
    let again = watch_until_frames(&mut m_rx, &mut w_rx, false).await;
    assert!(
        again.connected,
        "the phone called the restarted Monitor: {again:?}"
    );
    watch.stop();
    second.stop();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_both_ways_open_the_lan_wins() -> R {
    let relay = common::relay(false).await?;
    let (monitor, mut m_rx) = Monitor::start(monitor_config(true, vec![relay.clone()])).await?;
    camera(&monitor);
    let p = scan(&monitor)?;
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![relay]))?;
    let seen = watch_until_frames(&mut m_rx, &mut w_rx, false).await;
    assert_eq!(seen.way, Some("lan"));
    watch.stop();
    monitor.stop();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_is_turned_away() -> R {
    let (monitor, _m_rx) = Monitor::start(monitor_config(true, vec![])).await?;
    let mut p = scan(&monitor)?;
    p.code = Some("WRONGcode00".into());
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![]))?;
    let refused = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(e) = w_rx.recv().await {
            if let WatchEvent::Refused(r) = e {
                return Some(r);
            }
        }
        None
    })
    .await?;
    assert_eq!(refused.as_deref(), Some("expired"));
    watch.stop();
    monitor.stop();
    Ok(())
}

#[test]
fn a_pairing_with_no_way_in_is_refused() {
    let p = Pairing {
        signal: None,
        key: KEY.into(),
        name: "Den".into(),
        token: None,
        code: None,
        expires_at: 0,
        relay: None,
        relay_trusted: false,
        nostr_station: None,
    };
    assert!(Watch::start(watch_config(&p, vec![])).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_apps_own_code_lets_a_phone_in_and_broadcasts_reach_it() -> R {
    let (monitor, mut m_rx) = Monitor::start(monitor_config(true, vec![])).await?;
    camera(&monitor);
    let mut p = scan(&monitor)?;
    // The Android shell mints the code and draws its own QR.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    monitor.set_code("APPcode1234", now + 60_000);
    p.code = Some("APPcode1234".into());
    let (watch, mut w_rx) = Watch::start(watch_config(&p, vec![]))?;
    let seen = watch_until_frames(&mut m_rx, &mut w_rx, false).await;
    assert!(seen.connected && seen.refused.is_none());
    monitor.broadcast(serde_json::json!({ "type": "battery", "pct": 64, "charging": true }));
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = w_rx.recv().await {
            if let WatchEvent::Control(f) = e {
                if f["type"] == "battery" {
                    return Some(f);
                }
            }
        }
        None
    })
    .await?;
    let f = got.ok_or("no battery frame")?;
    assert_eq!(
        (f["pct"].as_i64(), f["charging"].as_bool()),
        (Some(64), Some(true))
    );
    watch.stop();
    monitor.stop();
    Ok(())
}
