//! Basic Pitch (Spotify, ICASSP 2022; Apache-2.0): a small network that
//! hears the notes of any instrument. For every frame (256 samples at
//! 22 050 Hz, 11.6 ms) it gives, for each of the 88 piano keys, how likely a
//! note sounds (`note`) and starts (`onset`), and a finer pitch picture
//! (`contour`: 264 bins of a third of a semitone from A0).
//!
//! The same steps as the package's `inference.py` and `note_creation.py`
//! (through the research port that was checked note for note against it):
//! the audio to mono at 22 050 Hz (scipy's `resample_poly` filter), windows
//! of 2 s less one hop overlapping by 30 frames, the network on each, 15
//! frames cut from each end of a window, then the posteriorgrams decoded
//! into notes as `output_to_notes_polyphonic` does — notes from the onset
//! peaks, then the "melodia trick" for held notes whose onset was not seen.
//! Two things differ, both corrections: a frame's time is exact (kept frame
//! k of window w is sample w·36 164 + k·256 of the audio; the package
//! approximates it), and a note's pitch comes from the contour with MIDI 21
//! at bin 1, the centre of its key's three bins (the package centres its
//! pitch bends one bin low, so they read 33 cents sharp).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;

/// The network's audio rate.
pub const SR: usize = 22_050;
/// Samples a frame: 86.13 frames a second.
pub const FFT_HOP: usize = 256;
/// One window: 2 s less one hop, the length the network was made for.
pub(crate) const N_WIN: usize = 2 * SR - FFT_HOP;
/// The frames the network gives for a window …
pub(crate) const WIN_FRAMES: usize = 172;
/// … of which 15 at each end overlap the neighbours and are cut.
pub(crate) const N_OLAP: usize = 30;
pub(crate) const OLAP: usize = N_OLAP * FFT_HOP;
/// Samples from one window to the next.
pub(crate) const HOP: usize = N_WIN - OLAP;
/// The frames kept of a window.
pub(crate) const KEEP: usize = WIN_FRAMES - N_OLAP;
/// Piano keys (MIDI 21…108) of the note and onset pictures.
pub const KEYS: usize = 88;
/// Contour bins, a third of a semitone each.
pub const BINS: usize = 264;
pub const MIDI0: u8 = 21;
/// The contour bin of MIDI 21 (bin 0 is a third of a semitone under it).
const CBIN0: usize = 1;
/// Windows a run of the network.
const BATCH: usize = 64;
const INPUT: &str = "serving_default_input_2:0";
const OUT_NOTE: &str = "StatefulPartitionedCall:1";
const OUT_ONSET: &str = "StatefulPartitionedCall:2";
const OUT_CONTOUR: &str = "StatefulPartitionedCall:0";

/// scipy's `firwin(41, 0.5, window=("kaiser", 5.0))`: the filter
/// `resample_poly` halves the rate with.
fn halfband() -> [f64; 41] {
    /// The modified Bessel function of order 0 by its series (x ≤ 5 here).
    fn i0(x: f64) -> f64 {
        let q = x * x / 4.0;
        let (mut s, mut t) = (1.0f64, 1.0f64);
        for k in 1..200 {
            t *= q / (k * k) as f64;
            s += t;
            if t < 1e-18 * s {
                break;
            }
        }
        s
    }
    let (beta, alpha) = (5.0, 20.0);
    let mut h = [0f64; 41];
    for (n, v) in h.iter_mut().enumerate() {
        let m = n as f64 - alpha;
        let y = std::f64::consts::PI * (0.5 * m);
        let sinc = if m == 0.0 { 1.0 } else { y.sin() / y };
        let r = m / alpha;
        let w = i0(beta * (1.0 - r * r).sqrt()) / i0(beta);
        *v = (0.5 * sinc) * w;
    }
    let s: f64 = h.iter().sum();
    for v in &mut h {
        *v /= s;
    }
    h
}

/// Half the rate, as scipy's `resample_poly(x, 1, 2)` does it for f32
/// audio, to the bit: its filter cast to f32, zeros outside the signal,
/// output sample o centred on input sample 2·o, the products summed in f32
/// from the oldest input sample to the newest (`upfirdn`'s order).
/// `ceil(n / 2)` samples.
pub fn halve(x: &[f32]) -> Vec<f32> {
    let h: Vec<f32> = halfband().iter().map(|&v| v as f32).collect();
    let n = x.len();
    (0..n.div_ceil(2))
        .map(|o| {
            // input sample 2·o − 20 + j meets tap 40 − j
            let mut s = 0f32;
            for j in 0..41 {
                if let Some(c) = (2 * o + j).checked_sub(20).filter(|&c| c < n) {
                    s += x[c] * h[40 - j];
                }
            }
            s
        })
        .collect()
}

/// `halve`'s filter, as it uses it (cast to f32).
pub fn halfband32() -> [f32; 41] {
    halfband().map(|v| v as f32)
}

/// Output sample o of `halve` with the input read through `at` (None outside
/// it): `h` from `halfband32`, the products summed in the same order, so a
/// stream halved a few samples at a time is `halve` of it whole, to the bit
/// — and, with `at` None past the samples in so far, what `halve` makes of
/// the input ending there.
pub fn halve_at(h: &[f32; 41], at: impl Fn(usize) -> Option<f32>, o: usize) -> f32 {
    let mut s = 0f32;
    for j in 0..41 {
        if let Some(v) = (2 * o + j).checked_sub(20).and_then(&at) {
            s += v * h[40 - j];
        }
    }
    s
}

/// Stereo at 44.1 kHz → the network's input: mono (L + R) / 2, then half
/// the rate.
pub fn to22(l: &[f32], r: &[f32]) -> Vec<f32> {
    let m: Vec<f32> = l.iter().zip(r).map(|(a, b)| (a + b) * 0.5).collect();
    halve(&m)
}

/// The network's pictures of a stretch of audio, frame by frame.
pub struct Post {
    pub frames: usize,
    /// `[frames][KEYS]`
    pub note: Vec<f32>,
    /// `[frames][KEYS]`
    pub onset: Vec<f32>,
    /// `[frames][BINS]`
    pub contour: Vec<f32>,
}

/// The sample (at `SR`) kept frame k sits at.
pub(crate) fn frame_sample(k: usize) -> usize {
    (k / KEEP) * HOP + (k % KEEP) * FFT_HOP
}

/// Frame k's time in the audio (s).
pub fn frame_time(k: usize) -> f64 {
    frame_sample(k) as f64 / SR as f64
}

/// The note network on the pack's runtime, on the CPU: it is small (a
/// 4-minute track in under a second).
pub struct Model {
    session: ort::session::Session,
}

impl Model {
    /// `runtime`: the pack's folder (its ONNX Runtime); `model`: the network's file.
    pub fn open(runtime: &Path, model: &Path) -> Result<Model, String> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
        Model::open_threads(runtime, model, threads)
    }

    /// The same on `threads` threads (one: on the caller's own thread — a
    /// live stream's notes, beside the sound).
    pub fn open_threads(runtime: &Path, model: &Path, threads: usize) -> Result<Model, String> {
        crate::spatial::core::runtime(runtime)?;
        let e = |e: ort::Error<ort::session::builder::SessionBuilder>| e.to_string();
        let session = ort::session::Session::builder()
            .map_err(|e| e.to_string())?
            .with_intra_threads(threads)
            .map_err(e)?
            .commit_from_file(model)
            .map_err(|e| e.to_string())?;
        Ok(Model { session })
    }

    /// The pictures of mono audio at `SR`: every frame whose time is inside
    /// the audio (the package drops ~0.3 s a minute at the end).
    pub fn run(&mut self, x: &[f32]) -> Result<Post, String> {
        post_of(self, x)
    }
}

/// The network over whole windows — what a file's pictures are made of, and
/// what a live stream asks of it a few windows at a time (a test stands in
/// for it with its own).
pub trait Windows {
    /// `nb` windows of `N_WIN` samples, one after another → the network's
    /// note, onset and contour pictures of each: `WIN_FRAMES` frames a
    /// window, window after window (none cut yet).
    fn windows(&mut self, input: Vec<f32>, nb: usize) -> Result<[Vec<f32>; 3], String>;
}

impl Windows for Model {
    fn windows(&mut self, input: Vec<f32>, nb: usize) -> Result<[Vec<f32>; 3], String> {
        let t = ort::value::Tensor::from_array(([nb, N_WIN, 1usize], input)).map_err(|e| e.to_string())?;
        let out = self.session.run(ort::inputs![INPUT => t]).map_err(|e| e.to_string())?;
        let mut got: [Vec<f32>; 3] = Default::default();
        for (dst, (name, width)) in got.iter_mut().zip([(OUT_NOTE, KEYS), (OUT_ONSET, KEYS), (OUT_CONTOUR, BINS)]) {
            let (shape, v) = out[name].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            let want = [nb as i64, WIN_FRAMES as i64, width as i64];
            if shape.iter().copied().collect::<Vec<i64>>() != want {
                return Err(format!("{name}: shape {shape:?}, expected {want:?}"));
            }
            dst.extend_from_slice(&v[..nb * WIN_FRAMES * width]);
        }
        Ok(got)
    }
}

/// A window's kept frames of the network's pictures (`WIN_FRAMES` frames of
/// `width` values each): the middle, 15 frames cut from each end.
pub(crate) fn kept(win: &[f32], width: usize) -> &[f32] {
    &win[N_OLAP / 2 * width..(WIN_FRAMES - N_OLAP / 2) * width]
}

/// The pictures of mono audio at `SR` by `net`, as `Model::run` makes them:
/// every frame whose time is inside the audio.
pub fn post_of(net: &mut dyn Windows, x: &[f32]) -> Result<Post, String> {
    // OLAP / 2 zeros in front: the first kept frame is the audio's first sample
    let padded = OLAP / 2 + x.len();
    let wins = padded.div_ceil(HOP);
    let at = |i: usize| if i < OLAP / 2 || i >= padded { 0.0 } else { x[i - OLAP / 2] };
    let (mut note, mut onset, mut contour) = (Vec::new(), Vec::new(), Vec::new());
    for b0 in (0..wins).step_by(BATCH) {
        let nb = BATCH.min(wins - b0);
        let mut input = vec![0f32; nb * N_WIN];
        for w in 0..nb {
            let s = (b0 + w) * HOP;
            for (i, v) in input[w * N_WIN..(w + 1) * N_WIN].iter_mut().enumerate() {
                *v = at(s + i);
            }
        }
        let got = net.windows(input, nb)?;
        for ((src, dst), width) in got.iter().zip([&mut note, &mut onset, &mut contour]).zip([KEYS, KEYS, BINS]) {
            for w in 0..nb {
                dst.extend_from_slice(kept(&src[w * WIN_FRAMES * width..(w + 1) * WIN_FRAMES * width], width));
            }
        }
    }
    let frames = (0..note.len() / KEYS).take_while(|&k| frame_sample(k) < x.len()).count();
    note.truncate(frames * KEYS);
    onset.truncate(frames * KEYS);
    contour.truncate(frames * BINS);
    Ok(Post { frames, note, onset, contour })
}

/// How the pictures become notes.
#[derive(Clone, Debug)]
pub struct Params {
    pub onset_thresh: f64,
    pub frame_thresh: f64,
    /// A note must last more than this many frames.
    pub min_note_frames: usize,
    pub infer_onsets: bool,
    pub melodia_trick: bool,
    /// Frames under `frame_thresh` a note may bridge.
    pub energy_tol: usize,
}

impl Default for Params {
    /// The package's own, but the shortest note: 58 ms (5 frames, so notes of
    /// 70 ms and longer) for 127.7 ms — bebop lines are faster than that.
    fn default() -> Params {
        Params {
            onset_thresh: 0.5,
            frame_thresh: 0.3,
            min_note_frames: 5,
            infer_onsets: true,
            melodia_trick: true,
            energy_tol: 11,
        }
    }
}

/// A decoded note: frames `[i0, i1)` on one key; `amp` its mean activation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    pub i0: usize,
    pub i1: usize,
    pub midi: u8,
    pub amp: f64,
}

/// The package's `get_infered_onsets`: onsets also where a key's activation
/// jumps (the smaller rise over one and over two frames), scaled so the
/// biggest jump is as strong as the strongest onset.
fn infer_onsets(onsets: &[f64], frames: &[f64], t: usize) -> Vec<f64> {
    let mut fd = jumps(frames, t);
    let mx = fd.iter().cloned().fold(0f64, f64::max);
    if mx > 0.0 {
        let top = onsets.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        for v in &mut fd {
            *v = top * *v / mx;
        }
    }
    onsets.iter().zip(&fd).map(|(a, b)| a.max(*b)).collect()
}

/// How far each key's activation jumps in each of `t` frames: the smaller
/// rise over one and over two frames (none in the first two).
pub(crate) fn jumps(frames: &[f64], t: usize) -> Vec<f64> {
    let mut fd = vec![0f64; t * KEYS];
    for i in 2..t {
        for f in 0..KEYS {
            let v = frames[i * KEYS + f];
            let d = (v - frames[(i - 1) * KEYS + f]).min(v - frames[(i - 2) * KEYS + f]);
            fd[i * KEYS + f] = d.max(0.0);
        }
    }
    fd
}

fn column_mean(frames: &[f64], i0: usize, i1: usize, f: usize) -> f64 {
    (i0..i1).map(|i| frames[i * KEYS + f]).sum::<f64>() / (i1 - i0) as f64
}

/// A note's frames are taken off the picture, with the keys next to it.
fn clear(rem: &mut [f64], i: usize, f: usize) {
    rem[i * KEYS + f] = 0.0;
    if f + 1 < KEYS {
        rem[i * KEYS + f + 1] = 0.0;
    }
    if f > 0 {
        rem[i * KEYS + f - 1] = 0.0;
    }
}

/// `output_to_notes_polyphonic`, step for step and in f64 like the package
/// (same notes in the same order): each onset peak over `onset_thresh`, the
/// last first, holds while the key stays over `frame_thresh` (gaps up to
/// `energy_tol` frames); then, while anything left is over `frame_thresh`,
/// the strongest frame left grows both ways into a note.
pub fn decode(post: &Post, p: &Params) -> Vec<Event> {
    let n = post.frames;
    if n < 2 {
        return Vec::new();
    }
    let frames: Vec<f64> = post.note.iter().map(|&v| v as f64).collect();
    let mut onsets: Vec<f64> = post.onset.iter().map(|&v| v as f64).collect();
    if p.infer_onsets {
        onsets = infer_onsets(&onsets, &frames, n);
    }
    let at = |i: usize, f: usize| i * KEYS + f;
    // peaks in time (scipy's argrelmax: strictly over both neighbours, never
    // the first or last frame), in the package's order: frame, then key
    let mut starts = Vec::new();
    for i in 1..n - 1 {
        for f in 0..KEYS {
            let v = onsets[at(i, f)];
            if v > onsets[at(i - 1, f)] && v > onsets[at(i + 1, f)] && v >= p.onset_thresh {
                starts.push((i, f));
            }
        }
    }
    let mut rem = frames.clone();
    let mut ev = Vec::new();
    for &(s0, f) in starts.iter().rev() {
        if s0 >= n - 1 {
            continue;
        }
        let (mut i, mut k) = (s0 + 1, 0);
        while i < n - 1 && k < p.energy_tol {
            if rem[at(i, f)] < p.frame_thresh {
                k += 1;
            } else {
                k = 0;
            }
            i += 1;
        }
        i -= k;
        if i - s0 <= p.min_note_frames {
            continue;
        }
        for j in s0..i {
            clear(&mut rem, j, f);
        }
        ev.push(Event { i0: s0, i1: i, midi: MIDI0 + f as u8, amp: column_mean(&frames, s0, i, f) });
    }
    if p.melodia_trick {
        // the package takes the maximum of what is left each time (the first
        // in frame-then-key order among equals); values only ever drop to
        // zero, so a heap with the stale entries skipped gives the same order
        let mut heap: BinaryHeap<(u64, Reverse<usize>)> = rem
            .iter()
            .enumerate()
            .filter(|(_, &v)| v > p.frame_thresh)
            .map(|(i, &v)| (v.to_bits(), Reverse(i)))
            .collect();
        while let Some((bits, Reverse(idx))) = heap.pop() {
            if rem[idx].to_bits() != bits {
                continue;
            }
            let (i_mid, f) = (idx / KEYS, idx % KEYS);
            rem[idx] = 0.0;
            let (mut i, mut k) = (i_mid + 1, 0);
            while i < n - 1 && k < p.energy_tol {
                if rem[at(i, f)] < p.frame_thresh {
                    k += 1;
                } else {
                    k = 0;
                }
                clear(&mut rem, i, f);
                i += 1;
            }
            let i_end = i - 1 - k;
            let (mut i, mut k) = (i_mid as isize - 1, 0);
            while i > 0 && k < p.energy_tol {
                if rem[at(i as usize, f)] < p.frame_thresh {
                    k += 1;
                } else {
                    k = 0;
                }
                clear(&mut rem, i as usize, f);
                i -= 1;
            }
            let i_start = (i + 1 + k as isize) as usize;
            if i_end as isize - i_start as isize <= p.min_note_frames as isize {
                continue;
            }
            ev.push(Event {
                i0: i_start,
                i1: i_end,
                midi: MIDI0 + f as u8,
                amp: column_mean(&frames, i_start, i_end, f),
            });
        }
    }
    ev
}

/// scipy's `gaussian(51, std=5)`: the pitch-bend window.
fn bend_window() -> [f64; 51] {
    let mut g = [0f64; 51];
    for (n, v) in g.iter_mut().enumerate() {
        let m = n as f64 - 25.0;
        *v = (-(m * m) / 50.0).exp();
    }
    g
}

/// A note's pitch per frame (MIDI, fractional) and the contour's value at
/// it: in each frame the peak of the contour weighted by the package's
/// pitch-bend window (±25 bins around the key's own bin), refined by a
/// parabola through the peak and its neighbours.
pub fn pitch_track(post: &Post, i0: usize, i1: usize, midi: u8) -> (Vec<f64>, Vec<f32>) {
    const TOL: isize = 25;
    let g = bend_window();
    let b0 = 3 * (midi - MIDI0) as isize + CBIN0 as isize;
    let lo = (b0 - TOL).max(0) as usize;
    let hi = ((b0 + TOL + 1) as usize).min(BINS);
    let g0 = (lo as isize - (b0 - TOL)) as usize;
    let width = hi - lo;
    let (mut pitch, mut peak) = (Vec::with_capacity(i1 - i0), Vec::with_capacity(i1 - i0));
    for i in i0..i1 {
        let raw = &post.contour[i * BINS + lo..i * BINS + hi];
        let mut a = 0;
        let mut best = f64::NEG_INFINITY;
        for (j, &v) in raw.iter().enumerate() {
            let w = v as f64 * g[g0 + j];
            if w > best {
                best = w;
                a = j;
            }
        }
        let v0 = raw[a];
        let (vm, vp) = (raw[a.saturating_sub(1)], raw[(a + 1).min(width - 1)]);
        // in f32 as the port does
        let den = vm - 2.0 * v0 + vp;
        let d = if den < 0.0 && a > 0 && a < width - 1 { (0.5 * (vm - vp) / den).clamp(-0.5, 0.5) } else { 0.0 };
        pitch.push(MIDI0 as f64 + ((lo + a) as f64 + d as f64 - CBIN0 as f64) / 3.0);
        peak.push(v0);
    }
    (pitch, peak)
}

/// A note of a source.
#[derive(Clone, Debug)]
pub struct Note {
    /// Start and end (s).
    pub on: f64,
    pub off: f64,
    /// MIDI, fractional: the median of its pitch track where the contour is
    /// at least half its peak.
    pub pitch: f64,
    /// Its key.
    pub midi: u8,
    /// Mean note activation (0…1).
    pub amp: f64,
    /// Its frames `[i0, i1)`.
    pub i0: usize,
    pub i1: usize,
}

/// The notes of a source with the pictures they came from and each note's
/// pitch track (and the contour's value along it).
pub struct Transcript {
    pub post: Post,
    pub notes: Vec<Note>,
    pub tracks: Vec<Vec<f32>>,
    #[allow(dead_code)] // kept with the transcript for diagnostics
    pub track_peaks: Vec<Vec<f32>>,
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The pictures → notes, ordered by start (then key).
pub fn transcribe(post: Post, p: &Params) -> Transcript {
    let mut ev = decode(&post, p);
    ev.sort_by_key(|e| (e.i0, e.midi));
    let last = post.frames.saturating_sub(1);
    let (mut notes, mut tracks, mut track_peaks) = (Vec::new(), Vec::new(), Vec::new());
    for e in ev {
        let (pt, pv) = pitch_track(&post, e.i0, e.i1, e.midi);
        let top = pv.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let good: Vec<f64> = pt.iter().zip(&pv).filter(|(_, &v)| v >= 0.5 * top).map(|(&p, _)| p).collect();
        let pitch = if good.is_empty() { e.midi as f64 } else { median(good) };
        notes.push(Note {
            on: frame_time(e.i0),
            off: frame_time(e.i1.min(last)),
            pitch,
            midi: e.midi,
            amp: e.amp,
            i0: e.i0,
            i1: e.i1,
        });
        tracks.push(pt.iter().map(|&v| v as f32).collect());
        track_peaks.push(pv);
    }
    Transcript { post, notes, tracks, track_peaks }
}

#[cfg(test)]
pub(crate) mod npy {
    use std::path::Path;

    /// A little-endian C-order .npy of f32 or f64 → (shape, values).
    pub fn read(path: &Path) -> (Vec<usize>, Vec<f64>) {
        let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(&b[1..6], b"NUMPY");
        let (hl, start) = if b[6] == 1 {
            (u16::from_le_bytes([b[8], b[9]]) as usize, 10)
        } else {
            (u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize, 12)
        };
        let head = std::str::from_utf8(&b[start..start + hl]).unwrap();
        assert!(head.contains("'fortran_order': False"), "{head}");
        let s = head.split("'shape': (").nth(1).unwrap();
        let shape = s[..s.find(')').unwrap()].split(',').filter_map(|v| v.trim().parse().ok()).collect();
        let data = &b[start + hl..];
        let vals = if head.contains("'<f4'") {
            data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect()
        } else if head.contains("'<f8'") {
            data.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
        } else {
            panic!("dtype: {head}")
        };
        (shape, vals)
    }

    pub fn f32s(path: &Path) -> Vec<f32> {
        read(path).1.into_iter().map(|v| v as f32).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Synthetic tones, three 30 s excerpts (chords, a guitar, a double
    /// bass) and a whole track (232 s: the network's batches of 64 windows).
    const CASES: [&str; 5] = ["synth", "ow_other", "vakh_guitar", "bebop_bass", "ow_other_full"];

    /// The Python port's dumps (`make_refs.py`): `AURA_NOTES_REFS` = its refs folder.
    fn refs() -> Option<PathBuf> {
        std::env::var_os("AURA_NOTES_REFS").map(Into::into)
    }

    /// The pack's folder (its runtime) and the network's file.
    fn network() -> Option<Model> {
        let pack = std::env::var_os("AURA_SPATIAL_PACK_DIR")?;
        let model = std::env::var_os("AURA_NOTES_MODEL")?;
        Some(Model::open(Path::new(&pack), Path::new(&model)).unwrap())
    }

    /// A Python note's frames and key.
    fn key_of(w: &serde_json::Value) -> (usize, usize, u8) {
        let u = |k: &str| w[k].as_u64().unwrap() as usize;
        (u("i0"), u("i1"), u("midi") as u8)
    }

    fn post_of(d: &Path, case: &str) -> Post {
        let note = npy::f32s(&d.join(format!("{case}.note.npy")));
        let onset = npy::f32s(&d.join(format!("{case}.onset.npy")));
        let contour = npy::f32s(&d.join(format!("{case}.contour.npy")));
        Post { frames: note.len() / KEYS, note, onset, contour }
    }

    fn python(d: &Path, case: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(d.join(format!("{case}.notes.json"))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn the_filter_halves_the_band() {
        let h = halfband();
        assert!((h.iter().sum::<f64>() - 1.0).abs() < 1e-15);
        for j in 0..20 {
            assert_eq!(h[j], h[40 - j]);
        }
        // a half-band filter: every second tap off the centre is zero
        for m in (2..=20).step_by(2) {
            assert!(h[20 + m].abs() < 1e-16, "{m}: {}", h[20 + m]);
        }
        // DC passes, the new Nyquist (a quarter of the old rate) is half, the old Nyquist is gone
        let at = |w: f64| h.iter().enumerate().map(|(n, v)| v * (w * (n as f64 - 20.0)).cos()).sum::<f64>();
        assert!((at(0.0) - 1.0).abs() < 1e-12);
        assert!((at(std::f64::consts::FRAC_PI_2) - 0.5).abs() < 1e-3);
        assert!(at(std::f64::consts::PI).abs() < 1e-3);
    }

    /// Halved a few samples at a time (`halve_at`) = `halve` of the whole,
    /// to the bit; with the input ending somewhere = `halve` of it to there.
    #[test]
    fn halving_a_few_samples_at_a_time_is_halving_the_whole() {
        let mut seed = 99u32;
        let x: Vec<f32> = (0..10_007)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let h = halfband32();
        let whole = halve(&x);
        let got: Vec<f32> = (0..whole.len()).map(|o| halve_at(&h, |c| x.get(c).copied(), o)).collect();
        assert!(got.iter().zip(&whole).all(|(a, b)| a.to_bits() == b.to_bits()));
        let m = 5_003;
        let part = halve(&x[..m]);
        let got: Vec<f32> = (0..part.len()).map(|o| halve_at(&h, |c| (c < m).then(|| x[c]), o)).collect();
        assert!(got.iter().zip(&part).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    /// Stands in for the network: every frame of a window says which input
    /// sample of the window it starts at.
    struct Probe;

    impl Windows for Probe {
        fn windows(&mut self, input: Vec<f32>, nb: usize) -> Result<[Vec<f32>; 3], String> {
            let mut got: [Vec<f32>; 3] = Default::default();
            for (dst, width) in got.iter_mut().zip([KEYS, KEYS, BINS]) {
                for w in 0..nb {
                    for f in 0..WIN_FRAMES {
                        let v = input[w * N_WIN + f * FFT_HOP];
                        dst.extend(std::iter::repeat_n(v, width));
                    }
                }
            }
            Ok(got)
        }
    }

    /// The windows of a track over more than one batch: each window's
    /// middle kept, window after window — kept frame k is the audio's sample
    /// `frame_sample(k)` — and only the frames inside the audio.
    #[test]
    fn the_windows_keep_their_middles_in_order() {
        let n = 70 * HOP + 1_234;
        let x: Vec<f32> = (0..n).map(|i| (i + 1) as f32).collect();
        let p = super::post_of(&mut Probe, &x).unwrap();
        assert_eq!(p.frames, (0..).take_while(|&k| frame_sample(k) < n).count());
        assert!(p.frames > BATCH * KEEP);
        for k in 0..p.frames {
            let want = x[frame_sample(k)];
            assert_eq!((p.note[k * KEYS], p.onset[k * KEYS + KEYS - 1], p.contour[k * BINS + BINS - 1]), (want, want, want), "frame {k}");
        }
    }

    #[test]
    fn frame_times_follow_the_windows() {
        assert_eq!(frame_sample(0), 0);
        assert_eq!(frame_sample(141), 141 * 256);
        assert_eq!(frame_sample(142), HOP);
        assert_eq!(HOP, 36_164);
        assert!((frame_time(142) - 36_164.0 / 22_050.0).abs() < 1e-15);
    }

    /// A key held with no onset seen becomes one note by the melodia trick
    /// (ending a frame early, as the package's does); a short blip does not;
    /// an onset peak starts a note that ends where the key falls away.
    #[test]
    fn decoding_finds_held_and_struck_notes() {
        let t = 60;
        let mut post =
            Post { frames: t, note: vec![0.0; t * KEYS], onset: vec![0.0; t * KEYS], contour: vec![0.0; t * BINS] };
        // key 40 (MIDI 61) held over frames 10..30, no onset
        for i in 10..30 {
            post.note[i * KEYS + 40] = 0.8;
        }
        // key 10 (MIDI 31): an onset at 35, sounding 35..50
        post.onset[35 * KEYS + 10] = 0.9;
        for i in 35..50 {
            post.note[i * KEYS + 10] = 0.6;
        }
        // key 70: a 3-frame blip
        for i in 5..8 {
            post.note[i * KEYS + 70] = 0.9;
        }
        let p = Params { infer_onsets: false, ..Params::default() };
        let mut ev = decode(&post, &p);
        ev.sort_by_key(|e| e.i0);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert_eq!((ev[0].i0, ev[0].i1, ev[0].midi), (10, 29, 61));
        assert!((ev[0].amp - 0.8).abs() < 1e-6);
        assert_eq!((ev[1].i0, ev[1].i1, ev[1].midi), (35, 50, 31));
    }

    #[test]
    #[ignore]
    fn the_filter_is_scipys() {
        let Some(d) = refs() else { return };
        let (_, want) = npy::read(&d.join("firwin.npy"));
        let got = halfband();
        let err = got.iter().zip(&want).fold(0f64, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-15, "err {err:e}");
    }

    /// Mono and half the rate = the port's `to22` (numpy + scipy), bit for bit.
    #[test]
    #[ignore]
    fn halving_is_scipys() {
        let Some(d) = refs() else { return };
        let x = npy::f32s(&d.join("resample.x44.npy"));
        let want = npy::f32s(&d.join("resample.x22.npy"));
        let n = x.len() / 2;
        let got = to22(&x[..n], &x[n..]);
        assert_eq!(got.len(), want.len());
        let differ = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(differ, 0, "{differ} of {} samples differ", got.len());
    }

    /// The decoding of the port's own pictures gives the port's notes exactly
    /// (frames, key, activation, pitch track).
    #[test]
    #[ignore]
    fn decoding_is_pythons() {
        let Some(d) = refs() else { return };
        for case in CASES {
            let tr = transcribe(post_of(&d, case), &Params::default());
            let py = python(&d, case);
            let want = py["notes"].as_array().unwrap();
            assert_eq!(tr.notes.len(), want.len(), "{case}: note count");
            for (i, (n, w)) in tr.notes.iter().zip(want).enumerate() {
                assert_eq!((n.i0, n.i1, n.midi), key_of(w), "{case} note {i}");
                let f = |k: &str| w[k].as_f64().unwrap();
                assert!((n.amp - f("amp")).abs() < 1e-12, "{case} note {i} amp");
                assert!((n.pitch - f("pitch")).abs() < 1e-9, "{case} note {i} pitch");
                assert!((n.on - f("on")).abs() < 1e-12 && (n.off - f("off")).abs() < 1e-12, "{case} note {i} time");
                let wt: Vec<f32> = w["trk"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
                assert_eq!(tr.tracks[i], wt, "{case} note {i} track");
            }
            eprintln!("{case}: {} notes, all equal", tr.notes.len());
        }
    }

    /// The network on the pack's runtime = onnxruntime in Python, on the same input.
    #[test]
    #[ignore]
    fn the_network_is_pythons() {
        let (Some(d), Some(mut m)) = (refs(), network()) else { return };
        for case in CASES {
            let x = npy::f32s(&d.join(format!("{case}.x22.npy")));
            let t0 = std::time::Instant::now();
            let got = m.run(&x).unwrap();
            let dt = t0.elapsed();
            let want = post_of(&d, case);
            assert_eq!(got.frames, want.frames, "{case}: frames");
            let mut worst = [0f32; 3];
            for (k, (a, b)) in [(&got.note, &want.note), (&got.onset, &want.onset), (&got.contour, &want.contour)]
                .into_iter()
                .enumerate()
            {
                worst[k] = a.iter().zip(b.iter()).fold(0f32, |m, (x, y)| m.max((x - y).abs()));
            }
            let [a, b, c] = worst;
            eprintln!("{case}: {} frames in {dt:?}, max |diff| note {a:e} onset {b:e} contour {c:e}", got.frames);
            assert!(worst.iter().all(|&w| w < 1e-4), "{case}: {worst:?}");
        }
    }

    /// Audio to notes, the whole way = the port's notes. The halving is
    /// bit-exact, the decoding exact, and the network's output within a few
    /// ulp of onnxruntime's in Python (the tests above); so a note can only
    /// differ where an activation sits that close to a threshold or to its
    /// neighbour's value. Allowed: 1 % of the notes, and those only by one
    /// frame at an end (a wiring fault — windows, trimming, resampling —
    /// moves far more notes than that). Measured 28.09: every note identical.
    #[test]
    #[ignore]
    fn transcription_is_pythons() {
        let (Some(d), Some(mut m)) = (refs(), network()) else { return };
        for case in CASES {
            let x = npy::f32s(&d.join(format!("{case}.x44.npy")));
            let n = x.len() / 2;
            let tr = transcribe(m.run(&to22(&x[..n], &x[n..])).unwrap(), &Params::default());
            let want: Vec<_> = python(&d, case)["notes"].as_array().unwrap().iter().map(key_of).collect();
            let got: Vec<_> = tr.notes.iter().map(|n| (n.i0, n.i1, n.midi)).collect();
            let near = |a: &(usize, usize, u8), b: &(usize, usize, u8)| {
                a.2 == b.2 && a.0.abs_diff(b.0) <= 1 && a.1.abs_diff(b.1) <= 1
            };
            let (mut differ, mut far) = (0, 0);
            for (xs, ys) in [(&want, &got), (&got, &want)] {
                for a in xs.iter().filter(|a| !ys.contains(a)) {
                    differ += 1;
                    far += !ys.iter().any(|b| near(a, b)) as usize;
                }
            }
            let (g, w) = (got.len(), want.len());
            eprintln!("{case}: {g} notes (Python {w}), {differ} not identical, {far} more than a frame off");
            assert_eq!(far, 0, "{case}");
            assert!(differ * 100 <= want.len(), "{case}: {differ} of {}", want.len());
        }
    }
}
