//! The LAN relay and the room client, wired together in memory: admission by
//! challenge, the room events, forwarding, and every refusal `LocalWeb.kt` makes.

use serde_json::{Value, json};
use tawny_core::ids::room_id;
use tawny_core::signal::lan::{ConnId, LanOut, LanRelay, MAX_MSG};
use tawny_core::signal::room::{Kind, Refusal, RoomClient, RoomOut};

const KEY: &str = "Zx9_-abcDEF0123456789";
const T0: u64 = 1_000_000;

fn query(station: bool) -> String {
    format!(
        "room={}&role={}",
        room_id(KEY),
        if station { "station" } else { "viewer" }
    )
}

fn sent_to(out: &[LanOut], conn: ConnId) -> Vec<Value> {
    out.iter()
        .filter_map(|o| match o {
            LanOut::Send(c, t) if *c == conn => serde_json::from_str(t).ok(),
            _ => None,
        })
        .collect()
}

fn closed(out: &[LanOut]) -> Vec<(ConnId, u16)> {
    out.iter()
        .filter_map(|o| {
            if let LanOut::Close(c, code, _) = o {
                Some((*c, *code))
            } else {
                None
            }
        })
        .collect()
}

fn client_frames(out: &[RoomOut]) -> Vec<String> {
    out.iter()
        .filter_map(|o| {
            if let RoomOut::Send(s) = o {
                Some(s.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Connects `conn` and runs the challenge with `client`; returns the relay's output.
fn join(
    relay: &mut LanRelay,
    client: &mut RoomClient,
    conn: ConnId,
    station: bool,
    ip: &str,
) -> Vec<LanOut> {
    let out = relay.on_connect(conn, &query(station), ip, T0);
    assert!(
        client_frames(&client.on_open()).is_empty(),
        "nothing secret before the challenge"
    );
    let challenge = sent_to(&out, conn);
    let answer = client_frames(&client.on_text(&challenge[0].to_string()));
    relay.on_text(conn, &answer[0])
}

#[test]
fn a_monitor_and_a_viewer_meet_and_frames_are_forwarded() {
    let mut relay = LanRelay::new(KEY);
    let mut station = RoomClient::new(Kind::Lan, true, KEY, Some("TICKETxx".into()));
    let mut viewer = RoomClient::new(Kind::Lan, false, KEY, Some("TICKETxx".into()));

    let out = join(&mut relay, &mut station, 1, true, "192.168.1.10");
    let welcome = &sent_to(&out, 1)[0];
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["role"], "station");
    station.on_text(&welcome.to_string());
    assert!(station.admitted());

    let out = join(&mut relay, &mut viewer, 2, false, "192.168.1.20");
    let w = &sent_to(&out, 2)[0];
    assert_eq!(w["type"], "welcome");
    assert_eq!(w["peers"][0]["role"], "station");
    let station_id = w["peers"][0]["id"].as_str().unwrap_or_default().to_owned();
    let joined = &sent_to(&out, 1)[0];
    assert_eq!(joined["type"], "peer-joined");
    assert_eq!(joined["role"], "viewer");
    let viewer_id = joined["id"].as_str().unwrap_or_default().to_owned();
    viewer.on_text(&w.to_string());

    // Viewer → Monitor: forwarded with `from`.
    let offer =
        client_frames(&viewer.send(&json!({ "type": "offer", "to": station_id, "sdp": {} })));
    let fwd = sent_to(&relay.on_text(2, &offer[0]), 1);
    assert_eq!(fwd[0]["type"], "offer");
    assert_eq!(fwd[0]["from"], viewer_id.as_str());

    // Not relayed: unknown types, no `to`, to ourselves.
    assert_eq!(
        relay.on_text(2, &json!({ "type": "evil", "to": station_id }).to_string()),
        vec![]
    );
    assert_eq!(
        relay.on_text(2, &json!({ "type": "offer" }).to_string()),
        vec![]
    );
    assert_eq!(
        relay.on_text(2, &json!({ "type": "offer", "to": viewer_id }).to_string()),
        vec![]
    );

    // The Viewer leaves: the Monitor is told.
    let left = sent_to(&relay.on_close(2), 1);
    assert_eq!(left[0], json!({ "type": "peer-left", "id": viewer_id }));
}

#[test]
fn a_wrong_key_is_refused_and_announced_to_no_one() {
    let mut relay = LanRelay::new(KEY);
    let mut station = RoomClient::new(Kind::Lan, true, KEY, None);
    join(&mut relay, &mut station, 1, true, "192.168.1.10");
    let mut intruder = RoomClient::new(Kind::Lan, false, "a-different-channel-key", None);
    let out = join(&mut relay, &mut intruder, 2, false, "192.168.1.66");
    assert_eq!(closed(&out), vec![(2, 4008)]);
    assert!(sent_to(&out, 1).is_empty(), "the Monitor never hears of it");
    assert_eq!(relay.members(), 1);
}

#[test]
fn a_captured_answer_is_useless_on_the_next_connection() {
    let mut relay = LanRelay::new(KEY);
    let mut v = RoomClient::new(Kind::Lan, false, KEY, None);
    let out = relay.on_connect(1, &query(false), "192.168.1.20", T0);
    let answer = client_frames(&v.on_text(&sent_to(&out, 1)[0].to_string()));
    relay.on_text(1, &answer[0]);
    // A sniffer replays the same answer on a fresh socket (fresh nonce).
    relay.on_connect(2, &query(false), "192.168.1.66", T0);
    assert_eq!(closed(&relay.on_text(2, &answer[0])), vec![(2, 4008)]);
}

#[test]
fn only_the_answer_is_accepted_before_admission_and_silence_times_out() {
    let mut relay = LanRelay::new(KEY);
    relay.on_connect(1, &query(false), "192.168.1.20", T0);
    // Frames before the answer are ignored, not forwarded.
    assert_eq!(
        relay.on_text(1, &json!({ "type": "offer", "to": "x" }).to_string()),
        vec![]
    );
    assert_eq!(relay.tick(T0 + 5_000), vec![]);
    assert_eq!(closed(&relay.tick(T0 + 5_001)), vec![(1, 4008)]);
}

#[test]
fn bad_rooms_oversized_frames_and_a_second_monitor_are_refused() {
    let mut relay = LanRelay::new(KEY);
    assert_eq!(
        closed(&relay.on_connect(1, "role=viewer", "192.168.1.20", T0)),
        vec![(1, 4000)]
    );
    assert_eq!(
        closed(&relay.on_connect(2, "room=NOTHEX&role=viewer", "192.168.1.20", T0)),
        vec![(2, 4000)]
    );
    assert_eq!(
        closed(&relay.on_text(3, &"x".repeat(MAX_MSG + 1))),
        vec![(3, 4009)]
    );
    let mut a = RoomClient::new(Kind::Lan, true, KEY, None);
    let mut b = RoomClient::new(Kind::Lan, true, KEY, None);
    join(&mut relay, &mut a, 10, true, "192.168.1.10");
    assert_eq!(
        closed(&join(&mut relay, &mut b, 11, true, "192.168.1.11")),
        vec![(11, 4004)]
    );
}

#[test]
fn the_room_holds_one_monitor_and_three_viewers() {
    let mut relay = LanRelay::new(KEY);
    let mut m = RoomClient::new(Kind::Lan, true, KEY, None);
    join(&mut relay, &mut m, 1, true, "192.168.1.10");
    for i in 0..3 {
        let mut v = RoomClient::new(Kind::Lan, false, KEY, None);
        assert_eq!(
            closed(&join(
                &mut relay,
                &mut v,
                2 + i,
                false,
                &format!("192.168.1.2{i}")
            )),
            [] as [(ConnId, u16); 0]
        );
    }
    let mut fifth = RoomClient::new(Kind::Lan, false, KEY, None);
    assert_eq!(
        closed(&join(&mut relay, &mut fifth, 9, false, "192.168.1.29")),
        vec![(9, 4003)]
    );
}

#[test]
fn one_device_cannot_hold_more_than_eight_sockets_even_unanswered() {
    let mut relay = LanRelay::new(KEY);
    for c in 0..8 {
        assert_eq!(
            closed(&relay.on_connect(c, &query(false), "192.168.1.66", T0)),
            [] as [(ConnId, u16); 0]
        );
    }
    assert_eq!(
        closed(&relay.on_connect(8, &query(false), "192.168.1.66", T0)),
        vec![(8, 4006)]
    );
    assert_eq!(
        closed(&relay.on_connect(9, &query(false), "192.168.1.67", T0)),
        [] as [(ConnId, u16); 0],
        "other devices are unaffected"
    );
}

#[test]
fn the_rendezvous_hello_carries_the_ticket_and_the_monitor_adds_its_proofs() {
    let viewer = RoomClient::new(Kind::Rendezvous, false, KEY, Some("TICKET123".into()));
    let hello: Value =
        serde_json::from_str(&client_frames(&viewer.on_open())[0]).unwrap_or_default();
    assert_eq!(hello, json!({ "type": "hello", "t": "TICKET123" }));
    let station = RoomClient::new(Kind::Rendezvous, true, KEY, Some("TICKET123".into()));
    let hello: Value =
        serde_json::from_str(&client_frames(&station.on_open())[0]).unwrap_or_default();
    assert_eq!(hello["t"], "TICKET123");
    assert_eq!(hello["hashT"].as_str().map(str::len), Some(64));
    assert_eq!(hello["a"].as_str().map(str::len), Some(64));
    // A rendezvous never challenges; a stray challenge is not answered.
    let mut v = RoomClient::new(Kind::Rendezvous, false, KEY, None);
    assert_eq!(
        v.on_text(&json!({ "type": "challenge", "n": "ab".repeat(16) }).to_string()),
        [] as [RoomOut; 0]
    );
    assert_eq!(
        v.url("wss://relay.example/"),
        format!("wss://relay.example/ws?room={}&role=viewer", room_id(KEY))
    );
}

#[test]
fn close_codes_read_like_app_js() {
    assert_eq!(Refusal::from_code(4008), Refusal::Admission);
    assert!(!Refusal::Admission.retryable());
    assert!(Refusal::from_code(4010).retryable());
    assert!(Refusal::from_code(1006).retryable());
    assert!(!Refusal::from_code(4004).retryable());
}

#[test]
fn only_the_monitor_can_turn_a_viewer_away() {
    let mut relay = LanRelay::new(KEY);
    let mut station = RoomClient::new(Kind::Lan, true, KEY, None);
    let mut viewer = RoomClient::new(Kind::Lan, false, KEY, None);
    let mut other = RoomClient::new(Kind::Lan, false, KEY, None);
    let out = join(&mut relay, &mut station, 1, true, "192.168.1.10");
    let station_id = sent_to(&out, 1)[0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let out = join(&mut relay, &mut viewer, 2, false, "192.168.1.20");
    let viewer_id = sent_to(&out, 1)[0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    join(&mut relay, &mut other, 3, false, "192.168.1.30");

    let kick = |to: &str| json!({ "type": "kick", "to": to }).to_string();
    assert_eq!(
        relay.on_text(3, &kick(&viewer_id)),
        vec![],
        "a viewer can't"
    );
    assert_eq!(
        relay.on_text(2, &kick(&station_id)),
        vec![],
        "nor the other way"
    );
    assert_eq!(relay.on_text(1, &kick(&station_id)), vec![], "not itself");
    assert_eq!(relay.on_text(1, &kick("nobody")), vec![]);
    assert_eq!(
        closed(&relay.on_text(1, &kick(&viewer_id))),
        vec![(2, tawny_core::signal::lan::CLOSE_UNSIGNED)]
    );
}

/// An older phone starts counting failures afresh whenever its socket opens,
/// so a device turned away is refused at the handshake for a while: only then
/// does it move on from the LAN to a way where its call gets a code.
#[test]
fn a_turned_away_device_is_refused_at_the_handshake_for_a_while() {
    use tawny_core::signal::lan::UNSIGNED_HOLD_MS;
    let mut relay = LanRelay::new(KEY);
    let mut station = RoomClient::new(Kind::Lan, true, KEY, None);
    let mut viewer = RoomClient::new(Kind::Lan, false, KEY, None);
    join(&mut relay, &mut station, 1, true, "192.168.1.10");
    let out = join(&mut relay, &mut viewer, 2, false, "192.168.1.20");
    let viewer_id = sent_to(&out, 1)[0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !relay.turns_away("192.168.1.20", T0),
        "not before it is turned away"
    );

    let kick = json!({ "type": "kick", "to": viewer_id }).to_string();
    assert_eq!(
        closed(&relay.on_text(1, &kick)),
        vec![(2, tawny_core::signal::lan::CLOSE_UNSIGNED)]
    );
    relay.on_close(2);

    let back = T0 + 1_000;
    assert!(
        relay.turns_away("192.168.1.20", back),
        "its next try is refused"
    );
    assert!(
        relay.turns_away("192.168.1.20", back + UNSIGNED_HOLD_MS - 1),
        "for the whole hold"
    );
    assert!(
        !relay.turns_away("192.168.1.30", back),
        "another device is not"
    );
    assert!(
        !relay.turns_away("192.168.1.20", back + UNSIGNED_HOLD_MS),
        "and then it may try again"
    );
    assert!(!relay.turns_away("192.168.1.20", back + UNSIGNED_HOLD_MS + 1));
}
