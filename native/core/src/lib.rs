//! The shared engine inside the Tawny apps.
//!
//! Everything here must give byte-for-byte the same answers as the JavaScript
//! engine (`public/app.js`) and the Android shell (`PairLink.kt`), because
//! apps built on this crate pair and call with apps that are not.

pub mod call;
#[cfg(feature = "engine")]
pub mod engine;
pub mod ids;
pub mod link;
#[cfg(feature = "net")]
pub mod net;
pub mod nostr;
pub mod relay;
pub mod sas;
pub mod signal;

pub use ids::{Secret, SecretKind};
pub use link::{Anchors, LinkError, Pairing};
