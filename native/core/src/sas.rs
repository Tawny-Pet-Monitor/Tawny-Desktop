//! The short security code both screens show after a call connects.
//!
//! Same recipe as `computeSas()` in `app.js`: HMAC-SHA256 with the channel key
//! over `"tawny-sas-v1|" + fpA + "|" + fpB`, where the two DTLS fingerprints are
//! normalised and sorted, then the first 6 bytes as Crockford base32, `XXX-XXX`.
//!
//! Also the signature on each offer and answer (`sm`): HMAC-SHA256 with the
//! channel key over `"tawny-sdp-v1|" + type + "|" + fingerprint`. It ties the
//! certificate a description names to someone holding the key, which is what
//! lets a call over the Monitor's own LAN relay go without a code: anyone else
//! on the Wi-Fi can carry frames between the two, but can't put their own
//! certificate in one.

use hmac::Mac;
use serde_json::Value;

use crate::ids::hmac_for;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A DTLS fingerprint as `app.js` normalises it: lower-case algorithm, one
/// space, upper-case hex. An empty algorithm means `sha-256`.
#[must_use]
pub fn normalise(algorithm: &str, fingerprint: &str) -> String {
    let alg = if algorithm.is_empty() {
        "sha-256".to_owned()
    } else {
        algorithm.to_lowercase()
    };
    format!("{alg} {}", fingerprint.to_uppercase())
}

/// Picks the two fingerprints the code is computed over, like
/// `negotiatedFingerprints()`. `remote` is the remote certificate when the
/// stack reports one; otherwise the remote description must name exactly one
/// fingerprint (several distinct ones is the decoy case, and fails closed).
#[must_use]
pub fn negotiated(local: &str, remote: Option<&str>, remote_sdp: &str) -> Option<[String; 2]> {
    let remote = match remote {
        Some(r) => r.to_owned(),
        None => sdp_fingerprint(remote_sdp)?,
    };
    let mut pair = [local.to_owned(), remote];
    pair.sort();
    Some(pair)
}

/// The one certificate fingerprint a description names, normalised; `None`
/// when it names none or several distinct ones (the decoy case fails closed).
#[must_use]
pub fn sdp_fingerprint(sdp: &str) -> Option<String> {
    let mut seen: Vec<String> = sdp
        .lines()
        .filter_map(|l| l.strip_prefix("a=fingerprint:"))
        .filter_map(|rest| {
            let mut it = rest.split(' ');
            let alg = it.next()?;
            let fp = it.next()?.trim_end();
            let hexish = !fp.is_empty() && fp.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
            hexish.then(|| normalise(alg, fp))
        })
        .collect();
    seen.sort();
    seen.dedup();
    if seen.len() != 1 {
        return None;
    }
    seen.pop()
}

/// The code for a channel key and two normalised fingerprints, in either order.
#[must_use]
pub fn code(key: &str, fp_a: &str, fp_b: &str) -> String {
    let (first, second) = if fp_a <= fp_b {
        (fp_a, fp_b)
    } else {
        (fp_b, fp_a)
    };
    let mut mac = hmac_for(key);
    mac.update(format!("tawny-sas-v1|{first}|{second}").as_bytes());
    let digest = mac.finalize().into_bytes();
    let chars: String = digest
        .iter()
        .take(6)
        .map(|b| char::from(ALPHABET[usize::from(b & 31)]))
        .collect();
    format!("{}-{}", &chars[..3], &chars[3..])
}

const SDP_DOMAIN: &str = "tawny-sdp-v1";

/// What an offer's or answer's signature says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signed {
    /// Signed with the channel key, over the one certificate it names.
    Yes,
    /// No signature: a Tawny from before signatures, or a relay that dropped it.
    No,
    /// A signature that doesn't check out, or a description it can't cover.
    Forged,
}

/// The `sm` for a description of this `kind` (`"offer"` or `"answer"`);
/// `None` when the description doesn't name exactly one certificate.
#[must_use]
pub fn sign_sdp(key: &str, kind: &str, sdp: &str) -> Option<String> {
    let fp = sdp_fingerprint(sdp)?;
    let mut mac = hmac_for(key);
    mac.update(format!("{SDP_DOMAIN}|{kind}|{fp}").as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

/// Checks the signature on an `offer` or `answer` frame.
#[must_use]
pub fn check_frame(key: &str, f: &Value) -> Signed {
    let Some(sm) = f.get("sm") else {
        return Signed::No;
    };
    let (Some(sm), Some(kind), Some(sdp)) =
        (sm.as_str(), f["type"].as_str(), f["sdp"]["sdp"].as_str())
    else {
        return Signed::Forged;
    };
    let (Ok(given), Some(fp)) = (hex::decode(sm), sdp_fingerprint(sdp)) else {
        return Signed::Forged;
    };
    let mut mac = hmac_for(key);
    mac.update(format!("{SDP_DOMAIN}|{kind}|{fp}").as_bytes());
    if mac.verify_slice(&given).is_ok() {
        Signed::Yes
    } else {
        Signed::Forged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_js() {
        assert_eq!(
            code("Zx9_-abcDEF", "sha-256 74:1F:BF", "sha-256 E4:DB:5A"),
            "BGC-YB3"
        );
    }

    #[test]
    fn order_does_not_matter() {
        assert_eq!(
            code("k", "sha-256 AA", "sha-256 BB"),
            code("k", "sha-256 BB", "sha-256 AA")
        );
    }

    #[test]
    fn normalises_like_js() {
        assert_eq!(normalise("SHA-256", "ab:cd"), "sha-256 AB:CD");
        assert_eq!(normalise("", "ab"), "sha-256 AB");
    }

    #[test]
    fn falls_back_to_the_one_sdp_fingerprint() {
        let sdp =
            "v=0\r\na=fingerprint:sha-256 aa:bb\r\nm=audio\r\na=fingerprint:sha-256 AA:BB\r\n";
        let got = negotiated("sha-256 CC:DD", None, sdp);
        assert_eq!(
            got,
            Some(["sha-256 AA:BB".to_owned(), "sha-256 CC:DD".to_owned()])
        );
    }

    #[test]
    fn two_different_sdp_fingerprints_fail_closed() {
        let sdp = "a=fingerprint:sha-256 AA:BB\r\na=fingerprint:sha-256 EE:FF\r\n";
        assert_eq!(negotiated("sha-256 CC:DD", None, sdp), None);
        assert_eq!(negotiated("sha-256 CC:DD", None, ""), None);
    }

    #[test]
    fn a_description_names_one_fingerprint() {
        let one = "v=0\r\na=fingerprint:sha-256 ab:CD:01\r\nm=audio\r\na=fingerprint:sha-256 AB:CD:01\r\n";
        assert_eq!(
            super::sdp_fingerprint(one).as_deref(),
            Some("sha-256 AB:CD:01"),
            "the same one twice, in either case, is one"
        );
        let decoy = "a=fingerprint:sha-256 AB:CD:01\r\na=fingerprint:sha-256 EF:01:02\r\n";
        assert_eq!(
            super::sdp_fingerprint(decoy),
            None,
            "two distinct ones fail closed"
        );
        assert_eq!(super::sdp_fingerprint("v=0\r\n"), None, "none");
        assert_eq!(
            super::sdp_fingerprint("a=fingerprint:sha-256 zz:yy\r\n"),
            None,
            "not hex"
        );
    }

    fn frame(kind: &str, sdp: &str, sm: Option<String>) -> Value {
        let mut f = serde_json::json!({ "type": kind, "sdp": { "type": kind, "sdp": sdp } });
        if let Some(sm) = sm {
            f["sm"] = Value::String(sm);
        }
        f
    }

    #[test]
    fn a_signed_description_checks_out_only_as_signed() {
        let sdp = "v=0\r\na=fingerprint:sha-256 ab:cd:01\r\n";
        let sm = sign_sdp("key", "offer", sdp);
        assert!(sm.is_some());
        assert_eq!(
            check_frame("key", &frame("offer", sdp, sm.clone())),
            Signed::Yes
        );
        assert_eq!(check_frame("key", &frame("offer", sdp, None)), Signed::No);
        assert_eq!(
            check_frame("other key", &frame("offer", sdp, sm.clone())),
            Signed::Forged,
            "someone without the key"
        );
        assert_eq!(
            check_frame("key", &frame("answer", sdp, sm.clone())),
            Signed::Forged,
            "an offer's signature on an answer"
        );
        let swapped = "v=0\r\na=fingerprint:sha-256 EE:FF:02\r\n";
        assert_eq!(
            check_frame("key", &frame("offer", swapped, sm)),
            Signed::Forged,
            "another certificate under a captured signature"
        );
        let mut odd = frame("offer", sdp, None);
        odd["sm"] = serde_json::json!(7);
        assert_eq!(check_frame("key", &odd), Signed::Forged);
    }

    #[test]
    fn the_signature_matches_js() {
        // The same recipe in app.js (`signSdp`) gives this for this input.
        assert_eq!(
            sign_sdp(
                "Zx9_-abcDEF",
                "answer",
                "a=fingerprint:sha-256 74:1f:BF\r\n"
            )
            .as_deref(),
            Some("7c5351338af121686f74e259c1d7feb9515cfb32a8e515a363e9dec6b9b80edb")
        );
    }
}
