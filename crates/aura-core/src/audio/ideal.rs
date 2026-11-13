//! The ideal filter of one track, made by formula (the tap ladder's Auto position).
//!
//! A track `K` source frames long only ever meets the taps of a linear-phase filter at lags within ±K frames, so
//! the filter that is exactly enough for it is a pure sinc with no window, `2·K·L` taps at the output rate:
//! more taps change no sample. Its minimum-phase partner is causal, so the track needs its first `K·L + 1` taps;
//! they come from the closed form of the minimum-phase wall with a flat floor of `A` dB,
//! `H(z) = k·[(1 − e^{jw}z⁻¹)/(1 − e^{−jw}z⁻¹)]^{jμ}`, μ = A·ln10/(20π), k = e^{−μ(π−w)}, by its recurrence
//! `(n+1)·h[n+1] = (2n·cos w + 2μ·sin w)·h[n] − (n−1)·h[n−1]`.
//!
//! Nothing is stored: a filter is named by a path of its own ([`path`], it ends in the branch-0 suffix) that
//! `dsp_core`'s branch-0 readers recognise and answer from here, so every place that plays a branch-0 rung
//! (Multi) plays this one the same way.

use crate::audio::dsp_core::{NpyBranch, BRANCH0_SUFFIX, BRANCH0_TAIL_TAPS};

/// The tap ladder's value that asks for the ideal filter (the slider's top position).
pub const AUTO_TAPS: usize = 2_000_000_000;

/// Whether the ladder's value `taps` asks for a track's own filter.
///
/// Its grid is the rack's own switch (`auto_shift`, on by default): on, the filter has an even length, so its centre
/// falls between two taps as in the stored rungs (1G was 10⁹ taps) and the output's samples sit half an output sample
/// off the source's own instants; off, the odd length puts every source sample on an output one. The two are the same
/// continuous waveform sampled at instants half an output sample apart.
pub fn is_auto(taps: usize) -> bool {
    taps == AUTO_TAPS
}

/// The minimum-phase floor, dB (closest to the shipped 1G minimum-phase filter); `AURA_IDEAL_FLOOR_DB` tries another.
pub const FLOOR_DB: f64 = 270.0;

const SCHEME: &str = "ideal-";

/// The whole filter's first taps the readers keep (`dsp_core::GROUP_DELAY_ANALYSIS_TAPS`).
const HEAD_TAPS: usize = 131_072;

fn floor_db() -> f64 {
    std::env::var("AURA_IDEAL_FLOOR_DB").ok().and_then(|v| v.parse::<f64>().ok()).filter(|a| (60.0..=400.0).contains(a)).unwrap_or(FLOOR_DB)
}

/// One ideal filter: `taps` of them at `rate` (the design rate, source family base × `l`), the wall at `fc` Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub linear: bool,
    pub rate: u32,
    pub fc: u32,
    pub l: usize,
    /// The whole filter's length at the output rate.
    pub taps: usize,
    /// The minimum-phase floor, dB (unused by the linear one).
    pub floor_db: f64,
}

/// The wall of a filter designed at `design_rate` for ×`l`: 21 050 Hz for the 44.1 kHz family, 23 000 Hz for 48.
pub fn wall_hz(design_rate: u32, l: usize) -> u32 {
    if (design_rate / l.max(1) as u32) % 11_025 == 0 { 21_050 } else { 23_000 }
}

/// The taps the linear filter of a track `src_frames` long has at ×`l`; what a rack on Auto shows for the track.
/// `even`: the length is even (the shifted grid), one tap fewer.
pub fn linear_taps(src_frames: u64, l: usize, even: bool) -> usize {
    2 * (src_frames.max(1) as usize) * l + if even { 0 } else { 1 }
}

/// `AURA_IDEAL_SHIFT=<n>` (a listening test): the linear filter's centre moved by n half-taps (n/2 output samples)
/// from the middle of its length, which the chain does not take back: the output is that much later.
fn centre_shift() -> i64 {
    std::env::var("AURA_IDEAL_SHIFT").ok().and_then(|v| v.parse::<i64>().ok()).filter(|n| n.abs() <= 4096).unwrap_or(0)
}

/// The taps of its minimum-phase partner.
pub fn minimum_taps(src_frames: u64, l: usize) -> usize {
    (src_frames.max(1) as usize) * l + 1
}

/// The ideal filter of a track `src_frames` long (source frames), designed at `design_rate` for ×`l`;
/// `phase` as the blobs name it (`linear_phase` / `minimum_phase`); `even` as [`linear_taps`] takes it.
pub fn spec_for(src_frames: u64, design_rate: u32, l: usize, phase: &str, even: bool) -> Spec {
    let linear = phase != "minimum_phase";
    Spec {
        linear,
        rate: design_rate,
        fc: wall_hz(design_rate, l),
        l,
        taps: if linear { linear_taps(src_frames, l, even) } else { minimum_taps(src_frames, l) },
        floor_db: if linear { 0.0 } else { floor_db() },
    }
}

/// The path that names `s`: no file, a name the branch-0 readers answer from [`branch0`].
pub fn path(s: &Spec) -> String {
    let kind = if s.linear { "lin".to_string() } else { format!("min{}", s.floor_db) };
    format!("{SCHEME}{kind}-{}-{}-x{}-{}{}", s.rate, s.fc, s.l, s.taps, BRANCH0_SUFFIX)
}

/// The filter a path of [`path`] names; `None` for any other path.
pub fn parse(path: &str) -> Option<Spec> {
    let name = path.rsplit(['/', '\\']).next()?;
    let body = name.strip_prefix(SCHEME)?.strip_suffix(BRANCH0_SUFFIX)?;
    let mut it = body.split('-');
    let kind = it.next()?;
    let rate = it.next()?.parse().ok()?;
    let fc = it.next()?.parse().ok()?;
    let l = it.next()?.strip_prefix('x')?.parse().ok()?;
    let taps = it.next()?.parse().ok()?;
    let (linear, floor_db) = match kind.strip_prefix("min") {
        Some(a) => (false, a.parse().ok()?),
        None if kind == "lin" => (true, 0.0),
        None => return None,
    };
    (it.next().is_none() && l > 0 && taps > 0 && rate > 0).then_some(Spec { linear, rate, fc, l, taps, floor_db })
}

pub fn is_ideal_path(path: &str) -> bool {
    parse(path).is_some()
}

/// A tap count as the player writes it for the listener: 86M, 423M, 1.27G.
pub fn taps_text(n: usize) -> String {
    let n = n as f64;
    if n >= 999.5e6 {
        format!("{:.2}G", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.0}M", n / 1e6)
    } else {
        format!("{:.0}k", (n / 1e3).max(1.0))
    }
}

/// Whether `t` reads as [`taps_text`] writes a size.
pub fn is_taps_text(t: &str) -> bool {
    let Some(num) = t.strip_suffix(['k', 'M', 'G']) else { return false };
    !num.is_empty() && num.parse::<f64>().is_ok() && num.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The pure sinc's taps: `at(i)` is tap `i` of the whole filter. The wall as a fraction of Nyquist is p/q, so the
/// sine's argument is periodic in the tap index and comes from an exact table (no angle ever exceeds π/2).
struct Sinc {
    p: u64,
    q: u64,
    per: u64,
    table: Vec<f64>,
    /// Twice the centre's index, `taps − 1`: odd for an even length, whose centre is between two taps.
    centre2: i64,
}

impl Sinc {
    fn new(s: &Spec) -> Sinc {
        let (fc, fs) = (s.fc as u64, s.rate as u64);
        let g = gcd(2 * fc, fs);
        let (p, q) = (2 * fc / g, fs / g);
        let per = 4 * q;
        let table = (0..per)
            .map(|r| {
                let (r2, sign) = if r < 2 * q { (r, 1.0) } else { (r - 2 * q, -1.0) };
                let r3 = if r2 <= q { r2 } else { 2 * q - r2 };
                sign * (std::f64::consts::PI * r3 as f64 / (2 * q) as f64).sin()
            })
            .collect();
        Sinc { p, q, per, table, centre2: s.taps as i64 - 1 + centre_shift() }
    }

    /// h[i] = sin(π·(p/q)·x)/(π·x), x = i − centre: the ideal low-pass of unit gain.
    #[inline]
    fn at(&self, i: usize) -> f64 {
        // y = 2x, a whole number for either parity of the length.
        let y = 2 * i as i64 - self.centre2;
        if y == 0 {
            return self.p as f64 / self.q as f64;
        }
        // sin(π·p·x/q) = table[(p·y) mod 4q], the table being sin(π·r/(2q)).
        let r = ((self.p as i128 * y as i128).rem_euclid(self.per as i128)) as usize;
        // + 0.0: a zero of the sinc is +0 on both sides of the centre.
        self.table[r] / (std::f64::consts::PI * (y as f64 * 0.5)) + 0.0
    }
}

fn linear_branch0(s: &Spec) -> NpyBranch {
    use rayon::prelude::*;
    let sinc = Sinc::new(s);
    let n0 = s.taps.div_ceil(s.l);
    let mut taps = vec![0.0f64; n0];
    taps.par_chunks_mut(1 << 16).enumerate().for_each(|(c, out)| {
        let base = c << 16;
        for (k, v) in out.iter_mut().enumerate() {
            *v = sinc.at((base + k) * s.l);
        }
    });
    let head = (0..HEAD_TAPS.min(s.taps)).map(|i| sinc.at(i)).collect();
    // Σh of the whole (endless) sinc: its gain at DC, one. The same for every length, so a longer filter is the
    // shorter one with taps added at its ends and nothing else changed.
    NpyBranch { taps, sum: 1.0, full_len: s.taps, head }
}

/// The minimum-phase wall's taps, in order: `f(i, h[i])` for every tap of the whole filter.
fn minimum_walk(s: &Spec, mut f: impl FnMut(usize, f64)) {
    let mu = s.floor_db * std::f64::consts::LN_10 / 20.0 / std::f64::consts::PI;
    let w = 2.0 * std::f64::consts::PI * s.fc as f64 / s.rate as f64;
    let (c2, s2) = (2.0 * w.cos(), 2.0 * mu * w.sin());
    let k = (-mu * (std::f64::consts::PI - w)).exp();
    let (mut prev, mut cur) = (k, s2 * k);
    f(0, prev);
    if s.taps > 1 {
        f(1, cur);
    }
    for i in 1..s.taps.saturating_sub(1) {
        let x = i as f64;
        let next = ((x * c2 + s2) * cur - (x - 1.0) * prev) / (x + 1.0);
        f(i + 1, next);
        prev = cur;
        cur = next;
    }
}

fn minimum_branch0(s: &Spec) -> NpyBranch {
    let mut taps = Vec::with_capacity(s.taps.div_ceil(s.l));
    let mut head = Vec::with_capacity(HEAD_TAPS.min(s.taps));
    let mut sum = -0.0f64;
    let (l, head_n) = (s.l, HEAD_TAPS.min(s.taps));
    minimum_walk(s, |i, v| {
        sum += v;
        if i % l == 0 {
            taps.push(v);
        }
        if i < head_n {
            head.push(v);
        }
    });
    NpyBranch { taps, sum, full_len: s.taps, head }
}

/// Branch 0 of the filter `s` at ×`l`, as `dsp_core::load_npy_f64_branch` gives a stored branch-0 rung.
pub fn branch0(s: &Spec, l: usize) -> Result<NpyBranch, String> {
    if s.l != l {
        return Err(format!("The ideal filter is made for ×{}, asked for at ×{}", s.l, l));
    }
    Ok(if s.linear { linear_branch0(s) } else { minimum_branch0(s) })
}

/// The whole filter's last `n` (≤ [`BRANCH0_TAIL_TAPS`]) taps.
pub fn tail(s: &Spec, n: usize) -> Result<Vec<f64>, String> {
    if n > BRANCH0_TAIL_TAPS.min(s.taps) {
        return Err(format!("An ideal filter keeps {} last taps, {} asked for", BRANCH0_TAIL_TAPS.min(s.taps), n));
    }
    if s.linear {
        let sinc = Sinc::new(s);
        return Ok((s.taps - n..s.taps).map(|i| sinc.at(i)).collect());
    }
    let mut out = Vec::with_capacity(n);
    let from = s.taps - n;
    minimum_walk(s, |i, v| {
        if i >= from {
            out.push(v);
        }
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(linear: bool, taps: usize) -> Spec {
        Spec { linear, rate: 705_600, fc: 21_050, l: 16, taps, floor_db: if linear { 0.0 } else { 270.0 } }
    }

    /// The gain of `h` at `hz` (a plain DFT sum at one frequency).
    fn gain(h: &[f64], rate: f64, hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / rate;
        let (mut re, mut im) = (0.0, 0.0);
        for (i, v) in h.iter().enumerate() {
            re += v * (w * i as f64).cos();
            im -= v * (w * i as f64).sin();
        }
        (re * re + im * im).sqrt()
    }

    fn whole(s: &Spec) -> Vec<f64> {
        if s.linear {
            let sinc = Sinc::new(s);
            (0..s.taps).map(|i| sinc.at(i)).collect()
        } else {
            let mut h = Vec::new();
            minimum_walk(s, |_, v| h.push(v));
            h
        }
    }

    #[test]
    fn a_path_names_its_filter_and_nothing_else_is_one() {
        for s in [spec(true, 423_360_001), spec(false, 211_680_001), Spec { floor_db: 222.5, ..spec(false, 17) }] {
            let p = path(&s);
            assert!(crate::audio::dsp_core::is_branch0_file(&p));
            assert_eq!(parse(&p), Some(s.clone()));
            assert_eq!(parse(&format!("C:\\somewhere\\{p}")), Some(s));
        }
        assert_eq!(parse("fir_1G_705600_linear_phase.b0.npy"), None);
        assert_eq!(parse("ideal-lin-705600-21050-x16-0.b0.npy"), None);
        assert_eq!(parse("ideal-lin-705600-21050-x16-33.npy"), None);
    }

    #[test]
    fn a_track_gets_two_taps_a_frame_at_the_output_rate_and_half_of_them_in_minimum_phase() {
        // 5 min of 44.1 kHz at ×16.
        let frames = 300 * 44_100;
        assert_eq!(linear_taps(frames, 16, false), 423_360_001);
        assert_eq!(linear_taps(frames, 16, true), 423_360_000);
        assert_eq!(minimum_taps(frames, 16), 211_680_001);
        let s = spec_for(frames, 705_600, 16, "linear_phase", false);
        assert_eq!((s.linear, s.fc, s.taps), (true, 21_050, 423_360_001));
        assert_eq!(spec_for(frames, 705_600, 16, "linear_phase", true).taps, 423_360_000);
        assert_eq!(spec_for(frames, 768_000, 16, "minimum_phase", true).fc, 23_000);
        // A hi-res source takes the filter of its own factor: 88.2 kHz ×8 is designed as 44.1 kHz ×8.
        assert_eq!(wall_hz(352_800, 8), 21_050);
        assert_eq!(taps_text(423_360_001), "423M");
        assert_eq!(taps_text(1_270_080_001), "1.27G");
        assert_eq!(taps_text(600_000), "600k");
        assert!(is_taps_text("423M") && is_taps_text("1.27G") && is_taps_text("600k"));
        assert!(!is_taps_text("PFR") && !is_taps_text("M") && !is_taps_text("x8M") && !is_taps_text("HP"));
    }

    #[test]
    fn the_linear_filter_is_a_symmetric_sinc_whose_branch_0_holds_its_centre() {
        let s = spec(true, 2 * 16 * 4_000 + 1);
        let h = whole(&s);
        let n = h.len();
        for i in 0..n / 2 {
            assert_eq!(h[i].to_bits(), h[n - 1 - i].to_bits(), "tap {i}");
        }
        let b = branch0(&s, 16).unwrap();
        assert_eq!(b.full_len, n);
        assert_eq!(b.taps.len(), n.div_ceil(16));
        for (k, v) in b.taps.iter().enumerate() {
            assert_eq!(v.to_bits(), h[k * 16].to_bits());
        }
        assert_eq!(b.taps[b.taps.len() / 2], 2.0 * 21_050.0 / 705_600.0);
        assert_eq!(b.head.len(), HEAD_TAPS.min(n));
        assert_eq!(b.head[..], h[..b.head.len()]);
        assert_eq!(tail(&s, 50).unwrap()[..], h[n - 50..]);
        assert!(tail(&s, 65).is_err());
        assert!(branch0(&s, 8).is_err());
        // Unit gain in the band, nothing above the wall (a truncated sinc: to its own ripple).
        assert!((gain(&h, 705_600.0, 1_000.0) - 1.0).abs() < 2e-3);
        assert!((gain(&h, 705_600.0, 15_000.0) - 1.0).abs() < 2e-3);
        assert!(gain(&h, 705_600.0, 30_000.0) < 2e-3);
    }

    #[test]
    fn an_even_linear_filter_is_the_sinc_half_a_tap_off_centre_and_mirrors_itself() {
        let s = spec(true, 2 * 16 * 4_000);
        let h = whole(&s);
        let n = h.len();
        for i in 0..n / 2 {
            assert_eq!(h[i].to_bits(), h[n - 1 - i].to_bits(), "tap {i}");
        }
        let (fc, fs) = (21_050.0f64, 705_600.0f64);
        for i in [0usize, 1, n / 2 - 1, n / 2, n / 2 + 7, n - 1] {
            let x = i as f64 - (n as f64 - 1.0) / 2.0;
            let want = (2.0 * std::f64::consts::PI * fc / fs * x).sin() / (std::f64::consts::PI * x);
            assert!((h[i] - want).abs() < 1e-15, "tap {i}: {} vs {want}", h[i]);
        }
        let b = branch0(&s, 16).unwrap();
        assert_eq!(b.taps.len(), n / 16);
        assert!((gain(&h, fs, 1_000.0) - 1.0).abs() < 2e-3);
        assert!(gain(&h, fs, 30_000.0) < 2e-3);
    }

    #[test]
    fn a_longer_linear_filter_is_the_shorter_one_with_taps_added_at_its_ends() {
        let (short, long) = (spec(true, 2 * 16 * 1_000 + 1), spec(true, 2 * 16 * 3_000 + 1));
        let (a, b) = (whole(&short), whole(&long));
        let off = (b.len() - a.len()) / 2;
        for (i, v) in a.iter().enumerate() {
            assert_eq!(v.to_bits(), b[off + i].to_bits(), "tap {i}");
        }
        assert_eq!(branch0(&short, 16).unwrap().sum, branch0(&long, 16).unwrap().sum);
    }

    #[test]
    fn the_minimum_phase_filter_is_a_causal_wall_with_unit_passband() {
        let s = spec(false, 16 * 60_000 + 1);
        let h = whole(&s);
        let b = branch0(&s, 16).unwrap();
        assert_eq!(b.taps.len(), h.len().div_ceil(16));
        for (k, v) in b.taps.iter().enumerate() {
            assert_eq!(v.to_bits(), h[k * 16].to_bits());
        }
        assert_eq!(b.sum.to_bits(), h.iter().fold(-0.0f64, |a, v| a + v).to_bits());
        assert_eq!(b.head[..], h[..b.head.len()]);
        assert_eq!(tail(&s, 64).unwrap()[..], h[h.len() - 64..]);
        // The energy sits at the start (causal, front-loaded), the passband is flat, the stopband is far down.
        let peak = h.iter().enumerate().fold((0, 0.0f64), |m, (i, v)| if v.abs() > m.1 { (i, v.abs()) } else { m });
        assert!(peak.0 < 400, "peak at tap {}", peak.0);
        assert!((gain(&h, 705_600.0, 1_000.0) - 1.0).abs() < 1e-3);
        assert!((gain(&h, 705_600.0, 15_000.0) - 1.0).abs() < 1e-3);
        assert!(gain(&h, 705_600.0, 40_000.0) < 1e-3);
    }
}
