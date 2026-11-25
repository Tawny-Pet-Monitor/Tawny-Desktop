//! Nostr, as Tawny uses it for signalling: NIP-01 events, NIP-44 encryption
//! and NIP-59 gift wraps. Ports of `public/nostr/*.js`, checked against the
//! official NIP-44 vectors and against wraps sealed by the JS code.

pub mod event;
pub mod nip44;
pub mod nip59;
