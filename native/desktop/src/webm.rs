//! A `WebM` file from encoded frames: VP8 video and Opus audio, as the old
//! Desktop's clips were. The whole clip is in memory (20 s at most), so
//! every size is known when it is written: a `SeekHead`, the clip's Info and
//! Tracks, a Cluster at each video keyframe, and Cues to seek by.

/// Which track a frame belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Video = 1,
    Audio = 2,
}

/// One encoded frame and its time from the clip's start.
#[derive(Debug, Clone)]
pub struct Frame {
    pub track: Track,
    pub ms: u64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// What the file needs to know besides the frames.
#[derive(Debug, Clone, Copy)]
pub struct Clip {
    pub width: u32,
    pub height: u32,
    /// Opus's lookahead, in 48 kHz samples (the stream's pre-skip).
    pub pre_skip: u16,
    /// How long the clip runs, ms.
    pub duration_ms: u64,
}

// Element ids (Matroska / `WebM`).
const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;

/// An element id's bytes (ids carry their own length marker).
fn id_bytes(id: u32) -> Vec<u8> {
    let b = id.to_be_bytes();
    let skip = b.iter().take_while(|x| **x == 0).count();
    b[skip..].to_vec()
}

/// A size as an EBML variable-length integer, in as few bytes as it fits.
#[must_use]
pub fn vint(n: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // All ones is reserved ("unknown size").
        let max = (1u64 << (7 * len)) - 1;
        if n < max {
            let marked = n | (1u64 << (7 * len));
            let bytes = marked.to_be_bytes();
            return bytes[8 - len as usize..].to_vec();
        }
    }
    // Beyond any clip this writes; the largest 8-byte size.
    vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend(vint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|x| **x == 0).count().min(7);
    element(id, &b[skip..])
}

/// A uint always 8 bytes long, so its element's size never depends on it.
fn uint8(id: u32, v: u64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

fn float(id: u32, v: f64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

fn text(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

/// Opus's identification header (RFC 7845), the track's `CodecPrivate`.
fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.push(1); // version
    h.push(1); // channels
    h.extend(pre_skip.to_le_bytes());
    h.extend(48_000u32.to_le_bytes());
    h.extend(0i16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family
    h
}

fn header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(uint(0x4286, 1)); // EBMLVersion
    b.extend(uint(0x42F7, 1)); // EBMLReadVersion
    b.extend(uint(0x42F2, 4)); // EBMLMaxIDLength
    b.extend(uint(0x42F3, 8)); // EBMLMaxSizeLength
    b.extend(text(0x4282, "webm")); // DocType
    b.extend(uint(0x4287, 4)); // DocTypeVersion
    b.extend(uint(0x4285, 2)); // DocTypeReadVersion
    element(EBML, &b)
}

#[expect(clippy::cast_precision_loss, reason = "a clip's length in ms")]
fn info(clip: &Clip) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(uint(0x2A_D7B1, 1_000_000)); // TimestampScale: ms
    b.extend(float(0x4489, clip.duration_ms as f64)); // Duration
    b.extend(text(0x4D80, "Tawny")); // MuxingApp
    b.extend(text(0x5741, "Tawny")); // WritingApp
    element(INFO, &b)
}

fn tracks(clip: &Clip) -> Vec<u8> {
    let mut video = Vec::new();
    video.extend(uint(0xD7, 1)); // TrackNumber
    video.extend(uint(0x73C5, 1)); // TrackUID
    video.extend(uint(0x83, 1)); // TrackType: video
    video.extend(uint(0x9C, 0)); // FlagLacing
    video.extend(text(0x86, "V_VP8")); // CodecID
    let mut v = Vec::new();
    v.extend(uint(0xB0, u64::from(clip.width))); // PixelWidth
    v.extend(uint(0xBA, u64::from(clip.height))); // PixelHeight
    video.extend(element(0xE0, &v));

    let mut audio = Vec::new();
    audio.extend(uint(0xD7, 2));
    audio.extend(uint(0x73C5, 2));
    audio.extend(uint(0x83, 2)); // TrackType: audio
    audio.extend(uint(0x9C, 0));
    audio.extend(text(0x86, "A_OPUS"));
    audio.extend(element(0x63A2, &opus_head(clip.pre_skip))); // CodecPrivate
    // CodecDelay (the pre-skip in ns) and SeekPreRoll (80 ms), as `WebM` asks.
    audio.extend(uint(
        0x56AA,
        u64::from(clip.pre_skip) * 1_000_000_000 / 48_000,
    ));
    audio.extend(uint(0x56BB, 80_000_000));
    let mut a = Vec::new();
    a.extend(float(0xB5, 48_000.0)); // SamplingFrequency
    a.extend(uint(0x9F, 1)); // Channels
    audio.extend(element(0xE1, &a));

    let mut b = element(0xAE, &video);
    b.extend(element(0xAE, &audio));
    element(TRACKS, &b)
}

/// A frame as a `SimpleBlock`, `rel` ms after its cluster's time.
fn simple_block(f: &Frame, rel: i16) -> Vec<u8> {
    let mut b = vec![0x80 | f.track as u8];
    b.extend(rel.to_be_bytes());
    // Every Opus packet stands alone; video only at keyframes.
    let key = f.track == Track::Audio || f.key;
    b.push(if key { 0x80 } else { 0 });
    b.extend_from_slice(&f.data);
    element(0xA3, &b)
}

/// The clusters, and for each video keyframe that starts one, its time and
/// where it sits from the start of the clusters.
fn clusters(frames: &[Frame]) -> (Vec<u8>, Vec<(u64, u64)>) {
    let mut out = Vec::new();
    let mut cues = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut start: Option<u64> = None;
    let close = |out: &mut Vec<u8>, body: &mut Vec<u8>| {
        if !body.is_empty() {
            out.extend(element(CLUSTER, body));
            body.clear();
        }
    };
    for f in frames {
        let video_key = f.track == Track::Video && f.key;
        let far = start.is_none_or(|s| f.ms.saturating_sub(s) > 30_000);
        if (video_key && start.is_some_and(|s| f.ms > s)) || far || start.is_none() {
            close(&mut out, &mut body);
            start = Some(f.ms);
            body.extend(uint(0xE7, f.ms)); // Timestamp
            if video_key {
                cues.push((f.ms, out.len() as u64));
            }
        } else if video_key && cues.last().is_none_or(|c| c.1 != out.len() as u64) {
            // A keyframe at the very start of the cluster it shares.
            cues.push((f.ms, out.len() as u64));
        }
        let rel = i16::try_from(f.ms - start.unwrap_or(f.ms)).unwrap_or(i16::MAX);
        body.extend(simple_block(f, rel));
    }
    close(&mut out, &mut body);
    (out, cues)
}

fn cues(points: &[(u64, u64)], clusters_at: u64) -> Vec<u8> {
    let mut b = Vec::new();
    for &(ms, pos) in points {
        let mut tp = uint(0xF7, 1); // CueTrack
        tp.extend(uint(0xF1, clusters_at + pos)); // CueClusterPosition
        let mut cp = uint(0xB3, ms); // CueTime
        cp.extend(element(0xB7, &tp));
        b.extend(element(0xBB, &cp));
    }
    element(CUES, &b)
}

fn seek_head(info_at: u64, tracks_at: u64, cues_at: u64) -> Vec<u8> {
    let mut b = Vec::new();
    for (id, at) in [(INFO, info_at), (TRACKS, tracks_at), (CUES, cues_at)] {
        let mut s = element(SEEK_ID, &id_bytes(id));
        s.extend(uint8(SEEK_POSITION, at));
        b.extend(element(SEEK, &s));
    }
    element(SEEK_HEAD, &b)
}

/// The clip as a `WebM` file. Frames are put in time order (video first at a
/// tie) and must not run longer than 30 s between video keyframes.
#[must_use]
pub fn write(clip: &Clip, frames: &mut [Frame]) -> Vec<u8> {
    frames.sort_by_key(|f| (f.ms, f.track as u8));
    let info = info(clip);
    let tracks = tracks(clip);
    let (clusters, points) = clusters(frames);
    // The `SeekHead`'s size doesn't depend on the positions (8-byte values),
    // so it can be measured first with stand-ins.
    let head_len = seek_head(0, 0, 0).len() as u64;
    let info_at = head_len;
    let tracks_at = info_at + info.len() as u64;
    let clusters_at = tracks_at + tracks.len() as u64;
    let cues_at = clusters_at + clusters.len() as u64;
    let cues = cues(&points, clusters_at);
    let mut seg = seek_head(info_at, tracks_at, cues_at);
    seg.extend(info);
    seg.extend(tracks);
    seg.extend(clusters);
    seg.extend(cues);
    let mut out = header();
    out.extend(element(SEGMENT, &seg));
    out
}

#[cfg(test)]
mod tests {
    use super::{
        CLUSTER, CUES, Clip, EBML, Frame, INFO, SEEK_HEAD, SEGMENT, TRACKS, Track, vint, write,
    };

    /// Reads an element: (id, header length, body length).
    fn read(b: &[u8]) -> (u32, usize, usize) {
        let id_len = b[0].leading_zeros() as usize + 1;
        let id = b[..id_len].iter().fold(0u32, |a, x| a << 8 | u32::from(*x));
        let s = &b[id_len..];
        let len = s[0].leading_zeros() as usize + 1;
        let mut size = u64::from(s[0]) & ((1u64 << (8 - len)) - 1);
        for x in &s[1..len] {
            size = size << 8 | u64::from(*x);
        }
        (id, id_len + len, usize::try_from(size).expect("fits"))
    }

    fn children(b: &[u8]) -> Vec<(u32, usize, &[u8])> {
        let mut at = 0;
        let mut out = Vec::new();
        while at < b.len() {
            let (id, head, len) = read(&b[at..]);
            out.push((id, at, &b[at + head..at + head + len]));
            at += head + len;
        }
        out
    }

    #[test]
    fn sizes_take_the_fewest_bytes() {
        assert_eq!(vint(0), [0x80]);
        assert_eq!(vint(126), [0xFE]);
        assert_eq!(vint(127), [0x40, 0x7F], "0x7F alone means unknown");
        assert_eq!(vint(300), [0x41, 0x2C]);
        assert_eq!(vint(1 << 20), [0x30, 0x00, 0x00]);
    }

    fn clip() -> Vec<u8> {
        let mut frames = Vec::new();
        for i in 0..60u64 {
            frames.push(Frame {
                track: Track::Video,
                ms: i * 40,
                key: i % 30 == 0,
                data: vec![u8::try_from(i).expect("small"); 100],
            });
        }
        for i in 0..120u64 {
            frames.push(Frame {
                track: Track::Audio,
                ms: i * 20,
                key: true,
                data: vec![7; 40],
            });
        }
        let c = Clip {
            width: 640,
            height: 360,
            pre_skip: 312,
            duration_ms: 2400,
        };
        write(&c, &mut frames)
    }

    #[test]
    fn a_clip_is_a_webm_with_every_frame_in_order() {
        let file = clip();
        let top = children(&file);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, EBML);
        assert!(top[0].2.windows(4).any(|w| w == b"webm"));
        assert_eq!(top[1].0, SEGMENT);
        let seg = children(top[1].2);
        let ids: Vec<u32> = seg.iter().map(|c| c.0).collect();
        assert_eq!(ids[..3], [SEEK_HEAD, INFO, TRACKS]);
        assert_eq!(*ids.last().expect("cues"), CUES);
        let tracks = top[1].2;
        assert!(tracks.windows(5).any(|w| w == b"V_VP8"));
        assert!(tracks.windows(6).any(|w| w == b"A_OPUS"));
        assert!(tracks.windows(8).any(|w| w == b"OpusHead"));
        // Two keyframes, two clusters; 180 blocks with rising times.
        let clusters: Vec<_> = seg.iter().filter(|c| c.0 == CLUSTER).collect();
        assert_eq!(clusters.len(), 2);
        let mut last = 0u64;
        let mut blocks = 0;
        for (_, _, body) in &clusters {
            let kids = children(body);
            assert_eq!(kids[0].0, 0xE7, "cluster time first");
            let base = kids[0].2.iter().fold(0u64, |a, x| a << 8 | u64::from(*x));
            for (id, _, b) in &kids[1..] {
                assert_eq!(*id, 0xA3);
                let t = base + u64::from(u16::from_be_bytes([b[1], b[2]]));
                assert!(t >= last, "{t} after {last}");
                last = t;
                blocks += 1;
            }
        }
        assert_eq!(blocks, 180);
        // The `SeekHead` points at what is there; the cues at the clusters.
        let seg_body = top[1].2;
        for (id, at) in [(INFO, seg[1].1), (TRACKS, seg[2].1)] {
            assert_eq!(read(&seg_body[at..]).0, id);
        }
        let cue_body = seg.last().expect("cues").2;
        for (_, _, point) in children(cue_body) {
            let pos = children(children(point)[1].2)[1].2;
            let at = pos.iter().fold(0usize, |a, x| a << 8 | usize::from(*x));
            assert_eq!(read(&seg_body[at..]).0, CLUSTER);
        }
    }
}
