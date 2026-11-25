//! Sound in and out of the running Monitor or Viewer, through the app.
//!
//! In: the app captures the microphone (on Android, `AudioRecord` with the
//! voice-call source, so the platform's echo cancelling applies) and pushes
//! 10 ms frames with [`push_mic`].
//!
//! Out: every incoming audio track (a Viewer's talk-back on the Monitor; the
//! Monitor's room on a Viewer) is read into a short buffer here
//! ([`SPEAKER`]), and the app's player pulls a mix of all of them.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use futures_util::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::audio_track::RtcAudioTrack;
use tawny_core::call::media::{AUDIO_FRAME, AUDIO_RATE};
use tokio::runtime::Runtime;

/// The running Monitor's or Viewer's microphone source.
pub(crate) static MIC: Mutex<Option<NativeAudioSource>> = Mutex::new(None);

/// Most sound held per track: 300 ms. Older sound is dropped, so a
/// stalled player never makes the voice lag further and further behind.
const HELD_MAX: usize = AUDIO_RATE as usize * 3 / 10;

/// Every incoming track's sound, waiting to be played.
#[derive(Default)]
pub(crate) struct Mix(Mutex<HashMap<String, VecDeque<i16>>>);

impl Mix {
    fn push(&self, id: &str, samples: &[i16]) {
        let Ok(mut b) = self.0.lock() else { return };
        let q = b.entry(id.to_owned()).or_default();
        q.extend(samples.iter().copied());
        let extra = q.len().saturating_sub(HELD_MAX);
        q.drain(..extra);
    }

    /// Forgets one track (its peer left).
    pub(crate) fn forget(&self, id: &str) {
        if let Ok(mut b) = self.0.lock() {
            b.remove(id);
        }
    }

    /// Forgets everything (stopped).
    pub(crate) fn clear(&self) {
        if let Ok(mut b) = self.0.lock() {
            b.clear();
        }
    }

    /// Fills `out` with the mix of every track, silence where there is none. Returns how many samples carried sound from someone.
    pub(crate) fn read(&self, out: &mut [i16]) -> usize {
        out.fill(0);
        let Ok(mut b) = self.0.lock() else { return 0 };
        let mut most = 0;
        for q in b.values_mut() {
            let n = q.len().min(out.len());
            for (o, s) in out.iter_mut().zip(q.drain(..n)) {
                *o = o.saturating_add(s);
            }
            most = most.max(n);
        }
        most
    }
}

/// What the app's player plays.
pub(crate) static SPEAKER: LazyLock<Arc<Mix>> = LazyLock::new(Arc::default);

/// Hands one 10 ms mono frame to the running Monitor or Viewer. `false` when
/// there is none, its microphone is off, or the frame isn't [`AUDIO_FRAME`] samples.
pub(crate) fn push_mic(rt: &Runtime, samples: &[i16]) -> bool {
    if samples.len() != AUDIO_FRAME {
        return false;
    }
    let Some(src) = MIC.lock().ok().and_then(|m| m.clone()) else {
        return false;
    };
    let f = AudioFrame {
        data: samples.into(),
        sample_rate: AUDIO_RATE,
        num_channels: 1,
        samples_per_channel: u32::try_from(AUDIO_FRAME).unwrap_or(480),
    };
    rt.block_on(src.capture_frame(&f)).is_ok()
}

/// Starts reading an incoming track into the mix, as `id`.
pub(crate) fn listen(rt: &Runtime, id: String, track: RtcAudioTrack) {
    let mix = SPEAKER.clone();
    rt.spawn(async move {
        let rate = i32::try_from(AUDIO_RATE).unwrap_or(48_000);
        let mut s = NativeAudioStream::new(track, rate, 1);
        while let Some(f) = s.next().await {
            mix.push(&id, &f.data);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talk_back_is_mixed_capped_and_forgotten() {
        let mix = Mix::default();
        mix.push("a", &[100; 4]);
        mix.push("b", &[i16::MAX, 5]);
        let mut out = [7i16; 4];
        assert_eq!(mix.read(&mut out), 4);
        assert_eq!(
            out,
            [i16::MAX, 105, 100, 100],
            "summed, saturating, then silence-padded"
        );
        assert_eq!(mix.read(&mut out), 0);
        assert_eq!(out, [0; 4], "nothing left: silence");
        mix.push("a", &vec![1; HELD_MAX + 10]);
        let mut big = vec![0i16; HELD_MAX + 10];
        assert_eq!(
            mix.read(&mut big),
            HELD_MAX,
            "older sound dropped past 300 ms"
        );
        mix.push("a", &[1]);
        mix.forget("a");
        assert_eq!(mix.read(&mut out), 0);
    }

    #[test]
    fn only_whole_ten_millisecond_frames_go_in() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        assert!(!push_mic(&rt, &[0; 100]), "wrong size");
        assert!(!push_mic(&rt, &[0; AUDIO_FRAME]), "no Monitor running");
    }
}
