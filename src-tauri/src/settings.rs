//! What the user changed on the Servers panel, kept next to the Chromium
//! profile as `settings.json`: their own Nostr relays (an empty list means
//! "use the build's"), and whether those relays are the only way to pair
//! (`nostrOnly`: no rendezvous behind them).
//!
//! Read once at startup and rewritten on every save (atomically: a temporary
//! file renamed over the old one, so a crash mid-write leaves the old file).
//! A missing or unreadable file is the same as no settings at all.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{MAX_RELAYS, relay_url_ok};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
  #[serde(default)]
  pub nostr_relays: Vec<String>,
  /// "Use only my relays": no rendezvous behind them. Never true with an
  /// empty list, so it can not leave the app with nothing to dial. The page
  /// tests the relays before it asks for this (web/app.js, checkOnlyMode).
  #[serde(default)]
  pub nostr_only: bool,
}

/// Why a list from the page was refused, in words the page can show.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
  NotARelay(String),
  TooMany,
}

impl std::fmt::Display for Invalid {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Invalid::NotARelay(u) => write!(f, "not a relay address: {u}"),
      Invalid::TooMany => write!(f, "at most {MAX_RELAYS} relays"),
    }
  }
}

impl Settings {
  /// Trimmed, de-duplicated, and every entry a URL the CSP can safely name.
  pub fn validated(relays: Vec<String>, only: bool) -> Result<Self, Invalid> {
    let mut out: Vec<String> = Vec::new();
    for u in relays.into_iter().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) {
      if !relay_url_ok(&u) {
        return Err(Invalid::NotARelay(u));
      }
      if !out.contains(&u) {
        out.push(u);
      }
    }
    if out.len() > MAX_RELAYS {
      return Err(Invalid::TooMany);
    }
    let nostr_only = only && !out.is_empty();
    Ok(Self { nostr_relays: out, nostr_only })
  }

  /// The saved settings, or the defaults if there are none (or they do not parse).
  pub fn load(path: &Path) -> Self {
    let Ok(text) = std::fs::read_to_string(path) else {
      return Self::default();
    };
    match serde_json::from_str::<Self>(&text) {
      // Re-validated: the file is the user's to edit, and whatever is in it
      // ends up in a CSP header.
      Ok(s) => Self::validated(s.nostr_relays, s.nostr_only).unwrap_or_else(|e| {
        log::warn!("settings.json ignored: {e}");
        Self::default()
      }),
      Err(e) => {
        log::warn!("settings.json unreadable, ignored: {e}");
        Self::default()
      }
    }
  }

  pub fn save(&self, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let tmp: PathBuf = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validated_trims_dedupes_and_refuses() {
    let s = Settings::validated(vec![" wss://a.example ".into(), "wss://a.example".into(), String::new()], false);
    assert_eq!(s, Ok(Settings { nostr_relays: vec!["wss://a.example".into()], nostr_only: false }));
    assert_eq!(
      Settings::validated(vec!["http://a.example".into()], false),
      Err(Invalid::NotARelay("http://a.example".into()))
    );
    let six = (0..6).map(|i| format!("wss://r{i}.example")).collect();
    assert_eq!(Settings::validated(six, false), Err(Invalid::TooMany));
  }

  #[test]
  fn only_my_relays_needs_a_relay_of_the_users_own() {
    let on = Settings::validated(vec!["wss://a.example".into()], true).expect("valid");
    assert!(on.nostr_only);
    let empty = Settings::validated(Vec::new(), true).expect("valid");
    assert!(!empty.nostr_only, "nothing to dial is never 'only'");
  }

  #[test]
  fn save_then_load_and_bad_files_are_defaults() {
    let dir = std::env::temp_dir().join(format!("tawny-settings-test-{}", std::process::id()));
    let path = dir.join("settings.json");
    let s = Settings { nostr_relays: vec!["wss://relay.example.org".into()], nostr_only: true };
    s.save(&path).expect("saved");
    assert_eq!(Settings::load(&path), s);
    std::fs::write(&path, "not json").expect("written");
    assert_eq!(Settings::load(&path), Settings::default());
    std::fs::write(&path, r#"{"nostrRelays":["wss://x.example; script-src *"]}"#).expect("written");
    assert_eq!(Settings::load(&path), Settings::default(), "a hand-edited bad entry never reaches the CSP");
    std::fs::write(&path, r#"{"nostrRelays":[],"nostrOnly":true}"#).expect("written");
    assert_eq!(Settings::load(&path), Settings::default(), "'only' with no relays is dropped on load");
    assert_eq!(Settings::load(&dir.join("missing.json")), Settings::default());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
