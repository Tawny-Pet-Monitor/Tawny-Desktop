//! The chimes a Viewer rings on the Monitor, and the sound a Monitor makes
//! when a phone starts watching. The same clips as the web app (mono Vorbis,
//! 32 kHz; see web/sounds/README.md for where they come from), built into the
//! program and decoded to the call's 48 kHz when played.

use std::io::Cursor;

use lewton::inside_ogg::OggStreamReader;
use tawny_core::call::media::AUDIO_RATE;

/// A chime's name, its button and its clip.
const CLIPS: &[(&str, &str, &[u8])] = &[
    ("bark", "Dog toy", include_bytes!("../ui/sounds/bark.ogg")),
    (
        "pspsps",
        "Psp psp psp",
        include_bytes!("../ui/sounds/pspsps.ogg"),
    ),
    ("meow", "Meow", include_bytes!("../ui/sounds/meow.ogg")),
    (
        "goodboy",
        "Good boy",
        include_bytes!("../ui/sounds/goodboy.ogg"),
    ),
    ("bell", "Bell", include_bytes!("../ui/sounds/bell.ogg")),
    ("viewer", "", include_bytes!("../ui/sounds/viewer.ogg")),
];

/// The button a chime is rung with ("Meow"), for the Viewer's toast; only
/// ever one of the names above, never what the far end sent.
#[must_use]
pub fn label(name: &str) -> Option<&'static str> {
    CLIPS
        .iter()
        .find(|(n, l, _)| *n == name && !l.is_empty())
        .map(|(_, l, _)| *l)
}

/// A clip as 48 kHz mono samples; `None` for a name that isn't one.
#[must_use]
pub fn samples(name: &str) -> Option<Vec<i16>> {
    let (_, _, ogg) = CLIPS.iter().find(|(n, _, _)| *n == name)?;
    let mut r = OggStreamReader::new(Cursor::new(*ogg)).ok()?;
    let rate = r.ident_hdr.audio_sample_rate;
    let channels = usize::from(r.ident_hdr.audio_channels.max(1));
    let mut mono = Vec::new();
    while let Ok(Some(pkt)) = r.read_dec_packet_itl() {
        for frame in pkt.chunks(channels) {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            let n = i32::try_from(frame.len()).unwrap_or(1).max(1);
            mono.push(i16::try_from(sum / n).unwrap_or(0));
        }
    }
    Some(crate::audio::resample(&mono, rate, AUDIO_RATE))
}

#[cfg(test)]
mod tests {
    use super::{label, samples};

    #[test]
    fn every_chime_decodes_to_about_its_length() {
        for (name, secs) in [
            ("bark", 1.6),
            ("meow", 1.1),
            ("bell", 1.8),
            ("viewer", 1.45),
        ] {
            let s = samples(name).expect(name);
            #[expect(clippy::cast_precision_loss, reason = "a test's rough length")]
            let got = s.len() as f64 / 48_000.0;
            assert!((got - secs).abs() < 0.25, "{name}: {got} s");
            assert!(
                s.iter().any(|v| v.unsigned_abs() > 1000),
                "{name} is audible"
            );
        }
        assert!(samples("../etc/passwd").is_none());
    }

    #[test]
    fn labels_are_ours_only() {
        assert_eq!(label("meow"), Some("Meow"));
        assert_eq!(label("viewer"), None);
        assert_eq!(label("<b>x</b>"), None);
    }
}
