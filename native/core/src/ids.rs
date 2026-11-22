//! Channel secrets and the values derived from them.
//!
//! A channel is identified by its key. The relay never sees the key: it sees
//! the room id (a hash of the key) and, from the Monitor, a proof of holding
//! the key (a second hash under another domain separator).

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// What a freshly minted secret is for. Each has the size `app.js` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// The channel key: 16 random bytes (`newKey()`).
    ChannelKey,
    /// The relay admission ticket: 16 random bytes (`newKey()`).
    Ticket,
    /// The ten-minute pairing code shown in the QR: 8 random bytes (`newPairCode()`).
    PairCode,
}

impl SecretKind {
    const fn len(self) -> usize {
        match self {
            Self::ChannelKey | Self::Ticket => 16,
            Self::PairCode => 8,
        }
    }
}

/// The operating system's random source failed.
#[derive(Debug, Error)]
#[error("the system random number generator failed: {0}")]
pub struct RandomError(String);

/// A random secret, base64url without padding, as `app.js` writes them.
///
/// `Debug` never prints the value, so a secret can't leak into a log by
/// accident; use [`Secret::expose`] where the value is really needed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Mints a new secret of the given kind.
    ///
    /// # Errors
    /// [`RandomError`] when the system RNG fails. There is no fallback: a
    /// guessable channel key would be worse than no key.
    pub fn new(kind: SecretKind) -> Result<Self, RandomError> {
        let mut bytes = vec![0u8; kind.len()];
        getrandom::fill(&mut bytes).map_err(|e| RandomError(e.to_string()))?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// Wraps a value that came from storage or a pairing link.
    #[must_use]
    pub fn from_existing(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret's text.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// `roomIdFor()`: the first 32 hex characters of SHA-256(`"tawny-room-v1|" + key`).
#[must_use]
pub fn room_id(key: &str) -> String {
    let mut id = sha256_hex(&format!("tawny-room-v1|{key}"));
    id.truncate(32);
    id
}

/// The Monitor's proof of holding the key, sent as `a` in its hello:
/// SHA-256 hex of `"tawny-auth-v1|" + key`.
#[must_use]
pub fn auth_proof(key: &str) -> String {
    sha256_hex(&format!("tawny-auth-v1|{key}"))
}

/// The ticket's hash, sent as `hashT` in the Monitor's hello so the relay can
/// admit Viewers that present the ticket itself.
#[must_use]
pub fn ticket_hash(ticket: &str) -> String {
    sha256_hex(ticket)
}

/// The LAN relay's challenge response: HMAC-SHA256(key, `"tawny-lan-v1|" + nonce`) in hex.
#[must_use]
pub fn lan_response(key: &str, nonce: &str) -> String {
    let mut mac = hmac_for(key);
    mac.update(format!("tawny-lan-v1|{nonce}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub(crate) fn hmac_for(key: &str) -> Hmac<Sha256> {
    // HMAC takes a key of any length, so this cannot fail; the fallback to an
    // empty-keyed MAC is unreachable and only exists to avoid a panic path.
    Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap_or_else(|_| {
        <Hmac<Sha256> as hmac::digest::KeyInit>::new(
            &hmac::digest::generic_array::GenericArray::default(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values from the JS recipes (node's crypto, same as WebCrypto).
    const KEY: &str = "Zx9_-abcDEF";

    #[test]
    fn room_id_matches_js() {
        assert_eq!(room_id("k"), "d08be28c4427c4c27a85f9200f8440b7");
        assert_eq!(room_id(KEY), "e25461a5e0ae5f562ae243789ad550b4");
    }

    #[test]
    fn auth_proof_matches_js() {
        assert_eq!(
            auth_proof(KEY),
            "e9a96612726a3494d57d503276837f38c71bb0c9ee947ba99efd447320b06d39"
        );
    }

    #[test]
    fn secrets_have_the_js_shape() -> Result<(), RandomError> {
        let key = Secret::new(SecretKind::ChannelKey)?;
        let code = Secret::new(SecretKind::PairCode)?;
        assert_eq!(key.expose().len(), 22);
        assert_eq!(code.expose().len(), 11);
        let ok = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        assert!(ok(key.expose()) && ok(code.expose()));
        assert_ne!(key, Secret::new(SecretKind::ChannelKey)?);
        Ok(())
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let s = Secret::from_existing("very-secret-value");
        assert_eq!(format!("{s:?}"), "Secret(…)");
    }
}
