//! The Nostr driver over real `WebSocket`s, against a tiny in-process relay.
#![cfg(feature = "net")]

use std::time::Duration;

use serde_json::{Value, json};
use tawny_core::net::nostr::{ChannelEvent, start};
use tawny_core::nostr::event;
use tawny_core::relay::health::{HealthBook, Verdict};
use tawny_core::signal::nostr::{Config, Role};
use tokio::sync::mpsc;

const KEY: &str = "Zx9_-abcDEF0123456789";
const STATION_SK: [u8; 32] = [0x61; 32];
const VIEWER_SK: [u8; 32] = [0x62; 32];
type R = Result<(), Box<dyn std::error::Error>>;

mod common;
use common::relay;

/// Waits up to 10 s for an event matching `f`, keeping every event seen on the way.
async fn until<F: FnMut(&ChannelEvent) -> bool>(
    rx: &mut mpsc::UnboundedReceiver<ChannelEvent>,
    seen: &mut Vec<ChannelEvent>,
    mut f: F,
) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = rx.recv().await {
            let hit = f(&e);
            let frame = if let ChannelEvent::Frame(v) = &e {
                Some(v.clone())
            } else {
                None
            };
            seen.push(e);
            if hit {
                return Some(frame.unwrap_or(Value::Null));
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

fn is_frame(e: &ChannelEvent, ty: &str) -> bool {
    matches!(e, ChannelEvent::Frame(v) if v["type"] == ty)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_and_a_handheld_pair_and_talk_over_real_sockets() -> R {
    let good = relay(false).await?;
    let bad = relay(true).await?;
    let relays = vec![good.clone(), bad.clone()];
    let station_pub = event::public_key(&STATION_SK)?;
    let viewer_pub = event::public_key(&VIEWER_SK)?;
    let (mut m_seen, mut v_seen) = (Vec::new(), Vec::new());

    let station = Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: STATION_SK,
        station_pub: String::new(),
        remembered: vec![],
    };
    let (monitor, mut m_rx) = start(station, &relays, HealthBook::default())?;
    let viewer = Config {
        channel_key: KEY.into(),
        role: Role::Viewer,
        secret: VIEWER_SK,
        station_pub: station_pub.clone(),
        remembered: vec![],
    };
    let (handheld, mut v_rx) = start(viewer, &relays, HealthBook::default())?;

    // Each side finds the other.
    let joined = until(&mut v_rx, &mut v_seen, |e| is_frame(e, "peer-joined"))
        .await
        .ok_or("Handheld never saw the Monitor")?;
    assert_eq!(joined["role"], "station");
    assert_eq!(joined["id"], station_pub.as_str());
    let heard = until(&mut m_rx, &mut m_seen, |e| is_frame(e, "peer-joined"))
        .await
        .ok_or("Monitor never saw the Handheld")?;
    assert_eq!(heard["role"], "viewer");
    assert_eq!(heard["id"], viewer_pub.as_str());

    // An offer one way, an answer the other.
    handheld.send(json!({ "type": "offer", "to": station_pub, "sdp": { "type": "offer", "sdp": "v=0" }, "pc": "CODE5678" }));
    let offer = until(&mut m_rx, &mut m_seen, |e| is_frame(e, "offer"))
        .await
        .ok_or("no offer")?;
    assert_eq!(offer["from"], viewer_pub.as_str());
    assert_eq!(offer["pc"], "CODE5678");
    assert!(offer.get("to").is_none());
    monitor.send(
        json!({ "type": "answer", "to": viewer_pub, "sdp": { "type": "answer", "sdp": "v=0" } }),
    );
    let answer = until(&mut v_rx, &mut v_seen, |e| is_frame(e, "answer"))
        .await
        .ok_or("no answer")?;
    assert_eq!(answer["from"], station_pub.as_str());

    // Both relays were judged: the good one passed its test, the refusing one
    // was dropped and written into the health book.
    for seen in [&m_seen, &v_seen] {
        let last = seen
            .iter()
            .rev()
            .find_map(|e| {
                if let ChannelEvent::Relays(r) = e {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .ok_or("no relay report")?;
        let verdict = |url: &str| last.iter().find(|r| r.url == url).map(|r| r.verdict);
        assert_eq!(verdict(&good), Some("ok"));
        assert_eq!(verdict(&bad), Some("refused"));
        let book = seen
            .iter()
            .rev()
            .find_map(|e| {
                if let ChannelEvent::Health(h) = e {
                    Some(h.clone())
                } else {
                    None
                }
            })
            .ok_or("no health book")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let now = u64::try_from(now)?;
        assert_eq!(book.get(&bad, now).map(|h| h.v), Some(Verdict::Refused));
        assert_eq!(book.get(&good, now).map(|h| h.v), Some(Verdict::Ok));
    }

    // Leaving: the Monitor is told at once, not after the 50 s timeout.
    handheld.close();
    until(&mut m_rx, &mut m_seen, |e| is_frame(e, "peer-left"))
        .await
        .ok_or("no peer-left")?;
    monitor.close();
    Ok(())
}
