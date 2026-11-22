//! Reads the old Desktop's storage in, once: the channels, this computer's
//! job on each, its keys and pairings, and the settings, so an update keeps
//! every pairing.
//!
//! The old Desktop kept them in Chromium's `localStorage` (a `LevelDB` inside
//! its CEF profile) and its relays in `settings.json` beside it. The `LevelDB`
//! is copied to a scratch folder and read there: the old files are never
//! written, and nothing leaves this computer. The rules are Android's
//! (`JsMigration.import`), plus the channels themselves and the settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::store::{Channel, Role, Store};

/// What was read, as counts (never the secrets themselves).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Channels added.
    pub channels: usize,
    /// Keys, bonds, gates and checks carried over.
    pub entries: usize,
    /// Where it came from.
    pub from: Option<PathBuf>,
}

/// The old profiles' `LevelDB`s on this computer, newest first.
#[must_use]
pub fn sources() -> Vec<PathBuf> {
    let Ok(base) = crate::store::data_local() else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for app in [crate::store::APP_ID, "tawny"] {
        let profile = base.join(app).join("profile");
        let Ok(dirs) = std::fs::read_dir(&profile) else {
            continue;
        };
        for d in dirs.flatten() {
            let db = d.path().join("Local Storage").join("leveldb");
            if db.join("CURRENT").is_file() {
                let when = std::fs::metadata(&db)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                found.push((when, db));
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Imports from the first old profile that has channels, once. Marks the
/// store migrated whatever it found, so this runs a single time.
///
/// # Errors
/// When the store can't be saved.
pub fn run(store: &mut Store) -> Result<Report> {
    if store.data.settings.migrated {
        return Ok(Report::default());
    }
    let mut report = Report::default();
    for db in sources() {
        let Ok(items) = read_local_storage(&db) else {
            continue;
        };
        if !items.contains_key("tawny.channels.v1") {
            continue;
        }
        report = import(&items, store);
        report.from = Some(db.clone());
        if let Some(settings) = db.ancestors().nth(4).map(|app| app.join("settings.json")) {
            import_relays(&settings, store);
        }
        break;
    }
    store.data.settings.migrated = true;
    store.save()?;
    Ok(report)
}

/// Every `tawny.*` item in a Chromium `localStorage` `LevelDB`, by name.
///
/// # Errors
/// When the copy or the database can't be read.
pub fn read_local_storage(db: &Path) -> Result<BTreeMap<String, String>> {
    let scratch = std::env::temp_dir().join(format!("tawny-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    for f in std::fs::read_dir(db)?.flatten() {
        let name = f.file_name();
        if name == "LOCK" || !f.path().is_file() {
            continue;
        }
        std::fs::copy(f.path(), scratch.join(&name))
            .with_context(|| format!("copying {}", f.path().display()))?;
    }
    let opts = rusty_leveldb::Options {
        create_if_missing: false,
        ..rusty_leveldb::Options::default()
    };
    let out = (|| -> Result<BTreeMap<String, String>> {
        let mut ldb = rusty_leveldb::DB::open(&scratch, opts).context("opening the old storage")?;
        let mut it = ldb.new_iter().context("reading the old storage")?;
        let mut items = BTreeMap::new();
        while let Some((k, v)) = rusty_leveldb::LdbIterator::next(&mut it) {
            if let (Some(name), Some(value)) = (item_name(&k), item_value(&v))
                && name.starts_with("tawny.")
            {
                items.insert(name, value);
            }
        }
        Ok(items)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    out
}

/// A `localStorage` record's item name: keys are `_<origin>\0\x01<name>`.
fn item_name(key: &[u8]) -> Option<String> {
    let rest = key.strip_prefix(b"_")?;
    let at = rest.windows(2).position(|w| w == b"\x00\x01")?;
    let origin = std::str::from_utf8(&rest[..at]).ok()?;
    // Only the old Desktop's own page (it served itself on 127.0.0.1).
    if !origin.starts_with("http://127.0.0.1") && !origin.starts_with("http://localhost") {
        return None;
    }
    Some(rest[at + 2..].iter().map(|&b| char::from(b)).collect())
}

/// A record's value: a type byte, then Latin-1 (1) or UTF-16LE (0).
fn item_value(v: &[u8]) -> Option<String> {
    let (kind, body) = v.split_first()?;
    match kind {
        1 => Some(body.iter().map(|&b| char::from(b)).collect()),
        0 => {
            let (pairs, _) = body.as_chunks::<2>();
            let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
            String::from_utf16(&units).ok()
        }
        _ => None,
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The relay room id the web app filed per-room entries under (core
/// `ids::room_id`).
fn room_id(key: &str) -> String {
    let d = Sha256::digest(format!("tawny-room-v1|{key}").as_bytes());
    d.iter()
        .take(16)
        .fold(String::with_capacity(32), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn hex_list(json: Option<&String>, len: usize) -> Vec<String> {
    json.and_then(|j| serde_json::from_str::<Vec<Value>>(j).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .filter(|s| is_hex(s, len))
        .collect()
}

/// Brings the items into the store.
pub fn import(items: &BTreeMap<String, String>, store: &mut Store) -> Report {
    let mut r = Report::default();
    let list: Vec<Value> = items
        .get("tawny.channels.v1")
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    for ch in &list {
        let (Some(id), Some(key)) = (
            ch.get("id").and_then(Value::as_str),
            ch.get("key").and_then(Value::as_str),
        ) else {
            continue;
        };
        if key.len() < 16 || store.data.channels.iter().any(|c| c.key == key) {
            continue;
        }
        let name = ch.get("name").and_then(Value::as_str).unwrap_or_default();
        let role = match items.get(&format!("tawny.role.{id}")).map(String::as_str) {
            Some("station") => Some(Role::Station),
            Some("viewer") => Some(Role::Viewer),
            _ => None,
        };
        let added = store.add_paired(name, key);
        if let Some(role) = role {
            store.set_role(&added.id, role);
        }
        r.channels += 1;
        r.entries += import_keys(items, store, id, &added);
    }
    let s = &mut store.data.settings;
    if let Some(t) = items
        .get("tawny.theme")
        .filter(|t| ["system", "light", "dark"].contains(&t.as_str()))
    {
        s.theme.clone_from(t);
    }
    if let Some(n) = items.get("tawny.myName") {
        s.my_name = crate::store::clip(n.trim(), crate::store::MY_NAME_MAX);
    }
    for role in ["station", "viewer"] {
        if items.contains_key(&format!("tawny.coach.{role}.v1"))
            && !s.coach_seen.iter().any(|r| r == role)
        {
            s.coach_seen.push(role.into());
        }
    }
    if r.channels > 0 || items.contains_key("tawny.onboarded.v1") {
        s.imported = true;
    }
    r
}

/// One channel's keys and pairings; how many were carried over.
fn import_keys(
    items: &BTreeMap<String, String>,
    store: &mut Store,
    id: &str,
    ch: &Channel,
) -> usize {
    let room = room_id(&ch.key);
    let get = |k: String| items.get(&k).map(|v| v.trim().to_lowercase());
    let mut n = 0;
    let k = store.keys_mut(&ch.key);
    if let Some(v) = get(format!("tawny.bond.{id}")).filter(|v| is_hex(v, 32)) {
        k.bond = Some(v);
        n += 1;
    }
    if let Some(v) = get(format!("tawny.nostrStationKey.{id}")).filter(|v| is_hex(v, 64)) {
        k.station_nostr = Some(v);
        n += 1;
    }
    if let Some(v) = get(format!("tawny.nostrKey.{room}")).filter(|v| is_hex(v, 64)) {
        k.viewer_nostr = Some(v);
        n += 1;
    }
    if let Some(v) = get(format!("tawny.nostrStation.{id}")).filter(|v| is_hex(v, 64)) {
        k.monitor_nostr = Some(v);
        n += 1;
    }
    // The rendezvous ticket (`t` in the link): base64url, as the web app makes.
    if let Some(t) = items
        .get(&format!("tawny.token.{id}"))
        .map(|t| t.trim().to_owned())
        && (16..=64).contains(&t.len())
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        k.ticket = Some(t);
        n += 1;
    }
    let paired = hex_list(items.get(&format!("tawny.paired.{id}")), 32);
    if !paired.is_empty() {
        k.gate = serde_json::to_string(&serde_json::json!({ "code": null, "known": paired })).ok();
        n += 1;
    }
    let peers = hex_list(items.get(&format!("tawny.nostrPeers.{room}")), 64);
    if !peers.is_empty() {
        k.known = peers;
        n += 1;
    }
    if items.contains_key(&format!("tawny.sasok.{id}")) {
        k.sas_ok = true;
        n += 1;
    }
    let prefix = format!("tawny.sasok.{id}.");
    for name in items.keys().filter(|name| name.starts_with(&prefix)) {
        let pid = name[prefix.len()..].to_lowercase();
        if is_hex(&pid, 32) && !k.viewer_sas_ok.contains(&pid) {
            k.viewer_sas_ok.push(pid);
            n += 1;
        }
    }
    n
}

/// The old Desktop's relays (`settings.json`: `nostrRelays`, `nostrOnly`).
fn import_relays(path: &Path, store: &mut Store) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let relays: Vec<String> = v
        .get("nostrRelays")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|r| {
                    (r.starts_with("wss://") || r.starts_with("ws://"))
                        && !r.contains(char::is_whitespace)
                })
                .take(5)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if !relays.is_empty() && store.data.settings.relays.is_empty() {
        store.data.settings.relays = relays;
        store.data.settings.relays_only = v.get("nostrOnly").and_then(Value::as_bool) == Some(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(origin: &str, name: &str) -> Vec<u8> {
        let mut k = format!("_{origin}").into_bytes();
        k.extend_from_slice(b"\x00\x01");
        k.extend_from_slice(name.as_bytes());
        k
    }

    #[test]
    fn records_decode_as_chromium_writes_them() {
        assert_eq!(
            item_name(&key("http://127.0.0.1:41817", "tawny.theme")).as_deref(),
            Some("tawny.theme")
        );
        assert_eq!(item_name(&key("https://evil.example", "tawny.theme")), None);
        assert_eq!(item_name(b"META:http://127.0.0.1"), None);
        assert_eq!(item_value(b"\x01dark").as_deref(), Some("dark"));
        let utf16: Vec<u8> = std::iter::once(0)
            .chain("Café".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(item_value(&utf16).as_deref(), Some("Café"));
    }

    #[test]
    fn room_ids_match_the_core() {
        // As JsMigration.roomId / core ids::room_id: 32 hex of a SHA-256.
        let r = room_id("L_F7wC98BnTz78KPFl-bRg");
        assert_eq!(r.len(), 32);
        assert!(is_hex(&r, 32));
    }

    #[test]
    fn an_old_desktop_comes_across_whole() {
        let dir = std::env::temp_dir().join(format!("tawny-mig-{}", std::process::id()));
        let mut store = Store::open(&dir.join("store.json")).expect("opens");
        let ch_key = "L_F7wC98BnTz78KPFl-bRg";
        let room = room_id(ch_key);
        let mut items = BTreeMap::new();
        let mut put = |k: &str, v: &str| {
            items.insert(k.to_owned(), v.to_owned());
        };
        put(
            "tawny.channels.v1",
            &format!(r#"[{{"id":"c1","name":"Biscuit","key":"{ch_key}"}}]"#),
        );
        put("tawny.role.c1", "station");
        put("tawny.nostrStationKey.c1", &"ab".repeat(32));
        put(&format!("tawny.nostrKey.{room}"), &"cd".repeat(32));
        put(
            "tawny.paired.c1",
            &format!(r#"["{}","bad"]"#, "ef".repeat(16)),
        );
        put(&format!("tawny.sasok.c1.{}", "12".repeat(16)), "1");
        put("tawny.theme", "dark");
        put("tawny.myName", "Sam");
        put("tawny.coach.station.v1", "1");
        let r = import(&items, &mut store);
        assert_eq!(r.channels, 1);
        assert_eq!(r.entries, 4);
        let ch = &store.data.channels[0];
        assert_eq!(
            (ch.name.as_str(), ch.key.as_str(), ch.role),
            ("Biscuit", ch_key, Some(Role::Station))
        );
        let k = &store.data.keys[ch_key];
        assert_eq!(k.station_nostr.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(k.viewer_nostr.as_deref(), Some("cd".repeat(32).as_str()));
        assert!(
            k.gate
                .as_deref()
                .is_some_and(|g| g.contains(&"ef".repeat(16)) && !g.contains("bad"))
        );
        assert_eq!(k.viewer_sas_ok, vec!["12".repeat(16)]);
        assert_eq!(store.data.settings.theme, "dark");
        assert_eq!(store.data.settings.my_name, "Sam");
        assert_eq!(store.data.settings.coach_seen, vec!["station".to_owned()]);
        assert!(store.data.settings.imported);
        // A second import adds nothing.
        assert_eq!(import(&items, &mut store).channels, 0);
    }
}
