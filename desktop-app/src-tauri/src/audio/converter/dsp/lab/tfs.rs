use std::f64::consts::PI;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Filename suffix for the derived blob. Versioned, because every version so
/// far held a filter that was wrong in a way the file itself does not record:
/// `tfs_phase` split the bands in time by the whole bulk delay, `tfs_phase_v2`
/// carried a group-delay hump through the middle of the crossover, and
/// `tfs_phase_v3` was made on a transform only next_pow2(N) long and cut to
/// its first N taps — the minimum-phase ringing after the centre runs on to
/// D + N, so on 1M and 30M its tail wrapped round to the start as an echo at
/// the wall ahead of the centre, and what was left was cut at D + N/2, which
/// left the stopband at −94…−126 dB against the linear filter's −191…−221
/// (see `derive_tfs`). None is found by this name any more, so a correct
/// filter is derived in their place.
const TFS_PHASE_SUFFIX: &str = "tfs_phase_v4";

/// How far a TFS filter looks ahead of its centre, ms. Below the crossover a
/// TFS filter is linear phase, and its ringing ahead of the centre at the
/// wall falls below −160 dB within ~20 ms of it: the window starts 30 ms
/// ahead and keeps the filter that was meant to −150 dB in the audio band,
/// where waiting for the whole half of a 30M filter took 42.5 s.
const LOOK_AHEAD_MS: u64 = 30;

/// The look-ahead of a TFS filter `taps` long at `out_rate`, output frames —
/// the trim it plays with (`Alignment::LookAhead`, the converter's): 30 ms,
/// or the whole half of a filter shorter than that (5k at the higher rates).
/// Whole numbers at every rate the ladder reaches.
pub fn look_ahead(taps: usize, out_rate: u32) -> usize {
    debug_assert!(out_rate % 100 == 0, "{out_rate} Hz: 30 ms is not a whole number of frames");
    let k = (out_rate as u64 * LOOK_AHEAD_MS / 1000) as usize;
    k.min(taps.saturating_sub(1) / 2)
}

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
#[cfg(test)]
fn unwrap_phase(angles: &[f64]) -> Vec<f64> {
    let mut out = angles.to_vec();
    unwrap_in_place(&mut out);
    out
}

// Phase unwrap in place: a transform of 2^26 bins has no room for a copy.
pub(super) fn unwrap_in_place(a: &mut [f64]) {
    for i in 1..a.len() {
        let diff = a[i] - a[i - 1];
        let k = (diff / (2.0 * PI)).round();
        a[i] -= k * 2.0 * PI;
    }
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
///
/// Fitted on the bins of a grid `m` long, read off a phase made on a grid
/// `stride` times finer: every `stride`-th bin is one of them. The fit moves
/// with the grid by ~1e-5 sample — −111…−125 dB at 10–20 kHz — so the grid it
/// is made on is part of what the filter is, and stays next_pow2(N) however
/// fine the transform the response is made on.
fn min_phase_bulk_delay(phi_min_u: &[f64], stride: usize, m: usize, fs: f64) -> f64 {
    let k_high = ((F_HIGH * m as f64 / fs).floor() as usize).min(m / 2);
    if k_high < 4 {
        return 0.0;
    }
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for k in 1..=k_high {
        let omega = 2.0 * PI * k as f64 / m as f64;
        num += phi_min_u[k * stride] * omega;
        den += omega * omega;
    }
    if den <= 0.0 {
        0.0
    } else {
        -num / den
    }
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
/// The response this describes lives from about D − 20 ms (the crossover's
/// own ringing ahead of the centre) to D + N (the minimum-phase ringing after
/// it), so it is made on a transform M = next_pow2(2N) long, where nothing of
/// it wraps round, and the N taps kept are the window that starts `K` =
/// [`look_ahead`] ahead of the centre: [d − K, d − K + N), d = (N − 1) div 2,
/// played with a trim of K. That lines it up to the sample as the linear
/// filter is lined up by its trim of d. Ahead of the window the response is
/// below −160 dB; after it, the stopband the linear filter has.
///
/// Both slices must have the same length N. Cancel is checked after each FFT
/// pass. Peak memory: the transform, its scratch and its plan's twiddles
/// (complex, M each) and two f64 arrays of M — 64·M bytes, 4.3 GB at 30M.
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

    // The grid τ0 is fitted on (v3's), and the one the response is made on.
    let m_fit = next_pow2(n);
    let m = next_pow2(2 * n);
    let stride = m / m_fit;
    // The phase's centre is the linear filter's own, half a sample off a tap
    // for an even N; the window and the trim are in whole taps.
    let d = (n - 1) as f64 / 2.0;
    let k_ahead = look_ahead(n, out_rate);
    let start = (n - 1) / 2 - k_ahead;
    let fs = out_rate as f64;

    // One plan: at 2^26 points a plan's twiddles are another gigabyte, so the
    // inverse is the forward transform of the conjugate.
    let fft_fwd = FftPlanner::<f64>::new().plan_fft_forward(m);
    let mut scratch = vec![Complex::new(0.0, 0.0); fft_fwd.get_inplace_scratch_len()];
    let zero = Complex::new(0.0, 0.0);

    // FFT the linear-phase filter; save magnitudes.
    let mut buf: Vec<Complex<f64>> = vec![zero; m];
    for (b, &x) in buf.iter_mut().zip(lin) {
        *b = Complex::new(x, 0.0);
    }
    fft_fwd.process_with_scratch(&mut buf, &mut scratch);
    let lin_mags: Vec<f64> = buf.iter().map(|c| c.norm()).collect();

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    // FFT the minimum-phase filter; unwrap its phase.
    buf.fill(zero);
    for (b, &x) in buf.iter_mut().zip(min_ph) {
        *b = Complex::new(x, 0.0);
    }
    fft_fwd.process_with_scratch(&mut buf, &mut scratch);
    let mut phi_min_u: Vec<f64> = buf.iter().map(|c| c.arg()).collect();
    unwrap_in_place(&mut phi_min_u);

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
    let tau0 = min_phase_bulk_delay(&phi_min_u, stride, m_fit, fs);
    crate::aelog!(
        "[TFS] Minimum-phase bulk delay held out of the blend: {:.1} samples ({:.3} ms)",
        tau0,
        tau0 * 1000.0 / fs
    );

    // H_tfs into the transform's buffer, the lower half and its mirror
    // (Hermitian, so the inverse is real).
    let half = m / 2;
    for k in 0..=half {
        let f_hz = k as f64 * fs / m as f64;
        let omega = 2.0 * PI * k as f64 / m as f64;
        let phi_ref = -omega * d;
        let w = weight_high(f_hz);
        let psi_min = phi_min_u[k] + omega * tau0;
        let phi_tfs = phi_ref + w * psi_min;
        let mag = lin_mags[k];
        buf[k] = Complex::new(mag * phi_tfs.cos(), mag * phi_tfs.sin());
    }
    drop(phi_min_u);
    buf[0] = Complex::new(buf[0].re, 0.0);
    buf[half] = Complex::new(buf[half].re, 0.0);
    for k in 1..half {
        buf[m - k] = buf[k].conj();
    }

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_string());
    }

    // IFFT → real (the real part of the forward transform of the conjugate);
    // the window of N taps from `start`.
    for c in buf.iter_mut() {
        c.im = -c.im;
    }
    fft_fwd.process_with_scratch(&mut buf, &mut scratch);
    let scale = 1.0 / m as f64;
    let out: Vec<f64> = buf[start..start + n].iter().map(|c| c.re * scale).collect();

    // Energy outside the window must be < 1e-8 of the total.
    let total_e: f64 = buf.iter().map(|c| c.re * c.re).sum();
    if total_e > 0.0 {
        let inside_e: f64 = buf[start..start + n].iter().map(|c| c.re * c.re).sum();
        let ratio = (total_e - inside_e).max(0.0) / total_e;
        if ratio >= 1e-8 {
            crate::aelog!(
                "[TFS] Warning: energy outside the window {:.2e} >= 1e-8 — filter may be truncated",
                ratio
            );
        }
    }

    // Magnitude check, after the window. Doing it on the spectrum before the
    // IFFT only restated the construction; the damage a cut does to the
    // passband is visible here or not at all.
    {
        buf.fill(zero);
        for (b, &x) in buf.iter_mut().zip(&out) {
            *b = Complex::new(x, 0.0);
        }
        fft_fwd.process_with_scratch(&mut buf, &mut scratch);
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

/// The derived blob's phase name for a source at `src_rate` played at
/// `out_rate`: `fir_<TAG>_<out_rate>_<this>.npy`.
///
/// The crossover is in hertz, so a TFS filter is derived for the rate it
/// plays at, from the pair of its factor (`filter::design_rate`). A 44.1/48
/// kHz source's pair is the output rate's own, and its name is the one it
/// always had; a hi-res source's names its factor as well — the ×4 filter at
/// 384 kHz (96 kHz up) is not the ×8 one (48 kHz up).
fn cache_phase(src_rate: u32, out_rate: u32) -> String {
    if crate::audio::converter::dsp::filter::design_rate(src_rate, out_rate) == out_rate {
        TFS_PHASE_SUFFIX.to_string()
    } else {
        format!("x{}_{}", out_rate / src_rate, TFS_PHASE_SUFFIX)
    }
}

/// Memory a derivation of a `taps`-long TFS filter holds at its peak, MB:
/// `derive_tfs`'s 64·M bytes and the two blobs it reads. A stream's linear
/// filter (`stream_linear`) is made the same way and holds the same.
pub(super) fn derive_mb(taps: usize) -> u64 {
    let m = next_pow2(2 * taps) as u64;
    ((m * 64 + taps as u64 * 16) / (1024 * 1024)).max(64)
}

/// The cached TFS-phase filter for `taps` taking a source at `src_rate` up to
/// `out_rate`, when there is one.
///
/// The suffix carries a version, and every earlier one is wrong in a way the
/// file itself cannot report: v1's treble runs D samples ahead of its bass,
/// v2 carries a 0.30 ms group-delay hump through the crossover. Both are
/// simply not found by this name, and a correct one is derived in their
/// place — the old blobs are left on disk, inert, for anyone comparing.
pub fn cached(taps: usize, src_rate: u32, out_rate: u32) -> Option<String> {
    crate::audio::converter::dsp::filter::find_blob(taps, out_rate, &cache_phase(src_rate, out_rate))
}

/// Resolve a cached TFS-phase filter or derive one from the lin+min blob pair
/// that takes a source at `src_rate` up to `out_rate`.
///
/// Returns the path of the TFS .npy on success. Returns Err when the cached
/// file is missing and the required lin/min blobs are also absent.
///
/// One at a time in this process (`one_at_a_time`).
pub fn resolve_or_derive(
    taps: usize,
    src_rate: u32,
    out_rate: u32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    one_at_a_time(|| resolve_or_derive_alone(taps, src_rate, out_rate, cancel))
}

/// The first TFS conversion after an update finds no blob of this version,
/// and a batch's workers (and the player) arrive at `resolve_or_derive`
/// together: each derived its own copy, holding the memory for it, and wrote
/// it over the others' while a worker that had already found the file was
/// reading it — the read came up short and that file failed. One at a time,
/// the first derives and saves it, and the rest, let in after it, find it.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `f` with no other TFS resolution running in this process.
fn one_at_a_time<R>(f: impl FnOnce() -> R) -> R {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    f()
}

/// `resolve_or_derive`'s work, run one at a time.
fn resolve_or_derive_alone(
    taps: usize,
    src_rate: u32,
    out_rate: u32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    use crate::audio::converter::decode::set_status;
    use crate::audio::converter::dsp::filter::{
        find_precomputed_filter, missing_filter_error, taps_label,
    };
    use crate::audio::dsp_core::load_npy_f64;
    use crate::audio::memory::{total_ram_mb, try_reserve_ram};

    // 1. Fast path: a cached TFS blob already exists (`cached`).
    if let Some(p) = cached(taps, src_rate, out_rate) {
        crate::aelog!("[TFS] Found cached TFS filter: {}", p);
        return Ok(PathBuf::from(p));
    }

    // 2. Need lin + min blobs to derive from: the pair of this factor.
    let lin_path = find_precomputed_filter(taps, src_rate, out_rate, "linear_phase")
        .ok_or_else(|| missing_filter_error(taps, src_rate, out_rate, "linear_phase"))?;
    let min_path = find_precomputed_filter(taps, src_rate, out_rate, "minimum_phase")
        .ok_or_else(|| missing_filter_error(taps, src_rate, out_rate, "minimum_phase"))?;

    crate::aelog!(
        "[TFS] No cached TFS filter — deriving from lin+min blobs (one-time)"
    );
    set_status("Deriving TFS filter (one-time)...");

    // Memory gate: the derivation's peak (`derive_tfs`, 64·M bytes) and the
    // two blobs it reads, held for as long as it runs. It waits for that much
    // to be free — for as long as it is not cancelled — and says so when this
    // machine could never hold it, rather than fail inside an allocation.
    let est_mb = derive_mb(taps);
    let _ram = loop {
        if let Some(t) = try_reserve_ram(est_mb) {
            break t;
        }
        if est_mb + 4096 > total_ram_mb() {
            return Err(format!(
                "[TFS] Deriving the {} TFS filter takes {:.1} GB of memory; this machine has {:.1} GB",
                taps_label(taps).unwrap_or("?"),
                est_mb as f64 / 1024.0,
                total_ram_mb() as f64 / 1024.0
            ));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_string());
        }
        set_status(&format!("Deriving TFS filter: waiting for {:.1} GB of free memory...", est_mb as f64 / 1024.0));
        std::thread::sleep(std::time::Duration::from_millis(1500));
    };

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
    // At the rate it plays at: the crossover is in hertz.
    let h_tfs = derive_tfs(&lin, &min_ph, out_rate, cancel)?;
    drop(lin);
    drop(min_ph);

    // Save next to the lin blob with the tfs_phase naming convention.
    let label = taps_label(taps).ok_or("[TFS] Tap count below minimum")?;
    let cache_name = format!("fir_{}_{}_{}.npy", label, out_rate, cache_phase(src_rate, out_rate));
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
    crate::audio::dsp_core::save_npy_f64(&cache_path, &h_tfs).map_err(|e| format!("[TFS] {}", e))?;
    crate::aelog!("[TFS] TFS filter cached: {}", cache_path.display());

    Ok(cache_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::{num_complex::Complex, FftPlanner};
    use std::sync::atomic::AtomicBool;

    /// Four callers arriving together for a blob none of them finds — a
    /// batch's workers on the first TFS file after an update — derive it
    /// once, the rest find it, and never two of them are inside at once.
    #[test]
    fn callers_arriving_together_derive_the_blob_once() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Barrier};
        let have = Arc::new(AtomicBool::new(false));
        let derived = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(4));
        let callers: Vec<_> = (0..4)
            .map(|_| {
                let (have, derived, inside, most, gate) =
                    (have.clone(), derived.clone(), inside.clone(), most.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    one_at_a_time(|| {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        if !have.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            derived.fetch_add(1, Ordering::SeqCst);
                            have.store(true, Ordering::SeqCst);
                        }
                        inside.fetch_sub(1, Ordering::SeqCst);
                    })
                })
            })
            .collect();
        for c in callers {
            c.join().expect("a caller");
        }
        assert_eq!(derived.load(Ordering::SeqCst), 1, "derived once");
        assert_eq!(most.load(Ordering::SeqCst), 1, "never two inside at once");
    }

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
        let got = min_phase_bulk_delay(&phi, 1, m, fs);
        assert!(
            (got - delay).abs() < 1e-9,
            "fit returned {got} for a {delay}-sample delay"
        );

        // Too few bins in the band to fit: returns 0.0 rather than a fit made
        // of two points, which leaves the blend as it was.
        assert_eq!(min_phase_bulk_delay(&phi, 1, 32, fs), 0.0);
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

    /// NPY round-trip: save_npy_f64 must produce bytes that load_npy_f64 reads back.
    #[test]
    fn npy_roundtrip() {
        let data: Vec<f64> = (0..17).map(|i| i as f64 * 0.5 - 2.0).collect();
        let dir = std::env::temp_dir();
        let path = dir.join("aura_tfs_roundtrip_test.npy");
        crate::audio::dsp_core::save_npy_f64(&path, &data).expect("write must succeed");
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
        let result = resolve_or_derive(30_000_000, 44_100, 352_800, &cancel);
        assert!(result.is_err(), "must Err when blobs are missing");
    }

    /// A 44.1/48 kHz source's TFS filter keeps the name it always had, so the
    /// filters already derived are found again; a hi-res source's names its
    /// factor, so the ×4 filter at 384 kHz is never taken for the ×8 one.
    #[test]
    fn the_cached_name_carries_a_hi_res_factor() {
        assert_eq!(cache_phase(44_100, 352_800), "tfs_phase_v4");
        assert_eq!(cache_phase(48_000, 768_000), "tfs_phase_v4");
        assert_eq!(cache_phase(96_000, 384_000), "x4_tfs_phase_v4");
        assert_eq!(cache_phase(192_000, 384_000), "x2_tfs_phase_v4");
        assert_eq!(cache_phase(88_200, 705_600), "x8_tfs_phase_v4");
    }

    /// 30 ms at every rate, a whole number of frames — or the whole half of a
    /// filter shorter than that.
    #[test]
    fn the_look_ahead_is_30_ms_or_half_the_filter() {
        assert_eq!(look_ahead(1_000_000, 352_800), 10_584);
        assert_eq!(look_ahead(30_000_000, 384_000), 11_520);
        assert_eq!(look_ahead(10_000_000, 768_000), 23_040);
        assert_eq!(look_ahead(5_000_000, 88_200), 2_646);
        assert_eq!(look_ahead(5_000, 88_200), 2_499);
        assert_eq!(look_ahead(5_000, 384_000), 2_499);
    }

    /// sinc × Kaiser β = 14 with the cutoff at `fc` cycles per sample, as the
    /// long blobs are made.
    fn kaiser_sinc(n: usize, fc: f64) -> Vec<f64> {
        let w = crate::player::analytics::spectra::kaiser(n, 14.0);
        let mid = (n - 1) as f64 / 2.0;
        (0..n)
            .map(|i| {
                let x = i as f64 - mid;
                let s = if x.abs() < 1e-12 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
                s * w[i]
            })
            .collect()
    }

    /// The TFS response as it was meant, made on a transform `m` long (the
    /// whole circle, unwindowed), τ0 fitted on next_pow2(N) as `derive_tfs`
    /// fits it. An independent copy of the construction, for the tests.
    fn intended(lin: &[f64], min_ph: &[f64], fs: f64, m: usize) -> Vec<f64> {
        let n = lin.len();
        let mut planner = FftPlanner::<f64>::new();
        let spec = |h: &[f64]| {
            let mut b: Vec<Complex<f64>> =
                h.iter().map(|&x| Complex::new(x, 0.0)).chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n)).collect();
            FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut b);
            b
        };
        let mags: Vec<f64> = spec(lin).iter().map(|c| c.norm()).collect();
        let phi = unwrap_phase(&spec(min_ph).iter().map(|c| c.arg()).collect::<Vec<_>>());
        let m_fit = next_pow2(n);
        let tau0 = min_phase_bulk_delay(&phi, m / m_fit, m_fit, fs);
        let d = (n - 1) as f64 / 2.0;
        let mut h: Vec<Complex<f64>> = (0..m)
            .map(|k| {
                let kk = if k <= m / 2 { k } else { 0 };
                let omega = 2.0 * PI * kk as f64 / m as f64;
                let ph = -omega * d + weight_high(kk as f64 * fs / m as f64) * (phi[kk] + omega * tau0);
                Complex::new(mags[kk] * ph.cos(), mags[kk] * ph.sin())
            })
            .collect();
        h[0] = Complex::new(h[0].re, 0.0);
        h[m / 2] = Complex::new(h[m / 2].re, 0.0);
        for k in 1..m / 2 {
            h[m - k] = h[k].conj();
        }
        planner.plan_fft_inverse(m).process(&mut h);
        h.iter().map(|c| c.re / m as f64).collect()
    }

    /// The largest |difference| of two responses in `lo..hi` Hz, dB re the
    /// peak of `a`'s magnitude.
    fn worst_db(a: &[f64], b: &[f64], fs: f64, lo: f64, hi: f64) -> f64 {
        let m = next_pow2(a.len() * 2);
        let spec = |h: &[f64]| {
            let mut v: Vec<Complex<f64>> =
                h.iter().map(|&x| Complex::new(x, 0.0)).chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h.len())).collect();
            FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut v);
            v
        };
        let sa = spec(a);
        let diff: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
        let sd = spec(&diff);
        let peak = sa[..m / 2].iter().map(|c| c.norm()).fold(0.0f64, f64::max);
        let (k0, k1) = ((lo * m as f64 / fs) as usize, (hi * m as f64 / fs) as usize);
        let worst = sd[k0..=k1].iter().map(|c| c.norm()).fold(0.0f64, f64::max);
        20.0 * (worst / peak).max(1e-300).log10()
    }

    /// v4 is the filter that was meant: its window of the response made on a
    /// transform twice as long matches the one made on sixteen times, to
    /// below −140 dB in 20 Hz – 20 kHz, and the response ahead of its window
    /// is below −140 dB. v3 — the transform next_pow2(N) long, cut to its
    /// first N — was not: on N a power of two its ringing after the centre
    /// wrapped round over the whole filter.
    #[test]
    fn v4_is_the_filter_that_was_meant_and_v3_was_not() {
        let n = 65_536usize;
        let fs = 352_800.0;
        let lin = kaiser_sinc(n, 21_050.0 / fs);
        let min_ph = make_min_phase_cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let v4 = derive_tfs(&lin, &min_ph, fs as u32, &cancel).expect("derive_tfs");

        let k = look_ahead(n, fs as u32);
        let start = (n - 1) / 2 - k;
        let full = intended(&lin, &min_ph, fs, 16 * next_pow2(n));
        let meant = &full[start..start + n];
        let v4_err = worst_db(meant, &v4, fs, 20.0, 20_000.0);
        assert!(v4_err < -140.0, "v4 against the filter meant: {v4_err:.1} dB in band");

        let total: f64 = full.iter().map(|x| x * x).sum();
        let ahead: f64 = full[..start].iter().map(|x| x * x).sum();
        let ahead_db = 10.0 * (ahead / total).log10();
        assert!(ahead_db < -140.0, "ahead of the window: {ahead_db:.1} dB");

        // v3: the same construction on next_pow2(N), its first N taps, against
        // the same taps of the filter meant.
        let v3 = intended(&lin, &min_ph, fs, next_pow2(n));
        let v3_err = worst_db(&full[..n], &v3[..n], fs, 20.0, 20_000.0);
        println!("in band against the filter meant: v4 {v4_err:.1} dB, v3 {v3_err:.1} dB; ahead of v4's window {ahead_db:.1} dB");
        assert!(v3_err > v4_err + 20.0, "v3 should show its wrap: {v3_err:.1} dB against v4's {v4_err:.1} dB");
    }

    /// The measure for the report: a 30M v4 derivation from the shipped pair
    /// — its time, the process's peak working set while it runs, and the
    /// memory gate's estimate. `AURA_FILTER_DIR` names the blobs; nothing is
    /// written. `cargo test --profile fast --bins v4_30m_measure -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn v4_30m_measure() {
        use crate::audio::converter::dsp::filter::find_blob;
        use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, SystemExt};
        let (taps, rate) = (30_000_000usize, 352_800u32);
        let (Some(lp), Some(mp)) = (find_blob(taps, rate, "linear_phase"), find_blob(taps, rate, "minimum_phase")) else {
            println!("v4_30m_measure: set AURA_FILTER_DIR");
            return;
        };
        let lin = crate::audio::dsp_core::load_npy_f64(&lp).expect("lin");
        let min_ph = crate::audio::dsp_core::load_npy_f64(&mp).expect("min");
        let pid = sysinfo::Pid::from_u32(std::process::id());
        // The process list is read once; then only this process, each tick.
        let mut sys = sysinfo::System::new();
        sys.refresh_processes_specifics(ProcessRefreshKind::new());
        let mut ws = move || {
            sys.refresh_process(pid);
            sys.process(pid).map(|p| p.memory()).unwrap_or(0)
        };
        let before = ws();
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let watch = {
            let done = done.clone();
            std::thread::spawn(move || {
                let mut peak = 0u64;
                while !done.load(Ordering::Relaxed) {
                    peak = peak.max(ws());
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                peak.max(ws())
            })
        };
        let t = std::time::Instant::now();
        let h = derive_tfs(&lin, &min_ph, rate, &AtomicBool::new(false)).expect("derive_tfs");
        let secs = t.elapsed().as_secs_f64();
        done.store(true, Ordering::Relaxed);
        let peak = watch.join().unwrap();
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        println!(
            "v4_30m_measure: 30M at {} Hz derived in {:.1} s; working set {:.2} GiB with the pair loaded, peak {:.2} GiB (+{:.2} GiB); the gate reserves {:.2} GiB with the pair",
            rate,
            secs,
            gib(before),
            gib(peak),
            gib(peak.saturating_sub(before)),
            derive_mb(taps) as f64 / 1024.0
        );
        assert_eq!(h.len(), taps);
    }

    /// The worst of `h`'s stopband (22.05 kHz up at `fs`), dB re its peak.
    fn stopband_db(h: &[f64], fs: f64) -> f64 {
        let m = next_pow2(h.len() * 4);
        let mut v: Vec<Complex<f64>> =
            h.iter().map(|&x| Complex::new(x, 0.0)).chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h.len())).collect();
        FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut v);
        let peak = v[..m / 2].iter().map(|c| c.norm()).fold(0.0f64, f64::max);
        let k0 = (22_050.0 * m as f64 / fs) as usize;
        let worst = v[k0..m / 2].iter().map(|c| c.norm()).fold(0.0f64, f64::max);
        20.0 * (worst / peak).log10()
    }

    /// The stopband v3 lost to its wrap is back. (How close it comes to the
    /// linear filter's own is a property of the minimum-phase blob, which
    /// this fixture only approximates: the shipped pair is held to that in
    /// `the_shipped_1m_v4_keeps_the_linear_stopband`.)
    #[test]
    fn v4_mends_the_stopband_v3_lost() {
        let n = 65_536usize;
        let fs = 352_800.0;
        let lin = kaiser_sinc(n, 21_050.0 / fs);
        let min_ph = make_min_phase_cepstral(&lin);
        let cancel = AtomicBool::new(false);
        let v4 = derive_tfs(&lin, &min_ph, fs as u32, &cancel).expect("derive_tfs");
        let v3 = intended(&lin, &min_ph, fs, next_pow2(n));
        let (s_lin, s_v4, s_v3) = (stopband_db(&lin, fs), stopband_db(&v4, fs), stopband_db(&v3[..n], fs));
        println!("stopband: linear {s_lin:.1} dB, v4 {s_v4:.1} dB, v3 {s_v3:.1} dB");
        assert!(s_v4 < -130.0, "v4's stopband {s_v4:.1} dB");
        assert!(s_v4 < s_v3 - 20.0, "v4 {s_v4:.1} dB against v3's {s_v3:.1} dB");
    }

    /// The shipped 1M pair at 352.8 kHz: v4's stopband is the linear
    /// filter's, to within 10 dB. Runs with `AURA_FILTER_DIR`; writes nothing.
    #[test]
    fn the_shipped_1m_v4_keeps_the_linear_stopband() {
        use crate::audio::converter::dsp::filter::find_blob;
        if std::env::var_os("AURA_FILTER_DIR").is_none() {
            println!("the_shipped_1m_v4_keeps_the_linear_stopband: set AURA_FILTER_DIR to run");
            return;
        }
        let rate = 352_800u32;
        let (Some(lp), Some(mp)) = (find_blob(1_000_000, rate, "linear_phase"), find_blob(1_000_000, rate, "minimum_phase")) else {
            println!("the 1M pair at {rate} Hz is not installed — skip");
            return;
        };
        let lin = crate::audio::dsp_core::load_npy_f64(&lp).expect("lin");
        let min_ph = crate::audio::dsp_core::load_npy_f64(&mp).expect("min");
        let v4 = derive_tfs(&lin, &min_ph, rate, &AtomicBool::new(false)).expect("derive_tfs");
        let (s_lin, s_v4) = (stopband_db(&lin, rate as f64), stopband_db(&v4, rate as f64));
        println!("shipped 1M at {rate} Hz, stopband: linear {s_lin:.1} dB, v4 {s_v4:.1} dB");
        assert!(s_v4 < s_lin + 10.0, "stopband: v4 {s_v4:.1} dB, linear {s_lin:.1} dB");
    }

    /// τ0 is fitted on v3's grid whatever the transform: v4's, made on a
    /// transform twice as long, is v3's to rounding.
    #[test]
    fn tau0_is_fitted_on_v3s_grid() {
        let n = 4097usize;
        let fs = 352_800.0;
        let lin = kaiser_sinc(n, 21_050.0 / fs);
        let min_ph = make_min_phase_cepstral(&lin);
        let phase_on = |m: usize| {
            let mut b: Vec<Complex<f64>> = min_ph
                .iter()
                .map(|&x| Complex::new(x, 0.0))
                .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - n))
                .collect();
            FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut b);
            unwrap_phase(&b.iter().map(|c| c.arg()).collect::<Vec<_>>())
        };
        let m3 = next_pow2(n);
        let m4 = next_pow2(2 * n);
        let v3 = min_phase_bulk_delay(&phase_on(m3), 1, m3, fs);
        let v4 = min_phase_bulk_delay(&phase_on(m4), m4 / m3, m3, fs);
        assert!((v4 - v3).abs() < 1e-9, "τ0: v3 {v3}, v4 {v4}");
        // All the bins of the finer grid would have moved it.
        let all = min_phase_bulk_delay(&phase_on(m4), 1, m4, fs);
        assert!((all - v3).abs() > (v4 - v3).abs(), "τ0 on every bin: {all}, v3 {v3}");
    }

    /// The crossover is in hertz at the rate a filter plays at. A hi-res TFS
    /// filter is the pair of its factor derived at the output rate
    /// (`resolve_or_derive`); derived at the factor's own rate — 192 kHz for
    /// 96 → 384 kHz — its crossover would play an octave up, at 3–8 kHz.
    #[test]
    fn the_crossover_is_placed_at_the_rate_the_filter_plays_at() {
        let n = 4097usize;
        let out_rate = 384_000u32;
        let fs = out_rate as f64;
        // Linear phase: a pure delay of D. Minimum phase: a pair of zeros just
        // inside the unit circle at 3 kHz, whose phase turns fast through the
        // crossover: tens of samples of group delay where it is blended in,
        // against about a tenth of a sample where it is not (the tail cut at
        // N).
        let mut lin = vec![0.0; n];
        lin[(n - 1) / 2] = 1.0;
        let (r, w0) = (0.99f64, 2.0 * PI * 3_000.0 / fs);
        let mut min_ph = vec![0.0; n];
        min_ph[..3].copy_from_slice(&[1.0, -2.0 * r * w0.cos(), r * r]);
        let cancel = AtomicBool::new(false);
        let at_out = derive_tfs(&lin, &min_ph, out_rate, &cancel).expect("derive_tfs");
        let at_half = derive_tfs(&lin, &min_ph, out_rate / 2, &cancel).expect("derive_tfs");

        // Group delay added over the linear-phase filter, in samples, at `f`
        // hertz where the filter plays.
        let m = next_pow2(n);
        let spec = |h: &[f64]| {
            let mut buf: Vec<Complex<f64>> = h
                .iter()
                .map(|&x| Complex::new(x, 0.0))
                .chain(std::iter::repeat(Complex::new(0.0, 0.0)).take(m - h.len()))
                .collect();
            FftPlanner::<f64>::new().plan_fft_forward(m).process(&mut buf);
            buf
        };
        let s_lin = spec(&lin);
        let added = |h: &[f64], f: f64| {
            let s = spec(h);
            let angles: Vec<f64> = (0..m / 2).map(|k| (s[k] / s_lin[k]).arg()).collect();
            let phase = unwrap_phase(&angles);
            let k = (f * m as f64 / fs).round() as usize;
            -(phase[k + 1] - phase[k]) * m as f64 / (2.0 * PI)
        };
        // Below 1.5 kHz the response is linear phase: nothing added.
        let low = added(&at_out, 1_400.0);
        assert!(low.abs() < 0.5, "at 1.4 kHz: {low}");
        // At 2.9 kHz the blend is past half way...
        let mid = added(&at_out, 2_900.0);
        assert!(mid.abs() > 5.0, "nothing blended in at 2.9 kHz: {mid}");
        // ...where the one derived at half the rate is still linear phase.
        let late = added(&at_half, 2_900.0);
        assert!(late.abs() < 0.5, "derived at half the rate, 2.9 kHz carries {late}");
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
