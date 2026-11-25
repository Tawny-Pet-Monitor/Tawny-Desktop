//! NIP-59 wraps from relays. Anyone can make a wrap that opens (the wrap key
//! is theirs, and so can the seal's be), so the seal and the rumor inside are
//! as untrusted as the wrap: this reaches each layer with fuzzed text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use tawny_core::nostr::event::{self, Template};
use tawny_core::nostr::{nip44, nip59};

const ME: [u8; 32] = [0x31; 32];
const SENDER: [u8; 32] = [0x32; 32];
const ONE_TIME: [u8; 32] = [0x33; 32];

fn x(pub_hex: &str) -> Option<[u8; 32]> {
    let v: Vec<u8> = (0..pub_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(pub_hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    v.try_into().ok()
}

fn sealed(plain: &str, from: &[u8; 32], to_pub: &str) -> Option<String> {
    let ck = nip44::conversation_key(&nip44::secret_key(from).ok()?, &x(to_pub)?).ok()?;
    nip44::encrypt_with_nonce(plain, &ck, &[9; 32], 128 * 1024).ok()
}

fn wrap_around(seal_json: &str, me_pub: &str) -> Option<Value> {
    let w = event::sign(
        Template {
            kind: nip59::KIND_EPHEMERAL_GIFT_WRAP,
            tags: vec![vec!["p".into(), me_pub.into()]],
            content: sealed(seal_json, &ONE_TIME, me_pub)?,
            created_at: 1_795_000_000,
        },
        &ONE_TIME,
    )
    .ok()?;
    serde_json::to_value(w).ok()
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let Ok(me_pub) = event::public_key(&ME) else {
        return;
    };
    let wrap = match mode % 3 {
        // The wrap itself, as JSON.
        0 => match serde_json::from_str::<Value>(text) {
            Ok(v) => Some(v),
            Err(_) => return,
        },
        // A wrap that opens, around fuzzed seal text.
        1 if !text.is_empty() => wrap_around(text, &me_pub),
        // A wrap and a signed seal that open, around fuzzed rumor text.
        _ if !text.is_empty() => (|| {
            let seal = event::sign(
                Template {
                    kind: nip59::KIND_SEAL,
                    tags: vec![],
                    content: sealed(text, &SENDER, &me_pub)?,
                    created_at: 1_795_000_000,
                },
                &SENDER,
            )
            .ok()?;
            wrap_around(&serde_json::to_string(&seal).ok()?, &me_pub)
        })(),
        _ => None,
    };
    if let Some(w) = wrap
        && let Ok((seal, rumor)) = nip59::unwrap(&w, &ME)
    {
        // What comes out is never someone it isn't.
        assert_eq!(rumor.pubkey, seal.pubkey);
    }
});
