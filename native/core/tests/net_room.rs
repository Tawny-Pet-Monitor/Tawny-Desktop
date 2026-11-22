//! The LAN relay server and room clients over real sockets.
#![cfg(feature = "net")]

use std::time::Duration;

use serde_json::{Value, json};
use tawny_core::net::room::{LanServer, RoomEvent, connect};
use tawny_core::signal::room::{Kind, Refusal};
use tokio::sync::mpsc::UnboundedReceiver;

const KEY: &str = "Zx9_-abcDEF0123456789";
type R = Result<(), Box<dyn std::error::Error>>;

async fn next_frame(rx: &mut UnboundedReceiver<RoomEvent>, ty: &str) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            if let RoomEvent::Frame(v) = e {
                if v["type"] == ty {
                    return Some(v);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_and_a_viewer_meet_on_the_lan_relay() -> R {
    let server = LanServer::start("127.0.0.1:0".parse()?, KEY).await?;
    let base = format!("ws://127.0.0.1:{}", server.port);
    let (_station, mut st_rx) = connect(Kind::Lan, &base, true, KEY, Some("TICKETxx".into()));
    next_frame(&mut st_rx, "welcome")
        .await
        .ok_or("Monitor not admitted")?;

    let (viewer, mut v_rx) = connect(Kind::Lan, &base, false, KEY, Some("TICKETxx".into()));
    let welcome = next_frame(&mut v_rx, "welcome")
        .await
        .ok_or("Viewer not admitted")?;
    let station_id = welcome["peers"][0]["id"]
        .as_str()
        .ok_or("no station in welcome")?
        .to_owned();
    let joined = next_frame(&mut st_rx, "peer-joined")
        .await
        .ok_or("Monitor not told")?;
    assert_eq!(joined["role"], "viewer");

    viewer.send(
        json!({ "type": "offer", "to": station_id, "sdp": { "type": "offer", "sdp": "v=0" } }),
    );
    let offer = next_frame(&mut st_rx, "offer")
        .await
        .ok_or("offer not forwarded")?;
    assert_eq!(offer["from"], joined["id"]);

    viewer.close();
    next_frame(&mut st_rx, "peer-left")
        .await
        .ok_or("Monitor not told the Viewer left")?;
    server.stop();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_key_is_refused_for_good() -> R {
    let server = LanServer::start("127.0.0.1:0".parse()?, KEY).await?;
    let base = format!("ws://127.0.0.1:{}", server.port);
    let (_c, mut rx) = connect(Kind::Lan, &base, false, "a-different-channel-key", None);
    let down = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            if let RoomEvent::Down(r) = e {
                return r;
            }
        }
        None
    })
    .await?;
    assert_eq!(down, Some(Refusal::Admission));
    // No retry: the stream ends after the log line.
    let rest = tokio::time::timeout(Duration::from_secs(3), async {
        let mut n = 0;
        while let Some(e) = rx.recv().await {
            if matches!(e, RoomEvent::Down(_) | RoomEvent::Up) {
                n += 1;
            }
        }
        n
    })
    .await?;
    assert_eq!(rest, 0, "it did not dial again");
    server.stop();
    Ok(())
}
