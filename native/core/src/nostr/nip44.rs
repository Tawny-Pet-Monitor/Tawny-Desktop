//! NIP-44 v2: `secp256k1` ECDH, HKDF-SHA256, `ChaCha20` and HMAC-SHA256
//! (encrypt-then-MAC), with length-hiding padding.
//!
//! Written to the NIP's pseudo-code step for step, like `public/nostr/nip44.js`,
//! and held to the official vector file (`tests/fixtures/nip44.vectors.json`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use k256::elliptic_curve::sec1::FromSec1Point;
use k256::{AffinePoint, Sec1Point, SecretKey};
use sha2::Sha256;
use thiserror::Error;

const VERSION: u8 = 2;
const SALT: &[u8] = b"nip44-v2";
const EXTENDED_PREFIX_THRESHOLD: usize = 65_536;

/// Signalling messages are a few KB; 65535 also keeps to the classic
/// two-byte length prefix every deployed client understands.
pub const DEFAULT_MAX_PLAINTEXT: usize = 65_535;

/// Why NIP-44 refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Nip44Error {
    /// The secret key is zero or not below the curve order.
    #[error("invalid secret key")]
    SecretKey,
    /// The public key is not an x coordinate on the curve.
    #[error("invalid public key")]
    PublicKey,
    /// The plaintext is empty or too long.
    #[error("invalid plaintext length")]
    PlaintextLength,
    /// The payload is not version 2 (or is a `#`-prefixed future format).
    #[error("unknown encryption version")]
    Version,
    /// The payload is too short or too long to be valid.
    #[error("invalid payload size")]
    PayloadSize,
    /// The decoded payload is too short.
    #[error("invalid data size")]
    DataSize,
    /// The payload is not base64.
    #[error("invalid base64")]
    Base64,
    /// The MAC does not verify.
    #[error("invalid MAC")]
    Mac,
    /// The padding is malformed.
    #[error("invalid padding")]
    Padding,
    /// The plaintext is not UTF-8.
    #[error("invalid UTF-8")]
    Utf8,
}

/// The shared key two parties derive for each other; symmetric.
#[derive(Clone, PartialEq, Eq)]
pub struct ConversationKey([u8; 32]);

impl std::fmt::Debug for ConversationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConversationKey(…)")
    }
}

impl ConversationKey {
    /// Wraps raw bytes (vectors, or a cached key).
    #[must_use]
    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    /// The raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap_or_else(|_| {
        <Hmac<Sha256> as hmac::digest::KeyInit>::new(
            &hmac::digest::generic_array::GenericArray::default(),
        )
    });
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// Parses a 32-byte secret key.
///
/// # Errors
/// [`Nip44Error::SecretKey`] when it is zero or not below the curve order.
pub fn secret_key(bytes: &[u8; 32]) -> Result<SecretKey, Nip44Error> {
    SecretKey::from_slice(bytes).map_err(|_| Nip44Error::SecretKey)
}

/// `conversation_key = HKDF-extract(IKM = shared_x, salt = "nip44-v2")`, the
/// x-only public key taken with even y as BIP-340 does.
///
/// # Errors
/// [`Nip44Error::PublicKey`] when `pub_x` is not on the curve.
pub fn conversation_key(
    secret: &SecretKey,
    pub_x: &[u8; 32],
) -> Result<ConversationKey, Nip44Error> {
    let mut sec1 = [0u8; 33];
    sec1[0] = 0x02;
    sec1[1..].copy_from_slice(pub_x);
    let point = Sec1Point::from_bytes(sec1).map_err(|_| Nip44Error::PublicKey)?;
    let affine: Option<AffinePoint> = AffinePoint::from_sec1_point(&point).into();
    let affine = affine.ok_or(Nip44Error::PublicKey)?;
    let shared = k256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), affine);
    Ok(ConversationKey(hmac(
        SALT,
        &[shared.raw_secret_bytes().as_slice()],
    )))
}

/// The three per-message keys: `HKDF-expand(PRK = conversation_key, info = nonce, L = 76)`.
#[must_use]
pub fn message_keys(ck: &ConversationKey, nonce: &[u8; 32]) -> ([u8; 32], [u8; 12], [u8; 32]) {
    let mut okm = Vec::with_capacity(96);
    let mut t: Vec<u8> = Vec::new();
    for i in 1u8..=3 {
        let block = hmac(&ck.0, &[&t, nonce, &[i]]);
        okm.extend_from_slice(&block);
        t = block.to_vec();
    }
    let mut key = [0u8; 32];
    let mut iv = [0u8; 12];
    let mut mac = [0u8; 32];
    key.copy_from_slice(&okm[..32]);
    iv.copy_from_slice(&okm[32..44]);
    mac.copy_from_slice(&okm[44..76]);
    (key, iv, mac)
}

/// The largest plaintext NIP-44 allows (`2^32 - 1` bytes).
pub const MAX_PLAINTEXT: usize = u32::MAX as usize;

/// `calc_padded_len`. Lengths past [`MAX_PLAINTEXT`] are clamped to it, so
/// this never overflows.
#[must_use]
pub fn padded_len(len: usize) -> usize {
    let len = len.min(MAX_PLAINTEXT);
    if len <= 32 {
        return 32;
    }
    // The smallest power of two strictly greater than len - 1.
    let next_power = 1usize << (usize::BITS - (len - 1).leading_zeros());
    let chunk = if next_power <= 256 {
        32
    } else {
        next_power / 8
    };
    chunk * ((len - 1) / chunk + 1)
}

fn pad(plaintext: &str, max_len: usize) -> Result<Vec<u8>, Nip44Error> {
    let bytes = plaintext.as_bytes();
    let len = bytes.len();
    if len < 1 || len > max_len.min(MAX_PLAINTEXT) {
        return Err(Nip44Error::PlaintextLength);
    }
    let mut out = if len >= EXTENDED_PREFIX_THRESHOLD {
        let mut p = vec![0u8, 0u8];
        p.extend_from_slice(
            &u32::try_from(len)
                .map_err(|_| Nip44Error::PlaintextLength)?
                .to_be_bytes(),
        );
        p
    } else {
        u16::try_from(len)
            .map_err(|_| Nip44Error::PlaintextLength)?
            .to_be_bytes()
            .to_vec()
    };
    out.extend_from_slice(bytes);
    out.resize(out.len() + padded_len(len) - len, 0);
    Ok(out)
}

fn unpad(padded: &[u8]) -> Result<String, Nip44Error> {
    if padded.len() < 2 {
        return Err(Nip44Error::Padding);
    }
    let mut len = usize::from(u16::from_be_bytes([padded[0], padded[1]]));
    let mut prefix = 2;
    if len == 0 {
        let ext: [u8; 4] = padded
            .get(2..6)
            .and_then(|s| s.try_into().ok())
            .ok_or(Nip44Error::Padding)?;
        len = usize::try_from(u32::from_be_bytes(ext)).map_err(|_| Nip44Error::Padding)?;
        if len < EXTENDED_PREFIX_THRESHOLD {
            return Err(Nip44Error::Padding);
        }
        prefix = 6;
    }
    let body = padded
        .get(prefix..prefix + len)
        .ok_or(Nip44Error::Padding)?;
    if len == 0 || padded.len() != prefix + padded_len(len) {
        return Err(Nip44Error::Padding);
    }
    String::from_utf8(body.to_vec()).map_err(|_| Nip44Error::Utf8)
}

/// Encrypts with a given nonce (vectors); use [`encrypt`] otherwise.
///
/// # Errors
/// [`Nip44Error::PlaintextLength`] for an empty or oversized plaintext.
pub fn encrypt_with_nonce(
    plaintext: &str,
    ck: &ConversationKey,
    nonce: &[u8; 32],
    max_len: usize,
) -> Result<String, Nip44Error> {
    let (key, iv, mac_key) = message_keys(ck, nonce);
    let mut buf = pad(plaintext, max_len)?;
    ChaCha20::new(&key.into(), &iv.into()).apply_keystream(&mut buf);
    let mac = hmac(&mac_key, &[nonce, &buf]);
    let mut out = Vec::with_capacity(1 + 32 + buf.len() + 32);
    out.push(VERSION);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&mac);
    Ok(STANDARD.encode(out))
}

/// Encrypts with a fresh random nonce.
///
/// # Errors
/// [`Nip44Error::PlaintextLength`], or [`Nip44Error::SecretKey`] if the RNG fails.
pub fn encrypt(plaintext: &str, ck: &ConversationKey) -> Result<String, Nip44Error> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|_| Nip44Error::SecretKey)?;
    encrypt_with_nonce(plaintext, ck, &nonce, DEFAULT_MAX_PLAINTEXT)
}

/// Decrypts; the MAC is checked first, in constant time, and nothing is
/// decrypted unless it verifies.
///
/// # Errors
/// Any [`Nip44Error`] the payload earns.
pub fn decrypt(payload: &str, ck: &ConversationKey, max_len: usize) -> Result<String, Nip44Error> {
    if payload.is_empty() || payload.starts_with('#') {
        return Err(Nip44Error::Version);
    }
    let max_payload = (65 + 6 + padded_len(max_len.clamp(1, MAX_PLAINTEXT))).div_ceil(3) * 4;
    if payload.len() < 132 || payload.len() > max_payload {
        return Err(Nip44Error::PayloadSize);
    }
    let data = STANDARD.decode(payload).map_err(|_| Nip44Error::Base64)?;
    if data.len() < 99 {
        return Err(Nip44Error::DataSize);
    }
    if data[0] != VERSION {
        return Err(Nip44Error::Version);
    }
    let nonce: [u8; 32] = data[1..33].try_into().map_err(|_| Nip44Error::DataSize)?;
    let (ct, tag) = data[33..].split_at(data.len() - 33 - 32);
    let (key, iv, mac_key) = message_keys(ck, &nonce);
    let mut m = Hmac::<Sha256>::new_from_slice(&mac_key).map_err(|_| Nip44Error::Mac)?;
    m.update(&nonce);
    m.update(ct);
    m.verify_slice(tag).map_err(|_| Nip44Error::Mac)?;
    let mut buf = ct.to_vec();
    ChaCha20::new(&key.into(), &iv.into()).apply_keystream(&mut buf);
    unpad(&buf)
}
