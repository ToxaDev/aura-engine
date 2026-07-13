use crate::audio::converter::decode::set_status;
use crate::audio::gpu::GpuDspProcessor;
use crate::audio::processor::DspProcessor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Check whether the current operation should abort:
/// either the specific file was cancelled (X button) or a global cancel was requested.
#[inline]
pub fn file_or_global_cancelled(file_cancel: &AtomicBool) -> bool {
    file_cancel.load(Ordering::Relaxed) || crate::audio::cancel_flag::check()
}
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let x2 = (x / 2.0).powi(2);
    for k in 1..50 {
        term *= x2 / (k as f64).powi(2);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}
/// Generate apodizing pre-filter coefficients.
/// This is a short FIR that gently rolls off frequencies near Nyquist
/// to suppress pre-ringing artifacts baked into CD recordings by ADC brick-wall filters.
fn generate_apodizing_coeffs(source_rate: u32, strength: u32) -> Vec<f64> {
    // Apodizing filter parameters based on strength
    let (rolloff_start_hz, taps) = match strength {
        1 => (20000.0, 2048),  // Gentle: rolloff from 20kHz, preserves most air
        2 => (19000.0, 4096),  // Moderate: rolloff from 19kHz, good balance
        3 => (18000.0, 4096),  // Strong: rolloff from 18kHz, maximum pre-ring suppression
        _ => return vec![1.0], // Off — passthrough
    };

    let nyquist = source_rate as f64 / 2.0;
    let fc_norm = (rolloff_start_hz / nyquist).min(0.99);

    // Kaiser-windowed sinc lowpass — extreme stopband rejection > 200 dB
    let half = taps / 2;
    let beta = 24.0;
    let i0_beta = bessel_i0(beta);
    let mut h: Vec<f64> = Vec::with_capacity(taps);
    let mut sum = 0.0;

    for i in 0..taps {
        let n = i as f64 - half as f64;
        // Sinc
        let sinc = if n.abs() < 1e-10 {
            fc_norm
        } else {
            (std::f64::consts::PI * fc_norm * n).sin() / (std::f64::consts::PI * n)
        };
        // Kaiser window
        let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
        let kaiser_arg = beta * (1.0 - arg * arg).max(0.0).sqrt();
        let kaiser = bessel_i0(kaiser_arg) / i0_beta;
        let val = sinc * kaiser;
        h.push(val);
        sum += val;
    }

    // Normalize
    if sum.abs() > 1e-15 {
        for v in h.iter_mut() {
            *v /= sum;
        }
    }

    crate::aelog!(
        "[CONV] Apodizing pre-filter: {} taps, rolloff {:.0}Hz, strength={}",
        taps, rolloff_start_hz, strength
    );

    crate::audio::dsp_core::to_minimum_phase(&h)
}
// Minimum taps for GPU apodizing to justify device setup overhead (~200ms).
// Smaller filters process faster on CPU FFT+Rayon even with use_gpu=true.
const GPU_APOD_MIN_TAPS: usize = 65_536;

/// Phase characteristic of a FIR filter.  Used by `fft_convolve_ola` to
/// pick the correct trim offset (centre for linear, head for minimum).
///
/// We use an enum instead of a boolean so a future contributor adding a
/// linear-phase apodizer cannot forget the flag — the type system makes
/// the choice explicit at every call site. (Old `is_min_phase: bool` API
/// left this as a reviewer's hope.)
///
/// `Linear` is currently only constructed in tests; production callers all
/// use `Minimum`. The `#[allow(dead_code)]` is intentional — keeping the
/// variant guarantees the API is ready for a hypothetical linear-phase
/// apodizer without re-introducing the original A.1 trim bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterPhase {
    /// Symmetric impulse response. Group delay = (h_len − 1) / 2 samples.
    #[allow(dead_code)]
    Linear,
    /// Energy front-loaded, group delay ≈ 0 in the passband. Output starts
    /// at sample 0.
    Minimum,
}

// ─────────────────────────────────────────────────────────
// FFT-based Overlap-Add convolution  (CPU + Rayon)
// O(n · log h)  instead of O(n · h)  — up to 1 000× faster
// for typical apodizing filters (4 096–8 192 taps, 5-min tracks)
//
// PRIOR BUG: this function used to ALWAYS centre-trim with offset=h_len/2,
// regardless of phase type. Apodizer filters are minimum-phase
// (see generate_apodizing_coeffs → to_minimum_phase), which has
// group delay ≈ 0, so centred trim discarded the first h_len/2 samples
// of real audio (~46 ms @ 4096 taps @ 44.1 kHz) and shifted the entire
// track forward by that amount. Fixed by routing through `phase`.
// ─────────────────────────────────────────────────────────
fn fft_convolve_ola(
    samples_l: &[f64],
    samples_r: &[f64],
    coeffs: &[f64],
    phase: FilterPhase,
    file_cancel: &AtomicBool,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    use rayon::prelude::*;
    use rustfft::num_complex::Complex;
    use rustfft::FftPlanner;

    let h_len = coeffs.len();
    if h_len == 0 {
        return Ok((samples_l.to_vec(), samples_r.to_vec()));
    }
    let n_input = samples_l.len();

    // Block size B = next power-of-2 ≥ h_len (ensures no circular aliasing)
    let b_size = h_len.next_power_of_two().max(512);
    let n_fft = b_size * 2; // OLA FFT length = 2×B guarantees B-sample overlap region

    // ─── Pre-compute H[ω] once on the calling thread ───
    let mut h_buf: Vec<Complex<f64>> = {
        let mut v = vec![Complex { re: 0.0, im: 0.0 }; n_fft];
        for (i, &c) in coeffs.iter().enumerate() {
            v[i].re = c;
        }
        v
    };
    {
        let mut pl = FftPlanner::<f64>::new();
        pl.plan_fft_forward(n_fft).process(&mut h_buf);
    }
    let h_freq = Arc::new(h_buf);

    let num_blocks = (n_input + b_size - 1) / b_size;

    let mut root_pl = FftPlanner::<f64>::new();
    let root_fwd = root_pl.plan_fft_forward(n_fft);
    let root_inv = root_pl.plan_fft_inverse(n_fft);

    // ─── Rayon-parallel block FFTs ───
    // Each thread: forward FFT(block) × H → IFFT  (independent per block)
    let blocks: Vec<Option<(Vec<f64>, Vec<f64>)>> = (0..num_blocks)
        .into_par_iter()
        .map(|block_idx| {
            if file_cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut bl = vec![Complex { re: 0.0, im: 0.0 }; n_fft];
            let mut br = vec![Complex { re: 0.0, im: 0.0 }; n_fft];

            let start = block_idx * b_size;
            let end = (start + b_size).min(n_input);
            if end > start {
                for i in 0..(end - start) {
                    bl[i].re = samples_l[start + i];
                    br[i].re = samples_r[start + i];
                }
            }

            root_fwd.process(&mut bl);
            root_fwd.process(&mut br);

            // Pointwise ×H[ω]
            let h = &*h_freq;
            for i in 0..n_fft {
                bl[i] *= h[i];
                br[i] *= h[i];
            }

            root_inv.process(&mut bl);
            root_inv.process(&mut br);

            let s = 1.0 / n_fft as f64;
            Some((
                bl.iter().map(|c| c.re * s).collect::<Vec<_>>(),
                br.iter().map(|c| c.re * s).collect::<Vec<_>>(),
            ))
        })
        .collect();

    if blocks.iter().any(|r| r.is_none()) {
        return Err("Cancelled".to_string());
    }

    // ─── Sequential overlap-add accumulation ───
    let out_len = n_input + h_len - 1;
    let mut out_l = vec![0.0f64; out_len];
    let mut out_r = vec![0.0f64; out_len];

    for (b, res) in blocks.iter().enumerate() {
        if let Some((bl, br)) = res {
            let start = b * b_size;
            let avail = n_fft.min(out_len.saturating_sub(start));
            for i in 0..avail {
                out_l[start + i] += bl[i];
                out_r[start + i] += br[i];
            }
        }
    }

    // Trim depending on phase type:
    //   Minimum: take first n_input samples (group delay ≈ 0)
    //   Linear : centre-align by offsetting h_len/2
    let off = match phase {
        FilterPhase::Minimum => 0,
        FilterPhase::Linear => h_len / 2,
    };
    Ok((
        out_l[off..off + n_input].to_vec(),
        out_r[off..off + n_input].to_vec(),
    ))
}
// Smaller filters process faster on CPU FFT+Rayon even with use_gpu=true.

// ─────────────────────────────────────────────────────────
// GPU apodizing  (only for large filters ≥ GPU_APOD_MIN_TAPS)
// Uses the same GpuDspProcessor OLA pipeline as the main FIR
// ─────────────────────────────────────────────────────────
fn apodize_gpu(
    samples_l: &mut Vec<f64>,
    samples_r: &mut Vec<f64>,
    coeffs: &[f64],
    precision: u32,
    file_cancel: &AtomicBool,
) -> Result<(), String> {
    set_status("Apodizing (GPU OLA)...");
    let taps = coeffs.len();
    let mut gpu = GpuDspProcessor::new_with_coefficients(coeffs, precision)?;

    let n = samples_l.len();
    let chunk = 32_768usize;

    // Algorithmic OLA latency, queried from the processor (1×b_size on GPU).
    // After processing real input we must flush zeros to drain the delay
    // line; otherwise the last b_size samples of audio never reach the
    // output. We then trim the leading latency samples.
    // Apodizer coefficients are minimum-phase ⇒ no extra group-delay trim.
    let b_size: usize = gpu.output_latency();
    let flush_samples = b_size + taps; // extra zeros to fully drain
    let total_out_capacity = n + flush_samples;
    let mut out_l = vec![0.0f64; total_out_capacity];
    let mut out_r = vec![0.0f64; total_out_capacity];

    // Pass 1: real input
    let mut pos = 0;
    while pos < n {
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".into());
        }
        let c = (n - pos).min(chunk);
        gpu.process_audio(
            &samples_l[pos..pos + c],
            &samples_r[pos..pos + c],
            &mut out_l[pos..pos + c],
            &mut out_r[pos..pos + c],
            c,
        );
        pos += c;
    }

    // Pass 2: flush zeros to drain the OLA delay line
    let zero_l = vec![0.0f64; chunk];
    let zero_r = vec![0.0f64; chunk];
    let mut flush_pos = 0usize;
    while flush_pos < flush_samples {
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".into());
        }
        let c = (flush_samples - flush_pos).min(chunk);
        let real_pos = n + flush_pos;
        gpu.process_audio(
            &zero_l[..c],
            &zero_r[..c],
            &mut out_l[real_pos..real_pos + c],
            &mut out_r[real_pos..real_pos + c],
            c,
        );
        flush_pos += c;
    }

    // Trim OLA leading latency. Apodizer is minimum-phase (group delay ≈ 0)
    // so we ONLY remove the b_size algorithmic latency, not (taps-1)/2.
    out_l.drain(..b_size);
    out_r.drain(..b_size);
    // Truncate back to original audio length
    out_l.truncate(n);
    out_r.truncate(n);

    *samples_l = out_l;
    *samples_r = out_r;
    crate::aelog!(
        "[CONV] Apodizing (GPU OLA): {} taps on {} samples done (trim={}, flush={})",
        taps, n, b_size, flush_samples
    );
    Ok(())
}
/// Apply apodizing pre-filter — dispatches to GPU OLA or CPU FFT+Rayon.
pub fn apply_apodizing(
    samples_l: &mut Vec<f64>,
    samples_r: &mut Vec<f64>,
    source_rate: u32,
    strength: u32,
    use_gpu: bool,
    precision: u32,
    file_cancel: &AtomicBool,
) -> Result<(), String> {
    if strength == 0 {
        return Ok(());
    }
    if source_rate > 48000 {
        crate::aelog!(
            "[CONV] Apodizing skipped: Hi-Res source ({}Hz)",
            source_rate
        );
        return Ok(());
    }
    let coeffs = generate_apodizing_coeffs(source_rate, strength);
    if coeffs.len() <= 1 {
        return Ok(());
    }

    set_status("Applying apodizing pre-filter (FFT+Rayon)...");
    apply_apodizing_coeffs(
        samples_l,
        samples_r,
        &coeffs,
        use_gpu,
        precision,
        file_cancel,
    )
}
/// Apply an already-computed apodizing coefficient set — GPU or CPU FFT.
fn apply_apodizing_coeffs(
    samples_l: &mut Vec<f64>,
    samples_r: &mut Vec<f64>,
    coeffs: &[f64],
    use_gpu: bool,
    precision: u32,
    file_cancel: &AtomicBool,
) -> Result<(), String> {
    let h = coeffs.len();

    if use_gpu && h >= GPU_APOD_MIN_TAPS {
        // GPU OLA: only beneficial for large filters (setup cost is ~200ms)
        crate::aelog!("[CONV] Apodizing via GPU OLA ({} taps)", h);
        apodize_gpu(samples_l, samples_r, coeffs, precision, file_cancel)
    } else {
        // CPU FFT+Rayon: optimal for short-to-medium filters (typical apodizing)
        if use_gpu && h < GPU_APOD_MIN_TAPS {
            crate::aelog!("[CONV] Apodizing via CPU FFT+Rayon ({} taps < {} threshold, GPU overhead not justified)",
                h, GPU_APOD_MIN_TAPS);
        }
        // Apodizer coefficients are minimum-phase (see generate_apodizing_coeffs → to_minimum_phase)
        // → FilterPhase::Minimum so the trim doesn't discard the first h_len/2 samples of audio.
        let (ol, or_) = fft_convolve_ola(
            samples_l, samples_r, coeffs, FilterPhase::Minimum, file_cancel)?;
        if file_or_global_cancelled(file_cancel) {
            return Err("Cancelled".into());
        }
        *samples_l = ol;
        *samples_r = or_;
        crate::aelog!(
            "[CONV] Apodizing applied: {} taps on {} samples (CPU FFT OLA + Rayon)",
            h,
            samples_l.len()
        );
        Ok(())
    }
}
// ════════════════════════════════════════════════════════════════════
// Adaptive Apodizer v3 — source forensics
// ════════════════════════════════════════════════════════════════════
//
// v2 answered one question — "do attacks carry near-Nyquist pre-ringing?" —
// and mapped the answer to one of three preset cutoffs. v3 keeps that
// time-domain detector verbatim (it survived real-world tuning) but turns
// guessing into measurement:
//
//   1. A Welch long-term spectrum feeds a spectral-cliff detector. A
//      brick-wall SRC/ADC leaves a signature no natural source has: tens
//      of dB dropped within a fraction of an octave, with only a noise
//      floor above. A cliff well below the container Nyquist unmasks
//      "fake hi-res" (44.1/48k masters upsampled into 88.2k+ containers),
//      which v2 skipped entirely as Hi-Res.
//   2. The pre-ring detector runs against the EFFECTIVE Nyquist —
//      cliff-derived for fake hi-res, container Nyquist otherwise — so
//      the same physics works inside any container rate.
//   3. The dominant frequency of each pre-ring burst is estimated (the
//      ring oscillates at the source filter's transition frequency), so
//      the apodizer lands just below the measured frequency instead of
//      one of three buckets.
//   4. Severity (median ring-over-background, dB) selects filter depth:
//      mild ringing gets a lighter filter (β=14) whose own time-domain
//      signature is shorter; strong ringing gets the full β=24.
//   5. Low-transient material (ambient, legato strings) that v2 could not
//      judge is now handled by the spectral evidence alone — gently, and
//      never inside a true hi-res container's own ADC band.
//   6. A mirror-image probe compares the spectral SHAPE just below each
//      candidate legacy Nyquist with the mirrored band above it, segment
//      by segment: a bad upstream SRC leaves images that correlate
//      bin-for-bin (a tone at f has a twin at 2·Ny−f); honest hi-res
//      content never does. This catches badly-upsampled fake hi-res whose
//      images defeat the cliff floor-check, and justifies cutting below
//      the original Nyquist regardless of the pre-ring verdict.
//
// Verdicts stay deliberately conservative. No cliff and no ringing →
// untouched (static preset fallback). Cliff WITHOUT pre-ring on
// transient-rich material means the source filter is minimum-phase or the
// master was already apodized — logged as a diagnosis, audio untouched:
// post-ring cannot be measured reliably (it hides inside each attack's own
// HF decay), but this indirect verdict follows from measurements we trust.

/// Spectral-cliff evidence from the Welch long-term spectrum.
#[derive(Clone, Copy, Debug)]
pub struct SpectralCliff {
    /// Center of the steepest 1/12-octave drop, Hz.
    pub freq_hz: f64,
    /// Level drop across that 1/12 octave, dB.
    pub drop_db: f64,
}

/// Mirror-image aliasing evidence from a bad upstream SRC.
#[derive(Clone, Copy, Debug)]
pub struct AliasEvidence {
    /// The legacy Nyquist the images mirror around, Hz.
    pub origin_nyquist_hz: f64,
    /// Mean per-segment spectral shape correlation (0..1) between the band
    /// below that Nyquist and the mirrored band above it.
    pub correlation: f64,
}

/// Everything the analyzers measured about one source file.
#[derive(Clone, Debug)]
pub struct SourceAnalysis {
    /// Nyquist of the ORIGINAL material: cliff-derived for fake hi-res,
    /// container Nyquist otherwise.
    pub effective_nyquist_hz: f64,
    /// Legacy rate whose Nyquist matches the cliff (fake-hi-res verdict).
    pub suspected_origin_rate: Option<u32>,
    pub cliff: Option<SpectralCliff>,
    /// Attacks that carried enough HF to be judged at all.
    pub attacks_analyzed: usize,
    /// Fraction of judged attacks with pre-ring.
    pub ring_fraction: f64,
    /// Median pre-ring over local HF background, dB.
    pub ring_severity_db: f64,
    /// Median dominant frequency of the pre-ring bursts, Hz.
    pub ring_freq_hz: Option<f64>,
    /// Mirror-image aliasing from a bad upstream SRC (hi-res containers).
    pub alias_probe: Option<AliasEvidence>,
}

/// Concrete apodizer parameters chosen by `decide_apodizer`.
#[derive(Clone, Debug)]
pub struct ApodizerPlan {
    pub fc_hz: f64,
    pub taps: usize,
    pub beta: f64,
    /// Human-readable verdict for the log.
    pub reason: String,
}

/// Welch-averaged long-term power spectrum in dB (arbitrary reference),
/// smoothed with a ~120 Hz moving average. Returns (psd_db, bin_hz).
/// At most ~128 segments, spread evenly across the whole track so intros
/// and fades cannot dominate.
fn welch_ltas(
    mono: &[f64],
    sample_rate: u32,
    file_cancel: &AtomicBool,
) -> Option<(Vec<f64>, f64)> {
    use rustfft::{num_complex::Complex, FftPlanner};
    const NSEG: usize = 16_384;
    let n = mono.len();
    if n < NSEG * 3 {
        return None;
    }
    let hop = NSEG / 2;
    let total_segs = (n - NSEG) / hop + 1;
    let stride = (total_segs + 127) / 128;
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(NSEG);
    let hann: Vec<f64> = (0..NSEG)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (NSEG as f64 - 1.0)).cos())
        .collect();
    let mut acc = vec![0.0f64; NSEG / 2];
    let mut count = 0usize;
    let mut buf = vec![Complex { re: 0.0, im: 0.0 }; NSEG];
    let mut seg = 0usize;
    while seg < total_segs {
        if count % 16 == 0 && file_or_global_cancelled(file_cancel) {
            return None;
        }
        let s = seg * hop;
        for i in 0..NSEG {
            buf[i].re = mono[s + i] * hann[i];
            buf[i].im = 0.0;
        }
        fft.process(&mut buf);
        for k in 0..NSEG / 2 {
            acc[k] += buf[k].norm_sqr();
        }
        count += 1;
        seg += stride;
    }
    if count == 0 {
        return None;
    }
    let bin_hz = sample_rate as f64 / NSEG as f64;
    let db: Vec<f64> = acc
        .iter()
        .map(|&p| 10.0 * (p / count as f64 + 1e-30).log10())
        .collect();
    // ~120 Hz moving average via prefix sums (odd width, ≥5 bins).
    let w = (((120.0 / bin_hz).round() as usize).max(5)) | 1;
    let half = w / 2;
    let m = db.len();
    let mut prefix = vec![0.0f64; m + 1];
    for i in 0..m {
        prefix[i + 1] = prefix[i] + db[i];
    }
    let sm: Vec<f64> = (0..m)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(m);
            (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect();
    Some((sm, bin_hz))
}

/// Find a brick-wall spectral cliff: the steepest drop across a 1/12-octave
/// window between max(8 kHz, 0.2×Nyquist) and 0.995×Nyquist.
///
/// A cliff needs ≥20 dB inside 1/12 octave (≥240 dB/oct sustained — far
/// beyond any natural rolloff). For cliffs well below Nyquist the region
/// above must be a floor (≥25 dB below the passband shoulder), which
/// protects steep-but-natural spectra that keep real energy above the knee.
/// Cliffs at ≥0.90×Nyquist skip that check — there is no meaningful "above".
pub fn analyze_spectral_cliff(
    mono: &[f64],
    sample_rate: u32,
    file_cancel: &AtomicBool,
) -> Option<SpectralCliff> {
    let (psd, bin_hz) = welch_ltas(mono, sample_rate, file_cancel)?;
    let ny = sample_rate as f64 / 2.0;
    let m = psd.len();
    let f_min = (0.20 * ny).max(8_000.0);
    let f_max = 0.995 * ny;
    let bi = |x: f64| -> usize { ((x / bin_hz).round() as usize).min(m - 1) };

    let mut best: Option<(f64, f64)> = None; // (freq, drop)
    let mut f = f_min;
    while f < f_max {
        let drop = psd[bi(f * (2.0f64).powf(-1.0 / 24.0))] - psd[bi(f * (2.0f64).powf(1.0 / 24.0))];
        if drop >= 20.0 && best.map_or(true, |(_, d)| drop > d) {
            best = Some((f, drop));
        }
        f += bin_hz.max(f * 0.002);
    }
    let (freq, drop) = best?;

    if freq < 0.90 * ny {
        // Above-cliff region must be floor-like, not continuing content.
        let a = bi(freq * 1.06);
        let b = bi((freq * 1.60).min(0.98 * ny));
        if b > a + 10 {
            let mut region: Vec<f64> = psd[a..b].to_vec();
            region.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let med_above = region[region.len() / 2];
            let shoulder = psd[bi(freq * 0.90)];
            if med_above > shoulder - 25.0 {
                return None; // energy continues above the knee — natural rolloff
            }
        }
    }
    Some(SpectralCliff { freq_hz: freq, drop_db: drop })
}

/// Smallest legacy rate whose Nyquist plausibly explains the cliff.
fn snap_origin_rate(cliff_hz: f64, container_rate: u32) -> Option<u32> {
    const LEGACY: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
    LEGACY.iter().copied().find(|&r| {
        r < container_rate
            && (r as f64 / 2.0) >= cliff_hz * 0.99
            && (r as f64 / 2.0) <= cliff_hz * 1.35
    })
}

/// Remove the least-squares line from `v` (mean + slope). Two smooth
/// spectral slopes correlate trivially; only the residual fine structure
/// is evidence of mirroring.
fn detrend_linear(v: &mut [f64]) {
    let n = v.len() as f64;
    if v.len() < 2 {
        return;
    }
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = v.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (i, &y) in v.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    let slope = if den > 0.0 { num / den } else { 0.0 };
    for (i, y) in v.iter_mut().enumerate() {
        *y -= mean_y + slope * (i as f64 - mean_x);
    }
}

/// Pearson correlation of two zero-mean (detrended) sequences.
fn pearson_zero_mean(a: &[f64], b: &[f64]) -> f64 {
    let mut sab = 0.0;
    let mut saa = 0.0;
    let mut sbb = 0.0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    if saa <= 0.0 || sbb <= 0.0 {
        return 0.0;
    }
    sab / (saa * sbb).sqrt()
}

/// Detect upstream-SRC mirror imaging around candidate legacy Nyquists.
///
/// For each 16384-sample segment the dB spectrum just below a candidate
/// Nyquist is compared — after linear detrending — with the MIRRORED band
/// just above it (bin ny−g−i vs bin ny+g+i). Real images correlate
/// bin-for-bin because f maps to 2·Ny−f; independent hi-res content and
/// flat dither floors do not. Energy-only correlation would false-positive
/// on ordinary loudness co-variation, spectral SHAPE does not.
///
/// Returns the best candidate with mean correlation ≥ 0.55 over ≥12
/// segments. Deliberately conservative: false negatives are acceptable,
/// false positives are not.
fn probe_mirror_aliasing(
    mono: &[f64],
    sample_rate: u32,
    candidates_ny: &[f64],
    file_cancel: &AtomicBool,
) -> Option<AliasEvidence> {
    use rustfft::{num_complex::Complex, FftPlanner};
    const NSEG: usize = 16_384;
    let container_ny = sample_rate as f64 / 2.0;
    let n = mono.len();
    if n < NSEG * 3 || candidates_ny.is_empty() {
        return None;
    }
    let bin_hz = sample_rate as f64 / NSEG as f64;

    struct Band {
        ny_o: f64,
        k_ny: usize,
        gap: usize,
        w: usize,
        sum_r: f64,
        segs: usize,
    }
    let mut bands: Vec<Band> = Vec::new();
    for &ny_o in candidates_ny {
        let w_hz = (0.30 * ny_o).min(0.90 * (container_ny - ny_o));
        let w = (w_hz / bin_hz) as usize;
        let gap = (250.0 / bin_hz).ceil() as usize;
        let k_ny = (ny_o / bin_hz).round() as usize;
        if w >= gap + 64 && k_ny > gap + w && k_ny + gap + w < NSEG / 2 {
            bands.push(Band { ny_o, k_ny, gap, w, sum_r: 0.0, segs: 0 });
        }
    }
    if bands.is_empty() {
        return None;
    }

    let hop = NSEG / 2;
    let total_segs = (n - NSEG) / hop + 1;
    let stride = (total_segs + 127) / 128;
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(NSEG);
    let hann: Vec<f64> = (0..NSEG)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (NSEG as f64 - 1.0)).cos())
        .collect();
    let mut buf = vec![Complex { re: 0.0, im: 0.0 }; NSEG];
    let mut seg = 0usize;
    let mut count = 0usize;
    while seg < total_segs {
        if count % 16 == 0 && file_or_global_cancelled(file_cancel) {
            return None;
        }
        let s = seg * hop;
        for i in 0..NSEG {
            buf[i].re = mono[s + i] * hann[i];
            buf[i].im = 0.0;
        }
        fft.process(&mut buf);
        for band in bands.iter_mut() {
            let m = band.w - band.gap;
            let mut below = vec![0.0f64; m];
            let mut above = vec![0.0f64; m];
            for i in 0..m {
                below[i] =
                    10.0 * (buf[band.k_ny - band.gap - i].norm_sqr() + 1e-30).log10();
                above[i] =
                    10.0 * (buf[band.k_ny + band.gap + i].norm_sqr() + 1e-30).log10();
            }
            detrend_linear(&mut below);
            detrend_linear(&mut above);
            let r = pearson_zero_mean(&below, &above);
            if r.is_finite() {
                band.sum_r += r;
                band.segs += 1;
            }
        }
        count += 1;
        seg += stride;
    }

    bands
        .iter()
        .filter(|b| b.segs >= 12)
        .map(|b| (b.ny_o, b.sum_r / b.segs as f64))
        .filter(|&(_, r)| r >= 0.55)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(ny_o, r)| AliasEvidence {
            origin_nyquist_hz: ny_o,
            correlation: r,
        })
}

/// Full source forensics: spectral cliff + time-domain pre-ring detection
/// (v2 core, band-parameterized) + per-burst ring-frequency estimation.
/// Returns None only on cancel or material too short/quiet to analyze.
pub fn analyze_source(
    samples_l: &[f64],
    samples_r: &[f64],
    sample_rate: u32,
    file_cancel: &AtomicBool,
) -> Option<SourceAnalysis> {
    let container_ny = sample_rate as f64 / 2.0;
    let n = samples_l.len();
    // Context windows need ~1 s of material minimum.
    if n < sample_rate as usize {
        return None;
    }

    // ── 0. Mono mix ──
    let mono: Vec<f64> = samples_l
        .iter()
        .zip(samples_r.iter())
        .map(|(&l, &r)| (l + r) * 0.5)
        .collect();

    // ── 1. Spectral pass: cliff → effective Nyquist ──
    let cliff = analyze_spectral_cliff(&mono, sample_rate, file_cancel);
    if file_or_global_cancelled(file_cancel) {
        return None;
    }
    let mut effective_ny = container_ny;
    let mut origin: Option<u32> = None;
    if let Some(c) = cliff {
        if sample_rate > 48_000 && c.freq_hz < 0.90 * container_ny {
            origin = snap_origin_rate(c.freq_hz, sample_rate);
            effective_ny = origin
                .map(|r| r as f64 / 2.0)
                .unwrap_or(c.freq_hz * 1.05)
                .min(container_ny);
        }
    }

    // ── 1b. Mirror-image alias probe (hi-res containers only) ──
    // With a cliff-derived origin, probe just that Nyquist (clean vs
    // aliased SRC diagnosis). Without one, probe every plausible legacy
    // Nyquist: strong images defeat the cliff floor-check — exactly the
    // badly-upsampled case — so aliasing alone may pin the origin.
    let mut alias: Option<AliasEvidence> = None;
    if sample_rate > 48_000 {
        let probe_set: Vec<f64> = match origin {
            Some(r) => vec![r as f64 / 2.0],
            None => [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000]
                .iter()
                .filter(|&&r| r < sample_rate && (r as f64 / 2.0) < 0.90 * container_ny)
                .map(|&r| r as f64 / 2.0)
                .collect(),
        };
        alias = probe_mirror_aliasing(&mono, sample_rate, &probe_set, file_cancel);
        if file_or_global_cancelled(file_cancel) {
            return None;
        }
        if origin.is_none() {
            if let Some(al) = &alias {
                origin = Some((al.origin_nyquist_hz * 2.0).round() as u32);
                effective_ny = al.origin_nyquist_hz;
            }
        }
    }

    // ── 2. Near-(effective-)Nyquist isolation ──
    // 127-tap linear-phase Kaiser highpass at 0.78 × effective Nyquist,
    // β=8 → ~80 dB stopband; its ±1.4 ms smear stays clear of the windows.
    let hp_taps = 127usize;
    let hp_edge_hz = 0.78 * effective_ny;
    let hp_edge_norm = hp_edge_hz / container_ny;
    let h_hp = {
        // Kaiser lowpass at the edge, spectrally inverted to a highpass.
        let beta = 8.0;
        let i0_beta = bessel_i0(beta);
        let half = hp_taps / 2;
        let mut h_lp: Vec<f64> = Vec::with_capacity(hp_taps);
        let mut sum = 0.0;
        for i in 0..hp_taps {
            let nd = i as f64 - half as f64;
            let sinc = if nd.abs() < 1e-10 {
                hp_edge_norm
            } else {
                (std::f64::consts::PI * hp_edge_norm * nd).sin() / (std::f64::consts::PI * nd)
            };
            let arg = (2.0 * i as f64) / (hp_taps as f64 - 1.0) - 1.0;
            let w = bessel_i0(beta * (1.0 - arg * arg).max(0.0).sqrt()) / i0_beta;
            let v = sinc * w;
            h_lp.push(v);
            sum += v;
        }
        let mut h: Vec<f64> = h_lp.iter().map(|v| -v / sum).collect();
        h[half] += 1.0;
        h
    };
    let hf = match fft_convolve_ola(&mono, &mono, &h_hp, FilterPhase::Linear, file_cancel) {
        Ok((l, _)) => l,
        Err(_) => return None, // cancelled
    };

    // ── 3. 1-ms RMS envelopes (broadband + near-Nyquist) ──
    let frame = (sample_rate as usize / 1000).max(8);
    let n_frames = n / frame;
    if n_frames < 200 {
        return None;
    }
    let mut bb_rms = vec![0.0f64; n_frames];
    let mut hf_rms = vec![0.0f64; n_frames];
    for f in 0..n_frames {
        if f % 8192 == 0 && file_or_global_cancelled(file_cancel) {
            return None;
        }
        let s = f * frame;
        let mut acc_b = 0.0;
        let mut acc_h = 0.0;
        for i in s..s + frame {
            acc_b += mono[i] * mono[i];
            acc_h += hf[i] * hf[i];
        }
        bb_rms[f] = (acc_b / frame as f64).sqrt();
        hf_rms[f] = (acc_h / frame as f64).sqrt();
    }

    // ── 4. Attack candidates across the whole track ──
    const MIN_LEVEL: f64 = 0.01; // −40 dBFS
    const JUMP_RATIO: f64 = 2.5; // ≈ +8 dB over the local past
    let mut cands: Vec<(usize, f64)> = Vec::new();
    for f in 100..n_frames.saturating_sub(10) {
        let cur = bb_rms[f];
        if cur < MIN_LEVEL || cur <= bb_rms[f - 1] {
            continue;
        }
        // Mean of the preceding 24 ms, excluding the 2 ms right before the
        // attack (where lookahead-limiter ramps or the attack's own filter
        // smear could live).
        let mut past = 0.0;
        for pf in (f - 26)..(f - 2) {
            past += bb_rms[pf];
        }
        past /= 24.0;
        let score = cur / (past + 1e-12);
        if score > JUMP_RATIO {
            cands.push((f, score));
        }
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let spacing = 250usize; // frames ≈ 250 ms apart
    let mut onsets: Vec<usize> = Vec::new();
    'outer: for &(f, _) in &cands {
        for &o in &onsets {
            if f.abs_diff(o) < spacing {
                continue 'outer;
            }
        }
        onsets.push(f);
        if onsets.len() >= 48 {
            break;
        }
    }

    // ── 5. Pre-ring measurement per onset + ring-frequency estimation ──
    const HF_ABS_FLOOR: f64 = 3.16e-4; // −70 dBFS
    const RING_NFFT: usize = 8192;
    let mut ring_planner = rustfft::FftPlanner::<f64>::new();
    let ring_fft = ring_planner.plan_fft_forward(RING_NFFT);
    let ring_bin_hz = sample_rate as f64 / RING_NFFT as f64;

    let mut analyzed = 0usize;
    let mut ringing = 0usize;
    let mut severities: Vec<f64> = Vec::new();
    let mut ring_freqs: Vec<f64> = Vec::new();
    for &f in &onsets {
        if f < 90 || f + 9 > n_frames {
            continue;
        }
        let mean = |a: usize, b: usize| -> f64 {
            let mut s = 0.0;
            for i in a..b {
                s += hf_rms[i];
            }
            s / (b - a).max(1) as f64
        };
        let pre = mean(f - 9, f - 3); // −9…−3 ms before the attack
        let post = mean(f, f + 8); // the attack's own HF
        // Robust local background: 25th percentile over −80…−10 ms
        let mut bg: Vec<f64> = hf_rms[f - 80..f - 10].to_vec();
        bg.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let far = bg[bg.len() / 4];

        if post < HF_ABS_FLOOR {
            continue; // attack carries no HF at all — cannot judge ringing
        }
        analyzed += 1;
        let is_ring =
            pre > 2.0 * far && pre > HF_ABS_FLOOR && pre < post && pre > 0.02 * post;
        if is_ring {
            ringing += 1;
            severities.push(20.0 * (pre / far.max(1e-12)).log10());

            // Dominant frequency of the pre-ring burst: Hann-windowed FFT of
            // the isolated HF band over the same −9…−3 ms window. The burst
            // oscillates at the source filter's transition frequency.
            let s0 = (f - 9) * frame;
            let s1 = (f - 3) * frame;
            let len = (s1 - s0).min(RING_NFFT);
            let mut buf =
                vec![rustfft::num_complex::Complex { re: 0.0, im: 0.0 }; RING_NFFT];
            for i in 0..len {
                let w = 0.5
                    - 0.5
                        * (2.0 * std::f64::consts::PI * i as f64 / (len as f64 - 1.0)).cos();
                buf[i].re = hf[s0 + i] * w;
            }
            ring_fft.process(&mut buf);
            let k_min = (hp_edge_hz / ring_bin_hz).ceil() as usize;
            let k_max = (((effective_ny * 1.02) / ring_bin_hz) as usize).min(RING_NFFT / 2 - 1);
            if k_max > k_min {
                let mut best_k = k_min;
                let mut best_p = 0.0f64;
                for k in k_min..=k_max {
                    let p = buf[k].norm_sqr();
                    if p > best_p {
                        best_p = p;
                        best_k = k;
                    }
                }
                ring_freqs.push(best_k as f64 * ring_bin_hz);
            }
        }
    }

    let frac = if analyzed > 0 {
        ringing as f64 / analyzed as f64
    } else {
        0.0
    };
    severities.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med_sev = if severities.is_empty() {
        0.0
    } else {
        severities[severities.len() / 2].min(99.0)
    };

    // Median ring frequency, accepted only if ≥60% of the bursts agree
    // within ±8% — otherwise the "ring" is broadband junk, not a filter.
    let ring_freq_hz = if ring_freqs.len() >= 3 {
        ring_freqs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = ring_freqs[ring_freqs.len() / 2];
        let agree = ring_freqs
            .iter()
            .filter(|&&x| (x - med).abs() <= 0.08 * med)
            .count();
        if agree * 10 >= ring_freqs.len() * 6
            && med > hp_edge_hz * 1.01
            && med < effective_ny * 1.03
        {
            Some(med)
        } else {
            None
        }
    } else {
        None
    };

    crate::aelog!(
        "[CONV] Source analysis v3: cliff={}, effective Nyquist={:.0} Hz{}, {} attacks judged, {:.0}% ringing (sev {:.1} dB), ring freq={}, aliasing={}",
        cliff
            .map(|c| format!("{:.0} Hz (−{:.0} dB)", c.freq_hz, c.drop_db))
            .unwrap_or_else(|| "none".to_string()),
        effective_ny,
        origin
            .map(|r| format!(" (origin ≈ {} Hz)", r))
            .unwrap_or_default(),
        analyzed,
        frac * 100.0,
        med_sev,
        ring_freq_hz
            .map(|f| format!("{:.0} Hz", f))
            .unwrap_or_else(|| "n/a".to_string()),
        alias
            .map(|al| format!("r {:.2} @ {:.0} Hz", al.correlation, al.origin_nyquist_hz))
            .unwrap_or_else(|| "none".to_string())
    );

    Some(SourceAnalysis {
        effective_nyquist_hz: effective_ny,
        suspected_origin_rate: origin,
        cliff,
        attacks_analyzed: analyzed,
        ring_fraction: frac,
        ring_severity_db: med_sev,
        ring_freq_hz,
        alias_probe: alias,
    })
}

/// Turn measurements into apodizer parameters — or a reasoned refusal.
///
/// Decision ladder (most→least evidence):
///   1. Confirmed pre-ring → cutoff just below the measured ring frequency
///      (fallback: v2's fraction buckets), depth from severity.
///   2. Cliff without ring on transient-rich material → minimum-phase or
///      pre-apodized source: diagnosis only, audio untouched.
///   3. Cliff on low-transient material → gentle cutoff at the cliff, but
///      never inside a true hi-res container's own ADC band.
///   4. Nothing → None (caller falls back to the static preset).
pub fn decide_apodizer(a: &SourceAnalysis, container_rate: u32) -> Option<ApodizerPlan> {
    let eff_ny = a.effective_nyquist_hz;
    let container_ny = container_rate as f64 / 2.0;
    // Taps scale with the container rate so the transition band stays
    // narrow in Hz (4096 @ ≤48k … 32768 @ ≥352.8k). Still below the GPU
    // threshold — the CPU FFT+Rayon path handles all of these.
    let taps = {
        let mult = ((container_rate as f64 / 44_100.0).round().max(1.0) as usize)
            .next_power_of_two();
        (4096usize * mult).min(32_768)
    };
    // Floor = v2's strongest preset (0.816×Ny = 18 kHz @ 44.1). Field case
    // 2026-07-13: the old 0.80 floor let a distrusted 18.1 kHz "ring" pull
    // the cutoff down to 17.64 kHz — audibly deep.
    let clamp_fc = |fc: f64| fc.max(0.816 * eff_ny).min(0.93 * eff_ny);
    // Quorum: 8 judged attacks for ordinary evidence, or 5 when the signal
    // is strong (≥40% ringing at ≥12 dB). Same field case: sibling album
    // tracks measured near-identically (43% of 7 vs 49% of 41) but one fell
    // under a flat 8-attack quorum → inconsistent treatment across an album.
    let quorum = a.attacks_analyzed >= 8
        || (a.attacks_analyzed >= 5
            && a.ring_fraction >= 0.40
            && a.ring_severity_db >= 12.0);
    let ring_confirmed = quorum && a.ring_fraction >= 0.25;
    let ring_refuted = a.attacks_analyzed >= 8 && a.ring_fraction < 0.25;
    let origin_note = a
        .suspected_origin_rate
        .map(|r| format!(", fake hi-res: origin ≈ {} Hz", r))
        .unwrap_or_default();
    let alias_note = a
        .alias_probe
        .map(|al| format!(", SRC images present (r {:.2})", al.correlation))
        .unwrap_or_default();

    if ring_confirmed {
        // Fraction keeps v2's field-tuned severity mapping (with its dead
        // zones for album consistency); the measured ring frequency turns
        // the bucket into a precise placement.
        let (margin, bucket) = if a.ring_fraction >= 0.55 {
            (0.91, 0.816)
        } else if a.ring_fraction >= 0.40 {
            (0.94, 0.862)
        } else {
            (0.97, 0.907)
        };
        // Trust the measured frequency for precise placement only inside
        // the plausible ADC/SRC transition zone (≥0.86×Ny ≈ 19 kHz @ 44.1).
        // Lower readings are lossy pre-echo or the music's own spectral
        // tilt hugging the analysis-band edge — real brick walls do not
        // live at 18 kHz. Those cases keep v2's field-tested buckets.
        let ring_hz_trusted = a.ring_freq_hz.filter(|&f| f >= 0.86 * eff_ny);
        let fc = clamp_fc(
            ring_hz_trusted
                .map(|f| f * margin)
                .unwrap_or(bucket * eff_ny),
        );
        if fc < 16_800.0 {
            return None; // never cut into the midrange, whatever we measured
        }
        let beta = if a.ring_severity_db >= 8.0 { 24.0 } else { 14.0 };
        let how = match (ring_hz_trusted, a.ring_freq_hz) {
            (Some(f), _) => format!("ring measured at {:.0} Hz", f),
            (None, Some(f)) => format!(
                "ring at {:.0} Hz is below the plausible ADC band (≥{:.0} Hz) — lossy pre-echo or spectral tilt suspected, using preset bucket",
                f,
                0.86 * eff_ny
            ),
            (None, None) => "ring frequency indeterminate — preset bucket".to_string(),
        };
        return Some(ApodizerPlan {
            fc_hz: fc,
            taps,
            beta,
            reason: format!(
                "pre-ring on {:.0}% of {} attacks ({:.1} dB), {}{}{}",
                a.ring_fraction * 100.0,
                a.attacks_analyzed,
                a.ring_severity_db,
                how,
                origin_note,
                alias_note
            ),
        });
    }

    // Mirror-image aliasing is audible junk above the original Nyquist and
    // justifies cutting REGARDLESS of the pre-ring verdict: a minimum-phase
    // upstream SRC leaves no pre-ring, but its images are just as real.
    // (Ring-confirmed cases were handled above — their more precise cutoff
    // removes the images anyway.) Probed origins are ≥22.05 kHz, so the fc
    // clamp can never reach the midrange here.
    if let Some(al) = a.alias_probe {
        let fc = clamp_fc(0.93 * al.origin_nyquist_hz);
        return Some(ApodizerPlan {
            fc_hz: fc,
            taps,
            beta: 24.0,
            reason: format!(
                "mirror-image SRC aliasing above {:.0} Hz (shape corr {:.2}) — bad upstream resampler, cutting below the original Nyquist{}",
                al.origin_nyquist_hz, al.correlation, origin_note
            ),
        });
    }

    if let Some(c) = a.cliff {
        if c.freq_hz < 17_000.0 {
            crate::aelog!(
                "[CONV] Adaptive Apodizer: spectral content ends near {:.0} Hz (lossy or dark source?) — below apodizer range, leaving untouched",
                c.freq_hz
            );
            return None;
        }
        if ring_refuted {
            crate::aelog!(
                "[CONV] Adaptive Apodizer: brick-wall cliff at {:.0} Hz but no pre-ring across {} attacks — source filter is likely minimum-phase or already apodized; leaving untouched",
                c.freq_hz,
                a.attacks_analyzed
            );
            return None;
        }
        // Too few transients to judge ringing — act on spectral evidence
        // alone, gently. Guard: never treat a true hi-res container's own
        // ADC band (cliff near ITS Nyquist) as a defect.
        let cliff_in_legacy_band = container_rate <= 48_000 || eff_ny < 0.90 * container_ny;
        if cliff_in_legacy_band {
            let fc = clamp_fc((c.freq_hz * 0.96).min(0.93 * eff_ny));
            if fc < 16_800.0 {
                return None;
            }
            return Some(ApodizerPlan {
                fc_hz: fc,
                taps,
                beta: 14.0,
                reason: format!(
                    "brick-wall cliff at {:.0} Hz (−{:.0} dB) with too few transients to confirm ring ({} usable) — gentle treatment{}{}",
                    c.freq_hz, c.drop_db, a.attacks_analyzed, origin_note, alias_note
                ),
            });
        }
    }
    None
}

/// Generate apodizing coefficients with a specific normalized cutoff and
/// Kaiser β (minimum-phase). β=24 ≈ 240 dB stopband for confirmed strong
/// ringing; β=14 ≈ 140 dB — still inaudible rejection, but with a shorter
/// time-domain signature for mild cases.
pub fn generate_apodizing_coeffs_adaptive(
    source_rate: u32,
    fc_norm: f64,
    taps: usize,
    beta: f64,
) -> Vec<f64> {
    let half = taps / 2;
    let i0_beta = bessel_i0(beta);
    let mut h: Vec<f64> = Vec::with_capacity(taps);
    let mut sum = 0.0;

    for i in 0..taps {
        let n = i as f64 - half as f64;
        let sinc = if n.abs() < 1e-10 {
            fc_norm
        } else {
            (std::f64::consts::PI * fc_norm * n).sin() / (std::f64::consts::PI * n)
        };
        let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
        let kaiser_arg = beta * (1.0 - arg * arg).max(0.0).sqrt();
        let kaiser = bessel_i0(kaiser_arg) / i0_beta;
        let val = sinc * kaiser;
        h.push(val);
        sum += val;
    }

    if sum.abs() > 1e-15 {
        for v in h.iter_mut() {
            *v /= sum;
        }
    }

    crate::aelog!(
        "[CONV] Adaptive apodizing: {} taps, β={:.0}, fc_norm={:.4}, cutoff={:.0}Hz",
        taps,
        beta,
        fc_norm,
        fc_norm * source_rate as f64 / 2.0
    );

    crate::audio::dsp_core::to_minimum_phase(&h)
}
/// Apply custom apodizing coefficients to audio (direct convolution, short filter)
/// (Kept for legacy reference; actual work is done by apply_apodizing_coeffs)
pub fn apply_custom_apodizing(
    samples_l: &mut Vec<f64>,
    samples_r: &mut Vec<f64>,
    coeffs: &[f64],
    use_gpu: bool,
    precision: u32,
    file_cancel: &AtomicBool,
) -> Result<(), String> {
    apply_apodizing_coeffs(
        samples_l,
        samples_r,
        coeffs,
        use_gpu,
        precision,
        file_cancel,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Trivial passthrough min-phase impulse: h = [1.0, 0.0, 0.0, ...].
    /// `to_minimum_phase` of an all-pass impulse is itself a single sample
    /// at index 0, so its group delay is exactly 0 and convolution must
    /// reproduce the input bit-for-bit.
    #[test]
    fn min_phase_passthrough_keeps_first_sample() {
        let cancel = AtomicBool::new(false);
        let mut h = vec![0.0; 1024];
        h[0] = 1.0;
        let input: Vec<f64> = (0..2048).map(|i| if i == 100 { 1.0 } else { 0.0 }).collect();
        let (out_l, _out_r) = fft_convolve_ola(&input, &input, &h, FilterPhase::Minimum, &cancel).unwrap();
        // The impulse at sample 100 in the input must still be at sample
        // 100 in the output (i.e. NOT shifted to sample 100 + h_len/2).
        assert_eq!(out_l.len(), input.len());
        let max_idx = out_l
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().partial_cmp(&b.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(
            max_idx, 100,
            "min-phase passthrough: peak should stay at 100, got {}",
            max_idx
        );
        assert!((out_l[100] - 1.0).abs() < 1e-9);
    }

    /// Same input, but with `is_min_phase=false`. The centred trim should
    /// place the impulse exactly back at sample 100 too — this guards
    /// against mistakenly flipping the trim policy in the future.
    #[test]
    fn linear_phase_centered_trim_keeps_first_sample() {
        let cancel = AtomicBool::new(false);
        // Build a symmetric lowpass via the same Kaiser sinc used by the
        // production apodizer (NOT minimum-phase converted).
        let taps = 1024;
        let half = taps / 2;
        let beta = 8.0;
        let i0 = |x: f64| {
            let mut sum = 1.0;
            let mut term = 1.0;
            let x2 = (x / 2.0).powi(2);
            for k in 1..50 {
                term *= x2 / (k as f64).powi(2);
                sum += term;
                if term < 1e-15 * sum {
                    break;
                }
            }
            sum
        };
        let i0b = i0(beta);
        let mut h: Vec<f64> = (0..taps)
            .map(|i| {
                let n = i as f64 - half as f64;
                let s = if n.abs() < 1e-12 {
                    0.45
                } else {
                    (std::f64::consts::PI * 0.45 * n).sin() / (std::f64::consts::PI * n)
                };
                let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
                let kw = i0(beta * (1.0 - arg * arg).max(0.0).sqrt()) / i0b;
                s * kw
            })
            .collect();
        let s_sum: f64 = h.iter().sum();
        for v in h.iter_mut() {
            *v /= s_sum;
        }

        let input: Vec<f64> = (0..4096).map(|i| if i == 1000 { 1.0 } else { 0.0 }).collect();
        let (out_l, _) = fft_convolve_ola(&input, &input, &h, FilterPhase::Linear, &cancel).unwrap();
        let max_idx = out_l
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().partial_cmp(&b.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(out_l.len(), input.len());
        // Centred linear-phase convolution should put the impulse back at 1000.
        assert!(
            max_idx.abs_diff(1000) <= 1,
            "linear-phase centered trim: expected peak ~1000, got {}",
            max_idx
        );
    }

    /// Output length must always equal input length, regardless of
    /// is_min_phase choice — both production callers truncate-by-construction.
    #[test]
    fn output_length_matches_input() {
        let cancel = AtomicBool::new(false);
        let h = vec![0.5, 0.5];
        let input = vec![1.0; 1024];
        let (out_min_l, out_min_r) = fft_convolve_ola(&input, &input, &h, FilterPhase::Minimum, &cancel).unwrap();
        let (out_lin_l, out_lin_r) = fft_convolve_ola(&input, &input, &h, FilterPhase::Linear, &cancel).unwrap();
        assert_eq!(out_min_l.len(), input.len());
        assert_eq!(out_min_r.len(), input.len());
        assert_eq!(out_lin_l.len(), input.len());
        assert_eq!(out_lin_r.len(), input.len());
    }

    // ═════════════════════════════════════════════════════════════════
    // Adaptive Apodizer v3 — synthetic source-forensics fixtures.
    // Each fixture fabricates a source with a KNOWN filter history and
    // asserts the analyzer measures it and the decider acts (or refuses)
    // correctly. Synthetic tests prove the mechanics; real-world threshold
    // tuning still happens on real libraries.
    // ═════════════════════════════════════════════════════════════════

    /// Deterministic white noise in [−amp, amp] (LCG — no rand dependency).
    fn lcg_noise(n: usize, amp: f64, seed: u64) -> Vec<f64> {
        let mut s = seed.max(1);
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((s >> 32) as u32) as f64 / u32::MAX as f64 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn one_pole_lp(x: &[f64], fs: f64, fc: f64) -> Vec<f64> {
        let a = 1.0 - (-2.0 * std::f64::consts::PI * fc / fs).exp();
        let mut y = 0.0;
        x.iter()
            .map(|&v| {
                y += a * (v - y);
                y
            })
            .collect()
    }

    /// Symmetric Kaiser-windowed sinc lowpass, unity DC gain. fc_norm is
    /// relative to Nyquist — the same convention as the production code.
    fn design_linphase_lp(taps: usize, fc_norm: f64, beta: f64) -> Vec<f64> {
        let half = taps / 2;
        let i0b = bessel_i0(beta);
        let mut h: Vec<f64> = (0..taps)
            .map(|i| {
                let n = i as f64 - half as f64;
                let s = if n.abs() < 1e-12 {
                    fc_norm
                } else {
                    (std::f64::consts::PI * fc_norm * n).sin() / (std::f64::consts::PI * n)
                };
                let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
                s * bessel_i0(beta * (1.0 - arg * arg).max(0.0).sqrt()) / i0b
            })
            .collect();
        let sum: f64 = h.iter().sum();
        for v in h.iter_mut() {
            *v /= sum;
        }
        h
    }

    fn add_tone(x: &mut [f64], fs: f64, freq: f64, amp: f64) {
        for (i, v) in x.iter_mut().enumerate() {
            *v += amp * (2.0 * std::f64::consts::PI * freq * i as f64 / fs).sin();
        }
    }

    /// Naive ×2 upsampler: zero-stuff + triangle kernel [0.5, 1, 0.5]
    /// (= linear interpolation). Leaves strong mirror images of the
    /// content around the ORIGINAL Nyquist — the classic bad-SRC artifact.
    fn upsample2_linear(x: &[f64]) -> Vec<f64> {
        let mut y = vec![0.0f64; x.len() * 2];
        for (i, &v) in x.iter().enumerate() {
            y[2 * i] = v;
        }
        let mut out = vec![0.0f64; y.len()];
        for i in 0..y.len() {
            let a = if i >= 1 { y[i - 1] } else { 0.0 };
            let c = if i + 1 < y.len() { y[i + 1] } else { 0.0 };
            out[i] = 0.5 * a + y[i] + 0.5 * c;
        }
        out
    }

    /// `width` = samples per click. A physical transient has a fixed analog
    /// area, so its sample width must scale with the container rate
    /// (1 @ 44.1k, 2 @ 88.2/96k) — otherwise hi-res fixtures under-drive
    /// the source filter's ring by exactly the rate ratio.
    fn add_clicks(
        x: &mut [f64],
        fs: usize,
        first_s: f64,
        spacing_s: f64,
        count: usize,
        amp: f64,
        width: usize,
    ) {
        for k in 0..count {
            let idx = ((first_s + k as f64 * spacing_s) * fs as f64) as usize;
            let a = if k % 2 == 0 { amp } else { -amp };
            for w in 0..width.max(1) {
                if idx + w < x.len() {
                    x[idx + w] += a;
                }
            }
        }
    }

    /// Bed of band-limited noise + click train, convolved with `h` (the
    /// "source filter" whose history we then try to recover), plus a tiny
    /// full-band dither floor so background percentiles are non-degenerate.
    /// `quiet_hf` steepens the bed to −24 dB/oct (clean recording — the ring
    /// must stand far above the HF background); `false` keeps a −6 dB/oct
    /// bed whose content reaches the brick-wall edge (ambient/steady case).
    fn make_source(
        sr: u32,
        seconds: usize,
        h: &[f64],
        phase: FilterPhase,
        clicks: bool,
        quiet_hf: bool,
    ) -> Vec<f64> {
        let cancel = AtomicBool::new(false);
        let n = seconds * sr as usize;
        let mut base = one_pole_lp(&lcg_noise(n, 0.006, 7), sr as f64, 4_000.0);
        if quiet_hf {
            for _ in 0..3 {
                base = one_pole_lp(&base, sr as f64, 3_500.0);
            }
        }
        if clicks {
            let width = (sr as usize / 44_100).max(1);
            add_clicks(&mut base, sr as usize, 0.6, 0.35, 15, 0.9, width);
        }
        let (mut sig, _) = fft_convolve_ola(&base, &base, h, phase, &cancel).unwrap();
        let dither = lcg_noise(n, 3.2e-5, 99); // ≈ −90 dBFS, full band
        for (s, d) in sig.iter_mut().zip(dither.iter()) {
            *s += d;
        }
        sig
    }

    /// CD-rate source stamped by a sharp LINEAR-phase filter at 21 kHz:
    /// the detector must confirm ringing, measure its frequency, and the
    /// decider must place the cutoff just below it with full depth.
    #[test]
    fn v3_linear_phase_ring_measured_and_treated() {
        let cancel = AtomicBool::new(false);
        let sr = 44_100u32;
        let h = design_linphase_lp(2048, 21_000.0 / 22_050.0, 8.0);
        let sig = make_source(sr, 6, &h, FilterPhase::Linear, true, true);

        let a = analyze_source(&sig, &sig, sr, &cancel).expect("analysis must run");
        assert!(a.attacks_analyzed >= 8, "attacks_analyzed={}", a.attacks_analyzed);
        assert!(a.ring_fraction >= 0.55, "ring_fraction={}", a.ring_fraction);
        let f_ring = a.ring_freq_hz.expect("ring frequency must be measured");
        assert!(
            (f_ring - 21_000.0).abs() < 600.0,
            "ring freq {:.0} Hz should be ≈21000",
            f_ring
        );

        let plan = decide_apodizer(&a, sr).expect("must act on confirmed ring");
        assert!(
            plan.fc_hz > 18_000.0 && plan.fc_hz < 20_600.0,
            "fc {:.0} Hz should sit just below the measured ring",
            plan.fc_hz
        );
        assert_eq!(plan.taps, 4096);
        assert!(plan.beta >= 20.0, "strong ring → deep filter, got β={}", plan.beta);
    }

    /// Same magnitude response but MINIMUM-phase: no pre-ring exists, and
    /// the cliff-without-ring diagnosis must refuse to touch the audio.
    #[test]
    fn v3_minimum_phase_source_left_alone() {
        let cancel = AtomicBool::new(false);
        let sr = 44_100u32;
        let h_lin = design_linphase_lp(2048, 21_000.0 / 22_050.0, 8.0);
        let h_min = crate::audio::dsp_core::to_minimum_phase(&h_lin);
        let sig = make_source(sr, 6, &h_min, FilterPhase::Minimum, true, true);

        let a = analyze_source(&sig, &sig, sr, &cancel).expect("analysis must run");
        assert!(a.attacks_analyzed >= 8, "attacks_analyzed={}", a.attacks_analyzed);
        assert!(
            a.ring_fraction < 0.25,
            "min-phase source must not show pre-ring, got {}",
            a.ring_fraction
        );
        assert!(
            decide_apodizer(&a, sr).is_none(),
            "cliff without pre-ring must be diagnosis-only"
        );
    }

    /// 44.1k master upsampled into an 88.2k container ("fake hi-res"):
    /// the cliff detector must find the 21 kHz edge, snap the origin to
    /// 44.1k, and the ring detector must work against the ORIGINAL Nyquist.
    /// v2 skipped this file entirely.
    #[test]
    fn v3_fake_hires_unmasked_and_treated() {
        let cancel = AtomicBool::new(false);
        let sr = 88_200u32;
        // Source filter at 21 kHz relative to the ORIGINAL 22.05k Nyquist,
        // designed here against the container rate: fc_norm = 21k/44.1k.
        let h = design_linphase_lp(4096, 21_000.0 / 44_100.0, 8.0);
        let sig = make_source(sr, 6, &h, FilterPhase::Linear, true, true);

        let a = analyze_source(&sig, &sig, sr, &cancel).expect("analysis must run");
        let cliff = a.cliff.expect("cliff at ≈21 kHz must be detected");
        assert!(
            (cliff.freq_hz - 21_000.0).abs() < 1_500.0,
            "cliff at {:.0} Hz should be ≈21000",
            cliff.freq_hz
        );
        assert_eq!(
            a.suspected_origin_rate,
            Some(44_100),
            "origin must snap to 44.1k"
        );
        assert!(
            (a.effective_nyquist_hz - 22_050.0).abs() < 1.0,
            "effective Nyquist must be the ORIGINAL 22050, got {:.0}",
            a.effective_nyquist_hz
        );
        assert!(a.ring_fraction >= 0.55, "ring_fraction={}", a.ring_fraction);
        assert!(
            a.alias_probe.is_none(),
            "clean SRC must show no mirror images, got {:?}",
            a.alias_probe
        );

        let plan = decide_apodizer(&a, sr).expect("fake hi-res with ring must be treated");
        assert!(
            plan.fc_hz > 18_000.0 && plan.fc_hz < 20_600.0,
            "fc {:.0} Hz must target the ORIGINAL band, not the container's",
            plan.fc_hz
        );
        assert_eq!(plan.taps, 8192, "taps must scale with the container rate");
    }

    /// True hi-res: smooth natural rolloff, broadband transients, no
    /// brick wall anywhere. The analyzer must find nothing actionable.
    #[test]
    fn v3_true_hires_left_alone() {
        let cancel = AtomicBool::new(false);
        let sr = 96_000u32;
        let n = 6 * sr as usize;
        // −18 dB/oct natural rolloff via a 3× one-pole cascade at 15 kHz.
        let mut sig = lcg_noise(n, 0.05, 11);
        for _ in 0..3 {
            sig = one_pole_lp(&sig, sr as f64, 15_000.0);
        }
        add_clicks(&mut sig, sr as usize, 0.6, 0.35, 15, 0.9, 2); // unfiltered → no pre-ring

        let a = analyze_source(&sig, &sig, sr, &cancel).expect("analysis must run");
        assert!(
            a.cliff.is_none(),
            "smooth rolloff must not read as a cliff, got {:?}",
            a.cliff
        );
        assert!(
            a.alias_probe.is_none(),
            "independent hi-res content must not correlate as mirror images, got {:?}",
            a.alias_probe
        );
        assert!(
            decide_apodizer(&a, sr).is_none(),
            "true hi-res must be left alone"
        );
    }

    /// Brick-walled CD-rate material with NO transients (ambient case):
    /// the time-domain detector cannot judge, so the spectral path must
    /// apply the gentle cliff-based treatment — v2 gave up here.
    #[test]
    fn v3_steady_brickwall_without_transients_gets_gentle_treatment() {
        let cancel = AtomicBool::new(false);
        let sr = 44_100u32;
        let h = design_linphase_lp(4096, 20_600.0 / 22_050.0, 8.0);
        // Bright bed (content up to the wall), no clicks — the ambient case.
        let sig = make_source(sr, 6, &h, FilterPhase::Linear, false, false);

        let a = analyze_source(&sig, &sig, sr, &cancel).expect("analysis must run");
        assert!(
            a.attacks_analyzed < 8,
            "steady noise must not produce judged attacks, got {}",
            a.attacks_analyzed
        );
        let cliff = a.cliff.expect("steady brick wall must show a cliff");
        assert!(
            (cliff.freq_hz - 20_600.0).abs() < 1_500.0,
            "cliff at {:.0} Hz should be ≈20600",
            cliff.freq_hz
        );

        let plan = decide_apodizer(&a, sr).expect("spectral evidence must trigger gentle path");
        assert!(
            plan.fc_hz > 19_000.0 && plan.fc_hz <= 20_600.0,
            "gentle fc {:.0} Hz should sit just below the cliff",
            plan.fc_hz
        );
        assert!(plan.beta < 20.0, "gentle path must use the light filter, got β={}", plan.beta);
    }

    /// 44.1k master upsampled ×2 by a BAD resampler (linear interpolation):
    /// mirror images of the tonal HF content sit above 22.05 kHz and defeat
    /// the cliff detector's floor check, and the (minimum-phase) mastering
    /// filter leaves NO pre-ring — every other detector is blind here. The
    /// mirror-shape probe must pin the origin and the decider must cut
    /// below the original Nyquist to remove the images.
    #[test]
    fn v3_aliased_fake_hires_rescued_by_mirror_probe() {
        let cancel = AtomicBool::new(false);
        let sr_src = 44_100u32;
        let n_src = 6 * sr_src as usize;
        let mut master = one_pole_lp(&lcg_noise(n_src, 0.006, 7), sr_src as f64, 4_000.0);
        for _ in 0..3 {
            master = one_pole_lp(&master, sr_src as f64, 3_500.0);
        }
        add_clicks(&mut master, sr_src as usize, 0.6, 0.35, 15, 0.9, 1);
        // Tonal HF structure — its mirrored copies are what the probe matches.
        add_tone(&mut master, sr_src as f64, 19_200.0, 0.020);
        add_tone(&mut master, sr_src as f64, 20_300.0, 0.014);
        add_tone(&mut master, sr_src as f64, 21_200.0, 0.010);
        // Minimum-phase mastering brick wall: deliberately NO pre-ring.
        let h_min = crate::audio::dsp_core::to_minimum_phase(&design_linphase_lp(
            2048,
            21_500.0 / 22_050.0,
            8.0,
        ));
        let (mut master, _) =
            fft_convolve_ola(&master, &master, &h_min, FilterPhase::Minimum, &cancel).unwrap();
        let dither = lcg_noise(n_src, 3.2e-5, 99);
        for (s, d) in master.iter_mut().zip(dither.iter()) {
            *s += d;
        }
        let sig = upsample2_linear(&master); // 88.2k container full of images

        let a = analyze_source(&sig, &sig, 88_200, &cancel).expect("analysis must run");
        let al = a.alias_probe.expect("mirror images must be detected");
        assert!(
            (al.origin_nyquist_hz - 22_050.0).abs() < 1.0,
            "images must mirror around 22050 Hz, got {:.0}",
            al.origin_nyquist_hz
        );
        assert!(al.correlation >= 0.55, "correlation={:.2}", al.correlation);
        assert_eq!(a.suspected_origin_rate, Some(44_100));
        assert!(
            a.ring_fraction < 0.25,
            "min-phase master must not ring, got {}",
            a.ring_fraction
        );

        let plan = decide_apodizer(&a, 88_200).expect("aliased fake hi-res must be treated");
        assert!(
            plan.fc_hz > 19_900.0 && plan.fc_hz <= 20_600.0,
            "fc {:.0} Hz must land just below the original Nyquist",
            plan.fc_hz
        );
        assert!(
            plan.reason.contains("alias"),
            "reason must name the aliasing: {}",
            plan.reason
        );
        assert_eq!(plan.taps, 8192);
    }

    fn mk_analysis(attacks: usize, frac: f64, sev: f64, ring: Option<f64>) -> SourceAnalysis {
        SourceAnalysis {
            effective_nyquist_hz: 22_050.0,
            suspected_origin_rate: None,
            cliff: None,
            attacks_analyzed: attacks,
            ring_fraction: frac,
            ring_severity_db: sev,
            ring_freq_hz: ring,
            alias_probe: None,
        }
    }

    /// Field case 2026-07-13 (two sibling tracks of one CD-rip album,
    /// exact log numbers): the tracks measured near-identically — 43% of 7
    /// attacks vs 49% of 41, ring 18206 vs 18126 Hz — yet one fell under
    /// the flat 8-attack quorum (no AA tag) while the other was cut at
    /// 17640 Hz because the implausible 18.1 kHz "ring" was trusted and
    /// the 0.80 floor allowed it. Both regressions are pinned here: strong
    /// evidence from ≥5 attacks qualifies, sub-0.86×Ny rings fall back to
    /// the bucket, and album siblings land on the SAME cutoff at ≥0.816×Ny.
    #[test]
    fn v3_album_consistency_and_ring_trust_window() {
        let track_a = mk_analysis(7, 3.0 / 7.0, 22.4, Some(18_206.0));
        let track_b = mk_analysis(41, 0.49, 19.8, Some(18_126.0));

        let p1 = decide_apodizer(&track_a, 44_100)
            .expect("7 attacks with strong ring evidence must now qualify");
        let p2 = decide_apodizer(&track_b, 44_100)
            .expect("41 attacks qualified before and must still qualify");
        assert!(
            (p1.fc_hz - p2.fc_hz).abs() < 1.0,
            "album siblings must get the same cutoff: {:.0} vs {:.0}",
            p1.fc_hz,
            p2.fc_hz
        );
        assert!(
            (p1.fc_hz - 0.862 * 22_050.0).abs() < 1.0,
            "implausible 18.1 kHz ring must fall back to the moderate bucket, got {:.0}",
            p1.fc_hz
        );
        assert!(
            p2.reason.contains("below the plausible ADC band"),
            "reason must explain the distrust: {}",
            p2.reason
        );

        // Weak evidence on few attacks must still refuse (25-30% of 5).
        assert!(
            decide_apodizer(&mk_analysis(5, 0.30, 22.0, None), 44_100).is_none(),
            "5 attacks with weak fraction must not qualify"
        );

        // Floor: a trusted ring just above the trust edge with the strong
        // margin must clamp at 0.816×Ny (18 kHz @ 44.1), never below.
        let deep = decide_apodizer(&mk_analysis(20, 0.60, 25.0, Some(19_100.0)), 44_100)
            .expect("confirmed strong ring");
        assert!(
            (deep.fc_hz - 0.816 * 22_050.0).abs() < 1.0,
            "floor must be 0.816×Ny, got {:.0}",
            deep.fc_hz
        );
    }
}
