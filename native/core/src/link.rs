//! Pairing links: reading them, and writing them.
//!
//! Two forms exist. The app's own is `tawny://pair?…`, parameters in the
//! query. A web Monitor hands out `https://<host>/#k=…&n=…&r=viewer&t=…&c=…&e=…`,
//! parameters in the *fragment*, because a fragment never reaches a server or
//! an access log, and the channel key must travel nowhere else. A query string
//! on an http(s) link is deliberately never read.
//!
//! The reading rules are a port of the Android shell's `PairLink.kt`, with its
//! tests (see `tests/link.rs`).

use std::net::Ipv4Addr;

use thiserror::Error;
use url::Url;

/// Stale past any plausible clock disagreement (`CLOCK_SKEW_MS` in `app.js`).
pub const CLOCK_SKEW_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_NAME: &str = "Pet camera";
const NAME_MAX: usize = 40;

/// What a pairing link says, once it has been read and vetted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// `ws://<lan-ip>:<port>` from `h=`, or `None` for a relay-only pairing.
    pub signal: Option<String>,
    /// The channel key from `k`.
    pub key: String,
    /// The room's name, cleaned for display.
    pub name: String,
    /// The relay admission ticket from `t`.
    pub token: Option<String>,
    /// The pairing code from `c`, presented to the Monitor to be let in.
    pub code: Option<String>,
    /// `e` in epoch milliseconds, or 0 when the link carried no deadline.
    pub expires_at: i64,
    /// `wss://host[:port]` the far end is on, when the link named one.
    pub relay: Option<String>,
    /// Whether [`Pairing::relay`] may be written into this device's settings
    /// without asking (see [`relay_adoptable`]).
    pub relay_trusted: bool,
    /// The Monitor's Nostr public key from `np`, 64 lower-case hex characters.
    pub nostr_station: Option<String>,
}

impl Pairing {
    /// Whether the code is stale on any plausible clock. Closer calls go to
    /// the Monitor, which refuses a lapsed code itself.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at > 0 && now_ms > self.expires_at.saturating_add(CLOCK_SKEW_MS)
    }
}

/// The relays this install already trusts: the user's own (Servers screen)
/// and the one the build was made with, each as `host[:port]`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Anchors<'a> {
    /// The relay the user set.
    pub preferred: Option<&'a str>,
    /// The relay this build was compiled with.
    pub build: Option<&'a str>,
    /// Whether this build has a rendezvous configured at all.
    pub has_rendezvous: bool,
}

/// Why a link was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinkError {
    /// Not a URL, or not a scheme a pairing link uses.
    #[error("not a pairing link")]
    NotALink,
    /// The channel key is missing or malformed.
    #[error("the link has no valid channel key")]
    BadKey,
    /// The ticket is malformed.
    #[error("the link's ticket is malformed")]
    BadTicket,
    /// The link names no LAN address, no relay and no Nostr key, and this
    /// build has no rendezvous.
    #[error("the link names nowhere to meet")]
    NowhereToMeet,
}

fn matches_b64(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Reads a pairing link.
///
/// # Errors
/// [`LinkError`] for anything that is not a usable pairing link.
pub fn parse(raw: &str, anchors: Anchors<'_>) -> Result<Pairing, LinkError> {
    let url = Url::parse(raw.trim()).map_err(|_| LinkError::NotALink)?;
    let scheme = url.scheme();
    let is_web = scheme == "https" || scheme == "http";
    let src = match (scheme, url.host_str()) {
        ("tawny", Some("pair")) => url.query().unwrap_or(""),
        _ if is_web => url
            .fragment()
            .filter(|f| !f.is_empty())
            .ok_or(LinkError::NotALink)?,
        _ => return Err(LinkError::NotALink),
    };
    let q = Params::parse(src);

    let key = q.get("k").ok_or(LinkError::BadKey)?;
    if !matches_b64(key, 16, 64) {
        return Err(LinkError::BadKey);
    }
    // A public IP in a pairing link is not something we dial.
    let h = q.get("h").filter(|h| is_private_host_port(h));
    let token = q.get("t");
    if token.is_some_and(|t| !matches_b64(t, 8, 64)) {
        return Err(LinkError::BadTicket);
    }

    // On a web link its own origin is the relay unless `rv=` names another,
    // and `rv=` counts only when it is the link's own host or a relay this
    // install already uses, so a forwarded link can't smuggle in a third host.
    let link_host = if is_web {
        url.host_str().filter(|h| !h.is_empty())
    } else {
        None
    };
    let mut relay_host_port = link_host.map(|h| {
        url.port()
            .map_or_else(|| h.to_owned(), |p| format!("{h}:{p}"))
    });
    if let Some(r) = q
        .get("rv")
        .filter(|rv| !rv.trim().is_empty())
        .and_then(split_relay)
    {
        let host = host_of(&r);
        if same_host(Some(host), link_host)
            || same_host(Some(host), anchors.preferred.map(host_of))
            || same_host(Some(host), anchors.build.map(host_of))
        {
            relay_host_port = Some(r);
        }
    }
    let relay = relay_host_port.as_ref().map(|hp| format!("wss://{hp}"));

    let nostr_station = q.get("np").map(str::to_lowercase).filter(|np| {
        np.len() == 64
            && np
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });

    // A Monitor's Nostr key is somewhere to meet too (the 1.0.5 Kotlin parser
    // missed this, and refused a Nostr-only link in a build with no rendezvous).
    if h.is_none() && relay.is_none() && nostr_station.is_none() && !anchors.has_rendezvous {
        return Err(LinkError::NowhereToMeet);
    }
    let relay_trusted = relay.is_some()
        && relay_adoptable(relay_host_port.as_deref(), anchors.preferred, anchors.build);

    let name: String = q
        .get("n")
        .map(clean_name)
        .filter(|n| !n.trim().is_empty())
        .map_or_else(
            || DEFAULT_NAME.to_owned(),
            |n| n.chars().take(NAME_MAX).collect(),
        );
    let code = q
        .get("c")
        .filter(|c| matches_b64(c, 8, 32))
        .map(str::to_owned);
    let expires_at = q
        .get("e")
        .and_then(|e| e.parse::<i64>().ok())
        .filter(|e| *e > 0)
        .map_or(0, |e| e.saturating_mul(1000));

    Ok(Pairing {
        signal: h.map(|h| format!("ws://{h}")),
        key: key.to_owned(),
        name,
        token: token.map(str::to_owned),
        code,
        expires_at,
        relay,
        relay_trusted,
        nostr_station,
    })
}

/// Drops control characters, line/paragraph separators, and bidi overrides
/// and isolates: the name is shown in a "Connect to …?" dialog, and those can
/// make it read as something other than what it is.
fn clean_name(raw: &str) -> String {
    raw.chars()
        .filter(|&c| {
            let cp = u32::from(c);
            !(c.is_control()
                || c == '\u{2028}'
                || c == '\u{2029}'
                || (0x202A..=0x202E).contains(&cp)
                || (0x2066..=0x2069).contains(&cp))
        })
        .collect()
}

/// May a relay named by a link be written into this device's settings
/// without anyone being asked?
///
/// The anchors: a name under `*.ts.net` (resolves only for members of that
/// tailnet), a private IPv4 address (cannot reach past the house), or exactly
/// the relay this install already uses, port included.
#[must_use]
pub fn relay_adoptable(
    host_port: Option<&str>,
    preferred: Option<&str>,
    build: Option<&str>,
) -> bool {
    let Some(hp) = host_port else { return false };
    let host = host_of(hp).to_lowercase();
    if host.is_empty() {
        return false;
    }
    if host.ends_with(".ts.net") || is_private_ipv4(&host) {
        return true;
    }
    same_endpoint(hp, preferred) || same_endpoint(hp, build)
}

fn host_of(host_port: &str) -> &str {
    host_port.split(':').next().unwrap_or("")
}

fn same_host(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if !a.trim().is_empty() && a.eq_ignore_ascii_case(b))
}

/// Same host and same port; an absent port means 443, the default for the
/// secure transports a relay is named with.
fn same_endpoint(a: &str, b: Option<&str>) -> bool {
    let Some(b) = b.filter(|b| !b.trim().is_empty()) else {
        return false;
    };
    let (ah, bh) = (host_of(a), host_of(b));
    !ah.trim().is_empty() && ah.eq_ignore_ascii_case(bh) && port_of(a) == port_of(b)
}

fn port_of(host_port: &str) -> u16 {
    host_port
        .split_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(443)
}

/// `https://h[:p]` or `wss://h[:p]` from an `rv=`, reduced to `h[:p]`. No
/// userinfo: `wss://good.ts.net@evil.example` is evil.example wearing a
/// trusted name.
fn split_relay(rv: &str) -> Option<String> {
    let (scheme, rest) = rv.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss")) {
        return None;
    }
    let hp = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!hp.trim().is_empty() && !hp.contains('@')).then(|| hp.to_owned())
}

/// `a.b.c.d:port` on a private network (RFC 1918 or link-local).
#[must_use]
pub fn is_private_host_port(host_port: &str) -> bool {
    let Some((ip, port)) = host_port.rsplit_once(':') else {
        return false;
    };
    let dotted = ip.split('.').count() == 4
        && ip
            .split('.')
            .all(|o| (1..=3).contains(&o.len()) && o.bytes().all(|b| b.is_ascii_digit()));
    let port_ok = (2..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit());
    dotted && port_ok && is_private_ipv4(ip)
}

fn is_private_ipv4(ip: &str) -> bool {
    let mut octets = [0u8; 4];
    let mut parts = ip.split('.');
    for slot in &mut octets {
        let Some(Ok(v)) = parts.next().map(str::parse::<u8>) else {
            return false;
        };
        *slot = v;
    }
    if parts.next().is_some() {
        return false;
    }
    let a = Ipv4Addr::from(octets);
    a.is_private() || a.is_link_local()
}

/// `a=1&b=2`, first occurrence winning, `+` as a space, a stray `%` kept as
/// itself (what `Uri.getQueryParameter` and `URLSearchParams` do).
struct Params(Vec<(String, String)>);

impl Params {
    fn parse(src: &str) -> Self {
        let mut out: Vec<(String, String)> = Vec::new();
        for part in src.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            let name = decode(name);
            if name.is_empty() || out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            out.push((name, decode(value)));
        }
        Self(out)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    // A stray or truncated `%` is kept as itself.
                    out.push(b'%');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// What a Monitor puts in its QR.
#[derive(Debug, Clone, Default)]
pub struct LinkParts<'a> {
    /// The channel key.
    pub key: &'a str,
    /// The room's name.
    pub name: &'a str,
    /// The relay admission ticket.
    pub token: Option<&'a str>,
    /// The current pairing code.
    pub code: &'a str,
    /// When the code lapses, epoch seconds.
    pub expires_s: i64,
    /// The Monitor's Nostr public key.
    pub nostr_station: Option<&'a str>,
    /// `ip:port` of the Monitor's LAN relay (app links only).
    pub lan: Option<&'a str>,
}

/// The app form, `tawny://pair?k=…&n=…[&h=…][&t=…]&c=…&e=…[&np=…]`, in the
/// order `pairingPayload()` writes it.
#[must_use]
pub fn build_app_link(p: &LinkParts<'_>) -> String {
    let mut fields = vec![format!("k={}", enc(p.key)), format!("n={}", enc(p.name))];
    if let Some(h) = p.lan {
        fields.push(format!("h={h}"));
    }
    if let Some(t) = p.token {
        fields.push(format!("t={}", enc(t)));
    }
    fields.push(format!("c={}", enc(p.code)));
    fields.push(format!("e={}", p.expires_s));
    if let Some(np) = p.nostr_station {
        fields.push(format!("np={np}"));
    }
    format!("tawny://pair?{}", fields.join("&"))
}

/// The web form, `<base>#k=…&n=…&r=viewer[&t=…]&c=…&e=…[&np=…]`, as
/// `pairLink()` writes it (form-encoded, so a space is `+`).
#[must_use]
pub fn build_web_link(base: &str, p: &LinkParts<'_>) -> String {
    let mut f = url::form_urlencoded::Serializer::new(String::new());
    f.append_pair("k", p.key)
        .append_pair("n", p.name)
        .append_pair("r", "viewer");
    if let Some(t) = p.token {
        f.append_pair("t", t);
    }
    f.append_pair("c", p.code)
        .append_pair("e", &p.expires_s.to_string());
    if let Some(np) = p.nostr_station {
        f.append_pair("np", np);
    }
    format!("{base}#{}", f.finish())
}

/// `Uri.encode`: everything but unreserved characters percent-encoded.
fn enc(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~!*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 15)]));
        }
    }
    out
}
