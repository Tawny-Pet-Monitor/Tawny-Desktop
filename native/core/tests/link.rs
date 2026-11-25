//! The Android shell's `PairLinkTest.kt`, case for case, plus link building.

use tawny_core::link::{
    self, Anchors, CLOCK_SKEW_MS, LinkError, LinkParts, Pairing, build_app_link, build_web_link,
    relay_adoptable,
};

const KEY: &str = "GpgOcJxe4H6eAhLZpgZ72w";
const WEB_LINK: &str = "https://tawny.ainu-anoles.ts.net/#k=GpgOcJxe4H6eAhLZpgZ72w\
    &n=Pet+camera&r=viewer&t=NvM7UXwo8X79vagb9VARxQ&c=WXXHeH07nDk&e=1789060818";

fn parse(raw: &str) -> Result<Pairing, LinkError> {
    link::parse(raw, Anchors::default())
}

fn ok(raw: &str) -> Pairing {
    match parse(raw) {
        Ok(p) => p,
        Err(e) => panic!("{raw} should parse: {e}"),
    }
}

#[test]
fn web_monitor_link_parses_exactly_as_reported() {
    let p = ok(WEB_LINK);
    assert_eq!(p.signal, None);
    assert_eq!(p.key, KEY);
    assert_eq!(p.name, "Pet camera");
    assert_eq!(p.token.as_deref(), Some("NvM7UXwo8X79vagb9VARxQ"));
    assert_eq!(p.code.as_deref(), Some("WXXHeH07nDk"));
    assert_eq!(p.expires_at, 1_789_060_818_000);
    assert_eq!(p.relay.as_deref(), Some("wss://tawny.ainu-anoles.ts.net"));
    assert!(p.relay_trusted);
}

#[test]
fn a_query_string_on_an_https_link_is_never_read() {
    assert!(
        parse("https://tawny.ainu-anoles.ts.net/?k=GpgOcJxe4H6eAhLZpgZ72w&e=1789060818").is_err()
    );
}

#[test]
fn tawny_pair_link_is_unchanged() {
    let p = ok("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.50:8820\
        &t=NvM7UXwo8X79vagb9VARxQ&c=WXXHeH07nDk&e=1789060818");
    assert_eq!(p.signal.as_deref(), Some("ws://192.168.1.50:8820"));
    assert_eq!(p.key, KEY);
    assert_eq!(p.name, "Pet camera");
    assert_eq!(p.token.as_deref(), Some("NvM7UXwo8X79vagb9VARxQ"));
    assert_eq!(p.code.as_deref(), Some("WXXHeH07nDk"));
    assert_eq!(p.expires_at, 1_789_060_818_000);
    assert_eq!(p.relay, None);
    assert!(!p.relay_trusted);
}

#[test]
fn control_and_bidi_override_characters_are_dropped_from_the_name() {
    let p = ok("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.20:8080\
        &n=Re%0Dx%0A%E2%80%AEtoR%E2%80%A8%00!");
    assert_eq!(p.name, "RextoR!");
    let p = ok("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.20:8080&n=%0D%E2%80%AE");
    assert_eq!(p.name, "Pet camera");
}

#[test]
fn a_public_ip_in_h_is_dropped_not_dialled() {
    assert_eq!(
        parse("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=8.8.8.8:8820"),
        Err(LinkError::NowhereToMeet)
    );
    let anchors = Anchors {
        has_rendezvous: true,
        ..Anchors::default()
    };
    let p = link::parse(
        "tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=8.8.8.8:8820",
        anchors,
    );
    assert_eq!(p.map(|p| p.signal), Ok(None));
}

#[test]
fn every_private_range_is_accepted() {
    for h in [
        "10.0.0.4:8820",
        "172.16.5.9:8820",
        "192.168.1.50:8820",
        "169.254.3.3:8820",
    ] {
        let p = ok(&format!("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h={h}"));
        assert_eq!(p.signal, Some(format!("ws://{h}")));
    }
}

#[test]
fn a_malformed_key_is_refused() {
    assert_eq!(
        parse("tawny://pair?k=short&h=192.168.1.50:8820"),
        Err(LinkError::BadKey)
    );
    assert_eq!(
        parse("tawny://pair?k=has+a+space+in+it+here&h=192.168.1.50:8820"),
        Err(LinkError::BadKey)
    );
    assert_eq!(
        parse("tawny://pair?h=192.168.1.50:8820"),
        Err(LinkError::BadKey)
    );
    assert_eq!(
        parse("https://tawny.ainu-anoles.ts.net/#k=nope"),
        Err(LinkError::BadKey)
    );
}

#[test]
fn a_malformed_token_is_refused_outright() {
    assert_eq!(
        parse("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.50:8820&t=no"),
        Err(LinkError::BadTicket)
    );
}

#[test]
fn an_expiry_in_the_past_reads_as_expired() {
    let now = 1_800_000_000_000;
    let past = Pairing {
        expires_at: 1000,
        ..ok(WEB_LINK)
    };
    assert!(past.is_expired(now));
    let future = Pairing {
        expires_at: now + 600_000,
        ..ok(WEB_LINK)
    };
    assert!(!future.is_expired(now));
    let none = ok("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.50:8820");
    assert_eq!(none.expires_at, 0);
    assert!(!none.is_expired(now));
}

#[test]
fn a_viewer_clock_hours_off_does_not_call_a_fresh_code_expired() {
    let now = 1_800_000_000_000;
    let skewed = Pairing {
        expires_at: now - 2 * 60 * 60 * 1000 + 600_000,
        ..ok(WEB_LINK)
    };
    assert!(!skewed.is_expired(now));
    let stale = Pairing {
        expires_at: now - CLOCK_SKEW_MS - 60_000,
        ..ok(WEB_LINK)
    };
    assert!(stale.is_expired(now));
}

#[test]
fn nonsense_is_refused() {
    for raw in [
        "",
        "not a link at all",
        "tawny://notpair?k=GpgOcJxe4H6eAhLZpgZ72w",
        "ftp://example.com/#k=GpgOcJxe4H6eAhLZpgZ72w",
        "https://tawny.ainu-anoles.ts.net/",
        "https://tawny.ainu-anoles.ts.net/#",
    ] {
        assert!(parse(raw).is_err(), "{raw:?} should be refused");
    }
}

#[test]
fn a_link_from_a_strangers_origin_never_gets_its_host_adopted() {
    let p = ok("https://evil.example/#k=GpgOcJxe4H6eAhLZpgZ72w&e=1789060818");
    assert_eq!(p.key, KEY);
    assert_eq!(p.relay.as_deref(), Some("wss://evil.example"));
    assert!(!p.relay_trusted);
}

#[test]
fn rv_pointing_at_a_third_host_is_ignored_not_adopted() {
    let p = ok(
        "https://tawny.ainu-anoles.ts.net/#k=GpgOcJxe4H6eAhLZpgZ72w&rv=wss%3A%2F%2Fevil.example",
    );
    assert_eq!(p.relay.as_deref(), Some("wss://tawny.ainu-anoles.ts.net"));
    assert!(p.relay_trusted);
}

#[test]
fn rv_naming_the_links_own_host_is_honoured() {
    let p = ok("https://tawny.ainu-anoles.ts.net/#k=GpgOcJxe4H6eAhLZpgZ72w\
        &rv=wss%3A%2F%2Ftawny.ainu-anoles.ts.net%3A8443");
    assert_eq!(
        p.relay.as_deref(),
        Some("wss://tawny.ainu-anoles.ts.net:8443")
    );
    assert!(p.relay_trusted);
}

#[test]
fn a_userinfo_trick_cannot_borrow_a_trusted_name() {
    let p = ok(
        "https://evil.example/#k=GpgOcJxe4H6eAhLZpgZ72w&rv=wss%3A%2F%2Fgood.ts.net%40evil.example",
    );
    assert_eq!(p.relay.as_deref(), Some("wss://evil.example"));
    assert!(!p.relay_trusted);
}

#[test]
fn a_name_that_merely_contains_ts_net_is_not_a_tailnet_name() {
    assert!(!relay_adoptable(Some("ts.net.evil.example"), None, None));
    assert!(!relay_adoptable(Some("evilts.net"), None, None));
    assert!(!relay_adoptable(Some("ts.net"), None, None));
    assert!(relay_adoptable(
        Some("tawny.ainu-anoles.ts.net"),
        None,
        None
    ));
    assert!(relay_adoptable(
        Some("TAWNY.AINU-ANOLES.TS.NET"),
        None,
        None
    ));
}

#[test]
fn the_relay_this_install_already_uses_is_an_anchor_and_the_port_is_part_of_it() {
    assert!(relay_adoptable(
        Some("relay.example.net"),
        Some("relay.example.net"),
        None
    ));
    assert!(relay_adoptable(
        Some("worker.example.dev"),
        None,
        Some("worker.example.dev")
    ));
    assert!(relay_adoptable(
        Some("relay.example.net"),
        Some("relay.example.net:443"),
        None
    ));
    assert!(relay_adoptable(
        Some("relay.example.net:443"),
        Some("relay.example.net"),
        None
    ));
    assert!(relay_adoptable(
        Some("relay.example.net:8443"),
        Some("relay.example.net:8443"),
        None
    ));
    assert!(!relay_adoptable(
        Some("relay.example.net:9999"),
        Some("relay.example.net"),
        None
    ));
    assert!(!relay_adoptable(
        Some("relay.example.net"),
        Some("relay.example.net:8443"),
        None
    ));
    assert!(!relay_adoptable(
        Some("relay.example.net:8443"),
        Some("relay.example.net:9999"),
        None
    ));
    assert!(!relay_adoptable(
        Some("relay.example.net:9999"),
        None,
        Some("relay.example.net")
    ));
    assert!(!relay_adoptable(
        Some("other.example.net"),
        Some("relay.example.net"),
        None
    ));
}

#[test]
fn a_port_never_weakens_the_reachability_anchors() {
    assert!(relay_adoptable(
        Some("tawny.ainu-anoles.ts.net"),
        None,
        None
    ));
    assert!(relay_adoptable(
        Some("tawny.ainu-anoles.ts.net:8443"),
        None,
        None
    ));
    assert!(relay_adoptable(Some("192.168.1.50:8099"), None, None));
    assert!(!relay_adoptable(Some("evil.example:443"), None, None));
}

#[test]
fn a_private_address_is_the_same_trust_as_h() {
    assert!(relay_adoptable(Some("192.168.1.50:8099"), None, None));
    assert!(relay_adoptable(Some("10.1.2.3"), None, None));
    assert!(!relay_adoptable(Some("8.8.8.8"), None, None));
    assert!(!relay_adoptable(Some(""), None, None));
    assert!(!relay_adoptable(None, None, None));
}

#[test]
fn a_strangers_origin_is_adopted_once_the_user_has_pointed_at_it_themselves() {
    let anchors = Anchors {
        preferred: Some("relay.example.net"),
        ..Anchors::default()
    };
    let p = link::parse(
        "https://relay.example.net/#k=GpgOcJxe4H6eAhLZpgZ72w",
        anchors,
    );
    assert_eq!(p.map(|p| p.relay_trusted), Ok(true));
}

#[test]
fn parameter_oddities_do_not_derail_the_parse() {
    let p = ok("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&&r=viewer&h=192.168.1.50:8820&");
    assert_eq!(p.signal.as_deref(), Some("ws://192.168.1.50:8820"));
    let base = "tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&h=192.168.1.50:8820";
    assert_eq!(
        ok(&format!("{base}&n=S%C3%A9amus%27%20cat")).name,
        "Séamus' cat"
    );
    assert_eq!(
        ok(&format!("{base}&n={}", "x".repeat(80)))
            .name
            .chars()
            .count(),
        40
    );
    assert_eq!(ok(&format!("{base}&n=")).name, "Pet camera");
    assert_eq!(
        ok(&format!("{base}&h=10.0.0.9:8820")).signal.as_deref(),
        Some("ws://192.168.1.50:8820")
    );
    assert_eq!(ok(&format!("{base}&c=nope")).code, None);
    assert_eq!(ok(&format!("{base}&n=100%25+cat")).name, "100% cat");
    // A truncated escape (the Kotlin harness can't express this one).
    assert_eq!(ok(&format!("{base}&n=50%")).name, "50%");
}

#[test]
fn an_explicit_port_on_a_web_link_is_carried_into_the_relay() {
    let p = ok("https://tawny.ainu-anoles.ts.net:8443/#k=GpgOcJxe4H6eAhLZpgZ72w");
    assert_eq!(
        p.relay.as_deref(),
        Some("wss://tawny.ainu-anoles.ts.net:8443")
    );
}

#[test]
fn np_is_read_from_an_app_link_and_from_a_web_link() {
    let np = format!("{}1", "a".repeat(63));
    let app = ok(&format!(
        "tawny://pair?k=abcdefghijklmnop1234&n=Cam&h=192.168.1.20:8443&c=abcdefgh&np={np}"
    ));
    assert_eq!(app.nostr_station.as_deref(), Some(np.as_str()));
    let web = ok(&format!(
        "https://tawny.example.ts.net/#k=abcdefghijklmnop1234&n=Cam&r=viewer&c=abcdefgh&np={np}"
    ));
    assert_eq!(web.nostr_station.as_deref(), Some(np.as_str()));
}

#[test]
fn np_in_capitals_is_taken_as_the_same_key() {
    let np = "AB".repeat(32);
    let p = ok(&format!(
        "tawny://pair?k=abcdefghijklmnop1234&h=192.168.1.20:8443&np={np}"
    ));
    assert_eq!(p.nostr_station, Some(np.to_lowercase()));
}

#[test]
fn a_link_without_np_or_with_a_malformed_one_still_pairs() {
    let none = ok("tawny://pair?k=abcdefghijklmnop1234&h=192.168.1.20:8443");
    assert_eq!(none.nostr_station, None);
    for bad in [
        "abc".to_owned(),
        "g".repeat(64),
        "a".repeat(65),
        format!("{}%22", "a".repeat(63)),
    ] {
        let p = ok(&format!(
            "tawny://pair?k=abcdefghijklmnop1234&h=192.168.1.20:8443&np={bad}"
        ));
        assert_eq!(p.nostr_station, None, "np refused: {bad}");
    }
}

// ------------------------------------------------------------- building

#[test]
fn a_built_web_link_reads_back() {
    let parts = LinkParts {
        key: KEY,
        name: "Pet camera",
        token: Some("NvM7UXwo8X79vagb9VARxQ"),
        code: "WXXHeH07nDk",
        expires_s: 1_789_060_818,
        nostr_station: None,
        lan: None,
    };
    let built = build_web_link("https://tawny.ainu-anoles.ts.net/", &parts);
    assert_eq!(built, WEB_LINK.replace(' ', ""));
    assert_eq!(ok(&built), ok(WEB_LINK));
}

#[test]
fn a_built_app_link_reads_back() {
    let np = "ab".repeat(32);
    let parts = LinkParts {
        key: KEY,
        name: "Séamus' cat",
        token: Some("NvM7UXwo8X79vagb9VARxQ"),
        code: "WXXHeH07nDk",
        expires_s: 1_789_060_818,
        nostr_station: Some(&np),
        lan: Some("192.168.1.50:8820"),
    };
    let p = ok(&build_app_link(&parts));
    assert_eq!(p.name, "Séamus' cat");
    assert_eq!(p.signal.as_deref(), Some("ws://192.168.1.50:8820"));
    assert_eq!(p.code.as_deref(), Some("WXXHeH07nDk"));
    assert_eq!(p.nostr_station.as_deref(), Some(np.as_str()));
}

#[test]
fn a_nostr_only_link_is_somewhere_to_meet() {
    let np = "f5e2fdb2c5953107f067cfd6ad11a62c508627337b49e54f38756423132d75e1";
    let p = parse(&format!(
        "tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&c=CODE5678xyz&np={np}"
    ));
    assert_eq!(p.map(|p| p.nostr_station), Ok(Some(np.to_owned())));
    assert_eq!(
        parse("tawny://pair?k=GpgOcJxe4H6eAhLZpgZ72w&np=nothex"),
        Err(LinkError::NowhereToMeet),
        "a malformed key doesn't count"
    );
}

#[test]
fn a_link_from_the_js_monitor_reads() {
    // As 1.0.5's web Monitor writes it (ticket, code, deadline, Nostr key).
    let raw = "tawny://pair?k=BzFQPUyYT7Sb9CQyN6nOYQ&n=Pet+camera&t=hlgLz0NkPayB2allg5_K4A&c=a-6dkh4oD7s&e=1795210160&np=b303b6758575b668b0bc2989158c7492c62bdabc7fb60e1d44081092be51506e";
    let p = parse(raw);
    assert!(p.is_ok(), "{p:?}");
    let Ok(p) = p else { return };
    assert_eq!(p.name, "Pet camera");
    assert_eq!(p.token.as_deref(), Some("hlgLz0NkPayB2allg5_K4A"));
    assert_eq!(p.code.as_deref(), Some("a-6dkh4oD7s"));
    assert!(p.nostr_station.is_some());
}
