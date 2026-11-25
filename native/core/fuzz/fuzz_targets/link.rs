//! Pairing links, as they arrive from a QR code, a click or another copy of
//! the app: never a panic, and whatever is accepted holds up.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tawny_core::link::{self, Anchors};

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    for anchors in [
        Anchors::default(),
        Anchors {
            preferred: Some("wss://relay.example"),
            build: Some("wss://rv.example"),
            has_rendezvous: true,
        },
    ] {
        if let Ok(p) = link::parse(raw, anchors) {
            // A LAN address is only ever a private one.
            if let Some(h) = p.signal.as_deref().and_then(|s| s.strip_prefix("ws://")) {
                assert!(link::is_private_host_port(h), "public LAN address {h}");
            }
            let _ = p.is_expired(0);
        }
    }
    let _ = link::is_private_host_port(raw);
});
