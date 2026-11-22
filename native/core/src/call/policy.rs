//! The Monitor's call rules, with no WebRTC in them: who may have the camera,
//! and how the uplink is shared. Ports of `pairingAllowed()`, `bitrateFor()`,
//! `tuneVideoSender()` and the power budget in `app.js`.

use serde::{Deserialize, Serialize};

/// One Monitor sends to at most this many Viewers.
pub const MAX_VIEWERS: usize = 3;
/// How many Viewer ids a Monitor remembers.
pub const MAX_REMEMBERED_PEERS: usize = 8;
/// How long a pairing code works.
pub const PAIR_TTL_MS: u64 = 10 * 60 * 1000;
/// A code is shown in a pairing link only with at least this long left, so
/// there is time to scan it and for the phone to call. Under that, a fresh
/// code is made.
pub const PAIR_MIN_LEFT_MS: u64 = 2 * 60 * 1000;

/// A Viewer's bond id: 32 lower-case hex characters.
#[must_use]
pub fn is_bond_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The code on screen and its deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCode {
    /// The code (`c` in the pairing link).
    pub code: String,
    /// When it stops working, epoch milliseconds.
    pub expires_ms: u64,
}

/// What the gate decided about an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A phone this Monitor already knows.
    Known,
    /// Let in on the current code; its bond id (if it sent one) is now remembered.
    Enrolled,
    /// No: no code on screen, a wrong or lapsed code, and not a known phone.
    Refused,
}

impl Gate {
    /// Whether the offer may have the camera.
    #[must_use]
    pub const fn allowed(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// The pairing gate: the only place a phone the Monitor has never seen
/// becomes one it has. It fails closed on every path, and enrolling a new
/// phone is the only branch that writes anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairGate {
    /// The code currently on screen, if any.
    pub code: Option<PairCode>,
    /// Bond ids of phones let in before, oldest first.
    pub known: Vec<String>,
}

impl PairGate {
    /// Mints a new code, valid for [`PAIR_TTL_MS`] from `now_ms`.
    ///
    /// # Errors
    /// [`crate::ids::RandomError`] if the system RNG fails.
    pub fn new_code(&mut self, now_ms: u64) -> Result<&PairCode, crate::ids::RandomError> {
        let code = crate::ids::Secret::new(crate::ids::SecretKind::PairCode)?
            .expose()
            .to_owned();
        Ok(self.code.insert(PairCode {
            code,
            expires_ms: now_ms + PAIR_TTL_MS,
        }))
    }

    /// Milliseconds left on the code on screen; 0 when there isn't one.
    #[must_use]
    pub fn code_left(&self, now_ms: u64) -> u64 {
        self.code
            .as_ref()
            .map_or(0, |c| c.expires_ms.saturating_sub(now_ms))
    }

    /// `pairingAllowed()`: decides on an offer carrying `pid` (bond id) and `pc` (code).
    pub fn check(&mut self, pid: Option<&str>, pc: Option<&str>, now_ms: u64) -> Gate {
        let pid = pid.filter(|p| is_bond_id(p));
        if pid.is_some_and(|p| self.known.iter().any(|k| k == p)) {
            return Gate::Known;
        }
        let Some(code) = &self.code else {
            return Gate::Refused;
        };
        if pc != Some(code.code.as_str()) || now_ms > code.expires_ms {
            return Gate::Refused;
        }
        if let Some(p) = pid {
            self.remember(p);
        }
        Gate::Enrolled
    }

    fn remember(&mut self, pid: &str) {
        self.known.retain(|k| k != pid);
        self.known.push(pid.to_owned());
        if self.known.len() > MAX_REMEMBERED_PEERS {
            let extra = self.known.len() - MAX_REMEMBERED_PEERS;
            self.known.drain(..extra);
        }
    }

    /// Forgets every phone (a re-pair, or "remove all devices").
    pub fn forget_all(&mut self) {
        self.known.clear();
    }
}

/// The video encoder's limits for one Viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Encoding {
    /// Bits per second.
    pub max_bitrate: u64,
    /// Frames per second.
    pub max_framerate: f64,
    /// Resolution divisor (1.0 = full capture size).
    pub scale_down_by: f64,
}

/// `bitrateFor()`: about 3 Mbit/s shared, 350 kbit/s to 1.5 Mbit/s each.
#[must_use]
pub fn bitrate_for(viewers: usize) -> u64 {
    let n = u64::try_from(viewers).unwrap_or(u64::MAX);
    (3_000_000 / n.saturating_add(1)).clamp(350_000, 1_500_000)
}

/// Dim mode's ceiling: nobody is looking at the Monitor's own screen, so it
/// spends as little as possible while still sending a useful picture
/// (960x540 → 640x360, 15 fps, 500 kbit/s).
pub const DIMMED: Encoding = Encoding {
    max_bitrate: 500_000,
    max_framerate: 15.0,
    scale_down_by: 1.5,
};
/// The camera's own frame rate in dim mode (the bigger saving: sensor and ISP too).
pub const DIMMED_CAPTURE_FPS: u32 = 15;

/// `tuneVideoSender()`: the limits for each Viewer given how many there are.
#[must_use]
pub fn encoding(viewers: usize, dimmed: bool) -> Encoding {
    let fps = if viewers >= 3 { 20.0 } else { 24.0 };
    let base = Encoding {
        max_bitrate: bitrate_for(viewers),
        max_framerate: fps,
        scale_down_by: 1.0,
    };
    if dimmed {
        Encoding {
            max_bitrate: base.max_bitrate.min(DIMMED.max_bitrate),
            max_framerate: base.max_framerate.min(DIMMED.max_framerate),
            scale_down_by: DIMMED.scale_down_by,
        }
    } else {
        base
    }
}

/// Whether the camera should be running: only while someone is watching. The
/// foreground service stays up meanwhile, so capture can start again within a
/// second of a Viewer joining, without any wake lock of the app's own.
#[must_use]
pub const fn capture_wanted(connected_viewers: usize) -> bool {
    connected_viewers > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: &str = "0123456789abcdef0123456789abcdef";
    const T0: u64 = 1_795_150_000_000;

    fn gate_with_code() -> PairGate {
        PairGate {
            code: Some(PairCode {
                code: "CODE5678xyz".into(),
                expires_ms: T0 + PAIR_TTL_MS,
            }),
            known: vec![],
        }
    }

    #[test]
    fn the_current_code_enrolls_and_remembers_the_phone() {
        let mut g = gate_with_code();
        assert_eq!(g.check(Some(PID), Some("CODE5678xyz"), T0), Gate::Enrolled);
        assert_eq!(g.known, vec![PID.to_owned()]);
        // Later, with no code on screen at all, the same phone is known.
        g.code = None;
        assert_eq!(g.check(Some(PID), None, T0 + 86_400_000), Gate::Known);
    }

    #[test]
    fn it_fails_closed() {
        let mut g = gate_with_code();
        assert_eq!(
            g.check(None, None, T0),
            Gate::Refused,
            "no code in the offer"
        );
        assert_eq!(g.check(None, Some("WRONGcode00"), T0), Gate::Refused);
        assert_eq!(
            g.check(None, Some("CODE5678xyz"), T0 + PAIR_TTL_MS + 1),
            Gate::Refused,
            "lapsed"
        );
        assert_eq!(
            PairGate::default().check(None, Some("CODE5678xyz"), T0),
            Gate::Refused,
            "no code on screen"
        );
        assert_eq!(
            g.known,
            Vec::<String>::new(),
            "nothing written on a refusal"
        );
    }

    #[test]
    fn a_malformed_bond_id_is_not_remembered_or_trusted() {
        let mut g = gate_with_code();
        assert_eq!(
            g.check(Some("NOT-A-BOND"), Some("CODE5678xyz"), T0),
            Gate::Enrolled
        );
        assert_eq!(g.known, Vec::<String>::new());
        g.known.push("NOT-A-BOND".into());
        g.code = None;
        assert_eq!(g.check(Some("NOT-A-BOND"), None, T0), Gate::Refused);
    }

    #[test]
    fn at_most_eight_phones_are_remembered_newest_last() {
        let mut g = gate_with_code();
        for i in 0..10 {
            let pid = format!("{i:032x}");
            g.check(Some(&pid), Some("CODE5678xyz"), T0);
        }
        assert_eq!(g.known.len(), MAX_REMEMBERED_PEERS);
        assert_eq!(g.known[0], format!("{:032x}", 2));
        assert_eq!(g.known[7], format!("{:032x}", 9));
    }

    #[test]
    fn the_ladder_matches_app_js() {
        assert_eq!(bitrate_for(0), 1_500_000);
        assert_eq!(bitrate_for(1), 1_500_000);
        assert_eq!(bitrate_for(2), 1_000_000);
        assert_eq!(bitrate_for(3), 750_000);
        assert_eq!(bitrate_for(20), 350_000);
        assert_eq!(encoding(3, false).max_framerate, 20.0);
        assert_eq!(encoding(1, false).max_framerate, 24.0);
        let dim = encoding(1, true);
        assert_eq!(
            (dim.max_bitrate, dim.max_framerate, dim.scale_down_by),
            (500_000, 15.0, 1.5)
        );
        assert_eq!(encoding(3, true).max_bitrate, 500_000);
    }

    #[test]
    fn the_camera_runs_only_while_someone_watches() {
        assert!(!capture_wanted(0));
        assert!(capture_wanted(1));
    }
}
