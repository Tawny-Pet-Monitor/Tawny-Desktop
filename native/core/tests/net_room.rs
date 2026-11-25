//! The LAN relay server and room clients over real sockets.
#![cfg(feature = "net")]

use std::time::Duration;

use serde_json::{Value, json};
use tawny_core::net::room::{ADMIT_WAIT, LanServer, RoomEvent, connect, connect_or_fall_back};
use tawny_core::signal::room::{Kind, Refusal};
use tokio::sync::mpsc::UnboundedReceiver;

const KEY: &str = "Zx9_-abcDEF0123456789";
type R = Result<(), Box<dyn std::error::Error>>;

async fn next_frame(rx: &mut UnboundedReceiver<RoomEvent>, ty: &str) -> Option<Value> {
    frame_within(rx, ty, 5).await
}

async fn frame_within(rx: &mut UnboundedReceiver<RoomEvent>, ty: &str, secs: u64) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(secs), async {
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

/// A free port with nothing listening on it.
async fn dead_base() -> Result<String, Box<dyn std::error::Error>> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(format!("ws://127.0.0.1:{port}"))
}

/// Both ends set to an own relay that is down, the build's one in reserve:
/// both give up on it and meet on the reserve (app.js's `fallBackToDefault`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_relay_that_is_down_falls_back_and_both_ends_meet() -> R {
    let server = LanServer::start("127.0.0.1:0".parse()?, KEY).await?;
    let reserve = format!("ws://127.0.0.1:{}", server.port);
    let own = dead_base().await?;
    let t = Some("TICKETxx".to_owned());
    let (_station, mut st_rx) =
        connect_or_fall_back(Kind::Lan, &own, Some(&reserve), true, KEY, t.clone());
    let (_viewer, mut v_rx) = connect_or_fall_back(Kind::Lan, &own, Some(&reserve), false, KEY, t);
    // Two failed dials (1 s apart) before the swap. They swap together, so
    // either may be let in first: the Monitor learns of the Viewer from its
    // `welcome` or from a `peer-joined`.
    let st_welcome = frame_within(&mut st_rx, "welcome", 10)
        .await
        .ok_or("Monitor never reached the reserve")?;
    frame_within(&mut v_rx, "welcome", 10)
        .await
        .ok_or("Viewer never reached the reserve")?;
    let met = st_welcome["peers"]
        .as_array()
        .is_some_and(|p| p.iter().any(|p| p["role"] == "viewer"))
        || next_frame(&mut st_rx, "peer-joined").await.is_some();
    assert!(met, "they didn't meet on the reserve");
    server.stop();
    Ok(())
}

/// A host that takes the socket and then says nothing (not a Tawny relay)
/// is given up on after [`ADMIT_WAIT`]: no dial ever fails, so only the wait
/// notices.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_relay_that_never_lets_us_in_is_given_up_after_the_wait() -> R {
    let server = LanServer::start("127.0.0.1:0".parse()?, KEY).await?;
    let reserve = format!("ws://127.0.0.1:{}", server.port);
    let mute = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let own = format!("ws://127.0.0.1:{}", mute.local_addr()?.port());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = mute.accept().await {
            if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                held.push(ws);
            }
        }
    });
    let started = tokio::time::Instant::now();
    let (_station, mut st_rx) = connect_or_fall_back(
        Kind::Lan,
        &own,
        Some(&reserve),
        true,
        KEY,
        Some("TICKETxx".into()),
    );
    let wait = ADMIT_WAIT.as_secs() + 5;
    frame_within(&mut st_rx, "welcome", wait)
        .await
        .ok_or("never let in on the reserve")?;
    assert!(
        started.elapsed() >= ADMIT_WAIT,
        "gave up before the wait was over"
    );
    server.stop();
    Ok(())
}

/// Without a reserve nothing changes: a dead relay is redialled, never left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_reserve_a_dead_relay_is_only_redialled() -> R {
    let own = dead_base().await?;
    let (_station, mut rx) = connect_or_fall_back(Kind::Lan, &own, None, true, KEY, None);
    let mut downs = 0;
    let _ = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(e) = rx.recv().await {
            match e {
                RoomEvent::Down(_) => downs += 1,
                RoomEvent::Log(l) => {
                    assert!(!l.contains("built-in"), "fell back with no reserve: {l}");
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(downs >= 2, "redialled {downs} times");
    Ok(())
}

/// An own relay that answers with a refusal is working: it is not swapped
/// for the reserve (as app.js, whose fatal codes never fall back). The other
/// end still uses it, and the two would never meet on the reserve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_relay_that_refuses_is_not_swapped_out() -> R {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    let server = LanServer::start("127.0.0.1:0".parse()?, KEY).await?;
    let reserve = format!("ws://127.0.0.1:{}", server.port);
    // A relay that takes each socket and closes it at once with 4010, "no
    // Monitor in this room": retryable, and it is answering.
    let refusing = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let own = format!("ws://127.0.0.1:{}", refusing.local_addr()?.port());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = refusing.accept().await {
            if let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await {
                let _ = ws
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(4010),
                        reason: "monitor offline".into(),
                    })))
                    .await;
            }
        }
    });
    let (_viewer, mut rx) = connect_or_fall_back(
        Kind::Lan,
        &own,
        Some(&reserve),
        false,
        KEY,
        Some("TICKETxx".into()),
    );
    let mut downs = 0;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            match e {
                RoomEvent::Down(r) => {
                    assert_eq!(r, Some(Refusal::MonitorOffline));
                    downs += 1;
                }
                RoomEvent::Log(l) => assert!(!l.contains("built-in"), "swapped on a refusal: {l}"),
                RoomEvent::Frame(f) => assert!(f["type"] != "welcome", "let in on the reserve"),
                RoomEvent::Up => {}
            }
        }
    })
    .await;
    assert!(downs >= 2, "dialled the refusing relay {downs} times");
    server.stop();
    Ok(())
}
