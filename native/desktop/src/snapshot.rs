//! Snapshot: the picture on screen, saved as a PNG in the user's Pictures
//! folder, under Tawny.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use slint::Image;

/// Saves `img` and says where.
///
/// # Errors
/// When the picture can't be read or the file can't be written.
pub fn save(img: &Image) -> Result<PathBuf> {
    let buf = img.to_rgba8().context("the picture isn't readable")?;
    let (w, h) = (buf.width(), buf.height());
    let mut px = tiny_skia::Pixmap::new(w, h).context("an empty picture")?;
    // Opaque video: premultiplied is the same as straight.
    px.data_mut().copy_from_slice(buf.as_bytes());
    let dir = pictures()?.join("Tawny");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = dir.join(format!("tawny-{}.png", stamp(now_secs())));
    px.save_png(&file)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// The user's Pictures folder: the one the desktop names (it is "Imágenes"
/// on a Spanish system), else ~/Pictures.
fn pictures() -> Result<PathBuf> {
    user_folder("XDG_PICTURES_DIR", "Pictures")
}

/// One of the user's folders (`XDG_PICTURES_DIR`, `XDG_VIDEOS_DIR`): as the
/// environment or the desktop's `user-dirs.dirs` names it, else
/// `~/<fallback>`.
///
/// # Errors
/// When there is no home folder.
pub fn user_folder(key: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(key).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = PathBuf::from(
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            std::env::var_os("HOME")
        }
        .context("no home folder")?,
    );
    if !cfg!(windows) {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from);
        for file in [
            config.join("user-dirs.dirs"),
            home.join(".config/user-dirs.dirs"),
        ] {
            if let Some(dir) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| user_dir(&text, key, &home))
            {
                return Ok(dir);
            }
        }
    }
    Ok(home.join(fallback))
}

/// A folder from xdg-user-dirs' `user-dirs.dirs` (`KEY="$HOME/Imágenes"`).
/// Only "$HOME/…" or an absolute path counts; `$HOME/` alone means unset.
fn user_dir(text: &str, key: &str, home: &std::path::Path) -> Option<PathBuf> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with('=')))?;
    let value = line.split_once('=')?.1.trim().trim_matches('"');
    if let Some(rest) = value.strip_prefix("$HOME") {
        let rest = rest.trim_start_matches('/');
        return (!rest.is_empty()).then(|| home.join(rest));
    }
    value.starts_with('/').then(|| PathBuf::from(value))
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `20261121-153045` for a moment (UTC), so names sort by time.
pub fn stamp(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::{stamp, user_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn the_desktop_names_the_pictures_folder() {
        let home = Path::new("/home/ana");
        let dirs = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Escritorio\"\nXDG_PICTURES_DIR=\"$HOME/Imágenes\"\n";
        assert_eq!(
            user_dir(dirs, "XDG_PICTURES_DIR", home),
            Some(PathBuf::from("/home/ana/Imágenes"))
        );
        assert_eq!(
            user_dir("XDG_PICTURES_DIR=\"/data/fotos\"", "XDG_PICTURES_DIR", home),
            Some(PathBuf::from("/data/fotos"))
        );
        assert_eq!(
            user_dir("XDG_PICTURES_DIR=\"$HOME/\"", "XDG_PICTURES_DIR", home),
            None
        );
        assert_eq!(
            user_dir("XDG_PICTURES_DIRX=\"$HOME/x\"", "XDG_PICTURES_DIR", home),
            None
        );
        assert_eq!(user_dir("", "XDG_PICTURES_DIR", home), None);
    }

    #[test]
    fn stamps_read_as_utc_dates() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(951_782_400), "20000229-000000");
        assert_eq!(stamp(1_795_275_874), "20261121-154434");
    }
}
