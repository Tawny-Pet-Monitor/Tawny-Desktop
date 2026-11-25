//! Viewers coming and going with sound on. The platform audio module used to
//! be shut down by libwebrtc whenever one call ended, so the next Viewer's
//! call crashed the Monitor (seen on Android). Sound now goes through the
//! app: these tests push real tones in and count non-silent frames out.
#![cfg(feature = "media")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use serde_json::{Value, json};
use tawny_core::call::media::{
    AUDIO_FRAME, AUDIO_RATE, MediaConfig, Station, StationConfig, StationEvent, Viewer,
    ViewerConfig, ViewerEvent,
};
use tawny_core::call::policy::{PAIR_TTL_MS, PairCode, PairGate};
use tokio::sync::mpsc::UnboundedReceiver;

const KEY: &str = "Zx9_-abcDEF0123456789";
const STATION_ID: &str = "station-1";
const CODE: &str = "CODE5678xyz";
type R = Result<(), Box<dyn std::error::Error>>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

fn relay(mut v: Value, from: &str) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("to");
        o.insert("from".into(), json!(from));
    }
    v
}

/// Pushes a 440 Hz tone, 10 ms at a time, forever.
fn tone(src: NativeAudioSource) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        let mut phase = 0f32;
        let step = 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
        loop {
            tick.tick().await;
            let data: Vec<i16> = (0..AUDIO_FRAME)
                .map(|_| {
                    phase = (phase + step) % (2.0 * std::f32::consts::PI);
                    #[expect(clippy::cast_possible_truncation, reason = "bounded by the amplitude")]
                    let s = (phase.sin() * 8000.0) as i16;
                    s
                })
                .collect();
            let f = AudioFrame {
                data: data.into(),
                sample_rate: AUDIO_RATE,
                num_channels: 1,
                samples_per_channel: u32::try_from(AUDIO_FRAME).unwrap_or(480),
            };
            if src.capture_frame(&f).await.is_err() {
                return;
            }
        }
    });
}

/// Counts frames on `track` that carry sound (not silence).
fn listen(track: RtcAudioTrack, heard: Arc<AtomicU32>) {
    tokio::spawn(async move {
        let mut s = NativeAudioStream::new(track, 48_000, 1);
        while let Some(f) = s.next().await {
            if f.data.iter().any(|x| x.unsigned_abs() > 1000) {
                heard.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}

/// Camera frames, so the calls are real calls.
fn camera(st: &Station) {
    let src = st.video_source();
    tokio::spawn(async move {
        loop {
            let b = I420Buffer::new(320, 240);
            src.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, b));
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
    });
}

struct Phone {
    id: String,
    vw: Viewer,
    rx: UnboundedReceiver<ViewerEvent>,
    heard: Arc<AtomicU32>,
}

fn phone(n: u8) -> Phone {
    let (vw, rx) = Viewer::start(ViewerConfig {
        channel_key: KEY.into(),
        pid: Some(format!("{n:032x}")),
        code: Some(CODE.into()),
        label: format!("Phone {n}"),
        name: None,
        mic: true,
        media: MediaConfig::default(),
    });
    if let Some(a) = vw.audio_source() {
        tone(a);
    }
    vw.set_talking(true);
    vw.signal(json!({ "type": "welcome", "id": format!("viewer-{n}"),
        "peers": [{ "id": STATION_ID, "role": "station" }] }));
    Phone {
        id: format!("viewer-{n}"),
        vw,
        rx,
        heard: Arc::default(),
    }
}

/// Moves frames between the Monitor and the phones until `done` or `secs`.
async fn run(
    st: &Station,
    st_rx: &mut UnboundedReceiver<StationEvent>,
    phones: &mut [Phone],
    talk: &Arc<AtomicU32>,
    secs: u64,
    done: impl Fn(&[Phone]) -> bool,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if done(phones) {
            return Ok(true);
        }
        while let Ok(e) = st_rx.try_recv() {
            match e {
                StationEvent::Signal(f) => {
                    if let Some(p) = phones.iter().find(|p| f["to"] == p.id.as_str()) {
                        p.vw.signal(relay(f, STATION_ID));
                    }
                }
                StationEvent::TalkAudio { track, .. } => listen(track, talk.clone()),
                _ => {}
            }
        }
        for p in phones.iter_mut() {
            while let Ok(e) = p.rx.try_recv() {
                match e {
                    ViewerEvent::Signal(f) => st.signal(relay(f, &p.id)),
                    ViewerEvent::Audio(t) => listen(t, p.heard.clone()),
                    ViewerEvent::Refused(r) => return Err(format!("{} refused: {r}", p.id)),
                    _ => {}
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(done(phones))
}

fn leave(st: &Station, p: &Phone) {
    p.vw.close();
    st.signal(json!({ "type": "peer-left", "id": p.id }));
}

fn heard(p: &Phone) -> u32 {
    p.heard.load(Ordering::Relaxed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn viewers_come_and_go_and_the_sound_keeps_working() -> R {
    let (st, mut st_rx) = Station::start(StationConfig {
        channel_key: KEY.into(),
        gate: PairGate {
            code: Some(PairCode {
                code: CODE.into(),
                expires_ms: now() + PAIR_TTL_MS,
            }),
            known: vec![],
        },
        width: 320,
        height: 240,
        mic: true,
        media: MediaConfig::default(),
    });
    camera(&st);
    tone(st.audio_source().ok_or("no microphone source")?);
    let talk = Arc::new(AtomicU32::new(0));

    // A: hears the Monitor, and the Monitor hears A's talk-back.
    let mut phones = vec![phone(1)];
    let ok = run(&st, &mut st_rx, &mut phones, &talk, 20, |p| {
        heard(&p[0]) >= 50
    })
    .await?;
    assert!(
        ok,
        "phone 1 heard the Monitor ({} frames)",
        heard(&phones[0])
    );
    let ok = run(&st, &mut st_rx, &mut phones, &talk, 10, |_| {
        talk.load(Ordering::Relaxed) >= 20
    })
    .await?;
    assert!(ok, "the Monitor heard phone 1 talk back");

    // A leaves; B joins on the same Monitor. This is the call that crashed.
    leave(&st, &phones[0]);
    let mut phones = vec![phone(2)];
    let ok = run(&st, &mut st_rx, &mut phones, &talk, 20, |p| {
        heard(&p[0]) >= 50
    })
    .await?;
    assert!(ok, "phone 2 heard the Monitor after phone 1 left");

    // C joins beside B; then B leaves and C must keep hearing.
    phones.push(phone(3));
    let ok = run(&st, &mut st_rx, &mut phones, &talk, 20, |p| {
        heard(&p[1]) >= 50
    })
    .await?;
    assert!(ok, "phone 3 heard the Monitor beside phone 2");
    leave(&st, &phones[0]);
    let before = heard(&phones[1]);
    let ok = run(&st, &mut st_rx, &mut phones, &talk, 10, |p| {
        heard(&p[1]) >= before + 100
    })
    .await?;
    assert!(ok, "phone 3 kept hearing after phone 2 left");

    for p in &phones {
        p.vw.close();
    }
    st.close();
    Ok(())
}
