//! A Monitor and a Viewer from `call::media`, wired together in memory (the
//! transport just swaps `to` for `from`), with synthetic camera frames.
#![cfg(feature = "media")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use serde_json::{Value, json};
use tawny_core::call::media::{
    MediaConfig, Station, StationConfig, StationEvent, Viewer, ViewerConfig, ViewerEvent,
};
use tawny_core::call::policy::{PAIR_TTL_MS, PairCode, PairGate};
use tokio::sync::mpsc;

const KEY: &str = "Zx9_-abcDEF0123456789";
const STATION_ID: &str = "station-1";
const VIEWER_ID: &str = "viewer-1";
const PID: &str = "0123456789abcdef0123456789abcdef";
type R = Result<(), Box<dyn std::error::Error>>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

fn station(code: &str) -> (Station, mpsc::UnboundedReceiver<StationEvent>) {
    let gate = PairGate {
        code: Some(PairCode {
            code: code.into(),
            expires_ms: now() + PAIR_TTL_MS,
        }),
        known: vec![],
    };
    Station::start(StationConfig {
        channel_key: KEY.into(),
        gate,
        width: 320,
        height: 240,
        mic: false,
        media: MediaConfig::default(),
    })
}

fn viewer(code: &str) -> (Viewer, mpsc::UnboundedReceiver<ViewerEvent>) {
    Viewer::start(ViewerConfig {
        channel_key: KEY.into(),
        pid: Some(PID.into()),
        code: Some(code.into()),
        label: "Test\u{1}Phone".into(),
        name: None,
        mic: false,
        media: MediaConfig::default(),
    })
}

/// The transport: frames out of one side go into the other with `from` set.
fn relay(mut v: Value, from: &str) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("to");
        o.insert("from".into(), json!(from));
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_viewer_calls_its_monitor_and_sees_the_camera() -> R {
    let (st, mut st_rx) = station("CODE5678xyz");
    let (vw, mut vw_rx) = viewer("CODE5678xyz");

    // Synthetic camera at 15 fps.
    let src = st.video_source();
    tokio::spawn(async move {
        let mut n = 0u8;
        loop {
            let mut b = I420Buffer::new(320, 240);
            b.data_mut().0.fill(n);
            n = n.wrapping_add(3);
            src.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, b));
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
    });

    vw.signal(json!({ "type": "welcome", "id": VIEWER_ID, "peers": [{ "id": STATION_ID, "role": "station" }] }));

    let (mut joined, mut gate, mut capture, mut st_sas, mut vw_sas) =
        (None, None, None, None, None);
    let decoded = Arc::new(AtomicU32::new(0));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while (st_sas.is_none() || vw_sas.is_none() || decoded.load(Ordering::Relaxed) < 10)
        && tokio::time::Instant::now() < deadline
    {
        tokio::select! {
            Some(e) = st_rx.recv() => match e {
                StationEvent::Signal(f) => { assert_eq!(f["to"], VIEWER_ID); vw.signal(relay(f, STATION_ID)); }
                StationEvent::Joined { id, label, pid, .. } => {
                    assert_eq!(pid.as_deref(), Some(PID), "the bond id comes with the join");
                    joined = Some((id, label));
                }
                StationEvent::Gate(g) => gate = Some(g),
                StationEvent::Capture(on) => capture = Some(on),
                StationEvent::Sas { code, .. } => st_sas = Some(code),
                _ => {}
            },
            Some(e) = vw_rx.recv() => match e {
                ViewerEvent::Signal(f) => { assert_eq!(f["to"], STATION_ID); st.signal(relay(f, VIEWER_ID)); }
                ViewerEvent::Sas(code) => vw_sas = Some(code),
                ViewerEvent::Video(track) => {
                    let decoded = decoded.clone();
                    tokio::spawn(async move {
                        let mut s = NativeVideoStream::new(track);
                        while let Some(f) = s.next().await {
                            assert_eq!((f.buffer.width(), f.buffer.height()), (320, 240));
                            decoded.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
                ViewerEvent::Refused(r) => return Err(format!("refused: {r}").into()),
                _ => {}
            },
            // Decoding happens in another task: look again every 100 ms.
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    assert_eq!(
        joined,
        Some((VIEWER_ID.to_owned(), "TestPhone".to_owned())),
        "label cleaned to printable ASCII"
    );
    assert_eq!(
        gate.map(|g| g.known),
        Some(vec![PID.to_owned()]),
        "the phone was enrolled"
    );
    assert_eq!(
        capture,
        Some(true),
        "the camera is wanted once someone is connected"
    );
    let (Some(Some(a)), Some(Some(b))) = (st_sas, vw_sas) else {
        return Err("no security code".into());
    };
    assert_eq!(a, b, "both ends show the same code");
    assert_eq!(a.len(), 7);
    assert!(
        decoded.load(Ordering::Relaxed) >= 10,
        "the Viewer decoded the Monitor's camera"
    );

    // The Viewer leaves: the Monitor stops wanting the camera.
    st.signal(json!({ "type": "peer-left", "id": VIEWER_ID }));
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = st_rx.recv().await {
            if let StationEvent::Capture(false) = e {
                return true;
            }
        }
        false
    })
    .await?;
    assert!(stopped);
    vw.close();
    st.close();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_gets_bye_expired_and_never_the_camera() -> R {
    let (st, mut st_rx) = station("CODE5678xyz");
    let (vw, mut vw_rx) = viewer("WRONGcode00");
    vw.signal(json!({ "type": "welcome", "id": VIEWER_ID, "peers": [{ "id": STATION_ID, "role": "station" }] }));
    let refused = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                Some(e) = st_rx.recv() => match e {
                    StationEvent::Signal(f) => vw.signal(relay(f, STATION_ID)),
                    StationEvent::Joined { .. } | StationEvent::Capture(true) => return Err("let in on a wrong code"),
                    _ => {}
                },
                Some(e) = vw_rx.recv() => match e {
                    ViewerEvent::Signal(f) => st.signal(relay(f, VIEWER_ID)),
                    ViewerEvent::Refused(r) => return Ok(r),
                    _ => {}
                },
            }
        }
    }).await?;
    assert_eq!(refused.map_err(str::to_owned)?, "expired");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_monitor_can_answer_one_viewer_and_disconnect_it() -> R {
    let (st, mut st_rx) = station("CODE5678xyz");
    let (vw, mut vw_rx) = viewer("CODE5678xyz");
    vw.signal(json!({ "type": "welcome", "id": VIEWER_ID, "peers": [{ "id": STATION_ID, "role": "station" }] }));
    let mut connected = false;
    let (mut acked, mut bye, mut left) = (false, false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !(acked && bye && left) && tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(e) = st_rx.recv() => match e {
                StationEvent::Signal(f) => vw.signal(relay(f, STATION_ID)),
                StationEvent::Connected { .. } if !connected => {
                    connected = true;
                    st.send_to(VIEWER_ID, json!({ "type": "chime-ack", "sound": "bark" }));
                }
                StationEvent::Left { id } => { assert_eq!(id, VIEWER_ID); left = true; }
                _ => {}
            },
            Some(e) = vw_rx.recv() => match e {
                ViewerEvent::Signal(f) => st.signal(relay(f, VIEWER_ID)),
                ViewerEvent::Control(f) if f["type"] == "chime-ack" => {
                    assert_eq!(f["sound"], "bark");
                    acked = true;
                    st.disconnect(VIEWER_ID);
                }
                ViewerEvent::Refused(_) | ViewerEvent::Left => bye = true,
                _ => {}
            },
        }
    }
    assert!(acked, "the ack reached that viewer");
    assert!(bye && left, "disconnect said bye and dropped the call");
    st.close();
    vw.close();
    Ok(())
}

/// "My camera": the Viewer's own camera reaches the Monitor only while it is
/// switched on; switching offers again so the Monitor hears the direction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn my_camera_reaches_the_monitor_only_while_on() -> R {
    let (st, mut st_rx) = station("CODEcam1234");
    let (vw, mut vw_rx) = viewer("CODEcam1234");

    // The Viewer's synthetic camera runs all along; frames go out only while on.
    let src = vw.video_source();
    tokio::spawn(async move {
        loop {
            let mut b = I420Buffer::new(640, 480);
            b.data_mut().0.fill(200);
            src.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, b));
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
    });

    vw.signal(json!({ "type": "welcome", "id": VIEWER_ID, "peers": [{ "id": STATION_ID, "role": "station" }] }));
    let got = Arc::new(AtomicU32::new(0));
    let mut connected = false;
    let mut offers = 0;
    let mut switched_on = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(e) = st_rx.recv() => match e {
                StationEvent::Signal(f) => vw.signal(relay(f, STATION_ID)),
                StationEvent::Connected { .. } => connected = true,
                StationEvent::ViewerVideo { id, track } => {
                    assert_eq!(id, VIEWER_ID);
                    let got = got.clone();
                    tokio::spawn(async move {
                        let mut s = NativeVideoStream::new(track);
                        while let Some(f) = s.next().await {
                            assert_eq!((f.buffer.width(), f.buffer.height()), (640, 480));
                            got.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
                _ => {}
            },
            Some(e) = vw_rx.recv() => {
                if let ViewerEvent::Signal(f) = e {
                    if f["type"] == "offer" {
                        offers += 1;
                    }
                    st.signal(relay(f, VIEWER_ID));
                }
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        if connected && !switched_on {
            // Connected and settled with the camera off: nothing arrived.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                got.load(Ordering::Relaxed),
                0,
                "nothing is sent before My camera is on"
            );
            vw.set_camera(true);
            switched_on = true;
        }
        if got.load(Ordering::Relaxed) >= 10 {
            break;
        }
    }
    assert!(
        got.load(Ordering::Relaxed) >= 10,
        "the Monitor decoded the Viewer's camera"
    );
    assert_eq!(
        offers, 2,
        "switching the camera on offered again (the slot's direction)"
    );

    // Off again: the frames stop.
    vw.set_camera(false);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = got.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        got.load(Ordering::Relaxed) <= before + 1,
        "My camera off stops the frames"
    );
    vw.close();
    st.close();
    Ok(())
}

/// A Viewer's call to `st`, pumped until both ends have shown a security
/// code for it (or `secs` pass). Returns the Viewer's code; the Monitor's
/// events go to `seen` (its code as `sas …`).
async fn connect(
    st: &Station,
    st_rx: &mut mpsc::UnboundedReceiver<StationEvent>,
    vw: &Viewer,
    vw_rx: &mut mpsc::UnboundedReceiver<ViewerEvent>,
    seen: &mut Vec<String>,
    secs: u64,
) -> Option<String> {
    vw.signal(json!({ "type": "welcome", "id": VIEWER_ID, "peers": [{ "id": STATION_ID, "role": "station" }] }));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let (mut mine, mut theirs) = (None, false);
    while mine.is_none() || !theirs {
        tokio::select! {
            Some(e) = st_rx.recv() => match e {
                StationEvent::Signal(f) => vw.signal(relay(f, STATION_ID)),
                StationEvent::Joined { id, .. } => seen.push(format!("joined {id}")),
                StationEvent::Left { id } => seen.push(format!("left {id}")),
                StationEvent::Sas { code, .. } => {
                    seen.push(format!("sas {}", code.as_deref().unwrap_or("none")));
                    theirs = true;
                }
                _ => {}
            },
            Some(e) = vw_rx.recv() => match e {
                ViewerEvent::Signal(f) => st.signal(relay(f, VIEWER_ID)),
                ViewerEvent::Sas(code) => mine = Some(code.unwrap_or_default()),
                _ => {}
            },
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    mine
}

/// A Viewer whose call died without a word (its phone dropped off the Wi-Fi)
/// calls again under the same room id, with a new connection. The Monitor
/// lets the dead call go and takes the new one as a new call, so it shows
/// the code that call has: renegotiating the old call onto the new
/// certificate also connects, but left the Monitor showing the old code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_call_from_the_same_viewer_replaces_its_dead_one() -> R {
    let (st, mut st_rx) = station("CODE5678xyz");
    let mut seen = Vec::new();
    let (first, mut first_rx) = viewer("CODE5678xyz");
    connect(&st, &mut st_rx, &first, &mut first_rx, &mut seen, 20)
        .await
        .ok_or("the first call never came up")?;
    // Gone without a bye or a peer-left: the Monitor still holds that call.
    first.close();
    let (again, mut again_rx) = viewer("CODE5678xyz");
    let code = connect(&st, &mut st_rx, &again, &mut again_rx, &mut seen, 20)
        .await
        .ok_or_else(|| format!("the new call was never answered: {seen:?}"))?;
    assert!(!code.is_empty(), "the Viewer has a code");
    assert_eq!(
        seen.last(),
        Some(&format!("sas {code}")),
        "the Monitor shows the new call's code: {seen:?}"
    );
    assert!(
        seen.iter().any(|e| *e == format!("left {VIEWER_ID}")),
        "the dead call is let go: {seen:?}"
    );
    st.close();
    again.close();
    Ok(())
}
