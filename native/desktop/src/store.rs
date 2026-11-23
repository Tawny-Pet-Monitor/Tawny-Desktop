//! What the Desktop keeps between runs: the channels, this device's job on
//! each, its keys and pairing state, and the user's settings.
//!
//! One JSON document in the app's data folder, the one the old Desktop used
//! (`$XDG_DATA_HOME/io.github.PressF4me.Tawny`, `%LOCALAPPDATA%\…` on
//! Windows, where the uninstaller's "delete app data" removes it), written
//! whole to a temporary file and renamed over the old one, so a crash
//! mid-write never leaves half a store. On Unix it is readable by the user
//! only. The same protection the old Desktop's browser storage had; the
//! secrets in it are per channel and per device.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// This device's job on a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The Monitor: stays with the pet, sends camera and sound.
    Station,
    /// The Viewer: watches.
    Viewer,
}

impl Role {
    /// The name the screens use ("station" / "viewer").
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Viewer => "viewer",
        }
    }
}

/// A channel as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// A local id, never shared.
    pub id: String,
    /// What the user called it.
    pub name: String,
    /// The channel key (base64url): it is what pairs devices.
    pub key: String,
    /// The job this device last did on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
}

/// A channel's secrets and pairing state on this device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    /// The Monitor's Nostr secret for this channel (hex).
    pub station_nostr: Option<String>,
    /// This Viewer's Nostr secret for this channel (hex).
    pub viewer_nostr: Option<String>,
    /// This Viewer's bond id with the Monitor (32 hex).
    pub bond: Option<String>,
    /// The Monitor's Nostr public key, as the pairing link named it.
    pub monitor_nostr: Option<String>,
    /// The Monitor's LAN relay (`ws://ip:port`), as the pairing link named it.
    pub lan: Option<String>,
    /// The rendezvous ticket the pairing link carried.
    pub ticket: Option<String>,
    /// The link's own rendezvous (`wss://…`), kept only when it is one a
    /// link may name without asking (a tailnet name, a private address, or
    /// the one already in use); otherwise Tawny's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<String>,
    /// The pairing code, kept until this Viewer is first let in.
    pub code: Option<String>,
    /// This Viewer checked the safety code once.
    pub sas_ok: bool,
    /// The Monitor checked the safety code with these Viewers (bond ids).
    pub viewer_sas_ok: Vec<String>,
    /// The Monitor's pairing gate, as the engine hands it over (JSON).
    pub gate: Option<String>,
    /// Viewer Nostr keys the Monitor has let in before.
    pub known: Vec<String>,
    /// The port this Monitor's LAN relay listened on last time. It is tried
    /// first on the next start, so the address in the phones' pairings
    /// still reaches it after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_port: Option<u16>,
}

/// The user's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "system", "light" or "dark".
    pub theme: String,
    /// Each Nostr relay's last verdict, so one that refused is skipped for a
    /// while instead of retried at every start (as on Android).
    #[serde(
        default,
        skip_serializing_if = "tawny_core::relay::health::HealthBook::is_empty"
    )]
    pub relay_health: tawny_core::relay::health::HealthBook,
    /// What the others see next to this device.
    pub my_name: String,
    /// The user's own Nostr relays; empty for the build's.
    pub relays: Vec<String>,
    /// Pair through the user's relays only.
    pub relays_only: bool,
    /// The roles whose walkthrough has been seen.
    pub coach_seen: Vec<String>,
    /// The last version whose update board was shown.
    pub whatsnew_seen: String,
    /// Set up (or skipped) once: the welcome isn't shown again.
    pub imported: bool,
    /// The old Desktop's storage was read in (once, whatever it held).
    pub migrated: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            relay_health: tawny_core::relay::health::HealthBook::default(),
            my_name: String::new(),
            relays: Vec::new(),
            relays_only: false,
            coach_seen: Vec::new(),
            whatsnew_seen: String::new(),
            imported: false,
            migrated: false,
        }
    }
}

/// Everything in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    /// The file's layout; bumped when it changes shape.
    pub version: u32,
    /// The channels, in the order the list shows them.
    pub channels: Vec<Channel>,
    /// Each channel's keys, by channel key.
    pub keys: BTreeMap<String, Keys>,
    /// The settings.
    pub settings: Settings,
}

/// The current layout.
const VERSION: u32 = 1;
/// The longest channel name kept (the page's `maxlength`).
pub const NAME_MAX: usize = 40;
/// The longest personal name kept.
pub const MY_NAME_MAX: usize = 24;

/// The store: the data and where it lives.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    /// What is in it now.
    pub data: Data,
}

impl Store {
    /// The store in the user's data folder.
    ///
    /// # Errors
    /// When the folder can't be found or the file can't be read or parsed.
    pub fn open_default() -> Result<Self> {
        Self::open(&default_dir()?.join("store.json"))
    }

    /// The store at `path`; empty when there is no file yet.
    ///
    /// # Errors
    /// When the file exists but can't be read or parsed.
    pub fn open(path: &Path) -> Result<Self> {
        let data = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            data,
        })
    }

    /// Writes the store out whole, then swaps it in.
    ///
    /// # Errors
    /// When the folder or the file can't be written.
    pub fn save(&mut self) -> Result<()> {
        self.data.version = VERSION;
        let dir = self.path.parent().context("store path has no folder")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.data)?;
        {
            let mut f = private_file(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// A new channel with a fresh key, at the end of the list.
    pub fn add_channel(&mut self, name: &str) -> Channel {
        let ch = Channel {
            id: random_id(),
            name: clip(name.trim(), NAME_MAX),
            key: new_channel_key(),
            role: None,
        };
        self.data.channels.push(ch.clone());
        ch
    }

    /// A channel another device made, joined by its pairing link: its key is
    /// the link's. An existing channel on that key is reused.
    pub fn add_paired(&mut self, name: &str, key: &str) -> Channel {
        if let Some(c) = self.data.channels.iter().find(|c| c.key == key) {
            return c.clone();
        }
        let ch = Channel {
            id: random_id(),
            name: clip(name.trim(), NAME_MAX),
            key: key.to_owned(),
            role: None,
        };
        self.data.channels.push(ch.clone());
        ch
    }

    /// A channel by its local id.
    #[must_use]
    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.data.channels.iter().find(|c| c.id == id)
    }

    /// Renames a channel; its key, and so its pairings, stay.
    pub fn rename(&mut self, id: &str, name: &str) {
        if let Some(c) = self.data.channels.iter_mut().find(|c| c.id == id) {
            c.name = clip(name.trim(), NAME_MAX);
        }
    }

    /// Remembers this device's job on a channel.
    pub fn set_role(&mut self, id: &str, role: Role) {
        if let Some(c) = self.data.channels.iter_mut().find(|c| c.id == id) {
            c.role = Some(role);
        }
    }

    /// Deletes a channel and every trace of its pairings, so a deleted
    /// channel stops admitting the devices it had let in.
    pub fn delete(&mut self, id: &str) {
        let Some(pos) = self.data.channels.iter().position(|c| c.id == id) else {
            return;
        };
        let ch = self.data.channels.remove(pos);
        // Another channel on the same key (an import can make one) keeps them.
        if !self.data.channels.iter().any(|c| c.key == ch.key) {
            self.data.keys.remove(&ch.key);
        }
    }

    /// A channel's keys, made on first use.
    pub fn keys_mut(&mut self, key: &str) -> &mut Keys {
        self.data.keys.entry(key.to_owned()).or_default()
    }

    /// The Monitor's Nostr secret for a channel, minted on first use.
    pub fn station_nostr(&mut self, key: &str) -> String {
        self.keys_mut(key)
            .station_nostr
            .get_or_insert_with(new_secret_hex)
            .clone()
    }

    /// This Viewer's Nostr secret for a channel, minted on first use.
    pub fn viewer_nostr(&mut self, key: &str) -> String {
        self.keys_mut(key)
            .viewer_nostr
            .get_or_insert_with(new_secret_hex)
            .clone()
    }

    /// This Viewer's bond id with a channel's Monitor, minted on first use.
    pub fn bond(&mut self, key: &str) -> String {
        self.keys_mut(key)
            .bond
            .get_or_insert_with(|| hex(&random_bytes::<16>()))
            .clone()
    }
}

/// The app's id: Tawny DevOps builds with its own, so it never shares the
/// real Tawny's channels and keys on the same computer.
pub const APP_ID: &str = match option_env!("TAWNY_APP_ID") {
    Some(id) => id,
    None => "io.github.PressF4me.Tawny",
};

/// The user's local data folder (`$XDG_DATA_HOME`, `%LOCALAPPDATA%`).
///
/// # Errors
/// When neither it nor the home folder is known.
pub fn data_local() -> Result<PathBuf> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
        return Ok(PathBuf::from(base));
    }
    if let Some(base) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(base));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share"))
}

/// The app's data folder.
///
/// # Errors
/// When the local data folder isn't known.
pub fn default_dir() -> Result<PathBuf> {
    Ok(data_local()?.join(APP_ID))
}

/// A file only the user can read (on Unix; Windows keeps it in the profile).
fn private_file(path: &Path) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("writing {}", path.display()))
}

/// At most `max` characters of `s`.
#[must_use]
pub fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    // The OS generator failing is not something to carry on from.
    if getrandom::fill(&mut b).is_err() {
        std::process::abort();
    }
    b
}

fn new_secret_hex() -> String {
    hex(&random_bytes::<32>())
}

fn random_id() -> String {
    hex(&random_bytes::<8>())
}

/// A fresh channel key: 16 random bytes, base64url without padding (as the
/// web app makes them).
#[must_use]
pub fn new_channel_key() -> String {
    base64url(&random_bytes::<16>())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tawny-store-{name}-{}", random_id()));
        dir.join("store.json")
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let s = Store::open(&scratch("missing")).expect("opens");
        assert_eq!(s.data.channels, Vec::new());
        assert_eq!(s.data.settings.theme, "system");
    }

    #[test]
    fn what_is_saved_reads_back() {
        let path = scratch("roundtrip");
        let mut s = Store::open(&path).expect("opens");
        let ch = s.add_channel("  Biscuit  ");
        s.set_role(&ch.id, Role::Station);
        let secret = s.station_nostr(&ch.key);
        s.data.settings.my_name = "Sam".into();
        s.save().expect("saves");

        let back = Store::open(&path).expect("reopens");
        assert_eq!(back.data.channels.len(), 1);
        assert_eq!(back.data.channels[0].name, "Biscuit");
        assert_eq!(back.data.channels[0].role, Some(Role::Station));
        assert_eq!(
            back.data.keys[&ch.key].station_nostr.as_deref(),
            Some(secret.as_str())
        );
        assert_eq!(back.data.settings.my_name, "Sam");
        let _ = fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = scratch("perms");
        let mut s = Store::open(&path).expect("opens");
        s.add_channel("x");
        s.save().expect("saves");
        let mode = fs::metadata(&path).expect("exists").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_deleted_channel_forgets_its_pairings() {
        let mut s = Store::open(&scratch("delete")).expect("opens");
        let a = s.add_channel("A");
        let b = s.add_channel("B");
        s.bond(&a.key);
        s.bond(&b.key);
        s.delete(&a.id);
        assert!(s.channel(&a.id).is_none());
        assert!(!s.data.keys.contains_key(&a.key));
        assert!(s.data.keys.contains_key(&b.key));
    }

    #[test]
    fn a_joined_channel_keeps_the_links_key_once() {
        let mut s = Store::open(&scratch("paired")).expect("opens");
        let a = s.add_paired("Biscuit", "L_F7wC98BnTz78KPFl-bRg");
        let b = s.add_paired("Other name", "L_F7wC98BnTz78KPFl-bRg");
        assert_eq!(a, b);
        assert_eq!(s.data.channels.len(), 1);
        assert_eq!(a.key, "L_F7wC98BnTz78KPFl-bRg");
    }

    #[test]
    fn secrets_are_minted_once() {
        let mut s = Store::open(&scratch("mint")).expect("opens");
        let ch = s.add_channel("A");
        let first = s.viewer_nostr(&ch.key);
        assert_eq!(first.len(), 64);
        assert_eq!(s.viewer_nostr(&ch.key), first);
        assert_eq!(s.bond(&ch.key).len(), 32);
    }

    #[test]
    fn names_are_clipped_and_keys_look_like_the_web_apps() {
        let mut s = Store::open(&scratch("clip")).expect("opens");
        let ch = s.add_channel(&"x".repeat(60));
        assert_eq!(ch.name.chars().count(), NAME_MAX);
        assert_eq!(ch.key.len(), 22);
        assert!(
            ch.key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn base64url_matches_known_values() {
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}
