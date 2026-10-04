//! The linear filter of a live stream: the shipped linear filter's magnitude
//! at every frequency and its phase up to 20 kHz, minimum phase only in the
//! last kilohertz under the wall.
//!
//! A linear-phase filter looks ahead by half its length — 1.4 s for 1M at
//! 352.8 kHz, 42.5 s for 30M — and a stream has to be that far in before its
//! first sound. All of that look-ahead is ringing at the wall: ahead of its
//! centre a linear filter rings only at its cutoff (21 050 Hz for the 44.1 kHz
//! family, 23 000 Hz for 48), with an envelope that falls as 1/t over the whole
//! half. Below the wall it is flat and rings nowhere.
//!
//! So this filter keeps the linear filter's |H| everywhere, the wall included,
//! and its phase — the same delay, (N − 1)/2 — up to 20 kHz (22 kHz for the
//! 48 kHz family). From 20.9 kHz (22.9) on it takes the minimum-phase filter's
//! phase instead, whose ringing comes after the centre only, through a smooth
//! crossover between the two. Its response then starts 50 ms ahead of the
//! centre: the window of N taps begins there, with a 10 ms fade-in, and what
//! lies further ahead is below −170 dB. A stream waits 50 ms for its filter
//! instead of half of it. In 20 Hz – 20 kHz it is the linear filter to −144 dB
//! and beyond; its stopband is the linear filter's or better; its wall stands
//! where the linear filter's does. Above 20 kHz its phase differs: the wall
//! rings after a transient, not before and after it.
//!
//! Streams only: a file has its future in hand and plays the shipped linear
//! filter. Made once from the shipped pair of the factor
//! (`filter::find_precomputed_filter`) and kept beside it as
//! `fir_<TAG>_<design rate>_stream_linear_v1.npy`. Everything is reckoned at
//! the blob's design rate (`filter::design_rate`), where its wall and the
//! crossover stand at the same fraction of the source's Nyquist: one file
//! serves a 44.1/48 kHz source at that rate and a hi-res source at twice or
//! four times it, where the look-ahead in frames is the same and in time half
//! or a quarter.
//!
//! What the stream plays is checked before it is kept (`derive`); a filter
//! that fails a check is not kept, and the stream plays the shipped one.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use super::tfs::unwrap_in_place;
use crate::audio::converter::dsp::filter::{blob_name, design_rate, find_blob, taps_label};

/// The cached filter's phase name: `fir_<TAG>_<design rate>_<this>.npy`.
const PHASE_SUFFIX: &str = "stream_linear_v1";

/// How far the filter looks ahead of its centre, ms.
const LOOK_AHEAD_MS: u64 = 50;
/// The window's fade-in, ms: cut hard 50 ms ahead, a 30M filter's stopband
/// would be −195 dB instead of the shipped −221.
const FADE_IN_MS: u64 = 10;

/// The crossover's ramp: the running integral of a Kaiser bump, read off a
/// table this fine. A sin² ramp — TFS's — has a second derivative that jumps
/// at its ends, and its ringing reaches −93 dB 20 ms ahead instead of −184.
const RAMP_BETA: f64 = 16.0;
const RAMP_TABLE: usize = 20_001;
/// The fade-in's shape, the same running integral.
const FADE_IN_BETA: f64 = 12.0;

/// Where the checks put their limits (`derive`), dB.
const BAND_ERROR_DB: f64 = -144.0;
const BAND_MAGNITUDE_DB: f64 = 1e-6;
/// The wall (within 10 Hz of the cutoff): |H| within 0.1 dB of the linear
/// filter's wherever that is above −100 dB, and nowhere off by more than
/// `wall_error_db` of the passband. Down its slope |H| falls by thousands of
/// dB a hertz on the long filters, so a relative measure means nothing at its
/// foot. 10M and shorter keep it to −153…−159 dB; 30M to −130.0 dB at
/// 352.8 kHz and −140.4 at 384 (0.048 / 0.0013 dB above −100 dB: the wall
/// moved by ~3e-5 Hz). That is the shipped pair, not the window: the linear
/// filter's |H| and the cepstral minimum-phase filter's phase do not quite
/// agree at the wall, and what they disagree by is left as an even floor round
/// the whole transform (−185…−188 dB), of which the window cuts some. A longer
/// look-ahead or window does not change it; only a minimum-phase filter made
/// more exactly from the linear one would.
const WALL_RELATIVE_DB: f64 = 0.1;
const WALL_RELATIVE_ABOVE_DB: f64 = -100.0;
const STOPBAND_MARGIN_DB: f64 = 1.0;

/// The most |H| may be off at the wall for a filter `n` long, dB of the
/// passband: −150 up to 10M, −125 beyond (`WALL_RELATIVE_DB`).
fn wall_error_db(n: usize) -> f64 {
    if n > 10_000_000 {
        -125.0
    } else {
        -150.0
    }
}
/// Below this the stopband is not compared: both are rounding there.
const STOPBAND_FLOOR_DB: f64 = -200.0;
const AHEAD_ENERGY_DB: f64 = -170.0;
const AFTER_ENERGY_DB: f64 = -150.0;

/// The family's frequencies at a blob's design rate, Hz: the crossover, the
/// wall (the cutoff), the source's Nyquist where the stopband starts.
#[derive(Clone, Copy, Debug)]
struct Edges {
    f_low: f64,
    f_high: f64,
    wall: f64,
    stop: f64,
}

/// Every blob is designed from a 44.1 or 48 kHz source (`optimize.py
/// --all-ratios`); its rate says which.
fn edges(design_rate: u32) -> Edges {
    if design_rate % 48_000 == 0 {
        Edges { f_low: 22_000.0, f_high: 22_900.0, wall: 23_000.0, stop: 24_000.0 }
    } else {
        Edges { f_low: 20_000.0, f_high: 20_900.0, wall: 21_050.0, stop: 22_050.0 }
    }
}

/// The look-ahead of the stream's linear filter `taps` long designed at
/// `design_rate`, frames: 50 ms — whole frames at every rate a blob is made
/// at. None when the filter's own half is no longer (5k): the shipped filter
/// waits no longer than this one would.
pub fn look_ahead(taps: usize, design_rate: u32) -> Option<usize> {
    debug_assert!(design_rate % 20 == 0, "{design_rate} Hz: 50 ms is not a whole number of frames");
    let k = (design_rate as u64 * LOOK_AHEAD_MS / 1000) as usize;
    (taps.saturating_sub(1) / 2 > k).then_some(k)
}

/// Up to where its phase is the linear filter's, Hz, for a source at
/// `src_rate` played at `out_rate`: 20 kHz (22 kHz for the 48 kHz family), a
/// hi-res source's at the same fraction of its own Nyquist.
pub fn linear_up_to_hz(src_rate: u32, out_rate: u32) -> f64 {
    let design = design_rate(src_rate, out_rate);
    edges(design).f_low * out_rate as f64 / design as f64
}

/// Where its wall stands, Hz, for a source at `src_rate` played at
/// `out_rate`: the shipped filter's cutoff.
pub fn wall_hz(src_rate: u32, out_rate: u32) -> f64 {
    let design = design_rate(src_rate, out_rate);
    edges(design).wall * out_rate as f64 / design as f64
}

/// The cached filter for `taps` designed at `design_rate`, when there is one.
fn cached_at(taps: usize, design_rate: u32) -> Option<String> {
    find_blob(taps, design_rate, PHASE_SUFFIX)
}

/// The pair it is made from is installed.
fn pair_installed(taps: usize, design_rate: u32) -> bool {
    find_blob(taps, design_rate, "linear_phase").is_some() && find_blob(taps, design_rate, "minimum_phase").is_some()
}

/// The cached stream's linear filter for `taps` taking a source at `src_rate`
/// up to `out_rate`, and its look-ahead in output frames: what a stream's
/// chain plays when it is there (the analyzer peeks its bank by this).
pub fn cached(taps: usize, src_rate: u32, out_rate: u32) -> Option<(String, usize)> {
    let design = design_rate(src_rate, out_rate);
    let k = look_ahead(taps, design)?;
    Some((cached_at(taps, design)?, k))
}

/// The look-ahead a stream's linear filter `taps` long gives a source at
/// `src_rate` played at `out_rate`, output frames, when the stream can have
/// it: cached, or its pair installed to make it from. None: the shipped
/// linear filter plays (`Alignment::Linear`).
pub fn available(taps: usize, src_rate: u32, out_rate: u32) -> Option<usize> {
    let design = design_rate(src_rate, out_rate);
    let k = look_ahead(taps, design)?;
    (cached_at(taps, design).is_some() || pair_installed(taps, design)).then_some(k)
}

// ─── Making it ───────────────────────────────────────────────────────────────

fn bessel_i0(x: f64) -> f64 {
    let x2 = (x * 0.5) * (x * 0.5);
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    for k in 1u32..=200 {
        term *= x2 / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The crossover's weight: 0 at and below `f_low`, 1 at and above `f_high`,
/// between them the normalised running integral of a Kaiser bump (β 16),
/// interpolated linearly on a 20 001-point table over [0, 1].
struct Ramp {
    t: Vec<f64>,
    c: Vec<f64>,
}

impl Ramp {
    fn new() -> Ramp {
        let n = RAMP_TABLE;
        let step = 1.0 / (n - 1) as f64;
        let mut t: Vec<f64> = (0..n).map(|j| j as f64 * step).collect();
        t[n - 1] = 1.0;
        let i0b = bessel_i0(RAMP_BETA);
        let mut c = Vec::with_capacity(n);
        let mut acc = 0.0f64;
        for &tj in &t {
            let u = 2.0 * tj - 1.0;
            acc += bessel_i0(RAMP_BETA * (1.0 - u * u).max(0.0).sqrt()) / i0b;
            c.push(acc);
        }
        let (c0, cn) = (c[0], c[n - 1]);
        for v in c.iter_mut() {
            *v = (*v - c0) / (cn - c0);
        }
        Ramp { t, c }
    }

    /// The weight at `x` ∈ [0, 1] of the crossover.
    fn at(&self, x: f64) -> f64 {
        let n = self.t.len();
        let mut j = ((x * (n - 1) as f64) as usize).min(n - 2);
        while j > 0 && self.t[j] > x {
            j -= 1;
        }
        while j + 2 < n && self.t[j + 1] <= x {
            j += 1;
        }
        let slope = (self.c[j + 1] - self.c[j]) / (self.t[j + 1] - self.t[j]);
        slope * (x - self.t[j]) + self.c[j]
    }
}

/// The window's fade-in over `n` taps: the running integral of a Kaiser
/// window (β 12) from 0, reversed from the fade-out it is the mirror of.
fn fade_in(n: usize) -> Vec<f64> {
    if n < 2 {
        return vec![1.0; n];
    }
    let alpha = (n - 1) as f64 / 2.0;
    let i0b = bessel_i0(FADE_IN_BETA);
    let mut c = Vec::with_capacity(n);
    let mut acc = 0.0f64;
    for i in 0..n {
        let r = (i as f64 - alpha) / alpha;
        acc += bessel_i0(FADE_IN_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        c.push(acc);
    }
    let last = c[n - 1];
    (0..n).map(|i| 1.0 - c[n - 1 - i] / last).collect()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

fn db_energy(x: f64) -> f64 {
    10.0 * x.max(1e-300).log10()
}

/// What `derive` measured on the filter it made.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checked {
    /// Energy of the response ahead of the window and after it, dB of the whole.
    pub ahead_db: f64,
    pub after_db: f64,
    /// Complex error against the linear filter in 20 Hz – 20 kHz (22 kHz for
    /// the 48 kHz family), dB of the passband.
    pub band_db: f64,
    /// |H| against the linear filter's from 20 Hz to 10 Hz under the wall, dB.
    pub magnitude_db: f64,
    /// |H| against the linear filter's within 10 Hz of the wall: dB of the
    /// passband, and dB where the linear filter is above −100 dB.
    pub wall_db: f64,
    pub wall_relative_db: f64,
    /// The stopband's peak over the linear filter's, near (a kilohertz past the
    /// source's Nyquist) and far, dB.
    pub stop_near_db: f64,
    pub stop_far_db: f64,
}

/// The stream's linear filter from the shipped pair `lin` and `min_ph` (one
/// length N, designed at `design_rate`), checked: Err when the pair is too
/// short for it, cancelled, or a check fails.
pub(crate) fn derive(lin: &[f64], min_ph: &[f64], design_rate: u32, cancel: &AtomicBool) -> Result<(Vec<f64>, Checked), String> {
    let k = look_ahead(lin.len(), design_rate)
        .ok_or_else(|| format!("{} taps are no longer than the look-ahead: the shipped filter plays", lin.len()))?;
    let fade = (design_rate as u64 * FADE_IN_MS / 1000) as usize;
    derive_at(lin, min_ph, design_rate, k, fade, cancel)
}

/// `derive` with the look-ahead `k_ahead` and the fade-in `fade`, frames.
///
/// Made on a transform M = next_pow2(2N) long, where nothing of the response
/// wraps round (it lives from D − 50 ms to D + N):
/// 1. |H| of the linear filter, the minimum-phase filter's phase φ unwrapped;
/// 2. τ, the minimum phase's group delay at f_low (a central difference): the
///    delay it adds there, held out of the high band so that the band joins
///    the linear phase's delay without a step;
/// 3. dev = φ + ωτ, less the whole number of turns that centres it over the
///    crossover (at weight 1 a whole turn changes nothing; inside the
///    crossover it keeps the swing ~8 rad instead of 30–45);
/// 4. the phase −ω(N − 1)/2 + w(f)·dev — the centre is the linear filter's
///    own, half a tap off one for an even N (a whole tap there is −15 dB in
///    the band);
/// 5. the window of N taps from D − K, D = (N − 1) div 2, played with a trim
///    of K: the output lines up with the linear filter's at its trim of D.
///
/// Then the checks, on the window as it plays (not on the spectrum it was
/// made from, which only restates the construction): the energy ahead of the
/// window (< −170 dB) and after it (< −150 dB); in the band, the complex error
/// against the linear filter (< −144 dB) and |H| (within 1e-6 dB up to 10 Hz
/// under the wall); |H| within 10 Hz of the wall (within 0.1 dB where the
/// linear filter is above −100 dB, nowhere off by more than −150 dB of the
/// passband, −125 beyond 10M: `wall_error_db`); the stopband no more than 1 dB
/// over the linear filter's. Peak memory as TFS's:
/// the transform, its scratch and its plan's twiddles, |H| and φ.
fn derive_at(
    lin: &[f64],
    min_ph: &[f64],
    design_rate: u32,
    k_ahead: usize,
    fade: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<f64>, Checked), String> {
    let n = lin.len();
    if n != min_ph.len() {
        return Err(format!("the linear ({}) and minimum-phase ({}) filters differ in length", n, min_ph.len()));
    }
    let d = (n - 1) / 2;
    if n < 8 || d <= k_ahead {
        return Err(format!("{} taps are no longer than the look-ahead", n));
    }
    let start = d - k_ahead;
    let fs = design_rate as f64;
    let e = edges(design_rate);
    let m = (2 * n).next_power_of_two();
    let half = m / 2;
    let stopped = || cancel.load(Ordering::Relaxed);
    let cancelled = || "Cancelled".to_string();

    // One plan: at 2^26 points a plan's twiddles are another gigabyte, so the
    // inverse is the forward transform of the conjugate.
    let fft = FftPlanner::<f64>::new().plan_fft_forward(m);
    let zero = Complex::new(0.0, 0.0);
    let mut scratch = vec![zero; fft.get_inplace_scratch_len()];
    let mut buf = vec![zero; m];
    for (b, &x) in buf.iter_mut().zip(lin) {
        *b = Complex::new(x, 0.0);
    }
    fft.process_with_scratch(&mut buf, &mut scratch);
    let mags: Vec<f64> = buf[..=half].iter().map(|c| c.norm()).collect();
    if stopped() {
        return Err(cancelled());
    }

    buf.fill(zero);
    for (b, &x) in buf.iter_mut().zip(min_ph) {
        *b = Complex::new(x, 0.0);
    }
    fft.process_with_scratch(&mut buf, &mut scratch);
    let mut dev: Vec<f64> = buf[..=half].iter().map(|c| c.arg()).collect();
    unwrap_in_place(&mut dev);
    if stopped() {
        return Err(cancelled());
    }

    let omega = |k: usize| k as f64 * (2.0 * PI / m as f64);
    let freq = |k: usize| omega(k) * fs / (2.0 * PI);
    let i_low = (e.f_low * m as f64 / fs).round() as usize;
    let tau = -(dev[i_low + 1] - dev[i_low - 1]) / (2.0 * 2.0 * PI / m as f64);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for (k, v) in dev.iter_mut().enumerate() {
        *v += omega(k) * tau;
        let f = freq(k);
        if f >= e.f_low && f <= e.f_high {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    let turns = ((lo + hi) / 2.0 / (2.0 * PI)).round();
    let ramp = Ramp::new();
    let centre = (n - 1) as f64 / 2.0;
    for k in 0..=half {
        let f = freq(k);
        let w = if f >= e.f_high {
            1.0
        } else if f > e.f_low {
            ramp.at((f - e.f_low) / (e.f_high - e.f_low))
        } else {
            0.0
        };
        let ph = -omega(k) * centre + w * (dev[k] - 2.0 * PI * turns);
        buf[k] = Complex::new(mags[k] * ph.cos(), mags[k] * ph.sin());
    }
    drop(dev);
    buf[0] = Complex::new(buf[0].re, 0.0);
    buf[half] = Complex::new(buf[half].re, 0.0);
    for k in 1..half {
        buf[m - k] = buf[k].conj();
    }
    if stopped() {
        return Err(cancelled());
    }

    // The response: the real part of the forward transform of the conjugate.
    for c in buf.iter_mut() {
        c.im = -c.im;
    }
    fft.process_with_scratch(&mut buf, &mut scratch);
    let scale = 1.0 / m as f64;
    let (mut total, mut ahead, mut after) = (0.0f64, 0.0f64, 0.0f64);
    for (i, c) in buf.iter().enumerate() {
        let v = c.re * scale;
        let e2 = v * v;
        total += e2;
        if i < start {
            ahead += e2;
        } else if i >= start + n {
            after += e2;
        }
    }
    let mut out: Vec<f64> = buf[start..start + n].iter().map(|c| c.re * scale).collect();
    for (o, g) in out.iter_mut().zip(fade_in(fade.min(n))) {
        *o *= g;
    }
    if stopped() {
        return Err(cancelled());
    }

    // The window as it plays, against the linear filter, both at unity DC
    // gain (the engine scales every filter by L / Σh). In the band the linear
    // filter is |H|·e^{−jω(N−1)/2}; the window starts `start` taps later.
    buf.fill(zero);
    for (b, &x) in buf.iter_mut().zip(&out) {
        *b = Complex::new(x, 0.0);
    }
    fft.process_with_scratch(&mut buf, &mut scratch);
    let g_b: f64 = out.iter().sum();
    let g_l: f64 = lin.iter().sum();
    let shift = centre - start as f64;
    let (mut band, mut magnitude, mut wall, mut wall_rel) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let wall_floor = 10f64.powf(WALL_RELATIVE_ABOVE_DB / 20.0);
    let (mut near_b, mut near_l, mut far_b, mut far_l) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for k in 1..=half {
        let f = freq(k);
        let hb = buf[k] / g_b;
        let ml = mags[k] / g_l.abs();
        if f >= 20.0 && f <= e.f_low {
            let a = -omega(k) * shift;
            let hl = Complex::new(ml * a.cos(), ml * a.sin());
            band = band.max((hb - hl).norm());
        }
        if f >= 20.0 && f <= e.wall - 10.0 {
            magnitude = magnitude.max(db(hb.norm() / ml).abs());
        }
        if f >= e.wall - 10.0 && f <= e.wall + 10.0 {
            wall = wall.max((hb.norm() - ml).abs());
            if ml >= wall_floor {
                wall_rel = wall_rel.max(db(hb.norm() / ml).abs());
            }
        }
        if f >= e.stop && f <= e.stop + 1000.0 {
            near_b = near_b.max(hb.norm());
            near_l = near_l.max(ml);
        } else if f > e.stop + 1000.0 {
            far_b = far_b.max(hb.norm());
            far_l = far_l.max(ml);
        }
    }
    let over = |b: f64, l: f64| if db(b) <= STOPBAND_FLOOR_DB { f64::NEG_INFINITY } else { db(b) - db(l) };
    let checked = Checked {
        ahead_db: db_energy(ahead / total),
        after_db: db_energy(after / total),
        band_db: db(band),
        magnitude_db: magnitude,
        wall_db: db(wall),
        wall_relative_db: wall_rel,
        stop_near_db: over(near_b, near_l),
        stop_far_db: over(far_b, far_l),
    };
    let fails: Vec<String> = [
        (checked.ahead_db < AHEAD_ENERGY_DB, format!("energy ahead of the window {:.1} dB", checked.ahead_db)),
        (checked.after_db < AFTER_ENERGY_DB, format!("energy after the window {:.1} dB", checked.after_db)),
        (checked.band_db < BAND_ERROR_DB, format!("error in the band {:.1} dB", checked.band_db)),
        (checked.magnitude_db <= BAND_MAGNITUDE_DB, format!("|H| in the band off by {:.2e} dB", checked.magnitude_db)),
        (checked.wall_db <= wall_error_db(n), format!("|H| at the wall off by {:.1} dB", checked.wall_db)),
        (
            checked.wall_relative_db <= WALL_RELATIVE_DB,
            format!("|H| at the wall off by {:.3} dB of itself", checked.wall_relative_db),
        ),
        (checked.stop_near_db <= STOPBAND_MARGIN_DB, format!("stopband {:+.1} dB over the linear filter's", checked.stop_near_db)),
        (checked.stop_far_db <= STOPBAND_MARGIN_DB, format!("far stopband {:+.1} dB over the linear filter's", checked.stop_far_db)),
    ]
    .into_iter()
    .filter(|(ok, _)| !ok)
    .map(|(_, why)| why)
    .collect();
    if !fails.is_empty() {
        return Err(format!("the stream filter fails its checks: {}", fails.join(", ")));
    }
    Ok((out, checked))
}

// ─── Making it once, ahead when it can be ────────────────────────────────────

/// A filter being made: (its taps' label, its design rate).
type Key = (&'static str, u32);

struct Job {
    key: Key,
    cancel: Arc<AtomicBool>,
    /// Streams waiting for it: one a stream waits for is never stopped.
    waiters: usize,
}

/// The filters being made, one at a time a key: what streams wait for and
/// what is made ahead of a stream (`prepare_ahead`). Whatever a stream needs
/// now comes first — a filter made ahead for anything else stops — and two
/// that want one filter wait for the one making of it.
struct Prep {
    jobs: Mutex<Vec<Job>>,
    ended: Condvar,
    /// Why the last making of a key failed, for the streams that waited.
    failed: Mutex<HashMap<Key, String>>,
}

type Make = Box<dyn FnOnce(&AtomicBool) -> Result<String, String> + Send>;

impl Prep {
    fn new() -> Prep {
        Prep { jobs: Mutex::new(Vec::new()), ended: Condvar::new(), failed: Mutex::new(HashMap::new()) }
    }

    fn global() -> &'static Prep {
        static PREP: OnceLock<Prep> = OnceLock::new();
        PREP.get_or_init(Prep::new)
    }

    /// Stop what is made ahead (nobody waits for it) of anything but `keep`.
    fn stop_others(jobs: &[Job], keep: Option<Key>) {
        for j in jobs.iter().filter(|j| j.waiters == 0 && Some(j.key) != keep) {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn start(&'static self, jobs: &mut Vec<Job>, key: Key, waiters: usize, make: Make) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.push(Job { key, cancel: cancel.clone(), waiters });
        let mine = cancel.clone();
        let spawned = std::thread::Builder::new().name("stream-linear".into()).spawn(move || {
            let result = make(&mine);
            self.end(key, &mine, result);
        });
        if let Err(e) = spawned {
            jobs.retain(|j| !Arc::ptr_eq(&j.cancel, &cancel));
            self.failed.lock().unwrap_or_else(|e| e.into_inner()).insert(key, format!("cannot start: {}", e));
        }
        cancel
    }

    fn end(&self, key: Key, cancel: &Arc<AtomicBool>, result: Result<String, String>) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|j| !Arc::ptr_eq(&j.cancel, cancel));
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(_) => {
                failed.remove(&key);
            }
            Err(e) => {
                failed.insert(key, e);
            }
        }
        drop(failed);
        drop(jobs);
        self.ended.notify_all();
    }

    /// The filter `key` for a stream that needs it now: `cached` when it is
    /// there, else the one being made (waited for), else made here and
    /// waited for. None: nothing to make it from (`can_make` false). Err: the
    /// making failed.
    fn obtain(
        &'static self,
        key: Key,
        cached: &dyn Fn() -> Option<String>,
        can_make: bool,
        make: impl FnOnce() -> Make,
    ) -> Result<Option<String>, String> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        Prep::stop_others(&jobs, Some(key));
        let mut make = Some(make);
        let mut waited = false;
        loop {
            if let Some(p) = cached() {
                return Ok(Some(p));
            }
            let mine = match jobs.iter_mut().find(|j| j.key == key) {
                Some(j) if !j.cancel.load(Ordering::Relaxed) => {
                    j.waiters += 1;
                    j.cancel.clone()
                }
                // Stopped and not gone yet: once it has, make it again.
                Some(_) => {
                    jobs = self.ended.wait(jobs).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                None if waited => {
                    let why = self.failed.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
                    return Err(why.unwrap_or_else(|| "it was not made".into()));
                }
                None if !can_make => return Ok(None),
                None => match make.take() {
                    Some(m) => self.start(&mut jobs, key, 1, m()),
                    None => return Err("it was not made".into()),
                },
            };
            jobs = self
                .ended
                .wait_while(jobs, |js| js.iter().any(|j| Arc::ptr_eq(&j.cancel, &mine)))
                .unwrap_or_else(|e| e.into_inner());
            waited = true;
        }
    }

    /// Make `want` ahead of a stream, in the background, unless it is cached,
    /// being made, or cannot be made; whatever is made ahead of anything else
    /// stops. None: nothing is wanted ahead now.
    fn ahead(&'static self, want: Option<(Key, &dyn Fn() -> bool, bool, Make)>) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        Prep::stop_others(&jobs, want.as_ref().map(|w| w.0));
        let Some((key, is_cached, can_make, make)) = want else { return };
        // One making a key at a time: one stopped and not gone yet is made
        // again the next time it is asked for.
        if jobs.iter().any(|j| j.key == key) || is_cached() || !can_make {
            return;
        }
        self.start(&mut jobs, key, 0, make);
    }

    /// Something is being made.
    fn busy(&self) -> bool {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|j| !j.cancel.load(Ordering::Relaxed))
    }
}

/// Make the filter for `taps` at `design_rate` from its pair and keep it
/// beside the linear blob — written whole under another name first, so a
/// stream never reads half of one. Waits for its memory as TFS does.
fn make_file(taps: usize, design_rate: u32, cancel: &AtomicBool) -> Result<String, String> {
    use crate::audio::dsp_core::{load_npy_f64, save_npy_f64};
    use crate::audio::memory::{total_ram_mb, try_reserve_ram};
    let t0 = std::time::Instant::now();
    let label = taps_label(taps).ok_or("no filter is designed for so few taps")?;
    let lin_path = find_blob(taps, design_rate, "linear_phase").ok_or("its linear-phase filter is not installed")?;
    let min_path = find_blob(taps, design_rate, "minimum_phase").ok_or("its minimum-phase filter is not installed")?;
    let est_mb = super::tfs::derive_mb(taps);
    let mut said = false;
    let _ram = loop {
        if let Some(r) = try_reserve_ram(est_mb) {
            break r;
        }
        if est_mb + 4096 > total_ram_mb() {
            return Err(format!(
                "making the {} stream filter takes {:.1} GB of memory; this machine has {:.1} GB",
                label,
                est_mb as f64 / 1024.0,
                total_ram_mb() as f64 / 1024.0
            ));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }
        if !said {
            crate::aelog!("[STREAM-LIN] {} at {} Hz: waiting for {:.1} GB of free memory", label, design_rate, est_mb as f64 / 1024.0);
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    };
    let lin = load_npy_f64(&lin_path)?;
    let min_ph = load_npy_f64(&min_path)?;
    let (h, c) = derive(&lin, &min_ph, design_rate, cancel)?;
    drop(lin);
    drop(min_ph);
    let name = blob_name(taps, design_rate, PHASE_SUFFIX).ok_or("no filter is designed for so few taps")?;
    let path = Path::new(&lin_path).parent().ok_or("the linear-phase filter has no folder")?.join(&name);
    let part = path.with_file_name(format!("{}.part", name));
    save_npy_f64(&part, &h)?;
    std::fs::rename(&part, &path).map_err(|e| format!("cannot keep {}: {}", path.display(), e))?;
    crate::aelog!(
        "[STREAM-LIN] {} at {} Hz made in {:.1} s: ahead {:.0} dB, after {:.0} dB, band {:.0} dB, |H| {:.1e} dB, wall {:.0} dB, stopband {:+.1}/{:+.1} dB — {}",
        label,
        design_rate,
        t0.elapsed().as_secs_f64(),
        c.ahead_db,
        c.after_db,
        c.band_db,
        c.magnitude_db,
        c.wall_db,
        c.stop_near_db,
        c.stop_far_db,
        path.display()
    );
    Ok(path.to_string_lossy().into_owned())
}

/// The stream's linear filter for `taps` taking a source at `src_rate` up to
/// `out_rate`, for a stream about to play it: the path and the look-ahead
/// (output frames, `Alignment::LookAhead`). Made here once — or the making
/// already under way waited for — when it is not cached. None: the shipped
/// linear filter plays (a short filter, or no pair to make it from). Err: it
/// could not be made (memory, a failed check); the shipped one plays too.
pub fn obtain(taps: usize, src_rate: u32, out_rate: u32) -> Result<Option<(String, usize)>, String> {
    let design = design_rate(src_rate, out_rate);
    let (Some(k), Some(label)) = (look_ahead(taps, design), taps_label(taps)) else {
        return Ok(None);
    };
    let path = Prep::global().obtain(
        (label, design),
        &|| cached_at(taps, design),
        pair_installed(taps, design),
        || Box::new(move |cancel: &AtomicBool| make_file(taps, design, cancel)),
    )?;
    Ok(path.map(|p| (p, k)))
}

/// Make the stream's linear filter for `want` (taps, source rate, output
/// rate) ahead of the stream that will play it, in the background: the
/// radio's view asks for the length a stream would play as it is shown and
/// as the rack is set. What is being made ahead of anything else stops.
/// None: nothing is wanted ahead (the rack plays no linear filter on
/// streams).
pub fn prepare_ahead(want: Option<(usize, u32, u32)>) {
    let target = want.and_then(|(taps, src_rate, out_rate)| {
        let design = design_rate(src_rate, out_rate);
        look_ahead(taps, design)?;
        Some((taps, taps_label(taps)?, design))
    });
    match target {
        None => Prep::global().ahead(None),
        Some((taps, label, design)) => Prep::global().ahead(Some((
            (label, design),
            &|| cached_at(taps, design).is_some(),
            pair_installed(taps, design),
            Box::new(move |cancel: &AtomicBool| make_file(taps, design, cancel)),
        ))),
    }
}

/// A stream's linear filter is being made (the radio's status says so).
pub fn preparing() -> bool {
    Prep::global().busy()
}

/// A pair made as the shipped ones are, for the tests here and the radio's.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{bessel_i0, edges};
    use rustfft::num_complex::Complex;
    use rustfft::FftPlanner;
    use std::f64::consts::PI;

    /// A lowpass the way the shipped ones are made: sinc × Kaiser β 14, its
    /// cutoff at the family's wall.
    pub(crate) fn designed(n: usize, rate: u32) -> Vec<f64> {
        let fc = edges(rate).wall / rate as f64;
        let mid = (n - 1) as f64 / 2.0;
        let i0b = bessel_i0(14.0);
        (0..n)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
                let r = x / mid;
                sinc * bessel_i0(14.0 * (1.0 - r * r).max(0.0).sqrt()) / i0b
            })
            .collect()
    }

    /// The minimum-phase filter of `lin` by its cepstrum, as the shipped ones
    /// are made, on a transform eight times next_pow2(N).
    pub(crate) fn cepstral(lin: &[f64]) -> Vec<f64> {
        let n = lin.len();
        let m = n.next_power_of_two() * 8;
        let mut p = FftPlanner::<f64>::new();
        let (fwd, inv) = (p.plan_fft_forward(m), p.plan_fft_inverse(m));
        let mut b: Vec<Complex<f64>> = (0..m).map(|i| Complex::new(if i < n { lin[i] } else { 0.0 }, 0.0)).collect();
        fwd.process(&mut b);
        let mut c: Vec<Complex<f64>> = b.iter().map(|v| Complex::new((v.norm() + 1e-30).ln(), 0.0)).collect();
        inv.process(&mut c);
        for (i, v) in c.iter_mut().enumerate() {
            let w = if i == 0 || i == m / 2 {
                1.0
            } else if i < m / 2 {
                2.0
            } else {
                0.0
            };
            *v = Complex::new(v.re / m as f64 * w, 0.0);
        }
        fwd.process(&mut c);
        let mut h: Vec<Complex<f64>> = c.iter().map(|v| v.exp()).collect();
        inv.process(&mut h);
        h[..n].iter().map(|v| v.re / m as f64).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{cepstral, designed};
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn it_looks_50_ms_ahead_in_whole_frames_and_not_at_all_when_short() {
        use crate::audio::converter::dsp::filter::TARGET_RATES;
        for rate in TARGET_RATES {
            let k = look_ahead(1_000_000, rate).expect("1M is longer than 50 ms");
            assert_eq!(k as u64 * 1000, rate as u64 * LOOK_AHEAD_MS, "{rate} Hz");
        }
        assert_eq!(look_ahead(30_000_000, 352_800), Some(17_640));
        assert_eq!(look_ahead(1_000_000, 384_000), Some(19_200));
        // 5k: its own half (7 ms at 352.8 kHz, 28 ms at 88.2) is shorter.
        assert_eq!(look_ahead(5_000, 352_800), None);
        assert_eq!(look_ahead(5_000, 88_200), None);
    }

    #[test]
    fn one_file_serves_a_rate_and_the_hi_res_source_of_its_factor() {
        // 48 kHz up to 192 kHz and 96 kHz up to 384 kHz both play the ×4 blob.
        assert_eq!(design_rate(48_000, 192_000), design_rate(96_000, 384_000));
        assert_eq!(blob_name(1_000_000, design_rate(96_000, 384_000), PHASE_SUFFIX).as_deref(), Some("fir_1M_192000_stream_linear_v1.npy"));
        assert_eq!(blob_name(30_000_000, 352_800, PHASE_SUFFIX).as_deref(), Some("fir_30M_352800_stream_linear_v1.npy"));
        // Linear up to 20 / 22 kHz; a hi-res source's at its own fraction.
        assert_eq!(linear_up_to_hz(44_100, 352_800), 20_000.0);
        assert_eq!(linear_up_to_hz(48_000, 384_000), 22_000.0);
        assert_eq!(linear_up_to_hz(96_000, 384_000), 44_000.0);
        assert_eq!(linear_up_to_hz(88_200, 352_800), 40_000.0);
    }

    /// The filter kept beside the blobs is not a blob: the inventory of what
    /// is installed, the lookup of a phase, the radio's installed lengths do
    /// not see it — its name carries no phase they know — and only its own
    /// lookup finds it. (The packs are named by length and rate alone, and
    /// nothing tidies the filter folder.)
    #[test]
    fn the_kept_filter_is_not_a_blob() {
        use crate::audio::converter::dsp::filter::{find_precomputed_filter, inventory};
        use crate::player::settings::{Phase, PlayerSettings};
        let dir = std::env::current_exe().unwrap().parent().unwrap().join("fir-optimizer").join("output");
        std::fs::create_dir_all(&dir).unwrap();
        // 48 kHz ×16: a cell no other test touches.
        let (taps, src, out) = (1_000_000usize, 48_000u32, 768_000u32);
        let cell = || {
            let inv = inventory();
            let c: Vec<(bool, bool)> =
                inv.iter().filter(|p| p.taps == taps && p.target_rate_hz == out).map(|p| (p.linear, p.minimum)).collect();
            c
        };
        let s = PlayerSettings { phase: Phase::Linear, fs_multiplier: 16, taps, ..PlayerSettings::default() };
        let installed = || crate::player::radio::chain::installed_length(&s, src);
        let (cell_before, installed_before) = (cell(), installed());
        let kept = dir.join("fir_1M_768000_stream_linear_v1.npy");
        std::fs::write(&kept, b"").unwrap();
        let (cell_after, installed_after) = (cell(), installed());
        let phases = ["linear_phase", "minimum_phase"].map(|p| find_precomputed_filter(taps, src, out, p));
        let own = cached(taps, src, out);
        std::fs::remove_file(&kept).ok();
        assert_eq!(cell_before, cell_after, "the inventory");
        assert_eq!(installed_before, installed_after, "the radio's installed lengths");
        for p in phases {
            assert!(p.map_or(true, |p| !p.contains(PHASE_SUFFIX)), "a phase's lookup");
        }
        let (path, k) = own.expect("its own lookup finds it");
        assert!(path.ends_with("fir_1M_768000_stream_linear_v1.npy") && k == 38_400, "{path} {k}");
    }

    #[test]
    fn the_ramps_run_from_0_to_1_and_rise() {
        let r = Ramp::new();
        assert_eq!((r.at(0.0), r.at(1.0)), (0.0, 1.0));
        // Half way, half up (the table's first and middle steps differ: 1e-4).
        assert!((r.at(0.5) - 0.5).abs() < 1e-3, "{}", r.at(0.5));
        let mut prev = 0.0;
        for i in 0..=1000 {
            let w = r.at(i as f64 / 1000.0);
            assert!(w >= prev);
            prev = w;
        }
        let f = fade_in(3528);
        assert_eq!(f[0], 0.0);
        assert!(f.windows(2).all(|p| p[1] >= p[0]));
        assert!(1.0 - f[3527] < 1e-6 && f[1763] > 0.49 && f[1764] < 0.51);
    }

    /// The three zones on a pair made as the shipped ones are (2^17 taps at
    /// 88.2 and 96 kHz — a filter this short is cut 50 ms ahead at the same
    /// fraction of its length as 1M is at 352.8): in the band it is the
    /// linear filter, its stopband is the linear filter's or better, its wall
    /// is where the linear filter's is; ahead of the window there is nothing
    /// left; and its output lines up with the linear filter's at its trim.
    #[test]
    fn it_is_the_linear_filter_in_the_band_at_the_wall_and_in_the_stopband() {
        for rate in [88_200u32, 96_000] {
            let n = 1 << 17;
            let lin = designed(n, rate);
            let min_ph = cepstral(&lin);
            let cancel = AtomicBool::new(false);
            let (h, c) = derive(&lin, &min_ph, rate, &cancel).unwrap_or_else(|e| panic!("{rate} Hz: {e}"));
            assert_eq!(h.len(), n);
            assert!(c.ahead_db < -185.0 && c.after_db < -180.0, "{rate} Hz: {c:?}");
            assert!(c.band_db < -160.0 && c.magnitude_db < 1e-6 && c.wall_db < -155.0, "{rate} Hz: {c:?}");
            assert!(c.wall_relative_db < 0.01, "{rate} Hz: {c:?}");
            assert!(c.stop_near_db < 0.0 && c.stop_far_db < 0.0, "{rate} Hz: {c:?}");
            // The peak is K into the window: the linear filter's centre.
            let k = look_ahead(n, rate).unwrap();
            let peak = h.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
            assert_eq!(peak, k, "{rate} Hz");
            // Its first tap is faded to nothing.
            assert_eq!(h[0], 0.0);
        }
    }

    /// The look-ahead cannot be saved on quietly: at 10 ms the crossover's
    /// ringing ahead of the window is still there (−135 dB at 96 kHz), and
    /// the checks refuse the filter rather than keep it.
    #[test]
    fn ten_ms_ahead_is_refused() {
        let rate = 96_000;
        let lin = designed(1 << 17, rate);
        let min_ph = cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let k = (rate as u64 * 10 / 1000) as usize;
        let err = derive_at(&lin, &min_ph, rate, k, k / 5, &cancel).expect_err("10 ms must fail its checks");
        assert!(err.contains("energy ahead of the window") && err.contains("error in the band"), "{err}");
    }

    /// A minimum-phase filter that is not one — the linear filter itself, or
    /// the minimum-phase one played backwards — leaves the wall ringing ahead
    /// of the window, and the checks refuse it.
    #[test]
    fn a_broken_minimum_phase_filter_is_refused() {
        let rate = 88_200;
        let lin = designed(1 << 17, rate);
        let min_ph = cepstral(&lin);
        let backwards: Vec<f64> = min_ph.iter().rev().copied().collect();
        let cancel = AtomicBool::new(false);
        for (what, broken) in [("the linear filter", &lin), ("backwards", &backwards)] {
            let err = derive(&lin, broken, rate, &cancel).expect_err(what);
            assert!(err.contains("energy ahead of the window"), "{what}: {err}");
        }
    }

    #[test]
    fn a_short_or_mismatched_pair_is_refused_and_a_cancelled_one_stops() {
        let cancel = AtomicBool::new(false);
        let lin = designed(5_000, 352_800);
        assert!(derive(&lin, &lin, 352_800, &cancel).is_err(), "5k is shorter than the look-ahead");
        let long = designed(1 << 17, 88_200);
        assert!(derive(&long, &long[..1000], 88_200, &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(derive(&long, &cepstral(&long), 88_200, &cancel).unwrap_err(), "Cancelled");
    }

    /// The making as streams and the radio's view use it, on a `Prep` of its own:
    /// two streams that want one filter wait for one making of it; one made
    /// ahead for another filter stops when a stream needs this one; a stream
    /// waiting for a making that failed hears why.
    #[test]
    fn one_making_at_a_time_and_a_stream_comes_first() {
        let prep: &'static Prep = Box::leak(Box::new(Prep::new()));
        let made = Arc::new(Mutex::new(None::<String>));
        let count = Arc::new(AtomicUsize::new(0));
        let make = |made: Arc<Mutex<Option<String>>>, count: Arc<AtomicUsize>| -> Make {
            Box::new(move |_cancel: &AtomicBool| {
                count.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
                *made.lock().unwrap() = Some("made.npy".into());
                Ok("made.npy".into())
            })
        };
        let key: Key = ("1M", 352_800);
        let streams: Vec<_> = (0..2)
            .map(|_| {
                let (made, count) = (made.clone(), count.clone());
                std::thread::spawn(move || {
                    let seen = made.clone();
                    prep.obtain(key, &move || seen.lock().unwrap().clone(), true, || make(made, count))
                })
            })
            .collect();
        for s in streams {
            assert_eq!(s.join().unwrap(), Ok(Some("made.npy".into())));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1, "made once");
        assert!(!prep.busy());

        // Made ahead for 30M, then a stream needs 5M: the 30M making stops.
        let stopped = Arc::new(AtomicBool::new(false));
        let s2 = stopped.clone();
        prep.ahead(Some((
            ("30M", 352_800),
            &|| false,
            true,
            Box::new(move |cancel: &AtomicBool| {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                s2.store(true, Ordering::SeqCst);
                Err("Cancelled".into())
            }),
        )));
        assert!(prep.busy(), "made ahead");
        let five = Arc::new(Mutex::new(None::<String>));
        let f2 = five.clone();
        let got = prep.obtain(("5M", 352_800), &move || f2.lock().unwrap().clone(), true, || {
            let five = five.clone();
            Box::new(move |_c: &AtomicBool| {
                *five.lock().unwrap() = Some("5M.npy".into());
                Ok("5M.npy".into())
            }) as Make
        });
        assert_eq!(got, Ok(Some("5M.npy".into())));
        let t0 = std::time::Instant::now();
        while !stopped.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(stopped.load(Ordering::SeqCst), "the 30M made ahead stopped");

        // A making that fails: the stream waiting hears why; with no pair,
        // nothing is made and the shipped filter plays.
        let got = prep.obtain(("10M", 384_000), &|| None, true, || {
            Box::new(|_c: &AtomicBool| Err("the stream filter fails its checks: x".to_string())) as Make
        });
        assert_eq!(got, Err("the stream filter fails its checks: x".into()));
        let got = prep.obtain(("10M", 768_000), &|| None, false, || -> Make { unreachable!() });
        assert_eq!(got, Ok(None));
    }

    /// The Rust filter against the study's numpy one (`linvar.py`, the same
    /// construction): 1M at 352.8 kHz, both at unity DC gain. Run with
    /// AURA_FILTER_DIR (the shipped blobs) and AURA_STREAM_LINEAR_REF (the
    /// study's `lv_1M_352800_B_20000_20900_0.05_1_0_0.01.npy`: its window's
    /// start, then the window).
    #[test]
    #[ignore]
    fn matches_the_studys_filter() {
        let (Some(dir), Some(reference)) = (std::env::var_os("AURA_FILTER_DIR"), std::env::var_os("AURA_STREAM_LINEAR_REF")) else {
            eprintln!("AURA_FILTER_DIR and AURA_STREAM_LINEAR_REF are not set");
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let load = |name: &str| crate::audio::dsp_core::load_npy_f64(dir.join(name).to_str().unwrap()).unwrap();
        let lin = load("fir_1M_352800_linear_phase.npy");
        let min_ph = load("fir_1M_352800_minimum_phase.npy");
        let r = crate::audio::dsp_core::load_npy_f64(std::path::Path::new(&reference).to_str().unwrap()).unwrap();
        let t0 = std::time::Instant::now();
        let (h, c) = derive(&lin, &min_ph, 352_800, &AtomicBool::new(false)).expect("derive");
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(r[0] as usize, (lin.len() - 1) / 2 - 17_640, "the window starts 50 ms ahead");
        let sum: f64 = h.iter().sum();
        let peak = r[1..].iter().fold(0.0f64, |a, &v| a.max(v.abs()));
        let (mut worst, mut at) = (0.0f64, 0usize);
        let mut diff_e = 0.0f64;
        let mut ref_e = 0.0f64;
        for (i, (&a, &b)) in h.iter().zip(&r[1..]).enumerate() {
            let d = (a / sum - b).abs();
            diff_e += d * d;
            ref_e += b * b;
            if d > worst {
                worst = d;
                at = i;
            }
        }
        eprintln!(
            "1M@352800: {:.2} s; worst |Δ| {:.3e} of the peak at tap {}; difference energy {:.1} dB; {:?}",
            secs,
            worst / peak,
            at,
            db_energy(diff_e / ref_e),
            c
        );
        assert!(worst / peak < 1e-12, "worst |Δ| {:.3e} of the peak", worst / peak);
    }

    /// The longer filters made from the shipped pairs (`AURA_STREAM_LINEAR_SIZES`,
    /// default "30M"; both families): how long, the process's peak working
    /// set, and the checks. Run with AURA_FILTER_DIR, alone (30M takes ~5 GB);
    /// nothing is written.
    #[test]
    #[ignore]
    fn measure_the_long_ones() {
        use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, SystemExt};
        if std::env::var_os("AURA_FILTER_DIR").is_none() {
            eprintln!("AURA_FILTER_DIR is not set");
            return;
        }
        let sizes = std::env::var("AURA_STREAM_LINEAR_SIZES").unwrap_or_else(|_| "30M".into());
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        for size in sizes.split(',') {
            let taps = match size.trim() {
                "1M" => 1_000_000,
                "5M" => 5_000_000,
                "10M" => 10_000_000,
                _ => 30_000_000,
            };
            for rate in [352_800u32, 384_000] {
                let (Some(lp), Some(mp)) = (find_blob(taps, rate, "linear_phase"), find_blob(taps, rate, "minimum_phase")) else {
                    eprintln!("{size} at {rate} Hz is not installed");
                    continue;
                };
                let lin = crate::audio::dsp_core::load_npy_f64(&lp).expect("lin");
                let min_ph = crate::audio::dsp_core::load_npy_f64(&mp).expect("min");
                let pid = sysinfo::Pid::from_u32(std::process::id());
                let mut sys = sysinfo::System::new();
                sys.refresh_processes_specifics(ProcessRefreshKind::new());
                let mut ws = move || {
                    sys.refresh_process(pid);
                    sys.process(pid).map(|p| p.memory()).unwrap_or(0)
                };
                let before = ws();
                let done = Arc::new(AtomicBool::new(false));
                let watch = {
                    let done = done.clone();
                    std::thread::spawn(move || {
                        let mut peak = 0u64;
                        while !done.load(Ordering::Relaxed) {
                            peak = peak.max(ws());
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        peak.max(ws())
                    })
                };
                let t0 = std::time::Instant::now();
                let r = derive(&lin, &min_ph, rate, &AtomicBool::new(false));
                let secs = t0.elapsed().as_secs_f64();
                done.store(true, Ordering::Relaxed);
                let peak = watch.join().unwrap();
                eprintln!(
                    "{}@{}: {:.1} s; working set {:.2} GiB with the pair, peak {:.2} GiB (+{:.2}); the gate reserves {:.2} GiB; {:?}",
                    size,
                    rate,
                    secs,
                    gib(before),
                    gib(peak),
                    gib(peak.saturating_sub(before)),
                    super::super::tfs::derive_mb(taps) as f64 / 1024.0,
                    r.as_ref().map(|x| x.1)
                );
                r.expect("passes its checks");
            }
        }
    }
}
