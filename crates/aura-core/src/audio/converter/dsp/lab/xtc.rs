//! Crosstalk Cancellation (XTC / transaural) — optional, opt-in spatial processor.
//!
//! ── LAB COPY. What changed, and why it had to ────────────────────────────────────────
//! The prototype this is derived from never ran: it was never registered in `dsp/mod.rs`
//! and nothing ever called it. That turned out to be lucky, because it inverted the very
//! thing it exists to do.
//!
//! Pass 2 normalises the two eigen-branches so the mono path comes out flat. The intent
//! is right and the magnitude was right — `rel` is already the RATIO |G₋|/|G₊|. The phase
//! was not: it kept `arg(G₋)` where dividing both branches by G₊ requires the DIFFERENCE
//! `arg(G₋) − arg(G₊)`. Scaling both branches by a common complex factor leaves the
//! cancellation exact (C·αH = αI); keeping one branch's absolute phase does not.
//!
//! Measured on the default 2000/3000/180 geometry, off-diagonal against diagonal of C·H —
//! what the WRONG ear hears relative to the right one:
//!
//!     f (Hz)      300    500    800   1000   2000   3000   4000   6000
//!     before     -0.4   +8.9   +7.3   -2.2   +6.1   +7.1  +16.7   -5.1     ← amplifies
//!     after      -0.4   -8.1  -26.3  -36.1  -13.7   -8.8  -17.2  -46.1     ← cancels
//!
//! Positive is the failure: the far ear hearing MORE of the wrong channel than the near
//! ear hears of the right one. The old formula did that at 64 of 155 probe frequencies
//! between 400 Hz and 6.95 kHz, worst case +37.3 dB — which is a crossfeed amplifier, and
//! is exactly what "everything collapses to mono and sounds muffled" is the sound of.
//! `roundtrip_cancels` and `old_absolute_phase_would_amplify` below hold that shut.
//!
//! ⚠ This is NOT part of the transparent / bit-perfect signal path. It deliberately
//! COLORS the signal and produces output that is acoustically correct ONLY for the one
//! loudspeaker geometry it was designed for, played over SPEAKERS (never headphones).
//! It is the one stage in the product that the transparency argument in
//! `DSP_MANIFESTO.md` does not cover, and it is off unless asked for: nothing here runs
//! until a listening triangle has been measured.
//!
//! ── Model ───────────────────────────────────────────────────────────────────────────
//! Symmetric two-source / two-ear model. The 2×2 acoustic transfer matrix from the
//! loudspeakers to the ears (centered, symmetric listener) is
//!
//!     C(ω) = [[A, B],
//!             [B, A]]      A: near speaker → near ear      B: far speaker → near ear
//!
//! A and B are the sound on a rigid sphere at the seat's angle and range (`sphere_ear`).
//! The first version here took them from the straight lines from speaker to ear plus a
//! first-order shadow, and graded itself on the same lines. Sound goes around a head:
//! at ±47° the lines say 382 µs between the ears, MIT's KEMAR measures 510–660 µs below
//! 1 kHz and about 400 µs above 1.4 kHz, and a sphere gives 590 and 450. Against the
//! dummy that filter gained under 3 dB over no filter and made things worse at 18 % of the
//! band, while reporting −16 dB. This one gains 9 dB and makes things worse at 7 %.
//!
//! We need speaker feeds `s` so the ears receive the intended `d`: C·s = d → s = C⁻¹·d.
//! Because C is symmetric it diagonalizes in the sum/difference (mono/side) eigenbasis:
//!
//!     λ₊ = A + B   (mono / center path)        λ₋ = A − B   (side path)
//!
//! The regularized (Tikhonov / Kirkeby) inverse of each scalar branch is
//!
//!     G± = conj(λ±) / (|λ±|² + β²)
//!
//! Both are divided by G₊, which holds the mono path flat, and the 2×2 filter recombines:
//!
//!     H_direct = (1 + G)/2     H_cross = (1 − G)/2     G = G₋/G₊
//!
//! so that  outL = H_direct·inL + H_cross·inR ,  outR = H_direct·inR + H_cross·inL .
//!
//! |G| runs below 1 as well as above it. Where the far ear hears the near one's signal
//! half a period late (around 1 kHz at this angle) the speakers' side path is LOUDER than
//! their mono path, and evening them out takes a cut. A floor of 1 here forbade the cut,
//! and with it most of the cancelling from 800 to 1500 Hz. `SIDE_GAIN_FLOOR` and
//! `SIDE_GAIN_CAP` are what bound it now.
//!
//! ── Numerical robustness ─────────────────────────────────────────────────────────────
//! * The effect is band-limited (identity outside 150 Hz–4.5 kHz). Below, inverting takes
//!   a boost a room's modes would not honour anyway; above, the head separates the ears
//!   by itself (−16 dB and falling on KEMAR) and no model holds a pinna's detail.
//! * Both filters are extracted with a COMMON window center so the inter-channel delay
//!   (the ITD that does the actual cancellation) is preserved.
//! * The filter is the same length in milliseconds at every rate, so a rate does not
//!   change what it does at the bottom of the band.
//! * `apply_xtc` preserves the exact sample count and time alignment of its input.

use rustfft::{num_complex::Complex, FftPlanner};
use std::f64::consts::PI;

/// Speed of sound in mm/s (so all geometry math stays in millimetres).
const C_MM_S: f64 = 343_000.0;

/// Max boost applied to the SIDE (difference) channel, linear. +6 dB. The mono/center
/// channel is held perfectly flat (gain 1.0) so centered content is never colored.
const SIDE_GAIN_CAP: f64 = 2.0;

/// Max cut of the side channel, linear: −6 dB. On KEMAR this takes 600–1000 Hz from −9
/// to −19 dB; −12 dB takes it no further, and is that much more wrong for a head that moved.
const SIDE_GAIN_FLOOR: f64 = 0.5;

/// XTC active band (Hz): identity outside `F_LO..F_HI`, full inside `F_LO_FULL..F_HI_FADE`.
pub const F_LO: f64 = 150.0;
pub const F_LO_FULL: f64 = 300.0;
const F_HI_FADE: f64 = 3_000.0;
const F_HI: f64 = 4_500.0;

/// Where on the sphere the ears sit, degrees off the nose. Duda & Martens' listeners fit
/// 100°, KEMAR fits 90°; between them it does no harm to either.
const EAR_DEG: f64 = 95.0;

/// Half the filter, seconds, at every rate. 8 ms costs a dB at 150–300 Hz, 16 gains nothing.
const HALF_S: f64 = 0.012;

/// Tikhonov regularization β at the two ends, interpolated log-linearly in log-frequency.
/// Larger β = gentler inversion. Below `BETA_LO_HZ` it stays `BETA_LF`.
const BETA_LF: f64 = 0.30;
const BETA_HF: f64 = 0.10;
const BETA_LO_HZ: f64 = 300.0;
const BETA_HI_HZ: f64 = 8_000.0;

/// Fractional-octave magnitude smoothing width (1/3 octave) — removes the comb peaks/
/// notches of the raw inversion so the result is smooth and uncolored.
const SMOOTH_OCT: f64 = 1.0 / 3.0;

/// Quarter wavelength at 2 kHz, in millimetres. Past this much difference between the
/// two speaker-to-head distances one symmetric filter stops describing the room: the
/// timing error it cannot represent exceeds a quarter period at the frequency where the
/// cancellation still has to be accurate.
const ASYM_SOFT_MM: f64 = 42.9;

/// Design-time FFT size, at the least. One-time cost; gives ~11 Hz bin resolution at 705.6 kHz.
const DESIGN_NFFT: usize = 65_536;

/// Validated loudspeaker geometry. All distances in millimetres.
#[derive(Clone, Copy, Debug)]
pub struct XtcGeometry {
    pub speaker_spacing_mm: f64,
    pub listener_distance_mm: f64,
    pub head_width_mm: f64,
    /// Global effect amount, 0.0 (bypass) .. 1.0 (full design).
    pub strength: f64,
}

/// Frequency-dependent regularization schedule (log-linear in log-f).
fn beta_at(f: f64) -> f64 {
    if f <= BETA_LO_HZ {
        BETA_LF
    } else if f >= BETA_HI_HZ {
        BETA_HF
    } else {
        let t = (f / BETA_LO_HZ).ln() / (BETA_HI_HZ / BETA_LO_HZ).ln();
        (BETA_LF.ln() + t * (BETA_HF.ln() - BETA_LF.ln())).exp()
    }
}

/// How far XTC has come in at `f` from the bottom of its band, 0 to 1. Cue Consistency
/// comes in on this same curve: a difference it writes below it would not reach one ear.
pub fn low_edge(f: f64) -> f64 {
    if f <= F_LO {
        0.0
    } else if f >= F_LO_FULL {
        1.0
    } else {
        0.5 * (1.0 - (PI * (f - F_LO) / (F_LO_FULL - F_LO)).cos())
    }
}

/// Cosine fade weight in [0,1] selecting how much XTC to apply at frequency `f`.
/// 0 = identity (pass-through), 1 = full XTC. Smooth fades at both band edges.
fn band_blend(f: f64) -> f64 {
    if f < F_LO || f > F_HI {
        0.0
    } else if f < F_LO_FULL {
        low_edge(f)
    } else if f > F_HI_FADE {
        let t = (f - F_HI_FADE) / (F_HI - F_HI_FADE);
        0.5 * (1.0 + (PI * t).cos())
    } else {
        1.0
    }
}

// ─────────────────────────── the head ───────────────────────────

/// The sphere that acts like a head this wide, mm. A head's width moves its acoustic
/// radius only by a quarter of itself (Algazi et al. 2001), the rest being its depth
/// and height: a 150 mm tape reading is not a 75 mm sphere, which on KEMAR loses a third
/// of what the filter does.
fn head_radius_mm(head_width_mm: f64) -> f64 {
    45.0 + head_width_mm / 4.0
}

/// Sound at a point on a rigid sphere from a source `r_m` from its centre, `gamma` radians
/// off that point's axis, relative to the free field at the centre (Duda & Martens 1998,
/// with range). →1 as f→0 and r→∞. In e^{+jωt}: a delay is −d(arg)/dω, and r/c is already out.
fn sphere_ear(f: f64, r_m: f64, gamma: f64, a_m: f64) -> Complex<f64> {
    let c = C_MM_S / 1000.0;
    let mu = 2.0 * PI * f * a_m / c;
    if mu < 1e-4 {
        return Complex::new(1.0, 0.0);
    }
    let rho = r_m / a_m;
    let z = mu * rho;
    let x = gamma.cos();
    // h⁽²⁾₀ and h⁽²⁾₁, then upward: stable, y_m dominates.
    let first = |z: f64| {
        let (sn, cs) = z.sin_cos();
        (
            Complex::new(sn / z, cs / z),
            Complex::new(sn / (z * z) - cs / z, cs / (z * z) + sn / z),
        )
    };
    let (mut hz_prev, mut hz) = first(z);
    let (mut hm_prev, mut hm) = first(mu);
    let (mut p_prev, mut p) = (1.0, x);
    // m = 0: h'₀ = −h₁.
    let mut sum = hz_prev / (-hm);
    let last = (mu * 1.5).max(mu + 12.0) as usize + 12;
    for m in 1..=last {
        let k = m as f64;
        let dh = hm_prev - hm * ((k + 1.0) / mu);
        if !dh.norm().is_finite() {
            break;
        }
        sum += hz * ((2.0 * k + 1.0) * p) / dh;
        let hz_next = hz * ((2.0 * k + 1.0) / z) - hz_prev;
        let hm_next = hm * ((2.0 * k + 1.0) / mu) - hm_prev;
        hz_prev = hz;
        hz = hz_next;
        hm_prev = hm;
        hm = hm_next;
        let p_next = ((2.0 * k + 1.0) * x * p - k * p_prev) / (k + 1.0);
        p_prev = p;
        p = p_next;
    }
    sum * Complex::from_polar(-rho / mu, z)
}

/// The four paths at one frequency for a head `dx_mm` right of the centre line:
/// `[left speaker → left ear, → right ear, right speaker → left ear, → right ear]`,
/// relative to what the centred seat has, so the distances count when the head moves.
fn paths(span_mm: f64, depth_mm: f64, head_mm: f64, dx_mm: f64, f: f64) -> [Complex<f64>; 4] {
    let a_m = head_radius_mm(head_mm) / 1000.0;
    let s = span_mm * 0.5;
    let r0 = s.hypot(depth_mm);
    let one = |from_x: f64, ear_deg: f64| {
        let x = from_x - dx_mm;
        let r = x.hypot(depth_mm);
        let az = x.atan2(depth_mm).to_degrees();
        let off = (az - ear_deg).abs();
        let off = if off > 180.0 { 360.0 - off } else { off };
        let travel = Complex::from_polar(r0 / r, -2.0 * PI * f * (r - r0) / C_MM_S);
        sphere_ear(f, r / 1000.0, off.to_radians(), a_m) * travel
    };
    [one(-s, -EAR_DEG), one(-s, EAR_DEG), one(s, -EAR_DEG), one(s, EAR_DEG)]
}

/// B/A of the centred seat on a log-frequency table: a series per point once, a lookup
/// per bin after. Magnitude and phase go on the table separately, the phase unwrapped,
/// so a bin between two points is between them in both.
struct PlantTable {
    ln_lo: f64,
    step: f64,
    ln_mag: Vec<f64>,
    phase: Vec<f64>,
}

impl PlantTable {
    const LO_HZ: f64 = 20.0;
    const HI_HZ: f64 = 12_000.0;
    const POINTS: usize = 500;

    fn new(span_mm: f64, depth_mm: f64, head_mm: f64) -> Self {
        let ln_lo = Self::LO_HZ.ln();
        let step = (Self::HI_HZ.ln() - ln_lo) / (Self::POINTS - 1) as f64;
        let mut ln_mag = Vec::with_capacity(Self::POINTS);
        let mut phase: Vec<f64> = Vec::with_capacity(Self::POINTS);
        for i in 0..Self::POINTS {
            let f = (ln_lo + step * i as f64).exp();
            let r = ratio_at(span_mm, depth_mm, head_mm, f);
            ln_mag.push(r.norm().ln());
            let mut ph = r.arg();
            if let Some(&last) = phase.last() {
                ph += 2.0 * PI * ((last - ph) / (2.0 * PI)).round();
            }
            phase.push(ph);
        }
        Self { ln_lo, step, ln_mag, phase }
    }

    fn ratio(&self, f: f64) -> Complex<f64> {
        let x = ((f.max(1.0).ln() - self.ln_lo) / self.step).clamp(0.0, (Self::POINTS - 1) as f64);
        let i = (x as usize).min(Self::POINTS - 2);
        let t = x - i as f64;
        let m = self.ln_mag[i] + t * (self.ln_mag[i + 1] - self.ln_mag[i]);
        let p = self.phase[i] + t * (self.phase[i + 1] - self.phase[i]);
        Complex::from_polar(m.exp(), p)
    }
}

/// B/A of the centred seat: the far speaker against the near one, at one ear.
fn ratio_at(span_mm: f64, depth_mm: f64, head_mm: f64, f: f64) -> Complex<f64> {
    let [ll, lr, _, _] = paths(span_mm, depth_mm, head_mm, 0.0, f);
    lr / ll
}

/// The `(A, B)` the filter is designed for, A taken as the reference.
fn acoustic_ab(span_mm: f64, depth_mm: f64, head_mm: f64, f: f64) -> (Complex<f64>, Complex<f64>) {
    (Complex::new(1.0, 0.0), ratio_at(span_mm, depth_mm, head_mm, f))
}

/// Design the `(h_direct, h_cross)` XTC FIR pair for the given geometry and output rate.
///
/// Both returned vectors have the same odd length and share a common bulk group delay of
/// `(len-1)/2` samples (which `apply_xtc` removes). Returns `Err` for degenerate geometry.
pub fn design_xtc_filters(
    geo: XtcGeometry,
    out_rate: u32,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    let strength = geo.strength.clamp(0.0, 1.0);
    if strength < 1e-4 {
        // Bypass: identity 2×2 (direct = unit impulse, cross = 0).
        return Ok((vec![1.0], vec![0.0]));
    }

    let s = geo.speaker_spacing_mm * 0.5; // speaker half-offset from center axis
    let e = geo.head_width_mm * 0.5; // ear half-offset from center axis
    let d = geo.listener_distance_mm; // depth to the ear plane

    if !(s.is_finite() && e.is_finite() && d.is_finite()) {
        return Err("XTC disabled: non-finite geometry".to_string());
    }
    if d <= 0.0 {
        return Err("XTC disabled: listening distance must be positive".to_string());
    }
    if s <= e {
        return Err(
            "XTC disabled: speaker spacing must exceed head width".to_string(),
        );
    }

    // A pair so far away or so close together that it is one source to the head.
    let d_ip = ((s - e) * (s - e) + d * d).sqrt();
    let d_cc = ((s + e) * (s + e) + d * d).sqrt();
    let delta_r = d_cc - d_ip;
    if delta_r < 1.0 {
        return Err("XTC disabled: geometry too symmetric (Δpath < 1 mm)".to_string());
    }

    let fs = out_rate as f64;
    // The same bin width at the top rates as at the rest.
    let n = ((fs * 0.085).ceil() as usize).next_power_of_two().max(DESIGN_NFFT);
    let half = n / 2;

    let plant = PlantTable::new(geo.speaker_spacing_mm, d, geo.head_width_mm);
    let itd_us = |f: f64| {
        // Phase delay read by going up the table from the bottom, so it is unwrapped.
        let x = (f.ln() - plant.ln_lo) / plant.step;
        -plant.phase[x.round() as usize] / (2.0 * PI * f) * 1e6
    };

    crate::aelog!(
        "[XTC] Geometry: spacing={:.0}mm dist={:.0}mm head={:.0}mm (sphere radius {:.1}mm) strength={:.2}",
        geo.speaker_spacing_mm, geo.listener_distance_mm, geo.head_width_mm, head_radius_mm(geo.head_width_mm), strength
    );
    crate::aelog!(
        "[XTC] Far ear late by {:.0}us at 500Hz, {:.0}us at 3kHz (straight line {:.0}us)  N_fft={}",
        itd_us(500.0), itd_us(3000.0), delta_r / C_MM_S * 1e6, n
    );

    // ══ Pass 1: regularized scalar inverses of the mono (sum) and side (diff) eigen-branches,
    //    relative to the near path. We keep their magnitudes + the side phase. ══
    let mut mag_p = vec![0.0f64; half + 1]; // |mono inverse|
    let mut mag_m = vec![0.0f64; half + 1]; // |side inverse|
    let mut ph_p = vec![0.0f64; half + 1]; // arg(mono inverse) — the half that was dropped
    let mut ph_m = vec![0.0f64; half + 1]; // arg(side inverse) — the spatial timing
    for k in 0..=half {
        let f = k as f64 * fs / n as f64;
        let a = Complex::new(1.0, 0.0);
        let b = plant.ratio(f);
        let lam_p = a + b;
        let lam_m = a - b;
        let beta = beta_at(f);
        let b2 = beta * beta;
        let g_p = lam_p.conj() / (lam_p.norm_sqr() + b2);
        let g_m = lam_m.conj() / (lam_m.norm_sqr() + b2);
        mag_p[k] = g_p.norm();
        mag_m[k] = g_m.norm();
        ph_p[k] = g_p.arg();
        ph_m[k] = g_m.arg();
    }

    // Fractional-octave magnitude smoothing (prefix-sum averaging) — removes the comb.
    let smooth = |mag: &[f64]| -> Vec<f64> {
        let mut pre = vec![0.0f64; mag.len() + 1];
        for i in 0..mag.len() {
            pre[i + 1] = pre[i] + mag[i];
        }
        let up = 2f64.powf(SMOOTH_OCT);
        let dn = 2f64.powf(-SMOOTH_OCT);
        (0..mag.len())
            .map(|k| {
                let lo = ((k as f64) * dn).floor() as usize;
                let hi = (((k as f64) * up).ceil() as usize).min(mag.len() - 1);
                (pre[hi + 1] - pre[lo]) / (hi - lo + 1) as f64
            })
            .collect()
    };
    let sm_p = smooth(&mag_p);
    let sm_m = smooth(&mag_m);

    // ══ Pass 2: build H_direct / H_cross. The MONO (center) path is held perfectly flat
    //    (gain 1.0) so centered content is never colored; the SIDE path gets a smoothed,
    //    bounded gain plus the RELATIVE phase of the two inverses. Magnitude and phase are
    //    faded toward identity at the band edges.
    //
    //    Both branches are divided by G₊, which is what pins the mono path to 1∠0 without
    //    disturbing the ratio that does the cancelling. Magnitude ratio and phase
    //    difference are two halves of one operation — taking one and not the other is the
    //    defect described at the top of this file. ══
    let mut hd = vec![Complex::new(0.0, 0.0); n];
    let mut hc = vec![Complex::new(0.0, 0.0); n];
    let mut max_side = 0.0f64;
    let mut min_side = f64::INFINITY;
    let mut clamp_lo = f64::INFINITY;
    let mut clamp_hi = 0.0f64;
    for k in 0..=half {
        let f = k as f64 * fs / n as f64;
        let bl = band_blend(f) * strength;
        let raw_rel = sm_m[k] / sm_p[k].max(1e-9);
        let rel = raw_rel.clamp(SIDE_GAIN_FLOOR, SIDE_GAIN_CAP);
        if raw_rel > SIDE_GAIN_CAP && bl > 0.0 {
            clamp_lo = clamp_lo.min(f);
            clamp_hi = clamp_hi.max(f);
        }
        let mag_side = 1.0 + (rel - 1.0) * bl;
        // arg(G₋) − arg(G₊), wrapped to (−π, π] BEFORE the band fade so the fade shortens
        // the rotation it is actually applying rather than an aliased image of it.
        let mut dphi = ph_m[k] - ph_p[k];
        dphi = (dphi + PI).rem_euclid(2.0 * PI) - PI;
        let g_p = Complex::new(1.0, 0.0); // mono / center: flat, no coloration
        let g_m = Complex::from_polar(mag_side, dphi * bl); // side: boost + relative phase
        max_side = max_side.max(mag_side);
        min_side = min_side.min(mag_side);

        let h_direct = (g_p + g_m) * 0.5;
        let h_cross = (g_p - g_m) * 0.5;
        hd[k] = h_direct;
        hc[k] = h_cross;
        if k > 0 && k < half {
            hd[n - k] = h_direct.conj();
            hc[n - k] = h_cross.conj();
        }
    }
    // Nyquist bin must be real.
    hd[half].im = 0.0;
    hc[half].im = 0.0;

    crate::aelog!(
        "[XTC] Mono path flat | side gain {:.2}..{:.2}dB (bounds {:.1}, {:.1}dB) | band {}-{}Hz",
        20.0 * min_side.max(1e-12).log10(),
        20.0 * max_side.max(1e-12).log10(),
        20.0 * SIDE_GAIN_FLOOR.log10(),
        20.0 * SIDE_GAIN_CAP.log10(),
        F_LO as u32, F_HI as u32
    );
    if clamp_hi > 0.0 {
        crate::aelog!(
            "[XTC] side-gain cap reached from {:.0}Hz to {:.0}Hz — cancellation there is              shallower than the geometry allows (raise SIDE_GAIN_CAP to trade level for depth)",
            clamp_lo, clamp_hi
        );
    }

    // ── To time domain, with a circular N/2 shift so the (non-causal) inverse impulse is
    //    centered at index N/2. Multiplying the spectrum by (-1)^k = e^{-jπk} shifts by N/2
    //    and preserves conjugate symmetry (N is even), so the IFFT stays real. ──
    for k in 0..n {
        if k & 1 == 1 {
            hd[k] = -hd[k];
            hc[k] = -hc[k];
        }
    }
    let mut planner = FftPlanner::<f64>::new();
    let ifft = planner.plan_fft_inverse(n);
    ifft.process(&mut hd);
    ifft.process(&mut hc);
    let scale = 1.0 / n as f64;
    let raw_d: Vec<f64> = hd.iter().map(|c| c.re * scale).collect();
    let raw_c: Vec<f64> = hc.iter().map(|c| c.re * scale).collect();

    // ── The same length in time at every rate. Sized once by its decay it came out 11.7 ms
    //    at 44.1 kHz and 5.5 ms at 705.6 kHz, and what it did at the bottom of the band
    //    moved with the rate. ──
    let tap_half = ((HALF_S * fs) as usize).min(half - 1);
    let tap_len = 2 * tap_half + 1;

    // Extract a COMMON window centered at N/2 (preserves the inter-channel ITD) and apply a
    // Hann taper to suppress truncation ripple.
    let extract = |raw: &[f64]| -> Vec<f64> {
        let mut taps = Vec::with_capacity(tap_len);
        for i in 0..tap_len {
            let src = half - tap_half + i;
            let win = 0.5 * (1.0 - (2.0 * PI * i as f64 / (tap_len as f64 - 1.0)).cos());
            taps.push(raw[src] * win);
        }
        taps
    };
    let mut h_direct = extract(&raw_d);
    let mut h_cross = extract(&raw_c);

    // Normalize so the mono (correlated) DC gain is exactly 1.0. With band-limiting this
    // correction is ~1.0 already; it guards against window-induced drift.
    let mono_dc: f64 = h_direct.iter().sum::<f64>() + h_cross.iter().sum::<f64>();
    if mono_dc.abs() > 1e-9 {
        let inv = 1.0 / mono_dc;
        for v in h_direct.iter_mut() {
            *v *= inv;
        }
        for v in h_cross.iter_mut() {
            *v *= inv;
        }
    }

    crate::aelog!(
        "[XTC] Filter: {} taps each, bulk delay {} samples ({:.2} ms), mono DC pre-norm {:.4}",
        tap_len, tap_half, tap_half as f64 / fs * 1000.0, mono_dc
    );

    Ok((h_direct, h_cross))
}

/// Apply the 2×2 XTC filter to a stereo buffer in place via FFT overlap-add.
///
/// `out_l`/`out_r` hold the input on entry and the XTC output on return. The output is
/// truncated back to the original sample count and the filters' common bulk group delay
/// `(M-1)/2` is removed, so the result is the SAME length and time-aligned with the input.
///
/// SAFETY/correctness: the inputs are cloned before any output is written, so the
/// cross-terms (outL needs inR, outR needs inL) are computed from un-clobbered data even
/// though out_l/out_r alias the inputs.
pub fn apply_xtc(out_l: &mut Vec<f64>, out_r: &mut Vec<f64>, h_direct: &[f64], h_cross: &[f64]) {
    let original_len = out_l.len();
    assert_eq!(original_len, out_r.len(), "XTC: channel length mismatch");
    let m = h_direct.len();
    assert_eq!(m, h_cross.len(), "XTC: filter length mismatch");
    if original_len == 0 || m == 0 {
        return;
    }
    // Trivial identity (bypass) filter: nothing to do.
    if m == 1 && (h_direct[0] - 1.0).abs() < 1e-12 && h_cross[0].abs() < 1e-12 {
        return;
    }

    let in_l = out_l.clone();
    let in_r = out_r.clone();

    let block = 8192usize;
    let fft_n = (block + m - 1).next_power_of_two();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(fft_n);
    let ifft = planner.plan_fft_inverse(fft_n);

    let to_spectrum = |h: &[f64]| -> Vec<Complex<f64>> {
        let mut v = vec![Complex::new(0.0, 0.0); fft_n];
        for (i, &x) in h.iter().enumerate() {
            v[i] = Complex::new(x, 0.0);
        }
        fft.process(&mut v);
        v
    };
    let hd_spec = to_spectrum(h_direct);
    let hc_spec = to_spectrum(h_cross);
    let norm = 1.0 / fft_n as f64;

    let conv_len = original_len + m - 1;
    let mut acc_l = vec![0.0f64; conv_len];
    let mut acc_r = vec![0.0f64; conv_len];

    let mut xl = vec![Complex::new(0.0, 0.0); fft_n];
    let mut xr = vec![Complex::new(0.0, 0.0); fft_n];
    let mut yl = vec![Complex::new(0.0, 0.0); fft_n];
    let mut yr = vec![Complex::new(0.0, 0.0); fft_n];

    let mut start = 0;
    while start < original_len {
        let end = (start + block).min(original_len);
        let blen = end - start;

        for c in xl.iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        for c in xr.iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        for i in 0..blen {
            xl[i] = Complex::new(in_l[start + i], 0.0);
            xr[i] = Complex::new(in_r[start + i], 0.0);
        }
        // The two channels transform independently, and four transforms per
        // block is the whole cost of this stage — so they go two at a time.
        //
        // What is NOT parallelised is the block loop itself, and that is
        // deliberate. The filter runs to forty milliseconds, which at the
        // output rates this stage sees is several times the 8192-sample
        // block, so one output sample collects contributions from three or
        // four blocks. Adding those in a different order would change the
        // last bit of every sample in the overlap. Splitting the transforms
        // instead leaves the accumulation exactly as it was, in order, one
        // block at a time — same file, half the wall clock on two cores.
        rayon::join(|| fft.process(&mut xl), || fft.process(&mut xr));

        // outL = H_direct·inL + H_cross·inR ; outR = H_direct·inR + H_cross·inL
        for i in 0..fft_n {
            yl[i] = xl[i] * hd_spec[i] + xr[i] * hc_spec[i];
            yr[i] = xr[i] * hd_spec[i] + xl[i] * hc_spec[i];
        }
        rayon::join(|| ifft.process(&mut yl), || ifft.process(&mut yr));

        let seg = blen + m - 1;
        for i in 0..seg {
            let idx = start + i;
            if idx < conv_len {
                acc_l[idx] += yl[i].re * norm;
                acc_r[idx] += yr[i].re * norm;
            }
        }
        start = end;
    }

    // Remove the common bulk group delay (filters centered at (M-1)/2) and restore length.
    let bulk = (m - 1) / 2;
    let take = |acc: &[f64]| -> Vec<f64> {
        let mut v = Vec::with_capacity(original_len);
        for i in 0..original_len {
            v.push(acc.get(bulk + i).copied().unwrap_or(0.0));
        }
        v
    };
    *out_l = take(&acc_l);
    *out_r = take(&acc_r);

    let peak = out_l
        .iter()
        .chain(out_r.iter())
        .fold(0.0f64, |mx, &v| mx.max(v.abs()));
    if peak > 2.0 {
        crate::aelog!(
            "[XTC] WARNING: post-XTC peak {:.2} dBFS ({:.3} linear) — true-peak normalizer \
             will reduce output level. Increase Headroom.",
            20.0 * peak.max(1e-12).log10(),
            peak
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════════════
// Lab entry point: run the thing, and report numbers someone can argue with.
//
// The owner listens and says what he hears; this has to say what it did, in terms that
// can be checked against that. So the report is measured from the FIR that was actually
// built, and never only against the one head it was built for: beside that it gives the
// same head with no filter at all, and with the head 20 mm off the centre line, which is
// the seat a listener actually has.
// ══════════════════════════════════════════════════════════════════════════════════════

use crate::audio::converter::types::XtcGeometry as UiGeometry;

/// The frequencies the report speaks about. Spread over the active band, at values a
/// listener can place: the bottom edge, the vocal range, presence, the top edge.
const PROBE_HZ: [f64; 9] = [200.0, 300.0, 500.0, 800.0, 1000.0, 1500.0, 2000.0, 3000.0, 4000.0];

/// How far the report moves the head to see what is left, mm.
const OFF_CENTRE_MM: f64 = 20.0;

/// One probe: the wrong ear against the right one, dB. Negative is separation.
#[derive(Clone, Copy, Debug)]
pub struct XtcProbe {
    pub hz: f64,
    /// The same speakers and head with no filter.
    pub none_db: f64,
    /// With the filter, the head where it was designed for.
    pub centred_db: f64,
    /// With the filter, the head `OFF_CENTRE_MM` to one side.
    pub off_db: f64,
}

pub struct XtcReport {
    pub span_angle_deg: f64,
    pub angle_verdict: &'static str,
    pub mean_distance_mm: f64,
    /// Perpendicular depth the filter was actually designed for.
    pub depth_mm: f64,
    pub asymmetry_mm: f64,
    pub span_mm: f64,
    pub head_mm: f64,
    pub depth: Vec<XtcProbe>,
    /// Over the probes, with the filter: the worst, and the mean, head centred.
    pub worst_depth_db: f64,
    pub mean_depth_db: f64,
    /// The mean with the head off centre, and the mean with no filter.
    pub mean_off_db: f64,
    pub mean_none_db: f64,
    pub taps: usize,
    pub peak_change_db: f64,
    pub rms_change_db: f64,
    pub side_boost_db: f64,
}

/// H(f) of a real FIR, with its bulk delay divided out so the phase read here is the phase
/// the signal actually gets — `apply_xtc` removes that delay too.
fn fir_response(h: &[f64], f: f64, fs: f64) -> Complex<f64> {
    let w = 2.0 * PI * f / fs;
    let bulk = (h.len() - 1) as f64 / 2.0;
    let mut acc = Complex::new(0.0, 0.0);
    for (n, &c) in h.iter().enumerate() {
        acc += Complex::from_polar(c, -w * n as f64);
    }
    acc * Complex::from_polar(1.0, w * bulk)
}

/// What the wrong ear hears, relative to what the right ear hears, in dB.
///
/// The whole point of the feature reduced to one number per frequency. The ear signals are
/// C·H: the diagonal is the channel that should arrive, the off-diagonal is the leak this
/// filter exists to remove. Negative is cancellation; POSITIVE means the filter is feeding
/// the wrong ear harder than the right one, which is the failure the prototype shipped with.
#[cfg(test)]
fn cancellation_db(
    h_direct: &[f64],
    h_cross: &[f64],
    span_mm: f64,
    dist_mm: f64,
    head_mm: f64,
    f: f64,
    fs: f64,
) -> f64 {
    let (a, b) = acoustic_ab(span_mm, dist_mm, head_mm, f);
    let hd = fir_response(h_direct, f, fs);
    let hc = fir_response(h_cross, f, fs);
    let diag = a * hd + b * hc;
    let off = a * hc + b * hd;
    20.0 * (off.norm() / diag.norm().max(1e-15)).log10()
}

/// The same with the head moved, from the filter's responses at `f`. Each channel sees
/// the move differently, so this is the two of them on average.
fn off_centre_db(hd: Complex<f64>, hc: Complex<f64>, span_mm: f64, dist_mm: f64, head_mm: f64, dx_mm: f64, f: f64) -> f64 {
    let [ll, lr, rl, rr] = paths(span_mm, dist_mm, head_mm, dx_mm, f);
    // Left input only, then right input only.
    let from_left = (lr * hd + rr * hc).norm() / (ll * hd + rl * hc).norm().max(1e-15);
    let from_right = (ll * hc + rl * hd).norm() / (lr * hc + rr * hd).norm().max(1e-15);
    10.0 * (from_left * from_right).max(1e-30).log10()
}

/// Descriptive, not a grade: what the filter does at this angle is in the report's numbers.
fn verdict_for_angle(deg: f64) -> &'static str {
    if deg < 20.0 {
        "narrow, well back from the speakers"
    } else if deg <= 70.0 {
        "a conventional listening triangle"
    } else if deg <= 120.0 {
        "wide, sitting close in"
    } else {
        "very wide, nearly level with the speakers"
    }
}

fn peak_rms(l: &[f64], r: &[f64]) -> (f64, f64) {
    let mut peak = 0.0f64;
    let mut sq = 0.0f64;
    for (&a, &b) in l.iter().zip(r.iter()) {
        peak = peak.max(a.abs()).max(b.abs());
        sq += a * a + b * b;
    }
    let n = (l.len() * 2).max(1) as f64;
    (peak, (sq / n).sqrt())
}

/// Design and apply. `Err` when the geometry cannot be filtered for — never a filter built
/// from filled-in blanks.
pub fn run(
    out_l: &mut Vec<f64>,
    out_r: &mut Vec<f64>,
    out_rate: u32,
    ui: &UiGeometry,
) -> Result<XtcReport, String> {
    let plan = plan(out_rate, ui)?;
    let (p0, r0) = peak_rms(out_l, out_r);
    apply_xtc(out_l, out_r, &plan.h_direct, &plan.h_cross);
    let (p1, r1) = peak_rms(out_l, out_r);
    Ok(plan.report((p0, r0), (p1, r1)))
}

/// Everything about one XTC run that does not depend on the audio: the two
/// filters, built for the measured triangle, and the measurements of what
/// they do. `run` applies it to a buffer in memory; the segmented route for
/// very long files streams it through `XtcStream` instead, with the same
/// arithmetic.
pub struct XtcPlan {
    pub h_direct: Vec<f64>,
    pub h_cross: Vec<f64>,
    span_angle_deg: f64,
    angle_verdict: &'static str,
    mean_distance_mm: f64,
    depth_mm: f64,
    asymmetry_mm: f64,
    span_mm: f64,
    head_mm: f64,
    depth: Vec<XtcProbe>,
    worst_depth_db: f64,
    mean_depth_db: f64,
    mean_off_db: f64,
    mean_none_db: f64,
    side_boost_db: f64,
}

impl XtcPlan {
    /// The report, given the (peak, RMS) of the audio before and after.
    pub fn report(&self, before: (f64, f64), after: (f64, f64)) -> XtcReport {
        let ((p0, r0), (p1, r1)) = (before, after);
        XtcReport {
            span_angle_deg: self.span_angle_deg,
            angle_verdict: self.angle_verdict,
            mean_distance_mm: self.mean_distance_mm,
            depth_mm: self.depth_mm,
            asymmetry_mm: self.asymmetry_mm,
            span_mm: self.span_mm,
            head_mm: self.head_mm,
            depth: self.depth.clone(),
            worst_depth_db: self.worst_depth_db,
            mean_depth_db: self.mean_depth_db,
            mean_off_db: self.mean_off_db,
            mean_none_db: self.mean_none_db,
            taps: self.h_direct.len(),
            peak_change_db: 20.0 * (p1.max(1e-12) / p0.max(1e-12)).log10(),
            rms_change_db: 20.0 * (r1.max(1e-12) / r0.max(1e-12)).log10(),
            side_boost_db: self.side_boost_db,
        }
    }

    /// A streaming twin of `apply_xtc` over these filters.
    pub fn stream(&self) -> XtcStream {
        XtcStream::new(&self.h_direct, &self.h_cross)
    }
}

/// Design the filters for a measured triangle. `Err` when the geometry cannot
/// be filtered for — never a filter built from filled-in blanks.
pub fn plan(out_rate: u32, ui: &UiGeometry) -> Result<XtcPlan, String> {
    if !ui.is_usable() {
        return Err("listening geometry is not a triangle".to_string());
    }
    let span = ui.speaker_span_mm;
    // The model wants the PERPENDICULAR depth to the ear plane, not the direct
    // speaker-to-head distance the dialog collects. See XtcGeometry::depth_mm.
    let dist = ui.depth_mm();
    let head = ui.head_width_mm;

    // The filter is symmetric; it can only be built for one distance. Say so rather than
    // averaging in silence — this is how far the room is from the one being modelled.
    let asym = ui.asymmetry_mm();
    if asym > ASYM_SOFT_MM {
        crate::aelog!(
            "[XTC] chair is {:.0}mm off the centre line ({:.0}us) - past a quarter wave at 2kHz, so the filter is built for the mean and the top of the band will not cancel cleanly",
            asym,
            asym / C_MM_S * 1e6
        );
    }

    let geo = XtcGeometry {
        speaker_spacing_mm: span,
        listener_distance_mm: dist,
        head_width_mm: head,
        strength: 1.0,
    };
    let (h_direct, h_cross) = design_xtc_filters(geo, out_rate)?;

    let fs = out_rate as f64;
    let depth: Vec<XtcProbe> = PROBE_HZ
        .iter()
        .map(|&f| {
            let hd = fir_response(&h_direct, f, fs);
            let hc = fir_response(&h_cross, f, fs);
            let (a, b) = acoustic_ab(span, dist, head, f);
            XtcProbe {
                hz: f,
                none_db: 20.0 * (b.norm() / a.norm()).log10(),
                centred_db: 20.0 * ((a * hc + b * hd).norm() / (a * hd + b * hc).norm().max(1e-15)).log10(),
                off_db: off_centre_db(hd, hc, span, dist, head, OFF_CENTRE_MM, f),
            }
        })
        .collect();
    let worst = depth.iter().map(|p| p.centred_db).fold(f64::NEG_INFINITY, f64::max);
    let mean_of = |of: fn(&XtcProbe) -> f64| depth.iter().map(of).sum::<f64>() / depth.len() as f64;
    let mean = mean_of(|p| p.centred_db);
    let mean_off = mean_of(|p| p.off_db);
    let mean_none = mean_of(|p| p.none_db);

    let side_boost_db = {
        let mut m = 0.0f64;
        let mut f = F_LO;
        while f < F_HI {
            let hd = fir_response(&h_direct, f, fs);
            let hc = fir_response(&h_cross, f, fs);
            m = m.max((hd - hc).norm());
            f *= 1.02;
        }
        20.0 * m.max(1e-12).log10()
    };

    Ok(XtcPlan {
        h_direct,
        h_cross,
        span_angle_deg: ui.span_angle_deg(),
        angle_verdict: verdict_for_angle(ui.span_angle_deg()),
        mean_distance_mm: ui.mean_distance_mm(),
        depth_mm: dist,
        asymmetry_mm: asym,
        span_mm: span,
        head_mm: head,
        depth,
        worst_depth_db: worst,
        mean_depth_db: mean,
        mean_off_db: mean_off,
        mean_none_db: mean_none,
        side_boost_db,
    })
}

/// `peak_rms` a chunk at a time: the same fold, in the same order, so the
/// numbers match the whole-buffer version to the last bit.
#[derive(Default)]
pub struct PeakRms {
    peak: f64,
    sq: f64,
    frames: usize,
}

impl PeakRms {
    pub fn push(&mut self, l: &[f64], r: &[f64]) {
        for (&a, &b) in l.iter().zip(r.iter()) {
            self.peak = self.peak.max(a.abs()).max(b.abs());
            self.sq += a * a + b * b;
        }
        self.frames += l.len().min(r.len());
    }

    pub fn finish(&self) -> (f64, f64) {
        let n = (self.frames * 2).max(1) as f64;
        (self.peak, (self.sq / n).sqrt())
    }
}

/// `apply_xtc`, fed a chunk at a time, for a file that never sits in memory
/// whole. The blocks start where `apply_xtc`'s do — every 8192 samples from
/// the first — whatever sizes the input arrives in, and each accumulator slot
/// takes its contributions block by block in the same order, so the output
/// is the same to the last bit (held by `stream_matches_apply_xtc`). Output
/// comes out `(M-1)/2` samples behind the input, which is the bulk delay
/// `apply_xtc` trims; `finish` flushes the rest.
pub struct XtcStream {
    m: usize,
    bulk: usize,
    identity: bool,
    fft: std::sync::Arc<dyn rustfft::Fft<f64>>,
    ifft: std::sync::Arc<dyn rustfft::Fft<f64>>,
    fft_n: usize,
    hd_spec: Vec<Complex<f64>>,
    hc_spec: Vec<Complex<f64>>,
    in_l: Vec<f64>,
    in_r: Vec<f64>,
    /// Accumulator for positions [acc_base, acc_base + acc_l.len()).
    acc_l: Vec<f64>,
    acc_r: Vec<f64>,
    acc_base: usize,
    /// Input consumed into blocks so far = start of the next block.
    next_start: usize,
    received: usize,
    emitted: usize,
    before: PeakRms,
    after: PeakRms,
}

const XTC_BLOCK: usize = 8192;

/// What a head off centre has to gain over no filter for the report to call it working, dB.
const LEVEL_OK_GAIN_DB: f64 = 2.0;

impl XtcStream {
    fn new(h_direct: &[f64], h_cross: &[f64]) -> Self {
        let m = h_direct.len();
        assert_eq!(m, h_cross.len(), "XTC: filter length mismatch");
        // The two cases `apply_xtc` returns from untouched.
        let identity = m == 0
            || (m == 1 && (h_direct[0] - 1.0).abs() < 1e-12 && h_cross[0].abs() < 1e-12);
        let fft_n = (XTC_BLOCK + m.max(1) - 1).next_power_of_two();
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(fft_n);
        let ifft = planner.plan_fft_inverse(fft_n);
        let to_spectrum = |h: &[f64]| -> Vec<Complex<f64>> {
            let mut v = vec![Complex::new(0.0, 0.0); fft_n];
            for (i, &x) in h.iter().enumerate() {
                v[i] = Complex::new(x, 0.0);
            }
            fft.process(&mut v);
            v
        };
        let hd_spec = to_spectrum(h_direct);
        let hc_spec = to_spectrum(h_cross);
        Self {
            m,
            bulk: m.saturating_sub(1) / 2,
            identity,
            fft,
            ifft,
            fft_n,
            hd_spec,
            hc_spec,
            in_l: Vec::with_capacity(XTC_BLOCK),
            in_r: Vec::with_capacity(XTC_BLOCK),
            acc_l: Vec::new(),
            acc_r: Vec::new(),
            acc_base: 0,
            next_start: 0,
            received: 0,
            emitted: 0,
            before: PeakRms::default(),
            after: PeakRms::default(),
        }
    }

    pub fn push(
        &mut self,
        l: &[f64],
        r: &[f64],
        emit: &mut dyn FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<(), String> {
        let n = l.len().min(r.len());
        self.before.push(&l[..n], &r[..n]);
        self.received += n;
        if self.identity {
            self.after.push(&l[..n], &r[..n]);
            self.emitted += n;
            return emit(&l[..n], &r[..n]);
        }
        let mut k = 0;
        while k < n {
            let take = (XTC_BLOCK - self.in_l.len()).min(n - k);
            self.in_l.extend_from_slice(&l[k..k + take]);
            self.in_r.extend_from_slice(&r[k..k + take]);
            k += take;
            if self.in_l.len() == XTC_BLOCK {
                self.process_block();
                self.emit_upto(self.next_start, emit)?;
            }
        }
        Ok(())
    }

    /// Flush the last partial block and every output still owed. Returns the
    /// (peak, RMS) before and after, for `XtcPlan::report`.
    pub fn finish(
        mut self,
        emit: &mut dyn FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<((f64, f64), (f64, f64)), String> {
        if !self.identity {
            if !self.in_l.is_empty() {
                self.process_block();
            }
            // Nothing else will be added: every position is final now.
            let end = self.received + self.m - 1;
            self.emit_upto(end, emit)?;
        }
        let (p1, _) = self.after.finish();
        if !self.identity && p1 > 2.0 {
            crate::aelog!(
                "[XTC] WARNING: post-XTC peak {:.2} dBFS ({:.3} linear) — true-peak normalizer \
                 will reduce output level. Increase Headroom.",
                20.0 * p1.max(1e-12).log10(),
                p1
            );
        }
        Ok((self.before.finish(), self.after.finish()))
    }

    /// One block of `apply_xtc`'s loop, on the samples buffered in `in_l/in_r`.
    fn process_block(&mut self) {
        let fft_n = self.fft_n;
        let blen = self.in_l.len();
        let start = self.next_start;
        let mut xl = vec![Complex::new(0.0, 0.0); fft_n];
        let mut xr = vec![Complex::new(0.0, 0.0); fft_n];
        for i in 0..blen {
            xl[i] = Complex::new(self.in_l[i], 0.0);
            xr[i] = Complex::new(self.in_r[i], 0.0);
        }
        let (fft, ifft) = (&self.fft, &self.ifft);
        rayon::join(|| fft.process(&mut xl), || fft.process(&mut xr));
        let mut yl = vec![Complex::new(0.0, 0.0); fft_n];
        let mut yr = vec![Complex::new(0.0, 0.0); fft_n];
        for i in 0..fft_n {
            yl[i] = xl[i] * self.hd_spec[i] + xr[i] * self.hc_spec[i];
            yr[i] = xr[i] * self.hd_spec[i] + xl[i] * self.hc_spec[i];
        }
        rayon::join(|| ifft.process(&mut yl), || ifft.process(&mut yr));
        let norm = 1.0 / fft_n as f64;
        let seg = blen + self.m - 1;
        let need = start + seg - self.acc_base;
        if self.acc_l.len() < need {
            self.acc_l.resize(need, 0.0);
            self.acc_r.resize(need, 0.0);
        }
        let at = start - self.acc_base;
        for i in 0..seg {
            self.acc_l[at + i] += yl[i].re * norm;
            self.acc_r[at + i] += yr[i].re * norm;
        }
        self.next_start += blen;
        self.in_l.clear();
        self.in_r.clear();
    }

    /// Emit every output whose accumulator slot is below `final_pos` — no later
    /// block can add to those — then drop the slots nothing will read again.
    fn emit_upto(
        &mut self,
        final_pos: usize,
        emit: &mut dyn FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<(), String> {
        let last = final_pos.saturating_sub(self.bulk).min(self.received);
        if last > self.emitted {
            let from = self.bulk + self.emitted - self.acc_base;
            let to = self.bulk + last - self.acc_base;
            let (ol, or) = (&self.acc_l[from..to], &self.acc_r[from..to]);
            self.after.push(ol, or);
            emit(ol, or)?;
            self.emitted = last;
        }
        let drop_to = (self.bulk + self.emitted).min(self.next_start);
        if drop_to > self.acc_base {
            let d = (drop_to - self.acc_base).min(self.acc_l.len());
            self.acc_l.drain(..d);
            self.acc_r.drain(..d);
            self.acc_base += d;
        }
        Ok(())
    }
}

impl XtcReport {
    /// Amber when a head a little off centre is not better off with the filter than without
    /// it: that is the seat there is, and at this geometry the filter is then not earning
    /// its colouration. Green only when it is.
    pub fn level(&self) -> &'static str {
        if self.mean_off_db > self.mean_none_db - LEVEL_OK_GAIN_DB {
            "warn"
        } else {
            "ok"
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{:.0} deg span ({}), wrong ear {:.1} dB with the head centred, {:.1} dB {:.0} mm off, {:.1} dB without XTC; peak {:+.2} dB",
            self.span_angle_deg,
            self.angle_verdict,
            self.mean_depth_db,
            self.mean_off_db,
            OFF_CENTRE_MM,
            self.mean_none_db,
            self.peak_change_db
        )
    }

    pub fn tag_value(&self) -> String {
        format!(
            "span={:.0}mm;dist={:.0}mm;head={:.0}mm;angle={:.1}deg;mean={:.1}dB;worst={:.1}dB;off={:.1}dB;none={:.1}dB",
            self.span_mm,
            self.mean_distance_mm,
            self.head_mm,
            self.span_angle_deg,
            self.mean_depth_db,
            self.worst_depth_db,
            self.mean_off_db,
            self.mean_none_db
        )
    }

    /// The block the owner reads after a listen. Everything in it is measured.
    pub fn log(&self) {
        crate::aelog!(
            "[XTC] geometry: span {:.0}mm, {:.0}mm to each speaker (= {:.0}mm deep), head {:.0}mm, asymmetry {:.0}mm",
            self.span_mm,
            self.mean_distance_mm,
            self.depth_mm,
            self.head_mm,
            self.asymmetry_mm
        );
        crate::aelog!(
            "[XTC] span angle {:.1} deg - {}",
            self.span_angle_deg,
            self.angle_verdict
        );
        crate::aelog!(
            "[XTC] beta {:.2} at {}Hz falling to {:.2} at {}Hz; side gain peak {:.2}dB (bounds {:.2}, {:.2}dB)",
            BETA_LF,
            BETA_LO_HZ as u32,
            BETA_HF,
            BETA_HI_HZ as u32,
            self.side_boost_db,
            20.0 * SIDE_GAIN_FLOOR.log10(),
            20.0 * SIDE_GAIN_CAP.log10()
        );
        crate::aelog!("[XTC] wrong ear vs right ear (negative is good) on a {:.0}mm sphere:", 2.0 * head_radius_mm(self.head_mm));
        crate::aelog!("[XTC]      Hz   no XTC  centred  {:.0}mm off", OFF_CENTRE_MM);
        for p in &self.depth {
            crate::aelog!(
                "[XTC]   {:>5.0}  {:>7.1}  {:>7.1}  {:>7.1}{}",
                p.hz,
                p.none_db,
                p.centred_db,
                p.off_db,
                if p.centred_db > 0.0 { "   ** AMPLIFYING **" } else { "" }
            );
        }
        crate::aelog!(
            "[XTC] mean {:.1}dB centred, {:.1}dB off centre, {:.1}dB no XTC; worst {:.1}dB over {} probes",
            self.mean_depth_db,
            self.mean_off_db,
            self.mean_none_db,
            self.worst_depth_db,
            self.depth.len()
        );
        crate::aelog!(
            "[XTC] filter {} taps, output time-aligned; level: peak {:+.2}dB, RMS {:+.2}dB",
            self.taps,
            self.peak_change_db,
            self.rms_change_db
        );
        crate::aelog!(
            "[XTC] that peak change is paid for by the output ceiling - level-match before any A/B"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_geo() -> XtcGeometry {
        XtcGeometry {
            speaker_spacing_mm: 2000.0,
            listener_distance_mm: 3000.0,
            head_width_mm: 180.0,
            strength: 1.0,
        }
    }

    /// Deterministic programme-like noise, so the test needs no RNG crate.
    fn noise(n: usize, seed: u64, amp: f64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                amp * ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0)
            })
            .collect()
    }

    /// The segmented route streams XTC instead of holding the file: the
    /// stream has to give `apply_xtc`'s output to the last bit, whatever size
    /// the chunks arrive in — including a length that is not a whole number
    /// of blocks, chunks smaller than a block and chunks spanning several.
    #[test]
    fn stream_matches_apply_xtc() {
        // A designed pair, and a synthetic one longer than two blocks, so that
        // one output sample collects three blocks — where the order of the
        // additions decides the last bit.
        let (hd, hc) = design_xtc_filters(default_geo(), 384_000).unwrap();
        let long_d = noise(20_001, 0x1234_5678_9abc_def1, 0.01);
        let long_c = noise(20_001, 0x0fed_cba9_8765_4321, 0.01);
        for (hd, hc) in [(&hd, &hc), (&long_d, &long_c)] {
            let n = 5 * XTC_BLOCK + 3_001;
            let l = noise(n, 0x9e37_79b9_7f4a_7c15, 0.7);
            let r = noise(n, 0xd1b5_4a32_d192_ed03, 0.7);

            let (mut bl, mut br) = (l.clone(), r.clone());
            apply_xtc(&mut bl, &mut br, hd, hc);

            for chunks in [&[1_000usize, 20_000, 7, 8_192][..], &[n][..], &[4_096][..]] {
                let (mut sl, mut sr) = (Vec::new(), Vec::new());
                let mut stream = XtcStream::new(hd, hc);
                let mut emit = |a: &[f64], b: &[f64]| -> Result<(), String> {
                    sl.extend_from_slice(a);
                    sr.extend_from_slice(b);
                    Ok(())
                };
                let (mut at, mut k) = (0usize, 0usize);
                while at < n {
                    let c = chunks[k % chunks.len()].min(n - at);
                    stream.push(&l[at..at + c], &r[at..at + c], &mut emit).unwrap();
                    at += c;
                    k += 1;
                }
                let (before, after) = stream.finish(&mut emit).unwrap();
                assert_eq!(sl.len(), n, "chunks {:?}", chunks);
                assert!(sl == bl && sr == br, "stream differs from apply_xtc, chunks {:?}", chunks);
                assert_eq!(before, peak_rms(&l, &r));
                assert_eq!(after, peak_rms(&bl, &br));
            }
        }
    }

    #[test]
    fn designs_equal_length_filters() {
        let (hd, hc) = design_xtc_filters(default_geo(), 384_000).unwrap();
        assert_eq!(hd.len(), hc.len());
        assert!(hd.len() >= 3 && hd.len() % 2 == 1);
    }

    #[test]
    fn degenerate_geometry_errors() {
        let g = XtcGeometry {
            speaker_spacing_mm: 150.0,
            listener_distance_mm: 3000.0,
            head_width_mm: 200.0,
            strength: 1.0,
        };
        assert!(design_xtc_filters(g, 384_000).is_err());
    }

    #[test]
    fn zero_strength_is_bypass() {
        let mut g = default_geo();
        g.strength = 0.0;
        let (hd, hc) = design_xtc_filters(g, 384_000).unwrap();
        assert_eq!(hd, vec![1.0]);
        assert_eq!(hc, vec![0.0]);
    }

    #[test]
    fn apply_preserves_length() {
        let (hd, hc) = design_xtc_filters(default_geo(), 192_000).unwrap();
        let n = 200_000;
        let mut l: Vec<f64> = (0..n).map(|i| 0.2 * (i as f64 * 0.01).sin()).collect();
        let mut r: Vec<f64> = (0..n).map(|i| 0.2 * (i as f64 * 0.013).cos()).collect();
        apply_xtc(&mut l, &mut r, &hd, &hc);
        assert_eq!(l.len(), n);
        assert_eq!(r.len(), n);
        assert!(l.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn identity_filter_is_transparent() {
        let n = 50_000;
        let l0: Vec<f64> = (0..n).map(|i| 0.3 * (i as f64 * 0.02).sin()).collect();
        let r0: Vec<f64> = (0..n).map(|i| 0.3 * (i as f64 * 0.017).sin()).collect();
        let mut l = l0.clone();
        let mut r = r0.clone();
        apply_xtc(&mut l, &mut r, &[1.0], &[0.0]);
        for i in 0..n {
            assert!((l[i] - l0[i]).abs() < 1e-12);
            assert!((r[i] - r0[i]).abs() < 1e-12);
        }
    }

    // Evaluate the FIR frequency response H(f) = Σ h[n]·e^{-jωn} at one frequency.
    fn freq_response(h: &[f64], f: f64, fs: f64) -> Complex<f64> {
        let w = 2.0 * PI * f / fs;
        let mut acc = Complex::new(0.0, 0.0);
        for (n, &c) in h.iter().enumerate() {
            acc += Complex::from_polar(c, -w * n as f64);
        }
        acc
    }

    // Mono (center) gain = |H_dd + H_dc|; side (width) gain = |H_dd - H_dc|.
    fn mono_gain(hd: &[f64], hc: &[f64], f: f64, fs: f64) -> f64 {
        (freq_response(hd, f, fs) + freq_response(hc, f, fs)).norm()
    }
    fn side_gain(hd: &[f64], hc: &[f64], f: f64, fs: f64) -> f64 {
        (freq_response(hd, f, fs) - freq_response(hc, f, fs)).norm()
    }

    // ══════════════════════════════════════════════════════════════════════════════════
    // The test that decides whether this feature works.
    //
    // The three tests this replaces asserted |H_dd + H_dc| = 1 and |H_dd − H_dc| > 1.
    // Both hold by construction for ANY pair of the form g_p = 1, g_m = anything: the
    // first is |g_p|, the second is |g_m|. They were satisfied exactly as well by the
    // broken filter that amplified crosstalk by 37 dB as by a correct one, which is why
    // the defect sat in the file with a green suite on top of it.
    //
    // A crosstalk canceller can only be graded at the EARS. That means multiplying the
    // filter matrix by the acoustic matrix and looking at what lands in the wrong one.
    // ══════════════════════════════════════════════════════════════════════════════════

    /// A spread of real rooms, not just the default: a normal triangle, a narrow pair, a
    /// wide pair, a nearfield desk, and a small head.
    const ROOMS: [(f64, f64, f64); 5] = [
        (2000.0, 3000.0, 180.0),
        (1000.0, 2500.0, 180.0),
        (2400.0, 2600.0, 180.0),
        (600.0, 700.0, 150.0),
        (1800.0, 2200.0, 140.0),
    ];

    #[test]
    fn roundtrip_never_amplifies_crosstalk() {
        // The one property that must hold everywhere: the wrong ear may not be fed harder
        // than the right one. This is the assertion the old formula fails — measured at 64
        // of 155 probe frequencies between 400 Hz and 6.95 kHz, worst case +37.3 dB.
        let fs = 384_000.0;
        for (span, dist, head) in ROOMS {
            let geo = XtcGeometry {
                speaker_spacing_mm: span,
                listener_distance_mm: dist,
                head_width_mm: head,
                strength: 1.0,
            };
            let (hd, hc) = design_xtc_filters(geo, fs as u32).unwrap();
            let mut f = F_LO;
            while f <= F_HI {
                let d = cancellation_db(&hd, &hc, span, dist, head, f, fs);
                assert!(
                    d < 0.1,
                    "XTC amplifies crosstalk at {:.0} Hz for {:?}: {:+.1} dB",
                    f,
                    (span, dist, head),
                    d
                );
                f *= 1.03;
            }
        }
    }

    #[test]
    fn roundtrip_actually_cancels() {
        // Not merely "does no harm": the filter has to remove a useful amount from the head
        // it was built for. The bars are set with margin below what the design measures, and
        // far below what the same head does with no filter, −2 to −11 dB at these.
        let fs = 384_000.0;
        let (span, dist, head) = ROOMS[0];
        let geo = XtcGeometry {
            speaker_spacing_mm: span,
            listener_distance_mm: dist,
            head_width_mm: head,
            strength: 1.0,
        };
        let (hd, hc) = design_xtc_filters(geo, fs as u32).unwrap();

        for (f, limit) in [
            (300.0, -8.0),
            (500.0, -10.0),
            (800.0, -10.0),
            (1000.0, -10.0),
            (2000.0, -10.0),
            (3000.0, -10.0),
        ] {
            let d = cancellation_db(&hd, &hc, span, dist, head, f, fs);
            assert!(
                d < limit,
                "too little cancellation at {:.0} Hz: {:.1} dB, wanted below {:.1}",
                f,
                d,
                limit
            );
        }

        // A ratio says nothing on its own: a filter that silenced BOTH ears would satisfy
        // every assertion above. So the diagonal — the channel that is supposed to arrive —
        // has to still be there. Without this, "cancellation" and "deafness" score alike.
        for f in [500.0, 1000.0, 2000.0, 4000.0, 6000.0] {
            let (a, b) = acoustic_ab(span, dist, head, f);
            let diag = (a * fir_response(&hd, f, fs) + b * fir_response(&hc, f, fs)).norm();
            assert!(
                diag > 0.20,
                "the wanted channel collapsed at {:.0} Hz: diagonal gain {:.3}",
                f,
                diag
            );
        }
    }

    /// B/A of MIT's KEMAR (Gardner & Martin 1995, normal pinna, measured at 1.4 m), between
    /// its ±45° and ±50° at ±46.8°: Hz, dB, phase in radians counted up from the bottom.
    const KEMAR_47: [(f64, f64, f64); 13] = [
        (200.0, -2.15, -0.832),
        (300.0, -3.10, -1.096),
        (400.0, -3.34, -1.402),
        (500.0, -3.40, -1.780),
        (650.0, -6.42, -2.109),
        (800.0, -5.46, -2.672),
        (1000.0, -7.54, -3.334),
        (1300.0, -13.33, -3.975),
        (1600.0, -11.88, -4.419),
        (2000.0, -9.75, -5.046),
        (2500.0, -10.38, -6.296),
        (3000.0, -9.73, -7.659),
        (4000.0, -15.04, -10.246),
    ];

    #[test]
    fn a_measured_head_is_better_off() {
        // The grade the model cannot give itself: a head that was measured, not the sphere
        // the filter was built for. The filter is built for a 1600 mm span at 1097 mm.
        use crate::audio::converter::types::XtcGeometry as Ui;
        let ui = Ui { speaker_span_mm: 1600.0, left_distance_mm: 1096.6, right_distance_mm: 1096.6, head_width_mm: 180.0 };
        let fs = 96_000.0;
        let plan = plan(fs as u32, &ui).unwrap();
        let (mut none, mut with) = (0.0, 0.0);
        for (f, db, ph) in KEMAR_47 {
            let b = Complex::from_polar(10f64.powf(db / 20.0), ph);
            let (hd, hc) = (fir_response(&plan.h_direct, f, fs), fir_response(&plan.h_cross, f, fs));
            let sep = 20.0 * ((hc + b * hd).norm() / (hd + b * hc).norm()).log10();
            assert!(sep < db + 0.5, "worse than no filter at {f} Hz: {sep:.1} against {db:.1} dB");
            // 200 Hz is on the way in; the band is full from 300.
            let bar = if f < F_LO_FULL { -5.0 } else { -8.0 };
            assert!(sep < bar, "too little at {f} Hz: {sep:.1} dB");
            none += db;
            with += sep;
        }
        let n = KEMAR_47.len() as f64;
        assert!(
            with / n < none / n - 7.0,
            "only {:.1} dB against {:.1} dB with no filter",
            with / n,
            none / n
        );
    }

    #[test]
    fn the_old_straight_line_model_fails_that_head() {
        // Proof the test above has teeth: the path lengths and first-order shadow this file
        // used to build from are off by 200 µs at the bottom, and it shows on the same data.
        let (span, dist, head) = (1600.0, (1096.6f64 * 1096.6 - 800.0 * 800.0).sqrt(), 180.0);
        let (s, e) = (span * 0.5, head * 0.5);
        let d_ip = ((s - e) * (s - e) + dist * dist).sqrt();
        let d_cc = ((s + e) * (s + e) + dist * dist).sqrt();
        let (mut none, mut with) = (0.0, 0.0);
        for (f, db, ph) in KEMAR_47 {
            let shadow = 1.0 / (1.0 + (f / 1200.0).powi(2)).sqrt();
            let model = Complex::from_polar(d_ip / d_cc * shadow, -2.0 * PI * f * (d_cc - d_ip) / C_MM_S);
            let (gp, gm) = (Complex::new(1.0, 0.0) + model, Complex::new(1.0, 0.0) - model);
            let rel = gp / gm;
            let g = Complex::from_polar(rel.norm().clamp(1.0, 2.0), rel.arg());
            let (hd, hc) = ((Complex::new(1.0, 0.0) + g) * 0.5, (Complex::new(1.0, 0.0) - g) * 0.5);
            let b = Complex::from_polar(10f64.powf(db / 20.0), ph);
            with += 20.0 * ((hc + b * hd).norm() / (hd + b * hc).norm()).log10();
            none += db;
        }
        let n = KEMAR_47.len() as f64;
        assert!(with / n > none / n - 7.0, "the old model passes: {:.1} against {:.1}", with / n, none / n);
    }

    #[test]
    fn the_sphere_is_a_sphere() {
        // →1 from far away at no frequency; a low tone's time difference is Kuhn's 3a/c·sin θ for a plane wave.
        assert!((sphere_ear(0.2, 500.0, 1.0, 0.09) - Complex::new(1.0, 0.0)).norm() < 1e-3);
        let (a, az) = (0.09, 30f64.to_radians());
        let r = |f: f64| {
            sphere_ear(f, 50.0, FRAC_PI_2_MINUS(az), a) / sphere_ear(f, 50.0, FRAC_PI_2_PLUS(az), a)
        };
        let itd = r(100.0).arg() / (2.0 * PI * 100.0);
        let kuhn = 3.0 * a / 343.0 * az.sin();
        assert!((itd.abs() - kuhn).abs() < 0.03 * kuhn, "itd {:.0} us, Kuhn {:.0} us", itd * 1e6, kuhn * 1e6);
        // The far ear is the later one and the quieter one at 3 kHz.
        let high = r(3000.0);
        assert!(high.norm() > 1.5, "near over far {:.2}", high.norm());
    }

    #[allow(non_snake_case)]
    fn FRAC_PI_2_MINUS(az: f64) -> f64 {
        PI / 2.0 - az
    }
    #[allow(non_snake_case)]
    fn FRAC_PI_2_PLUS(az: f64) -> f64 {
        PI / 2.0 + az
    }

    #[test]
    fn a_head_off_centre_is_still_better_off() {
        // The seat there is. A 20 mm move is enough to undo the top of the band, so the bar
        // is that it does no harm on average and keeps the bottom. Reported, so held here.
        use crate::audio::converter::types::XtcGeometry as Ui;
        let ui = Ui { speaker_span_mm: 1600.0, left_distance_mm: 1096.6, right_distance_mm: 1096.6, head_width_mm: 180.0 };
        let plan = plan(96_000, &ui).unwrap();
        assert!(plan.mean_off_db < plan.mean_none_db - 4.0, "{:.1} against {:.1}", plan.mean_off_db, plan.mean_none_db);
        let rep = plan.report((1.0, 1.0), (1.0, 1.0));
        assert_eq!(rep.level(), "ok", "{}", rep.summary());
        assert!(rep.tag_value().contains(";none="));
    }

    #[test]
    fn the_filter_does_the_same_at_every_rate() {
        // It was sized by its own decay, and came out 5.5 ms long at one rate and 11.7 ms at
        // another. Same length in time now: the same response, to within the bottom edge.
        let hz = [200.0, 300.0, 500.0, 1000.0, 2000.0, 4000.0];
        let at = |rate: u32| {
            let (hd, hc) = design_xtc_filters(default_geo(), rate).unwrap();
            assert_eq!((hd.len() - 1) / 2, (HALF_S * rate as f64) as usize);
            hz.map(|f| fir_response(&hd, f, rate as f64) - fir_response(&hc, f, rate as f64))
        };
        let (a, b) = (at(44_100), at(705_600));
        for (i, f) in hz.iter().enumerate() {
            let db = 20.0 * (a[i].norm() / b[i].norm()).log10();
            let deg = (a[i] / b[i]).arg().to_degrees();
            assert!(db.abs() < 0.1 && deg.abs() < 1.0, "{f} Hz differs by {db:.2} dB, {deg:.2} deg");
        }
    }

    #[test]
    fn the_side_cut_is_where_the_head_needs_it_and_bounded() {
        // A floor of 1 here forbade the cut and with it the cancelling around 1 kHz.
        let fs = 96_000.0;
        let (hd, hc) = design_xtc_filters(default_geo(), fs as u32).unwrap();
        let (mut low, mut mono) = (f64::INFINITY, 0.0f64);
        let mut f = F_LO;
        while f < F_HI {
            low = low.min(side_gain(&hd, &hc, f, fs));
            mono = mono.max((20.0 * mono_gain(&hd, &hc, f, fs).log10()).abs());
            f *= 1.02;
        }
        assert!(low < 0.8, "no cut anywhere: {low:.2}");
        assert!(low > SIDE_GAIN_FLOOR * 0.9, "past the floor: {low:.2}");
        assert!(mono < 0.05, "mono path moved by {mono:.3} dB");
    }

    #[test]
    fn absolute_side_phase_would_amplify() {
        // Proof that the test above has teeth: rebuild the filter pair the way the
        // prototype did — side branch carrying arg(G₋) instead of arg(G₋) − arg(G₊) — and
        // confirm it fails the property. If someone ever "simplifies" the phase back, the
        // test above starts failing for the reason demonstrated here.
        let (span, dist, head) = ROOMS[0];

        let mut worst = f64::NEG_INFINITY;
        let mut f = F_LO_FULL;
        while f <= F_HI_FADE {
            let (a, b) = acoustic_ab(span, dist, head, f);
            let (lam_p, lam_m) = (a + b, a - b);
            let b2 = beta_at(f) * beta_at(f);
            let gp = lam_p.conj() / (lam_p.norm_sqr() + b2);
            let gm = lam_m.conj() / (lam_m.norm_sqr() + b2);
            let bl = band_blend(f);
            let rel = (gm.norm() / gp.norm().max(1e-9)).clamp(SIDE_GAIN_FLOOR, SIDE_GAIN_CAP);
            let mag = 1.0 + (rel - 1.0) * bl;

            // the defect: absolute phase of the side inverse, not the difference. On a head
            // with a delay in A this was the file's first bug; A being the reference here,
            // the same wrong turn is a rotation by arg(G₊) too many or too few.
            let g_p = Complex::new(1.0, 0.0);
            let g_m = Complex::from_polar(mag, (gm.arg() - gp.arg() + PI * 0.5) * bl);
            let (h_direct, h_cross) = ((g_p + g_m) * 0.5, (g_p - g_m) * 0.5);
            let off = a * h_cross + b * h_direct;
            let diag = a * h_direct + b * h_cross;
            worst = worst.max(20.0 * (off.norm() / diag.norm().max(1e-15)).log10());
            f *= 1.03;
        }
        assert!(
            worst > -3.0,
            "a quarter turn off should all but undo the cancelling; measured {:+.1} dB",
            worst
        );
    }

    #[test]
    fn geometry_gate_refuses_blanks() {
        // The feature must never build a filter out of unfilled fields. Zeroes, a chair
        // inside the speaker baseline, and a head wider than the span are all refusals.
        use crate::audio::converter::types::XtcGeometry as Ui;
        let bad = [
            Ui { speaker_span_mm: 0.0, left_distance_mm: 0.0, right_distance_mm: 0.0, head_width_mm: 0.0 },
            Ui { speaker_span_mm: 2000.0, left_distance_mm: 0.0, right_distance_mm: 3000.0, head_width_mm: 180.0 },
            Ui { speaker_span_mm: 5000.0, left_distance_mm: 1000.0, right_distance_mm: 1000.0, head_width_mm: 180.0 },
            Ui { speaker_span_mm: 2000.0, left_distance_mm: 3000.0, right_distance_mm: 3000.0, head_width_mm: 900.0 },
        ];
        for g in bad {
            assert!(!g.is_usable(), "accepted an unusable triangle");
            let mut l = vec![0.0; 64];
            let mut r = vec![0.0; 64];
            assert!(run(&mut l, &mut r, 384_000, &g).is_err(), "ran on an unusable triangle");
        }
        let good = Ui { speaker_span_mm: 2000.0, left_distance_mm: 3000.0, right_distance_mm: 3000.0, head_width_mm: 180.0 };
        assert!(good.is_usable());
        assert!((good.span_angle_deg() - 38.94).abs() < 0.1, "angle {:.2}", good.span_angle_deg());
    }

    #[test]
    fn depth_is_derived_not_the_distance_typed_in() {
        // The dialog collects what a tape measure can reach — speaker to head —
        // but `design_xtc_filters` wants the PERPENDICULAR depth to the ear
        // plane. Passing the former as the latter was a real defect here, and it
        // hides at normal listening distances: over a 2000 mm span, 3000 mm
        // direct is 2828 mm deep and the two answers differ by 2 degrees. Close
        // in they diverge completely, which is the case pinned below.
        use crate::audio::converter::types::XtcGeometry as Ui;

        // A close-in seat: 1.6 m span with 860 mm diagonals puts the head only
        // 316 mm behind the speaker line. Kept as the arithmetic case because
        // the two readings are furthest apart here - NOT as a description of
        // anyone's room. The figure that prompted it turned out to be a DEPTH
        // typed into a diagonal field, which is why the window now asks for
        // width and depth first. See `MODES` in labxtc-geom.js.
        let near = Ui {
            speaker_span_mm: 1600.0,
            left_distance_mm: 860.0,
            right_distance_mm: 860.0,
            head_width_mm: 180.0,
        };
        assert!(near.is_usable());
        assert!(
            (near.span_angle_deg() - 136.94).abs() < 0.1,
            "span angle {:.2}",
            near.span_angle_deg()
        );
        assert!(
            (near.depth_mm() - 315.6).abs() < 0.5,
            "depth {:.1}, expected 315.6",
            near.depth_mm()
        );
        assert!(
            (near.depth_mm() - near.mean_distance_mm()).abs() > 500.0,
            "depth and direct distance must not be confusable here"
        );

        // Far field: the same two numbers nearly agree, which is exactly why the
        // defect survived the first round of testing.
        let far = Ui {
            speaker_span_mm: 2000.0,
            left_distance_mm: 3000.0,
            right_distance_mm: 3000.0,
            head_width_mm: 180.0,
        };
        assert!((far.depth_mm() - 2828.4).abs() < 0.5, "depth {:.1}", far.depth_mm());

        // An off-centre seat: depth still comes out of the trilateration.
        let off = Ui {
            speaker_span_mm: 2000.0,
            left_distance_mm: 3100.0,
            right_distance_mm: 2900.0,
            head_width_mm: 180.0,
        };
        let x = (3100.0f64 * 3100.0 - 2900.0 * 2900.0) / (2.0 * 2000.0);
        let want = (3100.0f64 * 3100.0 - (x + 1000.0) * (x + 1000.0)).sqrt();
        assert!((off.depth_mm() - want).abs() < 0.5);

        // A head level with the speakers has no depth and must be refused.
        let flat = Ui {
            speaker_span_mm: 2000.0,
            left_distance_mm: 1001.0,
            right_distance_mm: 1001.0,
            head_width_mm: 180.0,
        };
        assert!(!flat.is_usable(), "a seat on the baseline must be refused");

        // And the filter still cancels at the near geometry — a wide span is not
        // a disqualification, which is the other thing measurement corrected.
        let mut l = vec![0.0; 4096];
        let mut r = vec![0.0; 4096];
        let rep = run(&mut l, &mut r, 192_000, &near).expect("should design");
        assert!(
            rep.worst_depth_db < 0.1,
            "amplifies at a 137 degree span: {:.1} dB",
            rep.worst_depth_db
        );
        assert!(
            rep.mean_depth_db < -10.0,
            "mean cancellation only {:.1} dB at a 137 degree span",
            rep.mean_depth_db
        );
    }

    #[test]
    fn apply_preserves_length_and_alignment() {
        // The output has to line up with the input sample for sample: this runs after the
        // resampler and before the ceiling, and everything downstream assumes the length.
        use crate::audio::converter::types::XtcGeometry as Ui;
        let ui = Ui { speaker_span_mm: 2000.0, left_distance_mm: 3000.0, right_distance_mm: 3000.0, head_width_mm: 180.0 };
        let n = 48_000;
        let mut l: Vec<f64> = (0..n).map(|i| ((i as f64) * 0.01).sin() * 0.3).collect();
        let mut r: Vec<f64> = (0..n).map(|i| ((i as f64) * 0.013).sin() * 0.3).collect();
        let rep = run(&mut l, &mut r, 192_000, &ui).expect("should run");
        assert_eq!(l.len(), n);
        assert_eq!(r.len(), n);
        assert!(l.iter().all(|v| v.is_finite()) && r.iter().all(|v| v.is_finite()));
        assert!(rep.taps > 1, "degenerate filter");
        assert!(rep.worst_depth_db < 0.1, "reported amplification: {:.1}", rep.worst_depth_db);
    }

    #[test]
    fn highs_and_subbass_pass_through() {
        // Outside the active band the filter is identity: mono≈side≈1 and cross-feed≈0,
        // so highs and sub-bass are untouched (the earlier version mangled the highs).
        let fs = 384_000.0;
        let (hd, hc) = design_xtc_filters(default_geo(), fs as u32).unwrap();
        for f in [30.0, 60.0, 14_000.0, 18_000.0] {
            let m_db = 20.0 * mono_gain(&hd, &hc, f, fs).max(1e-12).log10();
            let s_db = 20.0 * side_gain(&hd, &hc, f, fs).max(1e-12).log10();
            let cross = freq_response(&hc, f, fs).norm();
            assert!(m_db.abs() < 1.0, "mono not flat out-of-band at {} Hz: {:.2} dB", f, m_db);
            assert!(s_db.abs() < 1.0, "side not flat out-of-band at {} Hz: {:.2} dB", f, s_db);
            assert!(cross < 0.05, "cross-feed not ~0 out-of-band at {} Hz: {:.3}", f, cross);
        }
    }

    #[test]
    fn side_boost_is_bounded() {
        // The side boost is capped (no runaway gain) across a sweep of valid geometries.
        let geos = [
            (1500.0, 2000.0, 150.0),
            (2000.0, 3000.0, 180.0),
            (2500.0, 1500.0, 160.0),
            (3000.0, 4000.0, 170.0),
        ];
        for (sp, di, hw) in geos {
            let g = XtcGeometry { speaker_spacing_mm: sp, listener_distance_mm: di, head_width_mm: hw, strength: 1.0 };
            let (hd, hc) = design_xtc_filters(g, 384_000).unwrap();
            let mut maxw = 0.0f64;
            let mut f = 200.0;
            while f < 8000.0 {
                maxw = maxw.max(side_gain(&hd, &hc, f, 384_000.0));
                f *= 1.05;
            }
            // SIDE_GAIN_CAP is +6 dB (2.0 linear); allow margin for window ripple.
            assert!(maxw <= 2.0 * 1.25, "side boost {:.3} too high for {:?}", maxw, (sp, di, hw));
        }
    }
}

/// The version of XTC a render records in its stamp (`AURA_STAGES`).
/// Raise it when a release changes what this stage writes into a render:
/// every render made with the older one then reads as not converted.
pub const STAGE_VERSION: u32 = 2;
