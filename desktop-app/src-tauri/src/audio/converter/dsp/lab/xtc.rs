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
//! Symmetric free-field two-source / two-ear model. The 2×2 acoustic transfer matrix from
//! the loudspeakers to the ears (centered, symmetric listener) is
//!
//!     C(ω) = [[A, B],
//!             [B, A]]      A = e^{-jω·τ_ip}          (ipsilateral: near speaker → near ear)
//!                          B = g · e^{-jω·τ_cc}      (contralateral: far  speaker → near ear)
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
//! and the 2×2 XTC filter recombines them:
//!
//!     H_direct = (G₊ + G₋) / 2        (applied to the same-side input)
//!     H_cross  = (G₊ − G₋) / 2        (applied to the opposite-side input)
//!
//! so that  outL = H_direct·inL + H_cross·inR ,  outR = H_direct·inR + H_cross·inL .
//!
//! Capping |G₊| and |G₋| bounds the mono- and side-path gains EXACTLY (the mono-sum output
//! gain is |H_direct + H_cross| = |G₊|), which is the correct way to limit the worst-case
//! boost — clipping |H_direct| alone does not (a verified failure mode of the naive design).
//!
//! ── Numerical robustness ─────────────────────────────────────────────────────────────
//! * The effect is band-limited (identity outside ~300 Hz–12 kHz) so the ill-conditioned
//!   low end (bass boost / dynamic-range loss) and the model-inaccurate top are passed
//!   through untouched. This also makes the mono DC gain exactly 1.0 by construction.
//! * Both filters are extracted with a COMMON window center so the inter-channel delay
//!   (the ITD that does the actual cancellation) is preserved.
//! * `apply_xtc` preserves the exact sample count and time alignment of its input.

use rustfft::{num_complex::Complex, FftPlanner};
use std::f64::consts::PI;

/// Speed of sound in mm/s (so all geometry math stays in millimetres).
const C_MM_S: f64 = 343_000.0;

/// Max boost applied to the SIDE (difference) channel, linear. +6 dB. The mono/center
/// channel is held perfectly flat (gain 1.0) so centered content is never colored.
const SIDE_GAIN_CAP: f64 = 2.0;

/// XTC active band (Hz). Outside this band the filters are identity (pass-through).
const F_LO: f64 = 300.0;
const F_HI: f64 = 8_000.0;

/// Head-shadow corner frequency (Hz). The contralateral (far-ear) path is low-passed at
/// ~−6 dB/oct above this, modelling the head blocking high frequencies from the far ear.
/// This is what keeps the highs clean: above a few kHz the cross term vanishes → passthrough.
const F_SHADOW: f64 = 1_200.0;

/// Tikhonov regularization β at the bottom and top of the active band, interpolated
/// log-linearly in log-frequency. Larger β = gentler inversion = less coloration.
const BETA_LF: f64 = 0.30;
const BETA_HF: f64 = 0.10;

/// Fractional-octave magnitude smoothing width (1/3 octave) — removes the comb peaks/
/// notches of the raw inversion so the result is smooth and uncolored.
const SMOOTH_OCT: f64 = 1.0 / 3.0;

/// Quarter wavelength at 2 kHz, in millimetres. Past this much difference between the
/// two speaker-to-head distances one symmetric filter stops describing the room: the
/// timing error it cannot represent exceeds a quarter period at the frequency where the
/// cancellation still has to be accurate.
const ASYM_SOFT_MM: f64 = 42.9;

/// Design-time FFT size. One-time cost; gives ~6 Hz bin resolution at 384 kHz.
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
    if f <= F_LO {
        BETA_LF
    } else if f >= F_HI {
        BETA_HF
    } else {
        let t = (f / F_LO).ln() / (F_HI / F_LO).ln();
        (BETA_LF.ln() + t * (BETA_HF.ln() - BETA_LF.ln())).exp()
    }
}

/// Cosine fade weight in [0,1] selecting how much XTC to apply at frequency `f`.
/// 0 = identity (pass-through), 1 = full XTC. Smooth fades at both band edges.
fn band_blend(f: f64) -> f64 {
    let fade_in_hi = F_LO * 2.0; // 300 → 600 Hz fade-in
    let fade_out_lo = F_HI * 0.75; // 6000 → 8000 Hz fade-out
    if f < F_LO || f > F_HI {
        0.0
    } else if f < fade_in_hi {
        let t = (f - F_LO) / (fade_in_hi - F_LO);
        0.5 * (1.0 - (PI * t).cos())
    } else if f > fade_out_lo {
        let t = (f - fade_out_lo) / (F_HI - fade_out_lo);
        0.5 * (1.0 + (PI * t).cos())
    } else {
        1.0
    }
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

    // Path lengths from each speaker to the near ear.
    let d_ip = ((s - e) * (s - e) + d * d).sqrt(); // ipsilateral (near speaker → near ear)
    let d_cc = ((s + e) * (s + e) + d * d).sqrt(); // contralateral (far speaker → near ear)
    let delta_r = d_cc - d_ip;
    if delta_r < 1.0 {
        return Err("XTC disabled: geometry too symmetric (Δpath < 1 mm)".to_string());
    }

    let tau_ip = d_ip / C_MM_S; // seconds
    let tau_cc = d_cc / C_MM_S;
    let g = d_ip / d_cc; // inverse-distance contralateral attenuation (0 < g < 1)

    let fs = out_rate as f64;
    let n = DESIGN_NFFT;
    let half = n / 2;

    crate::aelog!(
        "[XTC] Geometry: spacing={:.0}mm dist={:.0}mm head={:.0}mm strength={:.2}",
        geo.speaker_spacing_mm, geo.listener_distance_mm, geo.head_width_mm, strength
    );
    crate::aelog!(
        "[XTC] d_ip={:.0}mm d_cc={:.0}mm Δr={:.0}mm  ITD={:.1}µs  g={:.4}  N_fft={}",
        d_ip, d_cc, delta_r, (tau_cc - tau_ip) * 1e6, g, n
    );

    // ══ Pass 1: shadowed, regularized scalar inverses of the mono (sum) and side (diff)
    //    eigen-branches. We keep only their magnitudes + the side phase. ══
    let mut mag_p = vec![0.0f64; half + 1]; // |mono inverse|
    let mut mag_m = vec![0.0f64; half + 1]; // |side inverse|
    let mut ph_p = vec![0.0f64; half + 1]; // arg(mono inverse) — the half that was dropped
    let mut ph_m = vec![0.0f64; half + 1]; // arg(side inverse) — the spatial timing
    for k in 0..=half {
        let f = k as f64 * fs / n as f64;
        let w = 2.0 * PI * f;
        // Head shadow: the far ear receives progressively less HF → cross term vanishes at HF.
        let shadow = 1.0 / (1.0 + (f / F_SHADOW).powi(2)).sqrt();
        let a = Complex::from_polar(1.0, -w * tau_ip); // ipsilateral
        let b = Complex::from_polar(g * shadow, -w * tau_cc); // contralateral (shadowed)
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
    //    unity-floored (never narrows), capped boost plus the RELATIVE phase of the two
    //    inverses. Magnitude and phase are faded toward identity at the band edges.
    //
    //    Both branches are divided by G₊, which is what pins the mono path to 1∠0 without
    //    disturbing the ratio that does the cancelling. Magnitude ratio and phase
    //    difference are two halves of one operation — taking one and not the other is the
    //    defect described at the top of this file. ══
    let mut hd = vec![Complex::new(0.0, 0.0); n];
    let mut hc = vec![Complex::new(0.0, 0.0); n];
    let mut max_side = 0.0f64;
    let mut clamp_lo = f64::INFINITY;
    let mut clamp_hi = 0.0f64;
    for k in 0..=half {
        let f = k as f64 * fs / n as f64;
        let bl = band_blend(f) * strength;
        let raw_rel = sm_m[k] / sm_p[k].max(1e-9);
        let rel = raw_rel.clamp(1.0, SIDE_GAIN_CAP);
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
        "[XTC] Mono path flat | max side boost {:.2}dB (cap {:.1}dB) | shadow fc {}Hz band {}-{}Hz",
        20.0 * max_side.max(1e-12).log10(),
        20.0 * SIDE_GAIN_CAP.log10(),
        F_SHADOW as u32, F_LO as u32, F_HI as u32
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

    // ── Adaptive tap half-width: keep the response around the common center N/2 until the
    //    combined envelope decays below threshold. Sized on the REAL ring decay (driven by g
    //    and β), not on the irrelevant absolute speaker distance. ──
    let peak_env = (0..n)
        .map(|i| raw_d[i].abs().max(raw_c[i].abs()))
        .fold(0.0f64, f64::max)
        .max(1e-300);
    let thresh = peak_env * 1e-4;
    let max_half = (half - 1).min((0.040 * fs) as usize); // ≤ 40 ms one-sided
    let min_half = 256usize;
    let mut tap_half = min_half;
    for off in (min_half..=max_half).rev() {
        let lo = half - off;
        let hi = half + off;
        if raw_d[lo].abs().max(raw_c[lo].abs()) > thresh
            || raw_d[hi].abs().max(raw_c[hi].abs()) > thresh
        {
            tap_half = off;
            break;
        }
    }
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
             will reduce output level. Lower XTC Strength or increase Headroom.",
            20.0 * peak.max(1e-12).log10(),
            peak
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════════════
// Lab entry point: run the thing, and report numbers someone can argue with.
//
// The owner listens and says what he hears; this has to say what it did, in terms that
// can be checked against that. So the report is measured, never assumed — the cancellation
// depths come from the frequency response of the FIR that was actually built, evaluated
// against the same acoustic model it was designed for, not from the ideal it aimed at.
// ══════════════════════════════════════════════════════════════════════════════════════

use crate::audio::converter::types::XtcGeometry as UiGeometry;

/// The frequencies the report speaks about. Spread over the active band, at values a
/// listener can place: the bottom edge, the vocal range, presence, the top edge.
const PROBE_HZ: [f64; 8] = [300.0, 500.0, 800.0, 1000.0, 2000.0, 3000.0, 4000.0, 6000.0];

pub struct XtcReport {
    pub span_angle_deg: f64,
    pub angle_verdict: &'static str,
    pub mean_distance_mm: f64,
    /// Perpendicular depth the filter was actually designed for.
    pub depth_mm: f64,
    pub asymmetry_mm: f64,
    pub span_mm: f64,
    pub head_mm: f64,
    /// (Hz, dB) — the wrong ear against the right one. Negative is cancellation.
    pub depth: Vec<(f64, f64)>,
    pub worst_depth_db: f64,
    pub mean_depth_db: f64,
    pub taps: usize,
    pub peak_change_db: f64,
    pub rms_change_db: f64,
    pub side_boost_db: f64,
}

/// Path lengths and the shadowed contralateral term — one definition, shared by the
/// designer and by the measurement that grades it.
fn acoustic_ab(span_mm: f64, dist_mm: f64, head_mm: f64, f: f64) -> (Complex<f64>, Complex<f64>) {
    let s = span_mm * 0.5;
    let e = head_mm * 0.5;
    let d_ip = ((s - e) * (s - e) + dist_mm * dist_mm).sqrt();
    let d_cc = ((s + e) * (s + e) + dist_mm * dist_mm).sqrt();
    let w = 2.0 * PI * f;
    let shadow = 1.0 / (1.0 + (f / F_SHADOW).powi(2)).sqrt();
    let a = Complex::from_polar(1.0, -w * (d_ip / C_MM_S));
    let b = Complex::from_polar((d_ip / d_cc) * shadow, -w * (d_cc / C_MM_S));
    (a, b)
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
pub fn cancellation_db(
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

/// Descriptive, not a grade. The earlier version graded the angle on the received
/// wisdom that this method wants a 20-30 degree pair; measured against this filter
/// that does not hold - a sweep from 10 to 150 degrees gives a mean cancellation
/// between -18 and -26 dB at every angle. Head position is the constraint, not span.
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
    depth: Vec<(f64, f64)>,
    worst_depth_db: f64,
    mean_depth_db: f64,
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
    let depth: Vec<(f64, f64)> = PROBE_HZ
        .iter()
        .map(|&f| (f, cancellation_db(&h_direct, &h_cross, span, dist, head, f, fs)))
        .collect();
    let worst = depth
        .iter()
        .map(|&(_, d)| d)
        .fold(f64::NEG_INFINITY, f64::max);
    let mean = depth.iter().map(|&(_, d)| d).sum::<f64>() / depth.len() as f64;

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
                 will reduce output level. Lower XTC Strength or increase Headroom.",
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
    /// Amber when the filter never really got to work — a wide triangle, or a probe where
    /// the wrong ear is not actually being quietened. Green only when it cancelled.
    /// Amber only on something measured: a probe where the wrong ear is not
    /// actually being quietened. The span angle is no longer part of this - it
    /// was, on an assumption the measurements did not support.
    pub fn level(&self) -> &'static str {
        if self.worst_depth_db > -3.0 {
            "warn"
        } else {
            "ok"
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{:.0} deg span ({}), {:.1} dB mean cancellation, worst {:.1} dB; peak {:+.2} dB",
            self.span_angle_deg,
            self.angle_verdict,
            self.mean_depth_db,
            self.worst_depth_db,
            self.peak_change_db
        )
    }

    pub fn tag_value(&self) -> String {
        format!(
            "span={:.0}mm;dist={:.0}mm;head={:.0}mm;angle={:.1}deg;mean={:.1}dB;worst={:.1}dB",
            self.span_mm,
            self.mean_distance_mm,
            self.head_mm,
            self.span_angle_deg,
            self.mean_depth_db,
            self.worst_depth_db
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
            "[XTC] beta {:.2} at {}Hz falling to {:.2} at {}Hz; side boost peak {:.2}dB (cap {:.2}dB)",
            BETA_LF,
            F_LO as u32,
            BETA_HF,
            F_HI as u32,
            self.side_boost_db,
            20.0 * SIDE_GAIN_CAP.log10()
        );
        crate::aelog!("[XTC] cancellation, wrong ear vs right ear (negative is good):");
        for &(f, d) in &self.depth {
            crate::aelog!(
                "[XTC]   {:>5.0} Hz  {:>7.1} dB{}",
                f,
                d,
                if d > 0.0 { "   ** AMPLIFYING **" } else { "" }
            );
        }
        crate::aelog!(
            "[XTC] mean {:.1}dB, worst {:.1}dB over {} probes",
            self.mean_depth_db,
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
        // Not merely "does no harm": the filter has to remove a useful amount. Thresholds
        // are set below what the design measures so window ripple and truncation have room,
        // but far above what the natural (unfiltered) crosstalk is — which at these
        // frequencies runs about −1 to −11 dB, i.e. barely any separation at all.
        let fs = 384_000.0;
        let (span, dist, head) = ROOMS[0];
        let geo = XtcGeometry {
            speaker_spacing_mm: span,
            listener_distance_mm: dist,
            head_width_mm: head,
            strength: 1.0,
        };
        let (hd, hc) = design_xtc_filters(geo, fs as u32).unwrap();

        // (frequency, the most the wrong ear may hear). Design measures −26/−36/−14/−9/−17
        // at these; the bar is set with margin so this fails on a real regression, not on
        // a third-decimal drift.
        for (f, limit) in [
            (800.0, -12.0),
            (1000.0, -15.0),
            (2000.0, -6.0),
            (3000.0, -4.0),
            (4000.0, -8.0),
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

        // And it must beat doing nothing at all, by a wide margin, on average.
        let mean: f64 = PROBE_HZ
            .iter()
            .map(|&f| cancellation_db(&hd, &hc, span, dist, head, f, fs))
            .sum::<f64>()
            / PROBE_HZ.len() as f64;
        assert!(mean < -10.0, "mean cancellation only {:.1} dB", mean);
    }

    #[test]
    fn absolute_side_phase_would_amplify() {
        // Proof that the test above has teeth: rebuild the filter pair the way the
        // prototype did — side branch carrying arg(G₋) instead of arg(G₋) − arg(G₊) — and
        // confirm it fails the property. If someone ever "simplifies" the phase back, the
        // test above starts failing for the reason demonstrated here.
        let (span, dist, head) = ROOMS[0];
        let s_half = span * 0.5;
        let e = head * 0.5;
        let d_ip = ((s_half - e) * (s_half - e) + dist * dist).sqrt();
        let d_cc = ((s_half + e) * (s_half + e) + dist * dist).sqrt();

        let mut worst = f64::NEG_INFINITY;
        let mut f = F_LO;
        while f <= F_HI {
            let w = 2.0 * PI * f;
            let shadow = 1.0 / (1.0 + (f / F_SHADOW).powi(2)).sqrt();
            let a = Complex::from_polar(1.0, -w * (d_ip / C_MM_S));
            let b = Complex::from_polar((d_ip / d_cc) * shadow, -w * (d_cc / C_MM_S));
            let (lam_p, lam_m) = (a + b, a - b);
            let b2 = beta_at(f) * beta_at(f);
            let gp = lam_p.conj() / (lam_p.norm_sqr() + b2);
            let gm = lam_m.conj() / (lam_m.norm_sqr() + b2);
            let bl = band_blend(f);
            let rel = (gm.norm() / gp.norm().max(1e-9)).clamp(1.0, SIDE_GAIN_CAP);
            let mag = 1.0 + (rel - 1.0) * bl;

            // the defect: absolute phase of the side inverse, not the difference
            let g_p = Complex::new(1.0, 0.0);
            let g_m = Complex::from_polar(mag, gm.arg() * bl);
            let (h_direct, h_cross) = ((g_p + g_m) * 0.5, (g_p - g_m) * 0.5);
            let off = a * h_cross + b * h_direct;
            let diag = a * h_direct + b * h_cross;
            worst = worst.max(20.0 * (off.norm() / diag.norm().max(1e-15)).log10());
            f *= 1.03;
        }
        assert!(
            worst > 20.0,
            "the old formula should amplify badly; measured only {:+.1} dB",
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
        for f in [60.0, 120.0, 14_000.0, 18_000.0] {
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
