//! The short security code both screens show after a call connects.
//!
//! Same recipe as `computeSas()` in `app.js`: HMAC-SHA256 with the channel key
//! over `"tawny-sas-v1|" + fpA + "|" + fpB`, where the two DTLS fingerprints are
//! normalised and sorted, then the first 6 bytes as Crockford base32, `XXX-XXX`.

use hmac::Mac;

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
    let remote = if let Some(r) = remote {
        r.to_owned()
    } else {
        let mut seen: Vec<String> = remote_sdp
            .lines()
            .filter_map(|l| l.strip_prefix("a=fingerprint:"))
            .filter_map(|rest| {
                let mut it = rest.split(' ');
                let alg = it.next()?;
                let fp = it.next()?;
                let hexish =
                    !fp.is_empty() && fp.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
                hexish.then(|| normalise(alg, fp))
            })
            .collect();
        seen.sort();
        seen.dedup();
        if seen.len() != 1 {
            return None;
        }
        seen.pop()?
    };
    let mut pair = [local.to_owned(), remote];
    pair.sort();
    Some(pair)
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
}
