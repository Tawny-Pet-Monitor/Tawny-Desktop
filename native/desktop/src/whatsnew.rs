//! The update board: once per version, after an update, over the first
//! screen. A fresh install has nothing to be "new" against, so its first
//! version is marked seen without showing anything; an earlier run of this
//! app, or the old Desktop's storage read in this start, makes it an update.

use std::cell::RefCell;
use std::rc::Rc;

use crate::AppWindow;
use crate::store::Store;

/// Where every change is written out.
const CHANGELOG: &str =
    "https://github.com/PressF4me/Tawny-APK/blob/master/docs/changelog/desktop.md";

/// "0.2.0" → "0.2": the tag is a headline, not a build number.
fn short(version: &str) -> &str {
    let v = version.split('-').next().unwrap_or(version);
    v.strip_suffix(".0").unwrap_or(v)
}

/// Whether this start should show the board, given the last version shown
/// and whether the old Desktop's storage was found this start.
fn due(seen: &str, version: &str, from_old_desktop: bool) -> bool {
    seen != version && (!seen.is_empty() || from_old_desktop)
}

/// Shows the board when this version is new to an earlier install, and notes
/// the version as seen either way. Not over a pairing link being opened.
pub fn attach(ui: &AppWindow, store: &Rc<RefCell<Store>>, from_old_desktop: bool, has_link: bool) {
    ui.on_open_changelog(|| crate::tip::open_web(CHANGELOG));
    let version = env!("CARGO_PKG_VERSION");
    let mut st = store.borrow_mut();
    let seen = st.data.settings.whatsnew_seen.clone();
    if seen == version {
        return;
    }
    version.clone_into(&mut st.data.settings.whatsnew_seen);
    if let Err(e) = st.save() {
        eprintln!("tawny: noting the update board: {e:#}");
    }
    if has_link || !due(&seen, version, from_old_desktop) {
        return;
    }
    ui.set_whatsnew_version(short(version).into());
    ui.set_whatsnew_changelog(true);
    ui.set_sheet("whatsnew".into());
}

#[cfg(test)]
mod tests {
    use super::{due, short};

    #[test]
    fn the_tag_drops_a_trailing_zero() {
        assert_eq!(short("0.2.0"), "0.2");
        assert_eq!(short("0.2.1"), "0.2.1");
        assert_eq!(short("1.0.0-beta"), "1.0");
    }

    #[test]
    fn only_an_update_shows_the_board() {
        // A fresh install: nothing seen, no old Desktop.
        assert!(!due("", "0.2.0", false));
        // From the old Desktop, or from an earlier version of this one.
        assert!(due("", "0.2.0", true));
        assert!(due("0.1.9", "0.2.0", false));
        // Already shown.
        assert!(!due("0.2.0", "0.2.0", true));
    }
}
