//! One Tawny at a time. The first copy holds a lock on a file in its data
//! folder and listens on a loopback port; a copy started later (a pairing
//! link opened from the browser, a second click on the icon) hands its link
//! to the first and quits, so two never write the same store or run two
//! Monitors.
//!
//! A link handed over only ever opens "Join now": nothing joins without the
//! user's click, so another program on this computer sending one gains
//! nothing.

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// The longest message a later copy may send.
const MAX_LINE: u64 = 4096;

/// Being the one copy: the lock is held while this lives.
pub struct Instance {
    _lock: File,
}

/// What happened at start.
pub enum Start {
    /// This is the first copy; links from later ones go to `on_link`.
    First(Instance),
    /// Another copy runs; the link (if any) was handed to it.
    Handed,
}

/// Becomes the one copy, or hands `link` to the one that runs.
///
/// # Errors
/// When the data folder can't be used at all.
pub fn start(dir: &Path, link: Option<&str>, on_link: fn(&str)) -> Result<Start> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("instance.lock"))
        .context("opening the instance lock")?;
    let port_file = dir.join("instance.port");
    if lock.try_lock().is_err() {
        // Another copy holds it: wake it, with the link if there is one.
        let port: u16 = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|p| p.trim().parse().ok())
            .context("the running Tawny left no port")?;
        let mut s =
            TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2))
                .context("reaching the running Tawny")?;
        writeln!(s, "{}", link.unwrap_or("show"))?;
        return Ok(Start::Handed);
    }
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("listening for later copies")?;
    std::fs::write(&port_file, listener.local_addr()?.port().to_string())?;
    std::thread::Builder::new()
        .name("tawny-instance".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                let mut got = String::new();
                if BufReader::new(conn.take(MAX_LINE))
                    .read_line(&mut got)
                    .is_ok()
                {
                    let said = got.trim().to_owned();
                    if said == "show" || said.starts_with("tawny://") {
                        let _ = slint::invoke_from_event_loop(move || on_link(&said));
                    }
                }
            }
        })
        .context("starting the instance listener")?;
    Ok(Start::First(Instance { _lock: lock }))
}

#[cfg(test)]
mod tests {
    use super::{Start, start};

    #[test]
    fn a_second_copy_hands_over() {
        let dir = std::env::temp_dir().join(format!("tawny-single-{}", std::process::id()));
        let first = start(&dir, None, |_| {}).expect("first");
        assert!(matches!(first, Start::First(_)));
        let second = start(&dir, Some("tawny://pair?k=x"), |_| {}).expect("second");
        assert!(matches!(second, Start::Handed));
        drop(first);
        let again = start(&dir, None, |_| {}).expect("after the first quits");
        assert!(matches!(again, Start::First(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
