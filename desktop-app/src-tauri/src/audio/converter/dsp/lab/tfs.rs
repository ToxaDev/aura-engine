use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Filename suffix for the derived blob. Versioned, because every version so
/// far held a filter that was wrong in a way the file itself does not record:
/// `tfs_phase` split the bands in time by the whole bulk delay, `tfs_phase_v2`
/// carried a group-delay hump through the middle of the crossover (see
/// `derive_tfs`). Neither is found by this name any more, so a correct filter
/// is derived in their place. Still contains "tfs_phase", which is what the
/// pipeline matches on to force the linear-phase trim.
const TFS_PHASE_SUFFIX: &str = "tfs_phase_v3";

/// Crossover: below `F_LOW` the response stays linear-phase, above `F_HIGH` it
/// carries the full minimum-phase dispersion, and `weight_high` ramps between.
const F_LOW: f64 = 1500.0;
const F_HIGH: f64 = 4000.0;

fn next_pow2(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

// Phase unwrap along a sequence of angles.
fn unwrap_phase(angles: &[f64]) -> Vec<f64> {
    if angles.is_empty() {
        return vec![];
    }
    let mut out = angles.to_vec();
    for i in 1..out.len() {
        let diff = out[i] - out[i - 1];
        let k = (diff / (2.0 * PI)).round();
        out[i] -= k * 2.0 * PI;
    }
    out
}

// Blend weight: 0 at f <= 1500 Hz, 1 at f >= 4000 Hz, sin²-ramp between.
fn weight_high(f_hz: f64) -> f64 {
    if f_hz <= F_LOW {
        0.0
    } else if f_hz >= F_HIGH {
        1.0
    } else {
        // Smooth 0→1 ramp: sin²(t·π/2). Continuous at both edges — matches the
        // constant branches (0 at F_LOW, 1 at F_HIGH) and is monotone inside.
        let t = (f_hz - F_LOW) / (F_HIGH - F_LOW);
        let s = (t * PI / 2.0).sin();
        s * s
    }
}

/// The minimum-phase filter's own bulk delay, in samples, measured over the
/// band the crossover spans.
///
/// A minimum-phase lowpass is not dispersive far below its cutoff: through the
/// whole crossover its group delay is flat — 50 samples on the shipped 30M
/// blob — and a flat group delay is latency, not phase character. The
/// linear-phase reference D already supplies every sample of latency this
/// filter needs, so blending that flat part in a second time adds nothing and
/// costs a great deal: it is what put a group-delay hump in the middle of the
/// crossover in v2 (see `derive_tfs`).
///
/// Least-squares fit of `phi_min` to a straight line through the origin. The
/// natural ω² weighting of least squares is wanted here, not worked around:
/// it makes the bins nearest DC — where an unwrapped phase is least
/// trustworthy — count for almost nothing. Returns 0.0 when the transform is
/// too coarse to have bins in the band at all, which leaves the old
/// construction in place rather than applying a fit made of two points.
fn min_phase_bulk_delay(phi_min_u: &[f64], m: usize, fs: f64) -> f64 {
    let k_high = ((F_HIGH * m as f64 / fs).floor() as usize).min(m / 2);
    if k_high < 4 {
        return 0.0;
    }
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for k in 1..=k_high {
        let omega = 2.0 * PI * k as f64 / m as f64;
        num += phi_min_u[k] * omega;
        den += omega * omega;
    }
    if den <= 0.0 {
        0.0
    } else {
        -num / den
    }
}

// Minimal NPY v1 writer: f64 LE, 1-D shape, header padded to 64-byte alignment.
fn write_npy_f64(path: &Path, data: &[f64]) -> Result<(), String> {
    use std::io::Write;
    let n = data.len();
    let dict = format!(
        "{{'descr': '<f8', 'fortran_order': False, 'shape': ({},), }}",
        n
    );
    // 10-byte preamble + header_len bytes must be a multiple of 64.
    let base = dict.len() + 1; // +1 for trailing '\n'
    let total = ((10 + base + 63) / 64) * 64;
    let header_len = total - 10;

    let mut hdr = vec![b' '; header_len];
    hdr[..dict.len()].copy_from_slice(dict.as_bytes());
    hdr[header_len - 1] = b'\n';

    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("[TFS] Cannot create {}: {}", path.display(), e))?;
    f.write_all(b"\x93NUMPY")
        .map_err(|e| format!("[TFS] Write error: {}", e))?;
    f.write_all(&[1u8, 0u8])
        .map_err(|e| format!("[TFS] Write error: {}", e))?;
    f.write_all(&(header_len as u16).to_le_bytes())
        .map_err(|e| format!("[TFS] Write error: {}", e))?;
    f.write_all(&hdr)
        .map_err(|e| format!("[TFS] Write error: {}", e))?;
    let mut raw = vec![0u8; n * 8];
    for (i, &v) in data.iter().enumerate() {
        raw[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    f.write_all(&raw)
        .map_err(|e| format!("[TFS] Write error: {}", e))?;
    Ok(())
}

/// Core derivation: the linear-phase magnitude, carried at the linear-phase
/// bulk delay D = (N-1)/2, with minimum-phase *dispersion* faded in above
/// 1500-4000 Hz (sin² ramp).
///
/// Two things have to be taken off the minimum-phase branch before it is any
/// use here, and both are bulk delay rather than phase character.
///
/// A minimum-phase filter has its energy at t ≈ 0 while the linear-phase one
/// has it at t = D, so interpolating between the two raw phase functions does
/// not produce a filter whose top end is minimum-phase: it produces one whose
/// top end arrives D samples before its bottom end. At 30M taps and 352.8 kHz
/// that D is 42.5 seconds. Hence the reference delay below: the minimum-phase
/// contribution is added to D, not blended against it.
///
/// That leaves a smaller delay of the same kind. Through the whole crossover
/// the minimum-phase blob's group delay is flat — 50 samples on the shipped
/// 30M blob — which is latency, not character, and D already supplies all the
/// latency this filter wants. Adding it again
/// forces the excess phase to travel the corresponding 3.6 radians inside a
/// 2.5 kHz band, and the group delay that implies is a hump: 0.30 ms peaking
/// near 3 kHz, against the 0.15 ms of genuine dispersion being crossed over
/// to. Twice the size of the effect, in the presence region, with no physical
/// meaning — and on brickwall-limited material it raised output true peak by
/// 4.2 dB where the honest construction raises it by 1.0, which the output
/// ceiling then took straight back off the whole file. So `phi_min` is
/// referenced to its own bulk delay first, and what gets blended is the
/// dispersion alone.
///
/// Above the crossover the response therefore keeps the minimum-phase
/// asymmetry (energy after the peak, no pre-ringing) while staying
/// time-aligned with everything below it to within that dispersion.
///
/// FFT size M = next_pow2(N). Both slices must have the same length N.
/// Cancel is checked after each FFT pass.
pub(crate) fn derive_tfs(
    lin: &[f64],
    min_ph: &[f64],
    out_rate: u32,
    cancel: &AtomicBool,
) -> Result<Vec<f64>, String> {
    let n = lin.len();
    if n != min_ph.len() {
        return Err(format!(
            "[TFS] lin ({}) and min ({}) lengths differ",
            n,
            min_ph.len()
        ));
    }
    if n < 4 {
        return Err("[TFS] Filter too short for derivation".to_string());
    }

    let m = next_pow2(n);
    let d = (n - 1) as f64 / 2.0;
    let fs = out_rate as f64;

    let mut planner = FftPlanner::<f64>::new();
    let fft_fwd = planner.plan_fft_forward(m);
    let fft_inv = planner.plan_fft_inverse(m);

    // FFT the linear-phase filter; save magnitudes.
    let mut buf_lin: Vec<Complex<f64>> = lin
        .iter()
        .map(|&x| Complex::new(x, 0.0))
        .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n))
        .collect();
    fft_fwd.process(&mut buf_lin);
    let lin_mags: Vec<f64> = buf_lin.iter().map(|c| c.norm()).collect();
    drop(buf_lin);

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    // FFT the minimum-phase filter; unwrap its phase.
    let mut buf_min: Vec<Complex<f64>> = min_ph
        .iter()
        .map(|&x| Complex::new(x, 0.0))
        .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n))
        .collect();
    fft_fwd.process(&mut buf_min);
    let angles_min: Vec<f64> = buf_min.iter().map(|c| c.arg()).collect();
    drop(buf_min);
    let phi_min_u = unwrap_phase(&angles_min);

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    // Build H_tfs: |H_lin| magnitude, blended phase.
    //   phi_ref  = -omega * D            (linear-phase group delay D everywhere)
    //   psi_min  = phi_min_u + omega*t0  (min-phase phase, own bulk delay t0 out)
    //   phi_tfs  = phi_ref + w_high * psi_min
    //
    // psi_min is added to the reference delay rather than interpolated against
    // it: what the high band should take from the minimum-phase filter is its
    // dispersion — the part that puts the ringing after the peak instead of
    // before it — and nothing else. Both bulk delays are held out of the
    // blend, D because the filter already carries it and t0 because it would
    // be a second copy of the same thing. Group delay therefore runs from D
    // below the crossover to D + (gd_min(f) - t0) above it, with no hump in
    // between.
    let tau0 = min_phase_bulk_delay(&phi_min_u, m, fs);
    crate::aelog!(
        "[TFS] Minimum-phase bulk delay held out of the blend: {:.1} samples ({:.3} ms)",
        tau0,
        tau0 * 1000.0 / fs
    );

    let mut h_tfs: Vec<Complex<f64>> = (0..m)
        .map(|k| {
            let f_hz = k as f64 * fs / m as f64;
            let omega = 2.0 * PI * k as f64 / m as f64;
            let phi_ref = -omega * d;
            let w = weight_high(f_hz);
            let psi_min = phi_min_u[k] + omega * tau0;
            let phi_tfs = phi_ref + w * psi_min;
            let mag = lin_mags[k];
            Complex::new(mag * phi_tfs.cos(), mag * phi_tfs.sin())
        })
        .collect();

    // Enforce Hermitian symmetry so the IFFT is real.
    h_tfs[0] = Complex::new(h_tfs[0].re, 0.0);
    if m % 2 == 0 {
        let half = m / 2;
        h_tfs[half] = Complex::new(h_tfs[half].re, 0.0);
        for k in 1..half {
            h_tfs[m - k] = h_tfs[k].conj();
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    // IFFT → real
    fft_inv.process(&mut h_tfs);
    let scale = 1.0 / m as f64;
    let h_real: Vec<f64> = h_tfs.iter().map(|c| c.re * scale).collect();

    // Tail energy check: energy beyond N must be < 1e-8 of total.
    let total_e: f64 = h_real.iter().map(|x| x * x).sum();
    if total_e > 0.0 {
        let tail_e: f64 = h_real[n..].iter().map(|x| x * x).sum();
        let ratio = tail_e / total_e;
        if ratio >= 1e-8 {
            crate::aelog!(
                "[TFS] Warning: tail energy ratio {:.2e} >= 1e-8 — filter may be truncated",
                ratio
            );
        }
    }

    let out = h_real[..n].to_vec();

    // Magnitude check, after truncation. Doing it on the spectrum before the
    // IFFT only restated the construction; the damage a truncated tail does to
    // the passband is visible here or not at all.
    {
        let mut buf: Vec<Complex<f64>> = out
            .iter()
            .map(|&x| Complex::new(x, 0.0))
            .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n))
            .collect();
        fft_fwd.process(&mut buf);
        let passband_bins = (m / 2).saturating_sub(1);
        let peak = lin_mags[..passband_bins.max(1)]
            .iter()
            .cloned()
            .fold(0.0f64, f64::max)
            .max(1e-300);

        // Measured only where the linear-phase filter has a response left to
        // damage. Against the peak across every bin the check was unreadable:
        // it fired at 1.29e-2 on every derivation, and the bin it fired on was
        // 21.05 kHz, where the linear-phase filter sits 124 dB down in a
        // ripple null of its own. What fills that null is `lin_mags` itself —
        // a symmetric FIR's amplitude response goes negative between stopband
        // ripples and a magnitude does not follow it there — and a null filled
        // to -40 dB, above the audio band, is not worth a word. Across
        // 20 Hz - 20 kHz the same filter tracks the linear-phase magnitude to
        // four millionths of a dB, which is what this was meant to be
        // watching. -60 dB of peak is where it stops watching.
        let floor = peak * 1e-3;
        let mut max_err = 0.0f64;
        let mut max_err_hz = 0.0f64;
        for k in 1..passband_bins {
            if lin_mags[k] < floor {
                continue;
            }
            let e = (buf[k].norm() - lin_mags[k]).abs();
            if e > max_err {
                max_err = e;
                max_err_hz = k as f64 * fs / m as f64;
            }
        }
        if max_err / peak >= 1e-3 {
            crate::aelog!(
                "[TFS] Warning: magnitude error after truncation {:.2e} of peak at {:.0} Hz",
                max_err / peak,
                max_err_hz
            );
        }
    }

    Ok(out)
}

/// Resolve a cached TFS-phase filter or derive one from the lin+min blob pair.
///
/// Returns the path of the TFS .npy on success. Returns Err when the cached
/// file is missing and the required lin/min blobs are also absent.
pub fn resolve_or_derive(
    taps: usize,
    out_rate: u32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    use crate::audio::converter::decode::set_status;
    use crate::audio::converter::dsp::filter::{
        find_precomputed_filter, missing_filter_error, taps_label,
    };
    use crate::audio::dsp_core::load_npy_f64;
    use crate::audio::memory::await_free_ram;

    // 1. Fast path: a cached TFS blob already exists.
    //
    // The suffix carries a version, and every earlier one is wrong in a way
    // the file itself cannot report: v1's treble runs D samples ahead of its
    // bass, v2 carries a 0.30 ms group-delay hump through the crossover. Both
    // are simply not found by this name, and a correct one is derived in their
    // place — the old blobs are left on disk, inert, for anyone comparing.
    if let Some(p) = find_precomputed_filter(taps, out_rate, TFS_PHASE_SUFFIX) {
        crate::aelog!("[TFS] Found cached TFS filter: {}", p);
        return Ok(PathBuf::from(p));
    }

    // 2. Need lin + min blobs to derive from.
    let lin_path = find_precomputed_filter(taps, out_rate, "linear_phase")
        .ok_or_else(|| missing_filter_error(taps, out_rate, "linear_phase"))?;
    let min_path = find_precomputed_filter(taps, out_rate, "minimum_phase")
        .ok_or_else(|| missing_filter_error(taps, out_rate, "minimum_phase"))?;

    crate::aelog!(
        "[TFS] No cached TFS filter — deriving from lin+min blobs (one-time)"
    );
    set_status("Deriving TFS filter (one-time)...");

    // Memory gate: M * 16 * 4 bytes (4 complex-f64 buffers of length M).
    let m = next_pow2(taps);
    let est_mb = ((m as u64) * 64 / (1024 * 1024)).max(64);
    await_free_ram(est_mb, "[TFS] TFS derivation");

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    let lin = load_npy_f64(&lin_path)
        .map_err(|e| format!("[TFS] Cannot load lin blob: {}", e))?;
    let min_ph = load_npy_f64(&min_path)
        .map_err(|e| format!("[TFS] Cannot load min blob: {}", e))?;

    crate::aelog!(
        "[TFS] Loaded lin ({} coefficients) and min ({} coefficients)",
        lin.len(),
        min_ph.len()
    );

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    set_status("Deriving TFS filter — FFT phase blend...");
    let h_tfs = derive_tfs(&lin, &min_ph, out_rate, cancel)?;
    drop(lin);
    drop(min_ph);

    // Save next to the lin blob with the tfs_phase naming convention.
    let label = taps_label(taps).ok_or("[TFS] Tap count below minimum")?;
    let cache_name = format!("fir_{}_{}_{}.npy", label, out_rate, TFS_PHASE_SUFFIX);
    let cache_dir = PathBuf::from(&lin_path);
    let cache_dir = cache_dir
        .parent()
        .ok_or("[TFS] lin blob has no parent directory")?;
    let cache_path = cache_dir.join(&cache_name);

    crate::aelog!(
        "[TFS] Saving derived filter ({} coefficients) to: {}",
        h_tfs.len(),
        cache_path.display()
    );
    write_npy_f64(&cache_path, &h_tfs)?;
    crate::aelog!("[TFS] TFS filter cached: {}", cache_path.display());

    Ok(cache_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::{num_complex::Complex, FftPlanner};
    use std::sync::atomic::AtomicBool;

    /// Cutoff of the shipped blobs, normalised to the 352.8 kHz output rate.
    /// The tests that look at dispersion need a filter that turns over inside
    /// the audio band, the way the real one does.
    const FC_20K: f64 = 20_000.0 / 352_800.0;

    // Hann-windowed sinc lowpass. fc is in Hz; fs_half = out_rate / 2.
    // fc_norm = fc_hz / out_rate (normalised to the output sample rate).
    fn make_sinc_win(n: usize, fc_norm: f64) -> Vec<f64> {
        let mid = (n - 1) as f64 / 2.0;
        (0..n)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x.abs() < 1e-10 {
                    2.0 * fc_norm
                } else {
                    (2.0 * PI * fc_norm * x).sin() / (PI * x)
                };
                let w = 0.5 * (1.0 - (2.0 * PI * i as f64 / (n - 1) as f64).cos());
                sinc * w
            })
            .collect()
    }

    // Cepstral minimum-phase version of `lin`. Uses a 2× larger FFT for
    // accuracy; returns a filter of the same length.
    fn make_min_phase_cepstral(lin: &[f64]) -> Vec<f64> {
        let n = lin.len();
        let m = next_pow2(n) * 4; // extra headroom avoids circular artefacts
        let mut planner = FftPlanner::<f64>::new();

        // FFT the padded lin filter.
        let mut buf: Vec<Complex<f64>> = lin
            .iter()
            .map(|&x| Complex::new(x, 0.0))
            .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n))
            .collect();
        planner.plan_fft_forward(m).process(&mut buf);

        // Log-magnitude → real cepstrum via IFFT.
        let mut log_m: Vec<Complex<f64>> = buf
            .iter()
            .map(|c| Complex::new((c.norm() + 1e-30f64).ln(), 0.0))
            .collect();
        planner.plan_fft_inverse(m).process(&mut log_m);
        let sc = 1.0 / m as f64;

        // Window: keep causal half (fold anticausal energy into causal).
        let mut c_win: Vec<Complex<f64>> = log_m
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let r = c.re * sc;
                if i == 0 || (m % 2 == 0 && i == m / 2) {
                    Complex::new(r, 0.0)
                } else if i < m / 2 {
                    Complex::new(2.0 * r, 0.0)
                } else {
                    Complex::new(0.0, 0.0)
                }
            })
            .collect();

        // FFT → complex log of H_min → exp → H_min.
        planner.plan_fft_forward(m).process(&mut c_win);
        let mut h_min: Vec<Complex<f64>> = c_win.iter().map(|c| c.exp()).collect();

        // IFFT → real min-phase impulse response.
        planner.plan_fft_inverse(m).process(&mut h_min);
        let sc2 = 1.0 / m as f64;
        h_min[..n].iter().map(|c| c.re * sc2).collect()
    }

    // Energy centroid: sum(i * h[i]^2) / sum(h[i]^2).
    fn energy_centroid(h: &[f64]) -> f64 {
        let total: f64 = h.iter().map(|x| x * x).sum();
        if total == 0.0 {
            return 0.0;
        }
        h.iter()
            .enumerate()
            .map(|(i, x)| i as f64 * x * x)
            .sum::<f64>()
            / total
    }

    // Group delay at bin k via central finite difference of unwrapped phase.
    // Returns delay in samples.

    /// Energy centroid of the TFS filter must land within ±16 samples of D.
    /// We use a filter whose passband edge is well below 1500 Hz so all
    /// passband energy stays in the linear-phase (w=0) region, making the
    /// centroid equal D up to numeric noise.
    #[test]
    fn energy_centroid_within_16_of_d() {
        let n = 4097usize;
        let d = (n - 1) as f64 / 2.0; // 2048.0
        let out_rate = 352_800u32;

        // Cutoff at 500 Hz — entirely below the 1500 Hz blend boundary.
        let fc_norm = 500.0 / out_rate as f64;
        let lin = make_sinc_win(n, fc_norm);
        let min_ph = make_min_phase_cepstral(&lin);

        let cancel = AtomicBool::new(false);
        let h_tfs = derive_tfs(&lin, &min_ph, out_rate, &cancel)
            .expect("derive_tfs must succeed");

        assert_eq!(h_tfs.len(), n);
        let centroid = energy_centroid(&h_tfs);
        let diff = (centroid - d).abs();
        assert!(
            diff <= 16.0,
            "energy centroid {:.2} not within ±16 of D={:.2} (diff={:.2})",
            centroid,
            d,
            diff
        );
    }

    /// Band-limit `h` to [f0, f1] and return (peak index, peak magnitude).
    fn band_peak(h: &[f64], f0: f64, f1: f64, fs: f64) -> (usize, f64) {
        let m = next_pow2(h.len() * 2);
        let mut planner = FftPlanner::<f64>::new();
        let mut buf: Vec<Complex<f64>> = h
            .iter()
            .map(|&x| Complex::new(x, 0.0))
            .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h.len()))
            .collect();
        planner.plan_fft_forward(m).process(&mut buf);
        for k in 0..m {
            let f = if k <= m / 2 {
                k as f64 * fs / m as f64
            } else {
                (m - k) as f64 * fs / m as f64
            };
            if f < f0 || f > f1 {
                buf[k] = Complex::new(0.0, 0.0);
            }
        }
        planner.plan_fft_inverse(m).process(&mut buf);
        let mut best = (0usize, 0.0f64);
        for (i, c) in buf.iter().enumerate() {
            let a = (c.re / m as f64).abs();
            if a > best.1 {
                best = (i, a);
            }
        }
        best
    }

    /// The whole point of the fix: the bands must stay together in time.
    ///
    /// Interpolating between the linear-phase and minimum-phase *phase
    /// functions* puts the top end at t \u2248 0 and the bottom end at t = D. On a
    /// 30M-tap filter that is a 42-second split, heard as a dull track with a
    /// ghost of itself laid over it. Here the split must stay inside a few
    /// hundred samples \u2014 the minimum-phase excess, which is the intended effect.
    #[test]
    fn low_and_high_bands_stay_time_aligned() {
        let n = 4097usize;
        let d = (n - 1) as f64 / 2.0;
        let out_rate = 352_800u32;
        let fs = out_rate as f64;

        // Cutoff where the shipped blobs put it. A filter that only turns over
        // at 123 kHz has no dispersion anywhere near the bands under test, so
        // it would pass this whether the derivation works or not.
        let lin = make_sinc_win(n, FC_20K);
        let min_ph = make_min_phase_cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let h_tfs = derive_tfs(&lin, &min_ph, out_rate, &cancel).expect("derive_tfs");

        let (lo_i, lo_a) = band_peak(&h_tfs, 200.0, 1000.0, fs);
        let (hi_i, hi_a) = band_peak(&h_tfs, 8000.0, 20000.0, fs);
        assert!(lo_a > 0.0 && hi_a > 0.0, "both bands must carry energy");

        let split = (hi_i as f64 - lo_i as f64).abs();
        assert!(
            split <= 0.05 * d,
            "bands split by {:.0} samples (low at {}, high at {}); D={:.0}. \
             The high band must ride the same bulk delay, not arrive D early.",
            split,
            lo_i,
            hi_i,
            d
        );
        assert!(
            (lo_i as f64 - d).abs() <= 0.15 * d,
            "low band peaks at {} but D={:.0}",
            lo_i,
            d
        );
    }

    /// Above the crossover the response must still be minimum-phase *shaped*:
    /// more energy after the peak than before it. That asymmetry is what
    /// removes pre-ringing, and it is the reason to derive a TFS filter at all.
    #[test]
    fn high_band_keeps_minimum_phase_asymmetry() {
        let n = 4097usize;
        let out_rate = 352_800u32;
        let fs = out_rate as f64;

        let lin = make_sinc_win(n, FC_20K);
        let min_ph = make_min_phase_cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let h_tfs = derive_tfs(&lin, &min_ph, out_rate, &cancel).expect("derive_tfs");

        // Energy either side of the peak, in the high band.
        let m = next_pow2(h_tfs.len() * 2);
        let mut planner = FftPlanner::<f64>::new();
        let mut buf: Vec<Complex<f64>> = h_tfs
            .iter()
            .map(|&x| Complex::new(x, 0.0))
            .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h_tfs.len()))
            .collect();
        planner.plan_fft_forward(m).process(&mut buf);
        for k in 0..m {
            let f = if k <= m / 2 {
                k as f64 * fs / m as f64
            } else {
                (m - k) as f64 * fs / m as f64
            };
            if f < 8000.0 || f > 20000.0 {
                buf[k] = Complex::new(0.0, 0.0);
            }
        }
        planner.plan_fft_inverse(m).process(&mut buf);
        let band: Vec<f64> = buf.iter().map(|c| c.re / m as f64).collect();

        let peak = band
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let win = 400usize.min(peak).min(band.len() - peak - 1);
        let pre: f64 = band[peak - win..peak].iter().map(|x| x * x).sum();
        let post: f64 = band[peak + 1..peak + 1 + win].iter().map(|x| x * x).sum();
        assert!(
            post > pre,
            "high band should ring after the peak, not before it (pre={:.3e}, post={:.3e})",
            pre,
            post
        );
    }

    /// The crossover must not invent group delay of its own.
    ///
    /// v2 blended the minimum-phase filter's whole phase, its flat bulk delay
    /// included, which forced the excess phase to travel 3.6 radians inside
    /// 2.5 kHz. The group delay that implies is a hump peaking near 3 kHz at
    /// twice the dispersion actually being crossed over to. It shows up in no
    /// magnitude plot — the response stayed transparent to 0.0001 dB — and it
    /// cost 3 dB of output level on brickwall-limited material, because the
    /// peaks it built were exactly what the output ceiling then took off the
    /// whole file. Inside the crossover the group delay may be no more than
    /// the real dispersion just above it.
    #[test]
    fn crossover_adds_no_group_delay_hump() {
        let n = 4097usize;
        let out_rate = 352_800u32;
        let fs = out_rate as f64;
        let m = next_pow2(n);

        let lin = make_sinc_win(n, FC_20K);
        let min_ph = make_min_phase_cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let h_tfs = derive_tfs(&lin, &min_ph, out_rate, &cancel).expect("derive_tfs");

        let spec = |h: &[f64]| {
            let mut buf: Vec<Complex<f64>> = h
                .iter()
                .map(|&x| Complex::new(x, 0.0))
                .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h.len()))
                .collect();
            FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut buf);
            buf
        };
        let s_tfs = spec(&h_tfs);
        let s_lin = spec(&lin);

        // Group delay the TFS filter adds over the linear-phase one, in
        // samples, read off the unwrapped phase of the ratio. The bulk delay
        // D cancels in the ratio, so what is left is exactly the blend.
        let angles: Vec<f64> = (0..m / 2).map(|k| (s_tfs[k] / s_lin[k]).arg()).collect();
        let phase = unwrap_phase(&angles);
        let gd: Vec<f64> = (0..phase.len() - 1)
            .map(|k| -(phase[k + 1] - phase[k]) * m as f64 / (2.0 * PI))
            .collect();
        let bin_hz = fs / m as f64;

        let mut hump = f64::NEG_INFINITY;
        let mut hump_hz = 0.0;
        for (k, &g) in gd.iter().enumerate() {
            let f = k as f64 * bin_hz;
            if (F_LOW..=F_HIGH).contains(&f) && g > hump {
                hump = g;
                hump_hz = f;
            }
        }
        // The octave above the crossover, where w = 1 and every sample of
        // group delay present is dispersion the design asked for.
        let above: Vec<f64> = gd
            .iter()
            .enumerate()
            .filter(|(k, _)| {
                let f = *k as f64 * bin_hz;
                f > F_HIGH && f <= 2.0 * F_HIGH
            })
            .map(|(_, &g)| g)
            .collect();
        assert!(
            !above.is_empty(),
            "transform too coarse to have bins above the crossover"
        );
        let honest = above.iter().sum::<f64>() / above.len() as f64;

        // 25% plus four samples: enough slack for a 43 Hz bin grid and the
        // cepstral min-phase fixture, nowhere near the 2.3x of the v2 hump.
        let limit = honest * 1.25 + 4.0;
        assert!(
            hump <= limit,
            "group delay inside the crossover peaks at {:.1} samples at {:.0} Hz, \
             against {:.1} samples of real dispersion just above it (limit {:.1}). \
             The blend is adding delay of its own.",
            hump,
            hump_hz,
            honest,
            limit
        );
    }

    /// The bulk-delay fit must find the flat group delay a minimum-phase
    /// lowpass carries below its cutoff, which is what makes it safe to hold
    /// out of the blend.
    #[test]
    fn bulk_delay_fit_recovers_a_known_delay() {
        // A pure delay of 40 samples: phi = -omega * 40 exactly.
        let m = 8192usize;
        let fs = 352_800.0f64;
        let delay = 40.0f64;
        let phi: Vec<f64> = (0..m)
            .map(|k| -2.0 * PI * k as f64 / m as f64 * delay)
            .collect();
        let got = min_phase_bulk_delay(&phi, m, fs);
        assert!(
            (got - delay).abs() < 1e-9,
            "fit returned {got} for a {delay}-sample delay"
        );

        // Too few bins in the band to fit: returns 0.0 rather than a fit made
        // of two points, which leaves the blend as it was.
        assert_eq!(min_phase_bulk_delay(&phi, 32, fs), 0.0);
    }

    /// The blend weight must be continuous at both transition edges and
    /// strictly monotone inside 1500–4000 Hz. (Guards against the inverted
    /// cos² ramp bug: w jumped to ≈1 just above 1500 Hz and fell to ≈0 at
    /// 4000 Hz, flipping the phase blend across the whole transition band.)
    #[test]
    fn weight_high_monotone_and_continuous() {
        assert_eq!(weight_high(1500.0), 0.0);
        assert_eq!(weight_high(4000.0), 1.0);
        assert!(weight_high(1500.1) < 0.001, "must rise from 0 continuously");
        assert!(weight_high(3999.9) > 0.999, "must approach 1 continuously");
        let mut prev = -1.0f64;
        let mut f = 1400.0f64;
        while f <= 4100.0 {
            let w = weight_high(f);
            assert!(
                w >= prev - 1e-12,
                "weight must be monotone: w({:.0})={} < w(prev)={}",
                f, w, prev
            );
            assert!((0.0..=1.0).contains(&w));
            prev = w;
            f += 10.0;
        }
        // Midpoint sanity: sin²(π/4) = 0.5 at 2750 Hz.
        assert!((weight_high(2750.0) - 0.5).abs() < 1e-9);
    }

    /// NPY round-trip: write_npy_f64 must produce bytes that load_npy_f64 reads back.
    #[test]
    fn npy_roundtrip() {
        let data: Vec<f64> = (0..17).map(|i| i as f64 * 0.5 - 2.0).collect();
        let dir = std::env::temp_dir();
        let path = dir.join("aura_tfs_roundtrip_test.npy");
        write_npy_f64(&path, &data).expect("write must succeed");
        let loaded = crate::audio::dsp_core::load_npy_f64(
            path.to_str().unwrap(),
        )
        .expect("load must succeed");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.len(), data.len());
        for (a, b) in data.iter().zip(loaded.iter()) {
            assert!(
                (a - b).abs() < 1e-15,
                "round-trip mismatch: wrote {a} read {b}"
            );
        }
    }

    /// When no lin/min blobs exist on disk resolve_or_derive must return Err,
    /// never silently fall back to a wrong filter.
    #[test]
    fn missing_blobs_returns_err() {
        let cancel = AtomicBool::new(false);
        let result = resolve_or_derive(30_000_000, 352_800, &cancel);
        assert!(result.is_err(), "must Err when blobs are missing");
    }

    /// Cancelled flag propagates: derive_tfs returns Err("Cancelled") immediately.
    #[test]
    fn cancel_propagates() {
        let n = 4097usize;
        let fc_norm = 0.35f64;
        let out_rate = 352_800u32;
        let lin = make_sinc_win(n, fc_norm);
        let min_ph = make_min_phase_cepstral(&lin);

        let cancel = AtomicBool::new(true); // pre-cancelled
        let result = derive_tfs(&lin, &min_ph, out_rate, &cancel);
        assert!(
            result.is_err(),
            "pre-cancelled derive_tfs must return Err"
        );
        assert_eq!(result.unwrap_err(), "Cancelled");
    }
}
