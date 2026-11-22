//! A Monitor and a Handheld wired together in memory, plus the attacks
//! `transport.js` is built to refuse.

use serde_json::{Value, json};
use tawny_core::nostr::event::{self, Event};
use tawny_core::signal::nostr::{Config, Output, Role, Session, join_proof};

const KEY: &str = "Zx9_-abcDEF0123456789";
const T0: u64 = 1_795_150_000_000;
const STATION_SK: [u8; 32] = [0x41; 32];
const VIEWER_SK: [u8; 32] = [0x42; 32];

type R = Result<(), Box<dyn std::error::Error>>;

fn station() -> Result<Session, Box<dyn std::error::Error>> {
    Ok(Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: STATION_SK,
        station_pub: String::new(),
        remembered: vec![],
    })?)
}

fn viewer(key: &str, sk: [u8; 32]) -> Result<Session, Box<dyn std::error::Error>> {
    Ok(Session::new(Config {
        channel_key: key.into(),
        role: Role::Viewer,
        secret: sk,
        station_pub: event::public_key(&STATION_SK)?,
        remembered: vec![],
    })?)
}

fn published(out: &[Output]) -> Vec<Value> {
    out.iter()
        .filter_map(|o| match o {
            Output::Publish(e) => serde_json::to_value(e).ok(),
            _ => None,
        })
        .collect()
}

fn emitted(out: &[Output]) -> Vec<Value> {
    out.iter()
        .filter_map(|o| {
            if let Output::Emit(v) = o {
                Some(v.clone())
            } else {
                None
            }
        })
        .collect()
}

fn rejects(out: &[Output]) -> Vec<&'static str> {
    out.iter()
        .filter_map(|o| {
            if let Output::Reject { code, .. } = o {
                Some(*code)
            } else {
                None
            }
        })
        .collect()
}

/// Delivers every wrap in `out` to `to`, returning what `to` produced.
fn deliver(out: &[Output], to: &mut Session, now: u64) -> Vec<Output> {
    published(out)
        .iter()
        .flat_map(|w| to.receive(w, 2, now))
        .collect()
}

#[test]
fn join_proof_matches_js() {
    let p = join_proof(
        "Zx9_-abcDEF",
        &"aa".repeat(32),
        &"bb".repeat(32),
        1_795_150_000,
    );
    assert_eq!(
        p,
        "0139d31340d325b06ce871e7414bcaddadc635e6c3fecd3932dc2a371c96c2db"
    );
}

#[test]
fn a_handheld_joins_and_frames_flow_both_ways() -> R {
    let mut m = station()?;
    let mut v = viewer(KEY, VIEWER_SK)?;
    assert_eq!(emitted(&m.open(T0))[0]["type"], "welcome");
    let v_open = v.open(T0);
    assert_eq!(emitted(&v_open)[0]["type"], "welcome");

    // join → Monitor: peer-joined, remembers the key, answers here.
    let m_out = deliver(&v_open, &mut m, T0 + 50);
    assert!(rejects(&m_out).is_empty(), "{m_out:?}");
    let joined = emitted(&m_out);
    assert_eq!(joined[0]["type"], "peer-joined");
    assert_eq!(joined[0]["role"], "viewer");
    assert_eq!(joined[0]["id"], v.public_key());
    assert!(
        m_out
            .iter()
            .any(|o| matches!(o, Output::Remember(l) if l == &vec![v.public_key().to_owned()]))
    );

    // here → Handheld: peer-joined station.
    let v_out = deliver(&m_out, &mut v, T0 + 100);
    let e = emitted(&v_out);
    assert_eq!(e[0]["type"], "peer-joined");
    assert_eq!(e[0]["role"], "station");
    assert_eq!(e[0]["id"], m.public_key());

    // offer → Monitor, delivered with `from` and without `to`.
    let station_id = m.public_key().to_owned();
    let sent = v.send(&json!({ "type": "offer", "to": station_id, "sdp": { "type": "offer", "sdp": "v=0" }, "pc": "CODE5678" }), T0 + 200);
    let got = emitted(&deliver(&sent, &mut m, T0 + 250));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["type"], "offer");
    assert_eq!(got[0]["from"], v.public_key());
    assert_eq!(got[0]["pc"], "CODE5678");
    assert!(got[0].get("to").is_none());

    // ICE is batched: nothing until the 150 ms batch is due, then one envelope.
    let viewer_id = v.public_key().to_owned();
    let mut out = Vec::new();
    for i in 0..3 {
        out.extend(m.send(&json!({ "type": "ice", "to": viewer_id, "candidate": { "candidate": format!("c{i}") } }), T0 + 300));
    }
    assert_eq!(published(&out), [] as [Value; 0]);
    let flushed = m.tick(T0 + 460);
    assert_eq!(
        published(&flushed).len(),
        1,
        "three candidates, one envelope"
    );
    let ice = emitted(&deliver(&flushed, &mut v, T0 + 500));
    assert_eq!(ice.iter().filter(|f| f["type"] == "ice").count(), 3);
    Ok(())
}

#[test]
fn a_join_with_the_wrong_channel_key_is_refused() -> R {
    let mut m = station()?;
    m.open(T0);
    let mut stranger = viewer("not-the-channel-key-0000", [0x43; 32])?;
    let out = deliver(&stranger.open(T0), &mut m, T0 + 10);
    assert_eq!(rejects(&out), vec!["bad-proof"]);
    assert_eq!(emitted(&out), [] as [Value; 0]);
    Ok(())
}

#[test]
fn frames_from_a_key_that_never_joined_are_refused() -> R {
    // The Handheld joins one instance of the Monitor...
    let mut before = station()?;
    before.open(T0);
    let mut v = viewer(KEY, VIEWER_SK)?;
    let here = deliver(&v.open(T0), &mut before, T0);
    deliver(&here, &mut v, T0);
    // ...which restarts (same key, empty member list). Until the phone joins
    // again, its frames are not accepted.
    let mut after = station()?;
    after.open(T0 + 1_000);
    let sent = v.send(
        &json!({ "type": "offer", "to": after.public_key(), "sdp": {} }),
        T0 + 1_005,
    );
    assert_eq!(
        published(&sent).len(),
        1,
        "the Handheld still thinks it is a member"
    );
    assert_eq!(
        rejects(&deliver(&sent, &mut after, T0 + 1_010)),
        vec!["not-member"]
    );
    Ok(())
}

#[test]
fn a_handheld_only_listens_to_its_monitor() -> R {
    let mut v = viewer(KEY, VIEWER_SK)?;
    v.open(T0);
    // An impostor Monitor key answers `here`.
    let mut impostor = Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: [0x44; 32],
        station_pub: String::new(),
        remembered: vec![v.public_key().to_owned()],
    })?;
    let out = deliver(&impostor.open(T0), &mut v, T0 + 10);
    assert_eq!(rejects(&out), vec!["not-monitor"]);
    Ok(())
}

#[test]
fn stale_future_and_replayed_messages_are_refused() -> R {
    let mut m = station()?;
    m.open(T0);
    let mut v = viewer(KEY, VIEWER_SK)?;
    let join = v.open(T0);
    let wraps = published(&join);
    // Stale: delivered 91 s later.
    assert_eq!(
        rejects(&m.receive(&wraps[0], 2, T0 + 91_000)),
        vec!["stale"]
    );
    // Future: delivered 31 s early (a fresh join written now, received "before" it).
    let mut v2 = viewer(KEY, [0x45; 32])?;
    let early = published(&v2.open(T0 + 31_000));
    assert_eq!(
        rejects(&m.receive(&early[0], 2, T0 - 1_000)),
        vec!["future"]
    );
    // Replay: the same wrap twice from the only relay.
    let mut v3 = viewer(KEY, [0x46; 32])?;
    let w = published(&v3.open(T0));
    assert_eq!(rejects(&m.receive(&w[0], 1, T0 + 10)), [] as [&str; 0]);
    assert_eq!(rejects(&m.receive(&w[0], 1, T0 + 20)), vec!["replay"]);
    // From two relays, the second copy is normal and silently ignored.
    let mut v4 = viewer(KEY, [0x47; 32])?;
    let w = published(&v4.open(T0));
    m.receive(&w[0], 2, T0 + 10);
    assert_eq!(m.receive(&w[0], 2, T0 + 11), [] as [Output; 0]);
    Ok(())
}

#[test]
fn a_restarted_handheld_is_reported_as_leaving_and_rejoining() -> R {
    let mut m = station()?;
    m.open(T0);
    let mut v = viewer(KEY, VIEWER_SK)?;
    deliver(&v.open(T0), &mut m, T0);
    // Same key, new session (the app restarted).
    let mut again = viewer(KEY, VIEWER_SK)?;
    let types: Vec<Value> = emitted(&deliver(&again.open(T0 + 5_000), &mut m, T0 + 5_000))
        .iter()
        .map(|e| e["type"].clone())
        .collect();
    assert_eq!(types, vec![json!("peer-left"), json!("peer-joined")]);
    Ok(())
}

#[test]
fn presence_join_retries_and_a_quiet_member_times_out() -> R {
    let mut m = station()?;
    m.open(T0);
    let mut v = viewer(KEY, VIEWER_SK)?;
    v.open(T0);
    // No Monitor yet: the Handheld asks again after 3 s, not before.
    assert_eq!(published(&v.tick(T0 + 2_000)), [] as [Value; 0]);
    let retry = v.tick(T0 + 3_000);
    assert_eq!(published(&retry).len(), 1);
    // The Monitor hears it; then the Handheld goes silent for 51 s.
    deliver(&retry, &mut m, T0 + 3_100);
    let left = emitted(&m.tick(T0 + 3_100 + 50_001));
    assert_eq!(left[0]["type"], "peer-left");
    assert_eq!(left[0]["id"], v.public_key());
    Ok(())
}

#[test]
fn a_monitor_coming_up_says_here_to_phones_it_knows() -> R {
    let v_pub = event::public_key(&VIEWER_SK)?;
    let mut m = Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: STATION_SK,
        station_pub: String::new(),
        remembered: vec![v_pub.clone(), "not-a-key".into()],
    })?;
    let out = m.open(T0);
    let wraps: Vec<Event> = out
        .iter()
        .filter_map(|o| {
            if let Output::Publish(e) = o {
                Some(e.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(wraps.len(), 1, "one here, to the one valid remembered key");
    assert_eq!(wraps[0].tag_values("p").next(), Some(v_pub.as_str()));
    Ok(())
}

#[test]
fn a_handheld_cannot_be_told_join_and_a_monitor_cannot_be_told_here() -> R {
    let mut m = station()?;
    m.open(T0);
    // A Monitor-role session addressing another Monitor with `here`.
    let mut other = Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: [0x48; 32],
        station_pub: String::new(),
        remembered: vec![m.public_key().to_owned()],
    })?;
    assert_eq!(
        rejects(&deliver(&other.open(T0), &mut m, T0 + 10)),
        vec!["bad-type"]
    );
    Ok(())
}

/// A Monitor's `here` that lands before the Handheld's session is open (a
/// relay hands over one from a moment ago) must not start anything: frames
/// can't go out yet, and the same `here` after `open` would not start it again.
/// The `join` sent at `open` gets the answer that does.
#[test]
fn a_here_before_open_is_ignored_and_the_join_brings_the_monitor() -> R {
    let mut m = station()?;
    let mut v = viewer(KEY, VIEWER_SK)?;
    let _ = m.open(T0);
    let v_pub = event::public_key(&VIEWER_SK)?;

    // The Monitor says `here` to a phone it knows, before that phone is open.
    let mut early = Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: STATION_SK,
        station_pub: String::new(),
        remembered: vec![v_pub],
    })?;
    let here = early.open(T0);
    let got = deliver(&here, &mut v, T0 + 100);
    assert!(got.is_empty(), "nothing handled before open: {got:?}");

    // Open: join, the Monitor answers, and only now is it a peer.
    let join = v.open(T0 + 200);
    let welcome = emitted(&join);
    assert_eq!(welcome.len(), 1, "welcome on open");
    assert_eq!(welcome[0]["peers"], json!([]), "no Monitor yet");
    let answer = deliver(&join, &mut m, T0 + 300);
    let got = emitted(&deliver(&answer, &mut v, T0 + 400));
    assert!(
        got.iter()
            .any(|e| e["type"] == "peer-joined" && e["role"] == "station"),
        "the Monitor joins after open: {got:?}"
    );
    Ok(())
}
