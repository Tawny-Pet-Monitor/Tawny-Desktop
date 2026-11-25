//! A Monitor's Nostr session, fed messages that open: from a Handheld that
//! joined properly and from one that never did, of every type, with fuzzed
//! bodies. Only the joined one's frames may reach the call layer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use tawny_core::nostr::event::{self, Template};
use tawny_core::nostr::nip59;
use tawny_core::signal::nostr::{Config, KIND_SIGNAL, Output, Role, Session, join_proof};

const KEY: &str = "Zx9_-abcDEF0123456789";
const STATION: [u8; 32] = [0x51; 32];
const PHONE: [u8; 32] = [0x52; 32];
const STRANGER: [u8; 32] = [0x53; 32];
const NOW_S: u64 = 1_795_000_000;

fn wrap(from: &[u8; 32], to: &str, ty: &str, content: String, at: u64) -> Option<Value> {
    let w = nip59::gift_wrap(
        Template {
            kind: KIND_SIGNAL,
            tags: vec![vec!["p".into(), to.into()], vec!["t".into(), ty.into()]],
            content,
            created_at: at,
        },
        from,
        to,
        nip59::KIND_EPHEMERAL_GIFT_WRAP,
        at,
        None,
    )
    .ok()?;
    serde_json::to_value(w).ok()
}

fuzz_target!(|data: &[u8]| {
    let (Ok(station_pub), Ok(phone_pub), Ok(stranger_pub)) = (
        event::public_key(&STATION),
        event::public_key(&PHONE),
        event::public_key(&STRANGER),
    ) else {
        return;
    };
    let Ok(mut s) = Session::new(Config {
        channel_key: KEY.into(),
        role: Role::Station,
        secret: STATION,
        station_pub: String::new(),
        remembered: vec![],
    }) else {
        return;
    };
    let now_ms = NOW_S * 1000;
    let _ = s.open(now_ms);
    // The phone joins properly.
    let proof = join_proof(KEY, &phone_pub, &station_pub, NOW_S);
    if let Some(w) = wrap(
        &PHONE,
        &station_pub,
        "join",
        json!({ "proof": proof, "s": "x1" }).to_string(),
        NOW_S,
    ) {
        let _ = s.receive(&w, 1, now_ms);
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for (i, chunk) in text.split('\u{1}').take(16).enumerate() {
        let (from, from_pub) = if i % 2 == 0 {
            (&PHONE, &phone_pub)
        } else {
            (&STRANGER, &stranger_pub)
        };
        let ty = ["join", "here", "leave", "sig", "ping", ""][chunk.len() % 6];
        let at = NOW_S + (i as u64 % 3);
        let Some(w) = wrap(from, &station_pub, ty, chunk.to_owned(), at) else {
            continue;
        };
        for o in s.receive(&w, 1, now_ms + i as u64) {
            // The stranger never joined, so nothing of theirs reaches the call layer.
            if let Output::Emit(f) = o
                && from == &STRANGER
            {
                assert_ne!(f["from"].as_str(), Some(from_pub.as_str()), "{f}");
                assert_ne!(f["id"].as_str(), Some(from_pub.as_str()), "{f}");
            }
        }
    }
    let _ = s.tick(now_ms + 60_000);
});
