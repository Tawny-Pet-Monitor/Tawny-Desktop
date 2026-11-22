//! NIP-44 against the official vectors (paulmillr/nip44, `nip44.vectors.json`,
//! sha256 269ed0f69e4c192512cc779e78c555090cebc7c785b609e338a62afc3ce25040,
//! the checksum NIP-44 publishes), and NIP-59 against wraps sealed by the JS
//! engine (`fixtures/nip59-from-js.json`).

use serde_json::Value;
use sha2::{Digest, Sha256};
use tawny_core::nostr::event::{self, Template};
use tawny_core::nostr::nip44::{self, ConversationKey, DEFAULT_MAX_PLAINTEXT};
use tawny_core::nostr::nip59::{self, KIND_EPHEMERAL_GIFT_WRAP};

type R = Result<(), Box<dyn std::error::Error>>;

fn vectors() -> Result<Value, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/nip44.vectors.json"
    ))?;
    assert_eq!(
        hex::encode(Sha256::digest(text.as_bytes())),
        "269ed0f69e4c192512cc779e78c555090cebc7c785b609e338a62afc3ce25040",
        "vector file is not the published one"
    );
    Ok(serde_json::from_str::<Value>(&text)?["v2"].clone())
}

fn h32(v: &Value) -> [u8; 32] {
    let s = v.as_str().unwrap_or_default();
    hex::decode(s)
        .ok()
        .and_then(|b| b.try_into().ok())
        .unwrap_or([0; 32])
}

fn list<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    path.iter()
        .fold(v, |v, k| &v[*k])
        .as_array()
        .map_or(&[], Vec::as_slice)
}

#[test]
fn conversation_keys() -> R {
    let v = vectors()?;
    let cases = list(&v, &["valid", "get_conversation_key"]);
    assert_eq!(cases.len(), 35);
    for c in cases {
        let sk = nip44::secret_key(&h32(&c["sec1"]))?;
        let ck = nip44::conversation_key(&sk, &h32(&c["pub2"]))?;
        assert_eq!(
            hex::encode(ck.as_bytes()),
            c["conversation_key"].as_str().unwrap_or_default()
        );
    }
    Ok(())
}

#[test]
fn invalid_conversation_keys_are_refused() -> R {
    let v = vectors()?;
    for c in list(&v, &["invalid", "get_conversation_key"]) {
        let result = nip44::secret_key(&h32(&c["sec1"]))
            .and_then(|sk| nip44::conversation_key(&sk, &h32(&c["pub2"])));
        assert!(result.is_err(), "should refuse: {}", c["note"]);
    }
    Ok(())
}

#[test]
fn message_keys() -> R {
    let v = vectors()?;
    let mk = &v["valid"]["get_message_keys"];
    let ck = ConversationKey::from_bytes(h32(&mk["conversation_key"]));
    for k in list(mk, &["keys"]) {
        let (key, iv, mac) = nip44::message_keys(&ck, &h32(&k["nonce"]));
        assert_eq!(
            hex::encode(key),
            k["chacha_key"].as_str().unwrap_or_default()
        );
        assert_eq!(
            hex::encode(iv),
            k["chacha_nonce"].as_str().unwrap_or_default()
        );
        assert_eq!(hex::encode(mac), k["hmac_key"].as_str().unwrap_or_default());
    }
    Ok(())
}

#[test]
fn padded_lengths() -> R {
    let v = vectors()?;
    for pair in list(&v, &["valid", "calc_padded_len"]) {
        let (len, want) = (pair[0].as_u64().unwrap_or(0), pair[1].as_u64().unwrap_or(0));
        assert_eq!(
            u64::try_from(nip44::padded_len(usize::try_from(len)?))?,
            want,
            "len {len}"
        );
    }
    Ok(())
}

#[test]
fn encrypt_and_decrypt() -> R {
    let v = vectors()?;
    for c in list(&v, &["valid", "encrypt_decrypt"]) {
        let sk1 = nip44::secret_key(&h32(&c["sec1"]))?;
        let pub2 = event::public_key(&h32(&c["sec2"]))?;
        let ck = nip44::conversation_key(&sk1, &h32(&Value::String(pub2)))?;
        assert_eq!(
            hex::encode(ck.as_bytes()),
            c["conversation_key"].as_str().unwrap_or_default()
        );
        let plaintext = c["plaintext"].as_str().unwrap_or_default();
        let payload =
            nip44::encrypt_with_nonce(plaintext, &ck, &h32(&c["nonce"]), DEFAULT_MAX_PLAINTEXT)?;
        assert_eq!(payload, c["payload"].as_str().unwrap_or_default());
        assert_eq!(
            nip44::decrypt(&payload, &ck, DEFAULT_MAX_PLAINTEXT)?,
            plaintext
        );
    }
    Ok(())
}

#[test]
fn long_messages() -> R {
    let v = vectors()?;
    for c in list(&v, &["valid", "encrypt_decrypt_long_msg"]) {
        let ck = ConversationKey::from_bytes(h32(&c["conversation_key"]));
        let repeat: usize = c["repeat"].as_u64().map_or_else(
            || c["repeat"].as_str().unwrap_or("0").parse(),
            |n| Ok(usize::try_from(n).unwrap_or(0)),
        )?;
        let plaintext = c["pattern"].as_str().unwrap_or_default().repeat(repeat);
        assert_eq!(
            hex::encode(Sha256::digest(plaintext.as_bytes())),
            c["plaintext_sha256"].as_str().unwrap_or_default()
        );
        let payload =
            nip44::encrypt_with_nonce(&plaintext, &ck, &h32(&c["nonce"]), nip44::MAX_PLAINTEXT)?;
        assert_eq!(
            hex::encode(Sha256::digest(payload.as_bytes())),
            c["payload_sha256"].as_str().unwrap_or_default()
        );
        assert_eq!(
            nip44::decrypt(&payload, &ck, nip44::MAX_PLAINTEXT)?,
            plaintext
        );
    }
    Ok(())
}

#[test]
fn huge_limits_do_not_overflow() {
    assert_eq!(
        nip44::padded_len(usize::MAX),
        nip44::padded_len(nip44::MAX_PLAINTEXT)
    );
    let ck = ConversationKey::from_bytes([7; 32]);
    assert!(nip44::decrypt("not base64 but long enough", &ck, usize::MAX).is_err());
}

#[test]
fn bad_message_lengths_are_refused() -> R {
    let v = vectors()?;
    let ck = ConversationKey::from_bytes([7; 32]);
    for len in list(&v, &["invalid", "encrypt_msg_lengths"]) {
        let n = usize::try_from(len.as_u64().unwrap_or(0))?;
        if n > 1 << 20 {
            continue; // too large to allocate in a unit test; covered by the max_len check
        }
        assert!(
            nip44::encrypt_with_nonce(&"a".repeat(n), &ck, &[1; 32], DEFAULT_MAX_PLAINTEXT)
                .is_err(),
            "len {n}"
        );
    }
    Ok(())
}

#[test]
fn invalid_payloads_are_refused() -> R {
    let v = vectors()?;
    let cases = list(&v, &["invalid", "decrypt"]);
    assert_eq!(cases.len(), 12);
    for c in cases {
        let ck = ConversationKey::from_bytes(h32(&c["conversation_key"]));
        let r = nip44::decrypt(
            c["payload"].as_str().unwrap_or_default(),
            &ck,
            DEFAULT_MAX_PLAINTEXT,
        );
        assert!(r.is_err(), "should refuse: {}", c["note"]);
    }
    Ok(())
}

// ------------------------------------------------------------- NIP-59

fn fixture() -> Result<Value, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/nip59-from-js.json"
    ))?;
    Ok(serde_json::from_str(&text)?)
}

#[test]
fn wraps_sealed_by_the_js_engine_open_here() -> R {
    let f = fixture()?;
    let me = h32(&f["recipient_secret"]);
    let sender = f["sender_public"].as_str().unwrap_or_default();
    let cases = list(&f, &["cases"]);
    assert_eq!(cases.len(), 6);
    for c in cases {
        let name = c["name"].as_str().unwrap_or_default();
        let expect = c["expect"].as_str().unwrap_or_default();
        match nip59::unwrap(&c["wrap"], &me) {
            Ok((seal, rumor)) => {
                assert_eq!(expect, "ok", "{name}: opened, but JS expects {expect}");
                assert_eq!(seal.pubkey, sender);
                let t = &c["template"];
                assert_eq!(
                    u64::from(rumor.kind),
                    t["kind"].as_u64().unwrap_or(0),
                    "{name}"
                );
                assert_eq!(
                    rumor.content,
                    t["content"].as_str().unwrap_or_default(),
                    "{name}"
                );
                assert_eq!(
                    rumor.created_at,
                    t["created_at"].as_u64().unwrap_or(0),
                    "{name}"
                );
                assert_eq!(
                    rumor.tag_values("t").next(),
                    t["tags"][0][1].as_str(),
                    "{name}"
                );
            }
            Err(e) => assert_eq!(e.code(), expect, "{name}: {e}"),
        }
    }
    Ok(())
}

#[test]
fn a_rust_wrap_round_trips_and_hides_the_sender() -> R {
    let sender = [0x11; 32];
    let recipient = [0x22; 32];
    let rp = event::public_key(&recipient)?;
    let t = Template {
        kind: 25050,
        tags: vec![vec!["t".into(), "offer".into()]],
        content: "v=0\r\n".into(),
        created_at: 1_795_150_000,
    };
    let wrap = nip59::gift_wrap(
        t,
        &sender,
        &rp,
        KIND_EPHEMERAL_GIFT_WRAP,
        1_795_150_000,
        None,
    )?;
    assert_eq!(wrap.kind, KIND_EPHEMERAL_GIFT_WRAP);
    assert_ne!(
        wrap.pubkey,
        event::public_key(&sender)?,
        "the wrap must not be signed by the sender"
    );
    assert!(
        wrap.created_at <= 1_795_150_000
            && wrap.created_at >= 1_795_150_000 - nip59::EPHEMERAL_JITTER_S
    );
    let (seal, rumor) = nip59::unwrap(&serde_json::to_value(&wrap)?, &recipient)?;
    assert_eq!(seal.pubkey, event::public_key(&sender)?);
    assert_eq!(rumor.content, "v=0\r\n");
    assert!(nip59::unwrap(&serde_json::to_value(&wrap)?, &[0x33; 32]).is_err());
    Ok(())
}
