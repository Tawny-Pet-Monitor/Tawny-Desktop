//! Sound both ways, through the system's default devices (`cpal`: ALSA or
//! PulseAudio/PipeWire on Linux, WASAPI on Windows).
//!
//! - The speaker plays one mix of every far end (the Monitor's room on a
//!   Viewer; each Viewer's talk-back on the Monitor), at most 300 ms behind.
//! - The microphone is downmixed to mono, brought to 48 kHz and cut into the
//!   10 ms frames the call sends; it is passed on only while `open`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use futures_util::StreamExt as _;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::audio_track::RtcAudioTrack;
use tawny_core::call::media::{AUDIO_FRAME, AUDIO_RATE};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// The most sound held per far end: older is dropped, so it stays live.
const HELD_MAX: usize = AUDIO_RATE as usize * 3 / 10;

/// Every far end's sound, waiting to be played.
#[derive(Default)]
pub struct Mix {
    queues: Mutex<HashMap<String, VecDeque<i16>>>,
    /// The Viewer's "Mute": nothing is played.
    pub muted: AtomicBool,
}

impl Mix {
    fn push(&self, id: &str, samples: &[i16]) {
        let Ok(mut q) = self.queues.lock() else {
            return;
        };
        let q = q.entry(id.to_owned()).or_default();
        q.extend(samples.iter().copied());
        let extra = q.len().saturating_sub(HELD_MAX);
        q.drain(..extra);
    }

    /// Forgets a far end that left.
    pub fn forget(&self, id: &str) {
        if let Ok(mut q) = self.queues.lock() {
            q.remove(id);
        }
    }

    /// Fills `out` with the next samples of the mix, silence where none.
    fn read(&self, out: &mut [i16]) {
        out.fill(0);
        let Ok(mut q) = self.queues.lock() else {
            return;
        };
        let muted = self.muted.load(Ordering::Relaxed);
        for q in q.values_mut() {
            let n = q.len().min(out.len());
            for (o, s) in out.iter_mut().zip(q.drain(..n)) {
                if !muted {
                    *o = o.saturating_add(s);
                }
            }
        }
    }
}

/// Plays a far end's track into the mix until it ends.
/// With `tap`, the sound also goes to a clip being recorded.
pub fn listen(rt: &Runtime, mix: Arc<Mix>, id: String, track: RtcAudioTrack, tap: bool) {
    rt.spawn(async move {
        let rate = i32::try_from(AUDIO_RATE).unwrap_or(48_000);
        let mut s = NativeAudioStream::new(track, rate, 1);
        let dev = std::env::var_os("TAWNY_DEV").is_some();
        let mut n = 0u32;
        while let Some(f) = s.next().await {
            mix.push(&id, &f.data);
            if tap {
                crate::record::audio(&f.data);
            }
            n = n.wrapping_add(1);
            // Every second, how loud the far end is (developer runs only).
            if dev && n.is_multiple_of(100) {
                let len = f.data.len().max(1);
                let mean: u64 = f
                    .data
                    .iter()
                    .map(|v| u64::from(v.unsigned_abs()))
                    .sum::<u64>()
                    / len as u64;
                eprintln!("tawny: STATE sound from {id} level {mean}");
            }
        }
        mix.forget(&id);
    });
}

/// The speaker, playing the mix while it lives.
pub struct Speaker {
    _stream: Stream,
}

impl Speaker {
    /// Opens the default output.
    ///
    /// # Errors
    /// When there is no output device or it won't play.
    pub fn open(mix: Arc<Mix>) -> Result<Self> {
        let dev = cpal::default_host()
            .default_output_device()
            .context("no speaker")?;
        let cfg = dev.default_output_config().context("speaker settings")?;
        let format = cfg.sample_format();
        let cfg: StreamConfig = cfg.into();
        let stream = match format {
            SampleFormat::F32 => speaker::<f32>(&dev, &cfg, mix),
            SampleFormat::I16 => speaker::<i16>(&dev, &cfg, mix),
            SampleFormat::U16 => speaker::<u16>(&dev, &cfg, mix),
            SampleFormat::I32 => speaker::<i32>(&dev, &cfg, mix),
            other => Err(anyhow!("speaker format {other} not handled")),
        }?;
        stream.play().context("starting the speaker")?;
        Ok(Self { _stream: stream })
    }
}

fn speaker<T>(dev: &cpal::Device, cfg: &StreamConfig, mix: Arc<Mix>) -> Result<Stream>
where
    T: SizedSample + FromSample<i16>,
{
    let channels = usize::from(cfg.channels.max(1));
    let mut rs = Resampler::new(AUDIO_RATE, cfg.sample_rate.0);
    let mut src = Vec::new();
    let stream = dev
        .build_output_stream(
            cfg,
            move |out: &mut [T], _| {
                let frames = out.len() / channels;
                // Enough 48 kHz samples for this many device frames.
                let need = rs.needed(frames);
                src.resize(need, 0);
                mix.read(&mut src);
                let mut i = 0;
                rs.run(&src, frames, |s| {
                    for c in 0..channels {
                        out[i * channels + c] = T::from_sample(s);
                    }
                    i += 1;
                });
            },
            |e| eprintln!("tawny: speaker: {e}"),
            None,
        )
        .context("opening the speaker")?;
    Ok(stream)
}

/// The microphone, feeding a call's audio source while `open` is set.
pub struct Mic {
    /// The device stream (none for the test tone).
    _stream: Option<Stream>,
    /// Whether the microphone is passed on (the Monitor unmuted; the Viewer
    /// holding Talk).
    pub open: Arc<AtomicBool>,
}

impl Mic {
    /// Opens the default input into `source`.
    ///
    /// # Errors
    /// When there is no input device or it won't record.
    pub fn open(rt: &Runtime, source: NativeAudioSource, open: bool) -> Result<Self> {
        if std::env::var_os("TAWNY_FAKE_MIC").is_some_and(|v| v == "1") {
            return Ok(Self::tone(rt, source, open));
        }
        let dev = cpal::default_host()
            .default_input_device()
            .context("no microphone")?;
        let cfg = dev.default_input_config().context("microphone settings")?;
        let format = cfg.sample_format();
        let cfg: StreamConfig = cfg.into();
        let flag = Arc::new(AtomicBool::new(open));
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();
        rt.spawn(async move {
            while let Some(frame) = rx.recv().await {
                let f = AudioFrame {
                    data: frame.into(),
                    sample_rate: AUDIO_RATE,
                    num_channels: 1,
                    samples_per_channel: u32::try_from(AUDIO_FRAME).unwrap_or(480),
                };
                if source.capture_frame(&f).await.is_err() {
                    break;
                }
            }
        });
        let stream = match format {
            SampleFormat::F32 => mic::<f32>(&dev, &cfg, flag.clone(), tx),
            SampleFormat::I16 => mic::<i16>(&dev, &cfg, flag.clone(), tx),
            SampleFormat::U16 => mic::<u16>(&dev, &cfg, flag.clone(), tx),
            SampleFormat::I32 => mic::<i32>(&dev, &cfg, flag.clone(), tx),
            other => Err(anyhow!("microphone format {other} not handled")),
        }?;
        stream.play().context("starting the microphone")?;
        Ok(Self {
            _stream: Some(stream),
            open: flag,
        })
    }
}

impl Mic {
    /// A steady 440 Hz tone instead of a microphone (`TAWNY_FAKE_MIC=1`, for
    /// end-to-end runs on a computer without one).
    fn tone(rt: &Runtime, source: NativeAudioSource, open: bool) -> Self {
        let flag = Arc::new(AtomicBool::new(open));
        let gate = flag.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
            let mut t = 0f64;
            loop {
                tick.tick().await;
                if !gate.load(Ordering::Relaxed) {
                    continue;
                }
                let mut data = vec![0i16; AUDIO_FRAME];
                for s in &mut data {
                    #[expect(clippy::cast_possible_truncation, reason = "within ±8000")]
                    let v = (8000.0 * (t * 440.0 * std::f64::consts::TAU).sin()) as i16;
                    *s = v;
                    t += 1.0 / f64::from(AUDIO_RATE);
                }
                let f = AudioFrame {
                    data: data.into(),
                    sample_rate: AUDIO_RATE,
                    num_channels: 1,
                    samples_per_channel: u32::try_from(AUDIO_FRAME).unwrap_or(480),
                };
                if source.capture_frame(&f).await.is_err() {
                    break;
                }
            }
        });
        Self {
            _stream: None,
            open: flag,
        }
    }
}

fn mic<T>(
    dev: &cpal::Device,
    cfg: &StreamConfig,
    open: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<Vec<i16>>,
) -> Result<Stream>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let channels = usize::from(cfg.channels.max(1));
    let mut rs = Resampler::new(cfg.sample_rate.0, AUDIO_RATE);
    let mut mono = Vec::new();
    let mut pending: Vec<i16> = Vec::with_capacity(AUDIO_FRAME * 2);
    let stream = dev
        .build_input_stream(
            cfg,
            move |data: &[T], _| {
                if !open.load(Ordering::Relaxed) {
                    pending.clear();
                    return;
                }
                mono.clear();
                for frame in data.chunks(channels) {
                    let sum: i32 = frame.iter().map(|s| i32::from(i16::from_sample(*s))).sum();
                    let n = i32::try_from(frame.len()).unwrap_or(1).max(1);
                    mono.push(i16::try_from(sum / n).unwrap_or(0));
                }
                let out = rs.out_len(mono.len());
                rs.run(&mono, out, |s| pending.push(s));
                while pending.len() >= AUDIO_FRAME {
                    let rest = pending.split_off(AUDIO_FRAME);
                    let frame = std::mem::replace(&mut pending, rest);
                    if tx.send(frame).is_err() {
                        return;
                    }
                }
            },
            |e| eprintln!("tawny: microphone: {e}"),
            None,
        )
        .context("opening the microphone")?;
    Ok(stream)
}

/// A linear resampler from one rate to another, carrying its position over
/// from one block to the next.
struct Resampler {
    /// Source samples per output sample.
    step: f64,
    /// Where the next output falls, in source samples from the block start.
    pos: f64,
    /// The last sample of the previous block.
    last: i16,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from) / f64::from(to.max(1)),
            pos: 0.0,
            last: 0,
        }
    }

    /// Source samples to read for `out` output samples.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a small, positive count (one audio block)"
    )]
    fn needed(&self, out: usize) -> usize {
        (self.pos + self.step * out as f64).ceil().max(0.0) as usize
    }

    /// Output samples a block of `input` source samples makes.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a small, positive count (one audio block)"
    )]
    fn out_len(&self, input: usize) -> usize {
        ((input as f64 - self.pos) / self.step).ceil().max(0.0) as usize
    }

    /// Makes `out` samples from `src`, handing each to `emit`.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "positions within one audio block; samples within i16"
    )]
    fn run(&mut self, src: &[i16], out: usize, mut emit: impl FnMut(i16)) {
        if src.is_empty() {
            for _ in 0..out {
                emit(0);
            }
            return;
        }
        for _ in 0..out {
            let i = self.pos.floor();
            let frac = self.pos - i;
            let i = i as usize;
            let a = src.get(i).copied().unwrap_or(self.last);
            let b = src.get(i + 1).copied().unwrap_or(a);
            emit((f64::from(a) + (f64::from(b) - f64::from(a)) * frac).round() as i16);
            self.pos += self.step;
        }
        self.pos -= src.len() as f64;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
        self.last = src[src.len() - 1];
    }
}

/// Plays a short clip (a chime) into the mix as its own voice, whole.
pub fn play_clip(mix: &Mix, samples: &[i16]) {
    let Ok(mut q) = mix.queues.lock() else { return };
    let q = q.entry("chime".to_owned()).or_default();
    q.clear();
    q.extend(samples.iter().copied());
}

/// A whole clip from one rate to another.
#[must_use]
pub fn resample(src: &[i16], from: u32, to: u32) -> Vec<i16> {
    let mut rs = Resampler::new(from, to);
    let mut out = Vec::with_capacity(rs.out_len(src.len()));
    let n = rs.out_len(src.len());
    rs.run(src, n, |s| out.push(s));
    out
}

#[cfg(test)]
mod tests {
    use super::{Mix, Resampler};
    use std::sync::atomic::Ordering;

    #[test]
    fn far_ends_are_mixed_and_muting_silences_them() {
        let mix = Mix::default();
        mix.push("a", &[100; 4]);
        mix.push("b", &[i16::MAX, 5]);
        let mut out = [7i16; 4];
        mix.read(&mut out);
        assert_eq!(out, [i16::MAX, 105, 100, 100]);
        mix.push("a", &[9; 4]);
        mix.muted.store(true, Ordering::Relaxed);
        mix.read(&mut out);
        assert_eq!(out, [0; 4], "muted: drained but not played");
    }

    #[test]
    fn equal_rates_pass_samples_through() {
        let mut rs = Resampler::new(48_000, 48_000);
        let mut got = Vec::new();
        rs.run(&[1, 2, 3, 4], 4, |s| got.push(s));
        assert_eq!(got, [1, 2, 3, 4]);
    }

    #[test]
    fn halving_the_rate_keeps_every_other_sample() {
        let mut rs = Resampler::new(96_000, 48_000);
        assert_eq!(rs.out_len(8), 4);
        let mut got = Vec::new();
        rs.run(&[0, 10, 20, 30, 40, 50, 60, 70], 4, |s| got.push(s));
        assert_eq!(got, [0, 20, 40, 60]);
    }

    #[test]
    fn doubling_the_rate_fills_in_between() {
        let mut rs = Resampler::new(24_000, 48_000);
        assert_eq!(rs.needed(4), 2);
        let mut got = Vec::new();
        rs.run(&[0, 100], 4, |s| got.push(s));
        assert_eq!(got, [0, 50, 100, 100]);
    }
}
