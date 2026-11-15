//! What the page is told in `config.json`, and the CSP that follows from it.
//!
//! Fixed at build time, the way Android's `tawny.*` gradle properties are:
//!
//! ```text
//! TAWNY_RENDEZVOUS_URL=wss://…      the rendezvous both ends meet on off Wi-Fi
//! TAWNY_STUN_URLS=stun:…,stun:…     comma-separated; `off` for none
//! TAWNY_TURN_MODE=auto|always       auto = relay only when P2P fails
//! TAWNY_NOSTR_RELAYS=wss://…,…      Nostr relays to meet on; empty for none
//! TAWNY_NOSTR_MODE=prefer|only|off  prefer (default) = Nostr first, rendezvous as backup
//! TAWNY_NOSTR_MIRROR=http://…       DevOps inspector that gets each opened layer
//! ```
//!
//! Unset, a build gets the same servers as the Play Store app. Set
//! `TAWNY_RENDEZVOUS_URL=` (empty) for a build that meets only on its own origin.

use serde_json::json;

const DEFAULT_RENDEZVOUS: &str = "wss://tawny-rendezvous.tawny1.workers.dev";
const DEFAULT_STUN: &str = "stun:stun.cloudflare.com:3478,stun:stun.l.google.com:19302";
/// Public relays checked on 2026-11-13 to carry Tawny's ephemeral gift wraps
/// (kind 21059) round trip. Same list as Android's `tawny.nostrRelays`.
const DEFAULT_NOSTR: &str = "wss://relay.primal.net,wss://relay.nostr.net,wss://relay.snort.social";
/// How many relays a list may name, from the build or from the user.
pub const MAX_RELAYS: usize = 5;
/// Hosts "Tip in Bitcoin" talks to: the Lightning Address's LNURL service, the
/// BTC price for showing dollar amounts, and the relays the zap receipt goes to.
#[rustfmt::skip]
const TIP_CONNECT_SRC: [&str; 6] = [
  "https://strike.me", "https://api.coinbase.com",
  "wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net", "wss://relay.snort.social",
];

/// The update board's text for this release: `{ version, notes[], changelog }`.
/// Only sent when its `version` is this build's, so a version bump that forgot
/// to rewrite it shows no board rather than last release's notes.
const WHATSNEW: &str = include_str!("../whatsnew.json");

/// Whether a Chromium profile from an earlier run was already on disk at
/// startup. Set once from main(); see [`release`].
static RAN_BEFORE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_ran_before(v: bool) {
  RAN_BEFORE.store(v, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone)]
pub struct Config {
  pub rendezvous: String,
  pub stun: Vec<String>,
  pub turn_mode: String,
  pub nostr: Option<Nostr>,
}

/// Signaling over Nostr (web/nostr/transport.js): the relays both ends meet on,
/// with the rendezvous as the backup. The user's own list (Servers, see
/// settings.rs) replaces the build's.
#[derive(Clone)]
pub struct Nostr {
  pub relays: Vec<String>,
  pub mode: String,
  pub mirror: String,
}

impl Nostr {
  fn from_build() -> Option<Self> {
    let relays: Vec<String> = list(option_env!("TAWNY_NOSTR_RELAYS").unwrap_or(DEFAULT_NOSTR))
      .into_iter()
      .filter(|u| relay_url_ok(u))
      .take(MAX_RELAYS)
      .collect();
    let mode = match option_env!("TAWNY_NOSTR_MODE").unwrap_or("prefer").trim() {
      "only" => "only",
      "off" => return None,
      _ => "prefer",
    };
    if relays.is_empty() {
      return None;
    }
    let mirror = option_env!("TAWNY_NOSTR_MIRROR").unwrap_or("").trim();
    let mirror = if (mirror.starts_with("http://") || mirror.starts_with("https://")) && host_ok(bare_host(mirror)) {
      mirror.to_string()
    } else {
      String::new()
    };
    Some(Self { relays, mode: mode.to_string(), mirror })
  }
}

/// `ws://` or `wss://`, then a plain `host[:port]`, then an optional path: the
/// only shape that may go into config.json and, from there, the CSP.
pub fn relay_url_ok(u: &str) -> bool {
  (u.starts_with("ws://") || u.starts_with("wss://"))
    && host_ok(bare_host(u))
    && !u.chars().any(|c| c.is_whitespace() || c == '"' || c == ';' || c == ',')
}

impl Config {
  /// This build's config with the user's own relays (if any) in place of the
  /// build's. An empty list means "use the build's". `only` is the user's
  /// "use only my relays": the page then dials no rendezvous at all.
  pub fn with_user_relays(&self, user: &[String], only: bool) -> Self {
    let mut out = self.clone();
    if !user.is_empty() {
      let mode = if only {
        "only".to_string()
      } else {
        self.nostr.as_ref().map_or_else(|| "prefer".to_string(), |n| n.mode.clone())
      };
      let mirror = self.nostr.as_ref().map(|n| n.mirror.clone()).unwrap_or_default();
      out.nostr = Some(Nostr { relays: user.iter().take(MAX_RELAYS).cloned().collect(), mode, mirror });
    }
    out
  }

  /// The build's own relays, which the Servers panel shows as the default.
  pub fn build_relays(&self) -> Vec<String> {
    self.nostr.as_ref().map(|n| n.relays.clone()).unwrap_or_default()
  }

  pub fn from_build() -> Self {
    let rendezvous = option_env!("TAWNY_RENDEZVOUS_URL").unwrap_or(DEFAULT_RENDEZVOUS).trim().to_string();
    let stun = match option_env!("TAWNY_STUN_URLS").unwrap_or(DEFAULT_STUN).trim() {
      "off" => Vec::new(),
      s => list(s),
    };
    let turn_mode = match option_env!("TAWNY_TURN_MODE").unwrap_or("auto").trim() {
      "always" => "always",
      _ => "auto",
    }
    .to_string();
    Self { rendezvous, stun, turn_mode, nostr: Nostr::from_build() }
  }

  /// The body of `/config.json`, in the shape Android's build writes.
  pub fn json(&self) -> String {
    let mut v = json!({
      "stun": self.stun,
      "rendezvous": self.rendezvous,
      "turnMode": self.turn_mode,
      "authRequired": false,
      "release": release(WHATSNEW, env!("CARGO_PKG_VERSION"), RAN_BEFORE.load(std::sync::atomic::Ordering::Relaxed)),
      // This host keeps settings (GET/PUT /settings): the page shows Servers.
      "settings": "local",
    });
    if let Some(n) = &self.nostr {
      v["nostr"] = json!({ "relays": n.relays, "mode": n.mode, "mirror": n.mirror });
    }
    v.to_string()
  }

  /// `connect-src`: this origin, plus the one rendezvous the page dials, plus
  /// the tip sources ([TIP_CONNECT_SRC]).
  ///
  /// Same reasoning as `CONNECT_SRC` in Tawny Docker's server.js: the directive
  /// exists so a script that got into the page cannot post the channel key to an
  /// arbitrary host, so it stays a list and never grows a scheme-wide source.
  pub fn connect_src(&self) -> String {
    let mut out = vec!["'self'".to_string()];
    let rv = bare_host(&self.rendezvous);
    if host_ok(rv) {
      // A plain ws:// rendezvous (a LAN or test server) is dialled as ws:// and
      // http://; a wss:// one would never match those, and the reverse.
      let (ws, http) = if self.rendezvous.starts_with("ws://") { ("ws", "http") } else { ("wss", "https") };
      out.push(format!("{ws}://{rv}"));
      out.push(format!("{http}://{rv}"));
    }
    // Nostr relays (and the DevOps mirror), named exactly like the rendezvous.
    if let Some(n) = &self.nostr {
      for u in n.relays.iter().chain(std::iter::once(&n.mirror)) {
        let h = bare_host(u);
        if u.is_empty() || !host_ok(h) {
          continue;
        }
        let (ws, http) =
          if u.starts_with("ws://") || u.starts_with("http://") { ("ws", "http") } else { ("wss", "https") };
        out.push(format!("{ws}://{h}"));
        out.push(format!("{http}://{h}"));
      }
    }
    // Belt and braces only — Chromium does not gate ICE servers on connect-src.
    for u in &self.stun {
      let h = bare_host(u);
      if host_ok(h) {
        out.push(format!("stun://{h}"));
        out.push(format!("turn://{h}"));
        out.push(format!("turns://{h}"));
      }
    }
    // "Tip in Bitcoin" (web/tip.js): the tip service, the dollar price, and the
    // Nostr relays its paid receipt is published to. Named one by one; the
    // same list as Tawny Docker's server.js and the Android LocalWeb.kt.
    out.extend(TIP_CONNECT_SRC.iter().map(|s| s.to_string()));
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    out.join(" ")
  }

  pub fn csp(&self) -> String {
    [
      "default-src 'none'",
      "script-src 'self'",
      "style-src 'self'",
      "img-src 'self' data: blob:",
      "media-src 'self' blob:",
      "font-src 'self'",
      &format!("connect-src {}", self.connect_src()),
      "manifest-src 'self'",
      "base-uri 'none'",
      "form-action 'none'",
      "frame-ancestors 'none'",
    ]
    .join("; ")
  }
}

fn list(v: &str) -> Vec<String> {
  v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// `wss://host:port/path` -> `host:port`; `stun:host:3478` -> `host:3478`.
fn bare_host(u: &str) -> &str {
  let rest = match u.find(':') {
    Some(i) if u[..i].chars().all(|c| c.is_ascii_alphabetic()) => &u[i + 1..],
    _ => u,
  };
  let rest = rest.trim_start_matches("//");
  rest.split(['/', '?']).next().unwrap_or("")
}

/// A value that is not plainly `hostname[:port]` could truncate the policy.
fn host_ok(h: &str) -> bool {
  let (name, port) = match h.rsplit_once(':') {
    Some((n, p)) => (n, Some(p)),
    None => (h, None),
  };
  !name.is_empty()
    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    && port.is_none_or(|p| (1..=5).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit()))
}

/// `whatsnew.json`, when it describes `version`; otherwise null (no board).
/// `upgraded` tells the page this is not a first install even when nothing
/// was ever set up, so the board still shows on the welcome screen.
fn release(whatsnew: &str, version: &str, upgraded: bool) -> serde_json::Value {
  match serde_json::from_str::<serde_json::Value>(whatsnew) {
    Ok(mut v) if v["version"] == version => {
      v["upgraded"] = upgraded.into();
      v
    }
    _ => serde_json::Value::Null,
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn release_only_for_its_own_version() {
    let w = r#"{"version":"1.2.3","notes":["a"],"changelog":"https://x"}"#;
    assert_eq!(release(w, "1.2.3", false)["notes"][0], "a");
    assert_eq!(release(w, "1.2.3", true)["upgraded"], true);
    assert!(release(w, "1.2.4", true).is_null());
    assert!(release("not json", "1.2.3", true).is_null());
  }

  use super::*;

  #[test]
  fn hosts() {
    assert_eq!(bare_host("wss://a.example.dev/ws?x"), "a.example.dev");
    assert_eq!(bare_host("stun:stun.l.google.com:19302"), "stun.l.google.com:19302");
    assert!(host_ok("stun.l.google.com:19302"));
    assert!(!host_ok("evil.example; script-src *"));
    assert!(!host_ok(""));
  }

  #[test]
  fn connect_src_lists_rendezvous() {
    let c = Config {
      rendezvous: "wss://rv.example.dev".into(),
      stun: vec!["stun:s.example:3478".into()],
      turn_mode: "auto".into(),
      nostr: None,
    };
    let s = c.connect_src();
    assert!(s.starts_with("'self' wss://rv.example.dev https://rv.example.dev"));
    assert!(s.contains("stun://s.example:3478"));
  }

  #[test]
  fn connect_src_plain_ws_rendezvous() {
    let c = Config { rendezvous: "ws://192.168.1.5:8080".into(), stun: vec![], turn_mode: "auto".into(), nostr: None };
    assert_eq!(
      c.connect_src(),
      format!("'self' ws://192.168.1.5:8080 http://192.168.1.5:8080 {}", TIP_CONNECT_SRC.join(" "))
    );
  }

  #[test]
  fn connect_src_no_repeats() {
    let c = Config {
      rendezvous: String::new(),
      stun: vec!["stun:a.example:3478".into(), "stun:b.example:3478".into(), "stun:a.example:3478".into()],
      turn_mode: "auto".into(),
      nostr: None,
    };
    assert_eq!(c.connect_src().matches("stun://a.example:3478").count(), 1);
  }

  #[test]
  fn nostr_relays_and_mirror_in_config_and_connect_src() {
    let c = Config {
      rendezvous: String::new(),
      stun: vec![],
      turn_mode: "auto".into(),
      nostr: Some(Nostr {
        relays: vec!["ws://127.0.0.1:7447/relay".into(), "wss://nos.lol".into()],
        mode: "only".into(),
        mirror: "http://127.0.0.1:7447/mirror".into(),
      }),
    };
    let s = c.connect_src();
    assert!(s.contains("ws://127.0.0.1:7447 http://127.0.0.1:7447"));
    assert!(s.contains("wss://nos.lol https://nos.lol"));
    assert_eq!(s.matches("ws://127.0.0.1:7447").count(), 1, "relay and mirror share a host: listed once");
    let j: serde_json::Value = serde_json::from_str(&c.json()).expect("config.json is JSON");
    assert_eq!(j["nostr"]["mode"], "only");
    assert_eq!(j["nostr"]["relays"][1], "wss://nos.lol");
  }

  #[test]
  fn user_relays_replace_the_build_and_reach_the_csp() {
    let build = Config {
      rendezvous: String::new(),
      stun: vec![],
      turn_mode: "auto".into(),
      // Not one of the tip relays (TIP_CONNECT_SRC), which are always in the CSP.
      nostr: Some(Nostr { relays: vec!["wss://build.example".into()], mode: "prefer".into(), mirror: String::new() }),
    };
    let mine = build.with_user_relays(&["wss://relay.example.org".into()], false);
    assert!(mine.connect_src().contains("wss://relay.example.org"));
    assert!(!mine.connect_src().contains("build.example"), "the build's relays are replaced, not added to");
    assert_eq!(build.with_user_relays(&[], false).build_relays(), vec!["wss://build.example".to_string()]);
    let only = build.with_user_relays(&["wss://relay.example.org".into()], true);
    assert!(only.json().contains(r#""mode":"only""#), "the page is told there is no rendezvous behind them");
    assert!(!build.with_user_relays(&[], true).json().contains(r#""mode":"only""#), "never 'only' on the build's list");
    let j: serde_json::Value = serde_json::from_str(&mine.json()).expect("config.json is JSON");
    assert_eq!(j["nostr"]["mode"], "prefer");
    assert_eq!(j["settings"], "local");
  }

  #[test]
  fn relay_urls_that_could_break_the_csp_are_refused() {
    assert!(relay_url_ok("wss://relay.example.org"));
    assert!(relay_url_ok("ws://192.168.1.5:7447/relay"));
    assert!(!relay_url_ok("https://relay.example.org"));
    assert!(!relay_url_ok("wss://evil.example; script-src *"));
    assert!(!relay_url_ok("wss://a.example b.example"));
    assert!(!relay_url_ok("wss://"));
  }

  #[test]
  fn no_nostr_block_without_relays() {
    let c = Config { rendezvous: String::new(), stun: vec![], turn_mode: "auto".into(), nostr: None };
    let j: serde_json::Value = serde_json::from_str(&c.json()).expect("config.json is JSON");
    assert!(j.get("nostr").is_none());
  }
}
