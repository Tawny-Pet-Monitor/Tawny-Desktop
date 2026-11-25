//! A relay connection driven by scripted relay frames, and the health book.

use serde_json::{Value, json};
use tawny_core::nostr::event::{self, Template};
use tawny_core::relay::conn::{ConnState, Filter, MAX_FRAME, RelayConn, RelayOut};
use tawny_core::relay::health::{HealthBook, Verdict};

const KEY: [u8; 32] = [0x51; 32];
const T0: u64 = 1_795_150_000_000;
type R = Result<(), Box<dyn std::error::Error>>;

fn sent(out: &[RelayOut]) -> Vec<Value> {
    out.iter()
        .filter_map(|o| {
            if let RelayOut::Send(s) = o {
                serde_json::from_str(s).ok()
            } else {
                None
            }
        })
        .collect()
}

fn me() -> String {
    event::public_key(&KEY).unwrap_or_default()
}

fn filter() -> Filter {
    Filter {
        kinds: Some(vec![21_059]),
        tags: vec![('p', vec![me()])],
        ..Filter::default()
    }
}

fn wrap_for(p: &str, kind: u16) -> Result<event::Event, Box<dyn std::error::Error>> {
    Ok(event::sign(
        Template {
            kind,
            tags: vec![vec!["p".into(), p.into()]],
            content: "x".into(),
            created_at: 1_795_150_000,
        },
        &[0x52; 32],
    )?)
}

#[test]
fn subscriptions_are_sent_on_open_and_resent_on_reconnect() {
    let mut r = RelayConn::new("wss://relay.example", Some(KEY));
    let mut out = Vec::new();
    let id = r.subscribe(vec![filter()], &mut out);
    assert_eq!(out, Vec::new(), "nothing goes out before the socket opens");
    let req = sent(&r.on_open());
    assert_eq!(req[0][0], "REQ");
    assert_eq!(req[0][1], id.as_str());
    assert_eq!(req[0][2]["kinds"], json!([21_059]));
    assert_eq!(req[0][2]["#p"], json!([me()]));
    r.on_close();
    assert_eq!(sent(&r.on_open())[0][1], id.as_str());
}

#[test]
fn verified_matching_events_are_delivered_and_others_dropped() -> R {
    let mut r = RelayConn::new("wss://relay.example", None);
    r.on_open();
    let mut out = Vec::new();
    let id = r.subscribe(vec![filter()], &mut out);
    let good = serde_json::to_value(wrap_for(&me(), 21_059)?)?;
    let got = r.on_text(&json!(["EVENT", id, good]).to_string(), T0);
    assert!(matches!(&got[0], RelayOut::Event { raw, .. } if raw == &good));

    // A signature that does not verify.
    let mut forged = good.clone();
    forged["content"] = json!("y");
    assert!(
        matches!(&r.on_text(&json!(["EVENT", id, forged]).to_string(), T0)[0], RelayOut::Log(m) if m.contains("invalid"))
    );
    // Valid, but not what we asked for (someone else's p tag; wrong kind).
    let other = serde_json::to_value(wrap_for(&"cd".repeat(32), 21_059)?)?;
    assert!(
        matches!(&r.on_text(&json!(["EVENT", id, other]).to_string(), T0)[0], RelayOut::Log(m) if m.contains("did not ask"))
    );
    let wrong_kind = serde_json::to_value(wrap_for(&me(), 1)?)?;
    assert!(matches!(
        &r.on_text(&json!(["EVENT", id, wrong_kind]).to_string(), T0)[0],
        RelayOut::Log(_)
    ));
    // An unknown subscription, junk, and an oversized frame.
    assert_eq!(
        r.on_text(&json!(["EVENT", "nope", good]).to_string(), T0),
        Vec::new()
    );
    assert!(matches!(&r.on_text("not json", T0)[0], RelayOut::Log(_)));
    assert!(
        matches!(&r.on_text(&"x".repeat(MAX_FRAME + 1), T0)[0], RelayOut::Log(m) if m.contains("oversized"))
    );
    Ok(())
}

#[test]
fn auth_challenge_is_answered_with_a_signed_22242() -> R {
    let mut r = RelayConn::new("wss://relay.example", Some(KEY));
    r.on_open();
    let out = r.on_text(&json!(["AUTH", "challenge-123"]).to_string(), T0);
    assert!(out.contains(&RelayOut::State(ConnState::Authenticating)));
    let auth = sent(&out);
    assert_eq!(auth[0][0], "AUTH");
    let ev = event::from_json(&auth[0][1], true)?;
    ev.verify()?;
    assert_eq!(ev.kind, 22_242);
    assert_eq!(ev.pubkey, me());
    assert_eq!(ev.tag_values("relay").next(), Some("wss://relay.example"));
    assert_eq!(ev.tag_values("challenge").next(), Some("challenge-123"));
    let ok = r.on_text(&json!(["OK", ev.id, true, ""]).to_string(), T0 + 10);
    assert!(ok.contains(&RelayOut::Authed(me())));
    assert_eq!(r.state(), ConnState::Authenticated);
    Ok(())
}

/// Opens a connection and answers the relay's challenge; returns the AUTH event id.
fn challenged(r: &mut RelayConn, challenge: &str) -> String {
    r.on_open();
    let out = r.on_text(&json!(["AUTH", challenge]).to_string(), T0);
    sent(&out)
        .first()
        .and_then(|f| f[1]["id"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn only_one_auth_is_in_flight_at_a_time() {
    let mut r = RelayConn::new("wss://relay.example", Some(KEY));
    challenged(&mut r, "c1");
    let second = r.on_text(&json!(["AUTH", "c1"]).to_string(), T0 + 1);
    assert_eq!(sent(&second), Vec::<Value>::new());
}

#[test]
fn an_event_refused_for_want_of_auth_is_sent_again_once_after_signing_in() -> R {
    let mut r = RelayConn::new("wss://relay.example", Some(KEY));
    let auth_id = challenged(&mut r, "c2");
    let ev = wrap_for(&me(), 21_059)?;
    assert_eq!(sent(&r.publish(&ev, T0))[0][0], "EVENT");
    let refused = r.on_text(
        &json!(["OK", ev.id, false, "auth-required: sign in"]).to_string(),
        T0 + 5,
    );
    assert!(
        !refused
            .iter()
            .any(|o| matches!(o, RelayOut::Published { .. })),
        "held back, not reported"
    );
    // AUTH accepted: the event goes out again, once.
    let resent = sent(&r.on_text(&json!(["OK", auth_id, true, ""]).to_string(), T0 + 10));
    assert_eq!(resent.len(), 1);
    assert_eq!(resent[0][1]["id"], ev.id.as_str());
    // Refused again: reported, not retried.
    let again = r.on_text(
        &json!(["OK", ev.id, false, "auth-required: still no"]).to_string(),
        T0 + 20,
    );
    assert_eq!(
        again,
        vec![RelayOut::Published {
            id: ev.id.clone(),
            ok: false,
            message: "auth-required: still no".into()
        }]
    );
    Ok(())
}

#[test]
fn a_subscription_closed_for_auth_is_resent_after_signing_in() {
    let mut r = RelayConn::new("wss://relay.example", Some(KEY));
    let mut out = Vec::new();
    let id = r.subscribe(vec![filter()], &mut out);
    let auth_id = challenged(&mut r, "c3");
    let closed = r.on_text(
        &json!(["CLOSED", id, "auth-required: who are you"]).to_string(),
        T0 + 1,
    );
    assert!(
        !closed
            .iter()
            .any(|o| matches!(o, RelayOut::SubClosed { .. })),
        "kept, waiting for AUTH"
    );
    let after = sent(&r.on_text(&json!(["OK", auth_id, true, ""]).to_string(), T0 + 2));
    assert_eq!(after[0][0], "REQ");
    assert_eq!(after[0][1], id.as_str());
    // Any other CLOSED ends the subscription.
    let gone = r.on_text(&json!(["CLOSED", id, "blocked: no"]).to_string(), T0 + 3);
    assert_eq!(
        gone,
        vec![RelayOut::SubClosed {
            sub: id,
            why: "blocked: no".into()
        }]
    );
}

#[test]
fn publishing_times_out_and_fails_fast_when_closed() -> R {
    let mut r = RelayConn::new("wss://relay.example", None);
    let ev = wrap_for(&me(), 21_059)?;
    assert_eq!(
        r.publish(&ev, T0),
        vec![RelayOut::Published {
            id: ev.id.clone(),
            ok: false,
            message: "not connected".into()
        }]
    );
    r.on_open();
    r.publish(&ev, T0);
    assert_eq!(r.tick(T0 + 7_999), Vec::new());
    assert_eq!(
        r.tick(T0 + 8_000),
        vec![RelayOut::Published {
            id: ev.id.clone(),
            ok: false,
            message: "timeout".into()
        }]
    );
    // Accepted is reported as such.
    r.publish(&ev, T0);
    assert_eq!(
        r.on_text(&json!(["OK", ev.id, true, ""]).to_string(), T0 + 1),
        vec![RelayOut::Published {
            id: ev.id.clone(),
            ok: true,
            message: String::new()
        }]
    );
    Ok(())
}

#[test]
fn the_health_book_skips_refusers_for_a_while_but_never_all_of_them() {
    let urls: Vec<String> = ["wss://a", "wss://b", "wss://c"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let mut book = HealthBook::default();
    book.record("wss://a", Verdict::Refused, "blocked: paid relay", None, T0);
    book.record("wss://b", Verdict::Unreachable, "cannot connect", None, T0);
    book.record("wss://c", Verdict::Ok, "", Some(120), T0);
    let (use_, skipped) = book.pick(&urls, T0 + 1_000);
    assert_eq!(
        use_,
        vec!["wss://b".to_owned(), "wss://c".to_owned()],
        "a timeout is not held against a relay"
    );
    assert_eq!(
        skipped,
        vec![("wss://a".to_owned(), "blocked: paid relay".to_owned())]
    );
    // Six hours later it gets another chance.
    let (use_, _) = book.pick(&urls, T0 + 6 * 3600 * 1000);
    assert_eq!(use_.len(), 3);
    // If every relay refused, all are tried anyway.
    let mut bad = HealthBook::default();
    for u in &urls {
        bad.record(u, Verdict::NoEcho, "", None, T0);
    }
    assert_eq!(bad.pick(&urls, T0 + 1).0, urls);
    // It survives a round trip through storage.
    let text = serde_json::to_string(&book).unwrap_or_default();
    assert_eq!(serde_json::from_str::<HealthBook>(&text).ok(), Some(book));
}
