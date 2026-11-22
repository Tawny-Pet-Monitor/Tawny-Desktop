//! NIP-01 events: building, signing (BIP-340 Schnorr), checking.

use k256::FieldBytes;
use k256::schnorr::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A Nostr event. `sig` is absent on a rumor (NIP-59's unsigned inner event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// SHA-256 of the NIP-01 serialisation, lower-case hex.
    pub id: String,
    /// The author's x-only public key, lower-case hex.
    pub pubkey: String,
    /// Seconds since the epoch.
    pub created_at: u64,
    /// The event kind.
    pub kind: u16,
    /// Tags, each a non-empty list of strings.
    pub tags: Vec<Vec<String>>,
    /// The content.
    pub content: String,
    /// The BIP-340 signature, lower-case hex; `None` on a rumor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// What a new event is made of.
#[derive(Debug, Clone, Default)]
pub struct Template {
    /// The event kind.
    pub kind: u16,
    /// Tags.
    pub tags: Vec<Vec<String>>,
    /// The content.
    pub content: String,
    /// Seconds since the epoch.
    pub created_at: u64,
}

/// Why an event was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EventError {
    /// A field has the wrong type or length.
    #[error("bad shape: {0}")]
    Shape(&'static str),
    /// The id is not the hash of the event.
    #[error("id does not match content")]
    Id,
    /// The signature does not verify.
    #[error("bad signature")]
    Signature,
    /// The secret key is unusable.
    #[error("bad secret key")]
    Key,
}

fn is_hex(s: &str, bytes: usize) -> bool {
    s.len() == bytes * 2
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The NIP-01 serialisation whose SHA-256 is the event id. `serde_json` escapes
/// strings the way `JSON.stringify` does, which is what NIP-01 specifies.
#[must_use]
pub fn id_for(
    pubkey: &str,
    created_at: u64,
    kind: u16,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    let ser =
        serde_json::to_string(&(0, pubkey, created_at, kind, tags, content)).unwrap_or_default();
    hex::encode(Sha256::digest(ser.as_bytes()))
}

/// The x-only public key of a secret key, lower-case hex.
///
/// # Errors
/// [`EventError::Key`] for a zero or out-of-range key.
pub fn public_key(secret: &[u8; 32]) -> Result<String, EventError> {
    let sk = SigningKey::from_bytes(&FieldBytes::from(*secret)).map_err(|_| EventError::Key)?;
    Ok(hex::encode(sk.verifying_key().to_bytes()))
}

/// Builds and signs an event.
///
/// # Errors
/// [`EventError::Key`] for an unusable key or a failed RNG.
pub fn sign(t: Template, secret: &[u8; 32]) -> Result<Event, EventError> {
    let sk = SigningKey::from_bytes(&FieldBytes::from(*secret)).map_err(|_| EventError::Key)?;
    let pubkey = hex::encode(sk.verifying_key().to_bytes());
    let id = id_for(&pubkey, t.created_at, t.kind, &t.tags, &t.content);
    let id_bytes = hex::decode(&id).map_err(|_| EventError::Id)?;
    let mut aux = [0u8; 32];
    getrandom::fill(&mut aux).map_err(|_| EventError::Key)?;
    let sig = sk
        .sign_raw(&id_bytes, &aux)
        .map_err(|_| EventError::Signature)?;
    Ok(Event {
        id,
        pubkey,
        created_at: t.created_at,
        kind: t.kind,
        tags: t.tags,
        content: t.content,
        sig: Some(hex::encode(sig.to_bytes())),
    })
}

/// Strict NIP-01 shape check on raw JSON, then conversion. `signed` decides
/// whether `sig` must be present (events) or absent (rumors).
///
/// # Errors
/// [`EventError::Shape`] naming the first problem.
pub fn from_json(v: &Value, signed: bool) -> Result<Event, EventError> {
    let o = v.as_object().ok_or(EventError::Shape("not an object"))?;
    let s = |k: &'static str| o.get(k).and_then(Value::as_str);
    let id = s("id")
        .filter(|x| is_hex(x, 32))
        .ok_or(EventError::Shape("id"))?;
    let pubkey = s("pubkey")
        .filter(|x| is_hex(x, 32))
        .ok_or(EventError::Shape("pubkey"))?;
    let sig = match (signed, o.get("sig")) {
        (true, Some(Value::String(x))) if is_hex(x, 64) => Some(x.clone()),
        (true, _) => return Err(EventError::Shape("sig")),
        (false, None) => None,
        (false, Some(_)) => return Err(EventError::Shape("a rumor must be unsigned")),
    };
    let kind = o
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|k| u16::try_from(k).ok())
        .ok_or(EventError::Shape("kind"))?;
    let created_at = o
        .get("created_at")
        .and_then(Value::as_u64)
        .ok_or(EventError::Shape("created_at"))?;
    let content = s("content").ok_or(EventError::Shape("content"))?;
    let tags = o
        .get("tags")
        .and_then(Value::as_array)
        .ok_or(EventError::Shape("tags"))?
        .iter()
        .map(|t| {
            let arr = t.as_array().filter(|a| !a.is_empty())?;
            arr.iter()
                .map(|x| x.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(EventError::Shape(
            "tags must be non-empty arrays of strings",
        ))?;
    Ok(Event {
        id: id.to_owned(),
        pubkey: pubkey.to_owned(),
        created_at,
        kind,
        tags,
        content: content.to_owned(),
        sig,
    })
}

impl Event {
    /// Checks the id and, when present, the signature.
    ///
    /// # Errors
    /// [`EventError::Id`] or [`EventError::Signature`].
    pub fn verify(&self) -> Result<(), EventError> {
        if id_for(
            &self.pubkey,
            self.created_at,
            self.kind,
            &self.tags,
            &self.content,
        ) != self.id
        {
            return Err(EventError::Id);
        }
        let Some(sig) = &self.sig else { return Ok(()) };
        let pk = hex::decode(&self.pubkey).map_err(|_| EventError::Signature)?;
        // An x-only key is exactly 32 bytes; anything else is refused here.
        let pk = FieldBytes::try_from(pk.as_slice()).map_err(|_| EventError::Signature)?;
        let vk = VerifyingKey::from_bytes(&pk).map_err(|_| EventError::Signature)?;
        let sig = hex::decode(sig)
            .ok()
            .and_then(|b| Signature::try_from(b.as_slice()).ok())
            .ok_or(EventError::Signature)?;
        let id = hex::decode(&self.id).map_err(|_| EventError::Id)?;
        vk.verify_raw(&id, &sig).map_err(|_| EventError::Signature)
    }

    /// Every value of tag `name`.
    pub fn tag_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.tags
            .iter()
            .filter(move |t| t.first().is_some_and(|n| n == name))
            .filter_map(|t| t.get(1).map(String::as_str))
    }
}
