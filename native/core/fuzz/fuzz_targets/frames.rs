//! The frames peers and relays send: the LAN relay (any socket on the Wi-Fi),
//! the room client (a relay, or someone posing as one), a Nostr relay's
//! messages, and the signature check on offers and answers.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use tawny_core::ids::lan_response;
use tawny_core::relay::conn::RelayConn;
use tawny_core::sas::{self, Signed};
use tawny_core::signal::lan::{LanOut, LanRelay};
use tawny_core::signal::room::{Kind, RoomClient};

const KEY: &str = "Zx9_-abcDEF0123456789";

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let lines: Vec<&str> = text.split('\n').collect();

    // The LAN relay: one admitted Monitor, one admitted Viewer, one stranger,
    // and each line a frame from one of them.
    let mut relay = LanRelay::new(KEY);
    let room = tawny_core::ids::room_id(KEY);
    for (conn, role) in [(1u64, "station"), (2, "viewer"), (3, "viewer")] {
        let out = relay.on_connect(conn, &format!("room={room}&role={role}"), "10.0.0.9", 0);
        if conn == 3 {
            continue; // never answers
        }
        for o in out {
            if let LanOut::Send(_, t) = o
                && let Ok(v) = serde_json::from_str::<Value>(&t)
                && let Some(n) = v["n"].as_str()
            {
                let hello = json!({ "type": "hello", "r": lan_response(KEY, n) }).to_string();
                relay.on_text(conn, &hello);
            }
        }
    }
    for (i, line) in lines.iter().enumerate() {
        let conn = (i % 3) as u64 + 1;
        for o in relay.on_text(conn, line) {
            // Nothing reaches anyone else from the socket that never answered.
            if conn == 3 {
                assert!(
                    !matches!(o, LanOut::Send(to, _) if to != 3),
                    "the stranger got through: {o:?}"
                );
            }
        }
    }
    let _ = relay.tick(10_000);
    for c in 1..=3 {
        let _ = relay.on_close(c);
    }

    // The room client, on both sides, on both kinds of relay.
    for (kind, station) in [
        (Kind::Lan, false),
        (Kind::Lan, true),
        (Kind::Rendezvous, false),
    ] {
        let mut c = RoomClient::new(kind, station, KEY, Some("TICKET1234abcd".into()));
        let _ = c.on_open();
        for line in &lines {
            let _ = c.on_text(line);
        }
    }

    // A Nostr relay's messages.
    let mut r = RelayConn::new("wss://relay.example", Some([0x41; 32]));
    let _ = r.on_open();
    for line in &lines {
        let _ = r.on_text(line, 1_795_000_000_000);
    }

    // The signature on offers and answers: never "signed" without the key.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let got = sas::check_frame("not the key", &v);
        if got == Signed::Yes {
            let sdp = v["sdp"]["sdp"].as_str().unwrap_or("");
            assert_eq!(
                sas::sign_sdp("not the key", v["type"].as_str().unwrap_or(""), sdp).as_deref(),
                v["sm"].as_str()
            );
        }
        let _ = sas::check_frame(KEY, &v);
    }
});
