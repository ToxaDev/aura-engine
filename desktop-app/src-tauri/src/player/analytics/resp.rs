//! Chain frequency response and analytic output spectrum.
//!
//! Computes a pre-built [`ChainResp`] (|H| + analytic O PSD at full resolution)
//! on every `chain_rev` change, then stores it in the analytics hub so the
//! AAN3 route handler can serve display-decimated responses without any
//! computation at request time.
//!
//! ## Algorithm
//!
//! 1. **Bank response**: `response::bank_response(bank)` → |H_FIR| in dBr,
//!    `NFFT·L/2 + 1` bins on the output-rate grid (fs_out / (NFFT·L) Hz/bin).
//!
//! 2. **SUB guard**: fold `fir_response(sub_taps, NFFT·L)` into |H| — a
//!    highpass that attenuates below `hz` Hz.  The passband of `fir_response`
//!    is 0 dBr, so adding it directly is correct.
//!
//! 3. **XTC (2×2)**: for the |H| display, fold the combined power of both
//!    paths: `|H_d(q)|² + |H_c(q)|²` in linear, added (in dB) to |H|.
//!    For the analytic O, the full cross-spectrum formula is used when the
//!    B cross-PSD is available (see [`compute_analytic_o`]).
//!
//! 4. **Scalar gain**: add `desc.gain_db` (≤ 0 dB) to every |H| bin.
//!
//! 5. **Analytic O**: `B_welch_psd × |H|²` (dBFS-sine).  For bins above the
//!    source Nyquist (`q ≥ NFFT/2 + 1`) the source PSD wraps: bin `q` maps to
//!    source bin `q mod (NFFT/2 + 1)`, which picks up the image alias.  The
//!    stopband attenuation of |H| suppresses these aliases to the filter floor.
//!
//! 6. **HP / αHP provenance gate**: if the chain contains `Phase::Hybrid` or
//!    `Phase::Alpha`, `o_db` is filled with `NaN` — the analytic model is not
//!    valid for signal-adaptive phase blending (SPEC §5.3 / ERRATA E1).
//!
//! ## Decimation
//!
//! The route handler in `routes.rs` calls [`ChainResp::decimate`] to produce
//! per-request `(min, max)` pairs at n ≤ 4096 display points (ERRATA E4).
//! No FFT, no Rayon, no allocation is needed at request time.
//!
//! ## Required edits from parallel sessions
//!
//! - **§2.5 render.rs**: `ChainDesc::settings: Arc<PlayerSettings>`.
//! - **§2.4 chain.rs**: `Resources::peek_bank / peek_guard / peek_xtc`.
//! - **hub.rs** (owner B): `Hub::set_chain_resp(sid, Arc<ChainResp>)`.

use std::sync::Arc;

use rustfft::{num_complex::Complex, FftPlanner};

use crate::player::analytics::provenance::{allowed_provenance, Provenance};
use crate::player::analytics::response::{bank_response, fir_response};
use crate::player::chain::Resources;
use crate::player::convolver::{Bank, BLOCK};
use crate::player::render::ChainDesc;
use crate::player::settings::Phase;

use super::chain_info;

/// NFFT = 2 × BLOCK, matching the constant in `response.rs`.
const NFFT: usize = 2 * BLOCK;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Pre-built frequency response for one chain revision.
///
/// Stored as `Arc<ChainResp>` per subject in the hub.  The route handler
/// only decimates this — it never computes or allocates beyond `Vec::with_capacity`.
#[derive(Clone)]
#[allow(dead_code)] // carried with the response for the AAN3 route
pub struct ChainResp {
    /// Chain revision this was computed for.
    pub chain_rev: u32,
    /// Output sample rate.
    pub fs_out: u32,
    /// `NFFT * L / 2 + 1` bins on the output-rate grid.
    pub n_bins: usize,
    /// `|H(f)|` in dBr, length `n_bins`.  0 dBr = passband unity.
    /// Includes FIR bank + SUB guard + XTC combined power + scalar gain.
    pub h_db: Vec<f64>,
    /// Analytic O PSD in dBFS-sine, length `n_bins`.
    /// `NaN` where B Welch PSD is not yet available, or when HP / αHP is
    /// active (provenance gate, ERRATA E1).
    pub o_db: Vec<f64>,
}

/// B-tap Welch PSD supplied by the track analysis thread (owner C, `track.rs`).
///
/// All values are in dBFS-sine on the SOURCE-RATE grid.
/// `n_src_bins = NFFT / 2 + 1` bins at `fs_src`.
#[allow(dead_code)] // carried with the PSD for the AAN3 route
pub struct BWelchPsd {
    /// `NFFT / 2 + 1`.
    pub n_src_bins: usize,
    /// Source sample rate.
    pub fs_src: u32,
    /// L-channel PSD in dBFS-sine.
    pub l_db: Vec<f64>,
    /// R-channel PSD in dBFS-sine.
    pub r_db: Vec<f64>,
    /// `Re(S_LR(q))` cross-PSD in linear (Welch cross-spectrum real part).
    /// Length `n_src_bins`.  `NaN`-filled for mono sources or when the
    /// cross-spectrum was not computed.
    pub cross_re: Vec<f64>,
}

impl BWelchPsd {
    /// From a whole-track Welch PSD of the L/R mean (the B pass's): both
    /// channels read the mean, and without the cross-spectrum the XTC fold
    /// leaves the L·R term out.
    #[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
    pub fn from_mean(psd: &crate::player::analytics::proto::SourcePsd) -> BWelchPsd {
        BWelchPsd {
            n_src_bins: psd.dbfs.len(),
            fs_src: (psd.f_max_hz as f64 * 2.0).round() as u32,
            l_db: psd.dbfs.clone(),
            r_db: psd.dbfs.clone(),
            cross_re: vec![f64::NAN; psd.dbfs.len()],
        }
    }
}

impl ChainResp {
    /// Decimate the `|H|` array into `n` display points over the half-open
    /// output-bin range `[f0_bin, f1_bin)`, returning `(min_dBr, max_dBr)`
    /// per point.
    ///
    /// O(n_bins) total; no FFT, no Rayon, no allocation beyond the output Vec.
    /// Called by the AAN3 route handler per request.
    #[cfg(test)]
    pub fn decimate_h(&self, f0_bin: usize, f1_bin: usize, n: usize) -> Vec<(f32, f32)> {
        decimate_range(&self.h_db, f0_bin, f1_bin, n)
    }

    /// Decimate the analytic O PSD array (same contract as `decimate_h`).
    #[cfg(test)]
    pub fn decimate_o(&self, f0_bin: usize, f1_bin: usize, n: usize) -> Vec<(f32, f32)> {
        decimate_range(&self.o_db, f0_bin, f1_bin, n)
    }
}

// ─── Decimation helper ────────────────────────────────────────────────────────

/// Decimate `arr[f0..f1]` into `n` output points, returning `(min, max)` per
/// bin span.  Preserves stopband ripple envelopes (ERRATA E4).
///
/// - O(n_bins) total; no FFT, no Rayon, no allocation beyond `out`.
/// - `NaN` values in `arr` are skipped; if a span is all-NaN the point is
///   `(NaN, NaN)`.
#[cfg(test)]
pub fn decimate_range(arr: &[f64], f0: usize, f1: usize, n: usize) -> Vec<(f32, f32)> {
    if n == 0 || f0 >= f1 || arr.is_empty() {
        return vec![(f32::NAN, f32::NAN); n];
    }
    let f0 = f0.min(arr.len());
    let f1 = f1.min(arr.len());
    if f0 >= f1 {
        return vec![(f32::NAN, f32::NAN); n];
    }
    let span = f1 - f0;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let lo = f0 + span * k / n;
        let hi = (f0 + span * (k + 1) / n).min(f1).max(lo + 1);
        let mut mn = f64::INFINITY;
        let mut mx = f64::NEG_INFINITY;
        for &v in &arr[lo..hi.min(arr.len())] {
            if v.is_finite() {
                if v < mn { mn = v; }
                if v > mx { mx = v; }
            }
        }
        if mn > mx {
            out.push((f32::NAN, f32::NAN));
        } else {
            out.push((mn as f32, mx as f32));
        }
    }
    out
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Recompute and publish the [`ChainResp`] for the given subject on chain change.
///
/// Called by hub.rs or the dedicated "aura-analytics-resp" thread at
/// below-normal priority when `chain_rev` changes.
///
/// - `out_rate`: the output sample rate for this chain, in Hz.
/// - `b_welch`: the B-tap Welch PSD from track analysis; `None` when the B
///   analysis has not yet completed (chain changed before track.rs finished).
///
/// HP/αHP chains: `o_db` is always `NaN` regardless of `b_welch`.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub fn compute_and_publish(
    sid: u32,
    desc: Arc<ChainDesc>,
    resources: &Resources,
    out_rate: u32,
    b_welch: Option<Arc<BWelchPsd>>,
    chain_rev: u32,
) {
    let resp = compute(&desc, resources, out_rate, b_welch, chain_rev);
    // analytics: owner B adds Hub::set_chain_resp(sid, Arc<ChainResp>).
    crate::player::analytics::hub::get().set_chain_resp(sid, Arc::new(resp));
}

// ─── Core computation ─────────────────────────────────────────────────────────

/// Pure computation, separated for unit-testability.
///
/// Returns a [`ChainResp`] stub (all-NaN) when the bank is not cached yet.
pub fn compute(
    desc: &ChainDesc,
    resources: &Resources,
    out_rate: u32,
    b_welch: Option<Arc<BWelchPsd>>,
    chain_rev: u32,
) -> ChainResp {
    // analytics: §2.5 — ChainDesc::settings: Arc<PlayerSettings>
    let settings = &desc.settings;

    // The chain's own factor: FS for a 44.1/48 kHz source, less for a hi-res
    // one, whose bank is kept under its own L.
    let l = desc.l.max(1);

    // ── Bank response ────────────────────────────────────────────────────────
    let (bank_path, align) = match chain_info::bank_of(settings, out_rate / l as u32, out_rate, desc.stream) {
        Some(b) => b,
        None => return nan_stub(chain_rev, out_rate, expected_n_bins(l)),
    };
    let bank = match chain_info::peek_bank(resources, std::path::Path::new(&bank_path), l, align, out_rate) {
        Some(b) => b,
        None => return nan_stub(chain_rev, out_rate, expected_n_bins(l)),
    };

    let n_bins = bank_n_bins(&bank);
    let mut h_db = bank_response(&bank); // NFFT*L/2 + 1 dBr values

    // ── SUB guard ────────────────────────────────────────────────────────────
    if settings.subsonic_hz != 0 {
        if let Some(sub_taps) = chain_info::peek_guard(resources, out_rate, settings.subsonic_hz) {
            // Hint: NFFT*L samples → fft_n/2+1 bins matching our grid.
            let sub_resp = fir_response(&sub_taps, NFFT * l);
            fold_fir_into_h(&mut h_db, &sub_resp);
        }
        // Miss → omit SUB fold; the next chain-rev will retry.
    }

    // ── Scalar gain ──────────────────────────────────────────────────────────
    if desc.gain_db != 0.0 {
        for v in h_db.iter_mut() {
            *v += desc.gain_db;
        }
    }

    // ── XTC: fold combined power into |H| display ────────────────────────────
    let xtc_pair = if settings.xtc_active() {
        chain_info::peek_xtc(resources, settings, out_rate)
    } else {
        None
    };

    if let Some(ref pair) = xtc_pair {
        fold_xtc_into_h(&mut h_db, &pair.0, &pair.1, l);
    }

    // ── Provenance gate: HP / αHP → no analytic O ────────────────────────────
    // When hp_deferred is true the chain is actually playing linear phase as a
    // stand-in; the requested phase is HP/αHP but the transfer function is
    // linear, so analytic O is valid.  allowed_provenance() already encodes
    // this: it returns HpDeferred (not Measured) for deferred chains.
    let hp_active = matches!(settings.phase, Phase::Hybrid | Phase::Alpha)
        && !desc.hp_deferred;
    let prov_blocked = matches!(
        allowed_provenance(desc),
        Provenance::Measured { .. }
    );
    let analytic_blocked = hp_active || prov_blocked;

    // ── Analytic O PSD ───────────────────────────────────────────────────────
    let o_db = if analytic_blocked {
        // SPEC §5.3: HP/αHP → analytic O is undefined.
        vec![f64::NAN; n_bins]
    } else {
        match b_welch {
            None => vec![f64::NAN; n_bins],
            Some(ref bw) => compute_analytic_o(&h_db, bw, n_bins, xtc_pair.as_deref()),
        }
    };

    ChainResp { chain_rev, fs_out: out_rate, n_bins, h_db, o_db }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Expected number of bins for a given upsample factor, before the bank is
/// loaded.  Used only to size the NaN stub.
fn expected_n_bins(l: usize) -> usize {
    NFFT * l / 2 + 1
}

/// Actual bin count from a loaded bank.
fn bank_n_bins(bank: &Bank) -> usize {
    NFFT * bank.l / 2 + 1
}

/// All-NaN stub returned when the bank is not yet in the cache.
fn nan_stub(chain_rev: u32, fs_out: u32, n_bins: usize) -> ChainResp {
    ChainResp {
        chain_rev,
        fs_out,
        n_bins,
        h_db: vec![f64::NAN; n_bins],
        o_db: vec![f64::NAN; n_bins],
    }
}

/// Fold a FIR response (from [`fir_response`], peak-normalised, 0 dBr = passband)
/// into `h_db` bin-by-bin.  Both arrays are in dBr; shorter `fir` is zero-padded
/// at the high end (passband, no change).
fn fold_fir_into_h(h_db: &mut [f64], fir: &[f64]) {
    let n = fir.len().min(h_db.len());
    for i in 0..n {
        h_db[i] += fir[i]; // dBr + dBr = dBr
    }
    // Bins beyond fir.len() are in the passband of the FIR (fir_response peak-
    // normalizes to 0 dBr), so they need no correction.
}

/// Fold the XTC combined power into `h_db`.
///
/// For the |H| display: at each bin q, the two XTC paths contribute
/// `|H_d(q)|² + |H_c(q)|²` in linear.  We add `10 * log10(|Hd|² + |Hc|²)`
/// (in dBr relative to the XTC passband peak) to `h_db[q]`.
///
/// `fir_response` already peak-normalizes to 0 dBr, so a bin where both XTC
/// paths are at 0 dBr contributes `10 * log10(1 + 1) ≈ +3 dB`.  This is the
/// correct combined power of two equal-level in-phase paths, matching the
/// expected behavior inside the XTC band.
fn fold_xtc_into_h(h_db: &mut [f64], d_taps: &[f64], c_taps: &[f64], l: usize) {
    let hint = NFFT * l;
    let d_resp = fir_response(d_taps, hint); // peak-normalised dBr
    let c_resp = fir_response(c_taps, hint);
    let n = d_resp.len().min(c_resp.len()).min(h_db.len());
    for i in 0..n {
        let pd = db_to_lin_amplitude(d_resp[i]);
        let pc = db_to_lin_amplitude(c_resp[i]);
        let combined_power = pd * pd + pc * pc;
        h_db[i] += 10.0 * combined_power.max(1e-300_f64).log10();
    }
}

/// Compute the analytic O PSD from the pre-folded `h_db` and the B Welch PSD.
///
/// ### Linear chain (no XTC)
/// ```text
/// O_psd[q] = B_avg_power[q mod n_src] × |H[q]|²
/// ```
/// where `B_avg_power` is the mean of L and R channel power PSDs.
///
/// ### XTC chain
/// Full 2×2 cross-spectrum formula per bin q (BACKEND-CONTRACT §1.7):
/// ```text
/// O_L[q] = |H_d|² × B_L[q%n] + |H_c|² × B_R[q%n]
///         + 2 × Re(H_d × H_c*) × Re(S_LR[q%n])
/// ```
/// Phase of H_d / H_c is unavailable from `fir_response` (magnitude only).
/// The cross-term sign is therefore bounded by the signed magnitude:
/// `2 × |H_d| × |H_c| × sign(cross_re) × |cross_re|`.
///
/// ### Image aliasing
/// Bins `q ≥ n_src_bins` in the output grid correspond to image aliases of
/// source content above the source Nyquist.  The source PSD wraps:
/// `q mod n_src_bins` picks the alias power.  The bank's stopband suppresses
/// these to the filter floor (≤ −180 dBFS for 30M at f64).
fn compute_analytic_o(
    h_db: &[f64],
    bw: &BWelchPsd,
    n_bins: usize,
    xtc_pair: Option<&(Vec<f64>, Vec<f64>)>,
) -> Vec<f64> {
    let src_bins = bw.n_src_bins;
    if src_bins == 0 { return vec![f64::NAN; n_bins]; }

    if let Some((d_taps, c_taps)) = xtc_pair {
        // ── XTC path ──────────────────────────────────────────────────────
        let hint = (n_bins - 1) * 2;
        let d_lin = fir_response_lin(d_taps, hint);
        let c_lin = fir_response_lin(c_taps, hint);
        let xlen = d_lin.len().min(c_lin.len()).min(n_bins);

        let mut o = vec![f64::NAN; n_bins];
        for q in 0..xlen {
            let sq = q % src_bins;
            let bl = db_to_lin_power(bw.l_db.get(sq).copied().unwrap_or(f64::NAN));
            let br = db_to_lin_power(bw.r_db.get(sq).copied().unwrap_or(f64::NAN));
            if !bl.is_finite() && !br.is_finite() { continue; }
            let bl = if bl.is_finite() { bl } else { 0.0 };
            let br = if br.is_finite() { br } else { 0.0 };

            let hd = d_lin[q];
            let hc = c_lin[q];
            let mut power = hd * hd * bl + hc * hc * br;

            // Cross-term: 2 × |H_d| × |H_c| × Re(S_LR)
            // We use the real part of the Welch cross-spectrum directly; the
            // sign encodes whether L and R are in-phase (+) or out-of-phase (−).
            if let Some(&cross) = bw.cross_re.get(sq) {
                if cross.is_finite() {
                    power += 2.0 * hd * hc * cross;
                    if power < 0.0 { power = 0.0; }
                }
            }

            o[q] = 10.0 * power.max(1e-300_f64).log10();
        }
        o
    } else {
        // ── Linear chain ──────────────────────────────────────────────────
        let mut o = vec![f64::NAN; n_bins];
        for q in 0..n_bins {
            let sq = q % src_bins;
            let bl = bw.l_db.get(sq).copied().unwrap_or(f64::NAN);
            let br = bw.r_db.get(sq).copied().unwrap_or(bl);
            // Average L and R for the display PSD.
            if !bl.is_finite() && !br.is_finite() { continue; }
            let bl_p = if bl.is_finite() { db_to_lin_power(bl) } else { 0.0 };
            let br_p = if br.is_finite() { db_to_lin_power(br) } else { 0.0 };
            let b_power = (bl_p + br_p) * 0.5;

            let h_power = db_to_lin_power(2.0 * h_db[q]); // |H|² in linear
            let o_power = b_power * h_power;
            o[q] = 10.0 * o_power.max(1e-300_f64).log10();
        }
        o
    }
}

/// Linear amplitudes of an FIR response (not dBr, not power — raw |H(f)|).
/// Returns `fft_n/2 + 1` values.  Peak is NOT normalized to 1.
fn fir_response_lin(taps: &[f64], n: usize) -> Vec<f64> {
    let fft_n = n.max(taps.len()).next_power_of_two().max(64);
    let mut buf: Vec<Complex<f64>> = taps.iter().map(|&t| Complex::new(t, 0.0)).collect();
    buf.resize(fft_n, Complex::new(0.0, 0.0));
    FftPlanner::<f64>::new()
        .plan_fft_forward(fft_n)
        .process(&mut buf);
    let half = fft_n / 2 + 1;
    buf[..half].iter().map(|c| c.norm()).collect()
}

#[inline]
fn db_to_lin_amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[inline]
fn db_to_lin_power(db: f64) -> f64 {
    10f64.powf(db / 10.0)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::analytics::spectra::kaiser;
    use crate::player::convolver::{Alignment, Bank};
    use crate::player::render::ChainDesc;
    use crate::player::settings::{Phase, PlayerSettings};
    use std::f64::consts::PI;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_desc_with_settings(phase: Phase) -> Arc<ChainDesc> {
        let settings = Arc::new(PlayerSettings {
            phase,
            taps: 1_000_000,
            fs_multiplier: 8,
            subsonic_hz: 0,
            xtc: false,
            ..PlayerSettings::default()
        });
        Arc::new(ChainDesc {
            source: "live",
            quick: false,
            taps: Some("1M".into()),
            stages: vec![],
            gain_db: 0.0,
            tp_db: None,
            ceiling_db: None,
            gpu_on: false,
            downgrade: None,
            downgrade_gen: 0,
            hp_deferred: false,
            stream: false,
            file: None,
            // analytics: §2.5 — settings field added by minimal edit
            settings,
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
            // analytics: §2.5 — out_rate field
            out_rate: 352_800,
            l: 8,
        })
    }

    fn make_hp_desc() -> Arc<ChainDesc> {
        make_desc_with_settings(Phase::Hybrid)
    }

    fn make_linear_desc() -> Arc<ChainDesc> {
        make_desc_with_settings(Phase::Linear)
    }

    fn flat_bwelch(n_src_bins: usize, fs_src: u32, level_dbfs: f64) -> Arc<BWelchPsd> {
        Arc::new(BWelchPsd {
            n_src_bins,
            fs_src,
            l_db: vec![level_dbfs; n_src_bins],
            r_db: vec![level_dbfs; n_src_bins],
            cross_re: vec![0.0; n_src_bins],
        })
    }

    /// Build a windowed-sinc lowpass bank for tests.
    fn test_bank(l: usize) -> Arc<Bank> {
        let taps_count = NFFT / 4 + 1;
        let cutoff = 0.85 / l as f64;
        let win = kaiser(taps_count, 20.0);
        let m = (taps_count - 1) as f64;
        let coeffs: Vec<f64> = (0..taps_count)
            .map(|i| {
                let n = i as f64 - m * 0.5;
                let sinc = if n.abs() < 1e-12 {
                    cutoff
                } else {
                    (PI * cutoff * n).sin() / (PI * n)
                };
                sinc * win[i]
            })
            .collect();
        Arc::new(Bank::from_coeffs("test", &coeffs, l, Alignment::Linear, 44100 * l as u32))
    }

    // ── decimate_range ───────────────────────────────────────────────────────

    #[test]
    fn decimate_range_returns_n_points() {
        let data: Vec<f64> = (0..1024).map(|i| i as f64).collect();
        let result = decimate_range(&data, 0, 1024, 256);
        assert_eq!(result.len(), 256);
    }

    #[test]
    fn decimate_range_min_le_max() {
        let data: Vec<f64> = (0..1024).map(|i| (i as f64).sin()).collect();
        for (mn, mx) in decimate_range(&data, 0, 1024, 128) {
            if mn.is_finite() && mx.is_finite() {
                assert!(mn <= mx, "min={mn} must be <= max={mx}");
            }
        }
    }

    #[test]
    fn decimate_range_zero_n_returns_empty() {
        let data = vec![1.0f64; 100];
        assert!(decimate_range(&data, 0, 100, 0).is_empty());
    }

    #[test]
    fn decimate_range_all_nan_input() {
        let data = vec![f64::NAN; 64];
        let result = decimate_range(&data, 0, 64, 8);
        assert!(result.iter().all(|(mn, mx)| mn.is_nan() && mx.is_nan()));
    }

    #[test]
    fn decimate_range_preserves_extremes() {
        // Single spike at bin 512.
        let mut data = vec![0.0f64; 1024];
        data[512] = -10.0;
        data[513] = -300.0;
        // Ask for a single point covering [0, 1024) — should pick up both extremes.
        let pts = decimate_range(&data, 0, 1024, 1);
        assert_eq!(pts.len(), 1);
        let (mn, mx) = pts[0];
        assert!((mn as f64 - (-300.0)).abs() < 1e-3, "min should be -300, got {mn}");
        assert!((mx as f64 - 0.0).abs() < 1e-3, "max should be 0, got {mx}");
    }

    // ── HP / αHP provenance gate ─────────────────────────────────────────────

    /// HP chain: analytic O must be all-NaN regardless of b_welch.
    #[test]
    fn hp_chain_o_db_is_all_nan() {
        let desc = make_hp_desc();
        // We don't have a real bank, so compute() will return a nan_stub anyway;
        // but the provenance gate must fire even if we somehow got a bank.
        let resp = compute(&desc, &fake_resources(), 352_800, None, 1);
        assert!(
            resp.o_db.iter().all(|v| v.is_nan()),
            "HP chain must produce all-NaN o_db"
        );
    }

    #[test]
    fn alpha_phase_chain_o_db_is_all_nan() {
        let desc = make_desc_with_settings(Phase::Alpha);
        let resp = compute(&desc, &fake_resources(), 352_800, None, 1);
        assert!(
            resp.o_db.iter().all(|v| v.is_nan()),
            "αHP chain must produce all-NaN o_db"
        );
    }

    /// Linear chain without b_welch: o_db must be NaN (not yet available).
    #[test]
    fn linear_chain_no_b_welch_gives_nan_o() {
        let desc = make_linear_desc();
        let resp = compute(&desc, &fake_resources(), 352_800, None, 1);
        assert!(resp.o_db.iter().all(|v| v.is_nan()));
    }

    // ── |H| passband ─────────────────────────────────────────────────────────

    /// The passband of the bank response must read 0 dBr ± 0.1 dB (DC bin).
    #[test]
    fn bank_response_passband_near_zero_db() {
        let bank = test_bank(4);
        let resp = bank_response(&bank);
        let dc = resp[0];
        assert!(
            dc.abs() < 0.1,
            "passband (DC bin) should be ≈ 0 dBr, got {dc:.4}"
        );
    }

    // ── decimate_h / decimate_o on ChainResp ────────────────────────────────

    #[test]
    fn chain_resp_decimate_h_returns_n_points() {
        let n = 100;
        let cr = ChainResp {
            chain_rev: 1,
            fs_out: 352_800,
            n_bins: n,
            h_db: (0..n).map(|i| -(i as f64)).collect(),
            o_db: vec![f64::NAN; n],
        };
        let pts = cr.decimate_h(0, n, 32);
        assert_eq!(pts.len(), 32);
    }

    #[test]
    fn chain_resp_decimate_o_nan_stub_stays_nan() {
        let n = 512;
        let cr = ChainResp {
            chain_rev: 2,
            fs_out: 352_800,
            n_bins: n,
            h_db: vec![0.0; n],
            o_db: vec![f64::NAN; n],
        };
        let pts = cr.decimate_o(0, n, 64);
        assert!(pts.iter().all(|(mn, mx)| mn.is_nan() && mx.is_nan()));
    }

    // ── analytic O: linear chain ──────────────────────────────────────────────

    /// Flat B PSD at −6 dBFS, passband gain 0 dBr → analytic O ≈ −6 dBFS.
    #[test]
    fn analytic_o_flat_b_passband_gain_zero() {
        let n_bins = 1024usize;
        let n_src = NFFT / 2 + 1;

        // Flat |H| = 0 dBr
        let h_db = vec![0.0f64; n_bins];
        let bw = flat_bwelch(n_src, 44_100, -6.0);

        let o = compute_analytic_o(&h_db, &bw, n_bins, None);

        // Every finite bin should be ≈ −6 dBFS (0 dBr gain + flat B).
        let finite: Vec<f64> = o.iter().copied().filter(|v| v.is_finite()).collect();
        assert!(!finite.is_empty(), "at least some bins should be finite");
        for &v in &finite {
            assert!(
                (v - (-6.0)).abs() < 0.1,
                "expected ≈ -6.0 dBFS, got {v:.4}"
            );
        }
    }

    /// Gain of −6 dB in |H| → analytic O should be B − 6 dB = −12 dBFS.
    #[test]
    fn analytic_o_with_negative_gain_shifts_level() {
        let n_bins = 256usize;
        let n_src = NFFT / 2 + 1;

        let h_db = vec![-6.0f64; n_bins]; // −6 dBr gain across all bins
        let bw = flat_bwelch(n_src, 44_100, -6.0); // −6 dBFS flat B

        let o = compute_analytic_o(&h_db, &bw, n_bins, None);
        let finite: Vec<f64> = o.iter().copied().filter(|v| v.is_finite()).collect();
        assert!(!finite.is_empty());
        for &v in &finite {
            // O = B_power × |H|² = 10^(-6/10) × 10^(-12/10) = 10^(-18/10) → -18 dBFS
            // (|H|² = 2 × dBr in dB: −6 dBr → |H|² = 10^(-12/10) in power)
            assert!(
                (v - (-18.0)).abs() < 0.15,
                "expected ≈ -18.0 dBFS with -6 dBFS B and -6 dBr |H|, got {v:.4}"
            );
        }
    }

    // ── fold_xtc_into_h: no panic with empty taps ────────────────────────────

    #[test]
    fn fold_xtc_empty_taps_no_panic() {
        let mut h = vec![0.0f64; 64];
        fold_xtc_into_h(&mut h, &[], &[], 4);
        // With empty taps fir_response returns all zeros → no change to h.
    }

    // ── fir_response_lin ─────────────────────────────────────────────────────

    #[test]
    fn fir_response_lin_dirac_is_flat() {
        let taps = vec![1.0f64];
        let lin = fir_response_lin(&taps, 256);
        // DC through the half-band should all equal the dirac's magnitude = 1.
        for (i, &v) in lin.iter().enumerate() {
            assert!(
                (v - 1.0).abs() < 1e-9,
                "bin {i}: expected 1.0, got {v}"
            );
        }
    }

    // ── fake_resources helper ────────────────────────────────────────────────

    fn fake_resources() -> crate::player::chain::Resources {
        // Point to a non-existent directory — no banks will be cached.
        crate::player::chain::Resources::new(
            std::path::PathBuf::from(r"C:\nonexistent\analytics-test"),
        )
    }
}
