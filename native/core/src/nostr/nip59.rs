//! NIP-59 gift wrap: rumor → seal (kind 13) → wrap (kind 21059 ephemeral, or 1059).
//!
//! The rumor is the real message, unsigned. The seal is signed by the real
//! sender, has no tags, and carries the rumor encrypted to the recipient. The
//! wrap is signed by a one-time key, names only the recipient (`p` tag), and
//! carries the seal encrypted to the recipient. A relay learns "someone sent
//! about N bytes to this key", and nothing else.

use serde_json::Value;
use thiserror::Error;

use super::event::{self, Event, EventError, Template};
use super::nip44::{self, DEFAULT_MAX_PLAINTEXT};

/// The seal's kind.
pub const KIND_SEAL: u16 = 13;
/// A stored gift wrap.
pub const KIND_GIFT_WRAP: u16 = 1059;
/// An ephemeral gift wrap: what Tawny signalling uses.
pub const KIND_EPHEMERAL_GIFT_WRAP: u16 = 21_059;
/// strfry (damus, nos.lol, most big relays) drops ephemeral events older than
/// 60 s, so ephemeral wraps are back-dated by at most this much.
pub const EPHEMERAL_JITTER_S: u64 = 20;
const STORED_JITTER_S: u64 = 2 * 24 * 3600;

/// Why a wrap was refused. The codes match `UnwrapError.code` in `nip59.js`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnwrapError {
    /// Not kind 1059 or 21059.
    #[error("not a gift wrap")]
    NotAWrap,
    /// The wrap itself fails NIP-01.
    #[error("wrap: {0}")]
    BadWrap(EventError),
    /// The wrap names someone else.
    #[error("wrap is addressed to someone else")]
    NotForMe,
    /// The wrap's content does not decrypt.
    #[error("wrap does not decrypt: {0}")]
    DecryptWrap(nip44::Nip44Error),
    /// The seal fails NIP-01, is not kind 13, or has tags.
    #[error("seal: {0}")]
    BadSeal(&'static str),
    /// The seal's content does not decrypt.
    #[error("seal does not decrypt: {0}")]
    DecryptSeal(nip44::Nip44Error),
    /// The rumor is malformed, signed, or its id does not match.
    #[error("rumor: {0}")]
    BadRumor(&'static str),
    /// The rumor claims an author other than the seal's signer.
    #[error("rumor author is not the seal signer")]
    Impersonation,
    /// Our own key is unusable.
    #[error("bad key")]
    Key,
}

impl UnwrapError {
    /// The same short code `nip59.js` uses.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotAWrap => "not-a-wrap",
            Self::BadWrap(_) => "bad-wrap",
            Self::NotForMe => "not-for-me",
            Self::DecryptWrap(_) => "decrypt-wrap",
            Self::BadSeal(_) => "bad-seal",
            Self::DecryptSeal(_) => "decrypt-seal",
            Self::BadRumor(_) => "bad-rumor",
            Self::Impersonation => "impersonation",
            Self::Key => "bad-key",
        }
    }
}

/// Why wrapping failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WrapError {
    /// Signing or key handling failed.
    #[error(transparent)]
    Event(#[from] EventError),
    /// Encryption failed.
    #[error(transparent)]
    Nip44(#[from] nip44::Nip44Error),
}

fn jitter(wrap_kind: u16) -> u64 {
    let span = if wrap_kind == KIND_EPHEMERAL_GIFT_WRAP {
        EPHEMERAL_JITTER_S
    } else {
        STORED_JITTER_S
    };
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return span / 2;
    }
    u64::from_le_bytes(b) % (span + 1)
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn ck(secret: &[u8; 32], pub_hex: &str) -> Result<nip44::ConversationKey, nip44::Nip44Error> {
    let sk = nip44::secret_key(secret)?;
    let pk = unhex32(pub_hex).ok_or(nip44::Nip44Error::PublicKey)?;
    nip44::conversation_key(&sk, &pk)
}

/// Wraps a message from `sender` for `recipient_pub`; returns the wrap.
/// `now` is the current time in seconds.
///
/// # Errors
/// [`WrapError`] for bad keys or an oversized message.
pub fn gift_wrap(
    t: Template,
    sender: &[u8; 32],
    recipient_pub: &str,
    wrap_kind: u16,
    now: u64,
    expires_in: Option<u64>,
) -> Result<Event, WrapError> {
    let sender_pub = event::public_key(sender)?;
    let id = event::id_for(&sender_pub, t.created_at, t.kind, &t.tags, &t.content);
    let rumor = Event {
        id,
        pubkey: sender_pub,
        created_at: t.created_at,
        kind: t.kind,
        tags: t.tags,
        content: t.content,
        sig: None,
    };
    let rumor_json = serde_json::to_string(&rumor).map_err(|_| EventError::Shape("rumor"))?;
    let seal = event::sign(
        Template {
            kind: KIND_SEAL,
            tags: vec![],
            content: nip44::encrypt(&rumor_json, &ck(sender, recipient_pub)?)?,
            created_at: now.saturating_sub(jitter(wrap_kind)),
        },
        sender,
    )?;
    let seal_json = serde_json::to_string(&seal).map_err(|_| EventError::Shape("seal"))?;

    // A one-time key: never reused, never stored.
    let mut one_time = [0u8; 32];
    loop {
        getrandom::fill(&mut one_time).map_err(|_| EventError::Key)?;
        if nip44::secret_key(&one_time).is_ok() {
            break;
        }
    }
    let mut tags = vec![vec!["p".to_owned(), recipient_pub.to_owned()]];
    if let Some(secs) = expires_in {
        tags.push(vec!["expiration".to_owned(), (now + secs).to_string()]);
    }
    let content = nip44::encrypt(&seal_json, &ck(&one_time, recipient_pub)?)?;
    let wrap = event::sign(
        Template {
            kind: wrap_kind,
            tags,
            content,
            created_at: now.saturating_sub(jitter(wrap_kind)),
        },
        &one_time,
    )?;
    one_time.fill(0);
    Ok(wrap)
}

/// Opens a wrap addressed to `recipient`, with every check NIP-59 and NIP-17
/// call for, in an order that never decrypts anything unauthenticated.
/// Returns `(seal, rumor)`.
///
/// # Errors
/// The first [`UnwrapError`] the wrap earns.
pub fn unwrap(wrap: &Value, recipient: &[u8; 32]) -> Result<(Event, Event), UnwrapError> {
    let me = event::public_key(recipient).map_err(|_| UnwrapError::Key)?;
    let kind = wrap.get("kind").and_then(Value::as_u64);
    if kind != Some(u64::from(KIND_GIFT_WRAP)) && kind != Some(u64::from(KIND_EPHEMERAL_GIFT_WRAP))
    {
        return Err(UnwrapError::NotAWrap);
    }
    let wrap = event::from_json(wrap, true).map_err(UnwrapError::BadWrap)?;
    wrap.verify().map_err(UnwrapError::BadWrap)?;
    if !wrap.tag_values("p").any(|p| p == me) {
        return Err(UnwrapError::NotForMe);
    }

    let wrap_ck = ck(recipient, &wrap.pubkey).map_err(UnwrapError::DecryptWrap)?;
    let seal_json = nip44::decrypt(&wrap.content, &wrap_ck, DEFAULT_MAX_PLAINTEXT)
        .map_err(UnwrapError::DecryptWrap)?;
    let seal_v: Value =
        serde_json::from_str(&seal_json).map_err(|_| UnwrapError::BadSeal("not JSON"))?;
    let seal = event::from_json(&seal_v, true).map_err(|_| UnwrapError::BadSeal("shape"))?;
    seal.verify()
        .map_err(|_| UnwrapError::BadSeal("id or signature"))?;
    if seal.kind != KIND_SEAL {
        return Err(UnwrapError::BadSeal("not kind 13"));
    }
    if !seal.tags.is_empty() {
        return Err(UnwrapError::BadSeal("tags must be empty"));
    }

    let seal_ck = ck(recipient, &seal.pubkey).map_err(UnwrapError::DecryptSeal)?;
    let rumor_json = nip44::decrypt(&seal.content, &seal_ck, DEFAULT_MAX_PLAINTEXT)
        .map_err(UnwrapError::DecryptSeal)?;
    let rumor_v: Value =
        serde_json::from_str(&rumor_json).map_err(|_| UnwrapError::BadRumor("not JSON"))?;
    let rumor =
        event::from_json(&rumor_v, false).map_err(|_| UnwrapError::BadRumor("shape, or signed"))?;
    rumor
        .verify()
        .map_err(|_| UnwrapError::BadRumor("id does not match"))?;
    // NIP-17: without this, anyone could seal a rumor that claims another author.
    if rumor.pubkey != seal.pubkey {
        return Err(UnwrapError::Impersonation);
    }
    Ok((seal, rumor))
}
