//! Frequency-response calculation for the player's filter bank and for
//! arbitrary FIR filters.
//!
//! ## Bank response — mathematical derivation
//!
//! A filter `h[n]` of length `T` is split into `L` polyphase branches by
//! `polyphase_decompose`: `h_p[k] = h[k·L + p]` for `p = 0…L−1`.
//!
//! Each branch is split into partitions of `BLOCK = 32768` taps; each
//! partition is transformed into an `NFFT = 2·BLOCK`-point complex spectrum
//! `P[p][b][m]` (the DFT bin at `m`).
//!
//! **Step 1 — branch DFT at bin m:**
//! The inter-partition delay at `Ω = 2π·m/NFFT` is
//! `e^{−i·(2π·m/NFFT)·b·BLOCK} = e^{−iπ·m·b} = (−1)^{m·b}`,
//! so the branch DTFT evaluates to
//! `H_p[m] = Σ_b (−1)^{m·b} · P[p][b][m]`.
//!
//! **Step 2 — full polyphase response on the output-rate grid:**
//! For output-rate bin `q` (`Ω_q = 2π·q/(NFFT·L)`),
//! `H(q) = Σ_p exp(−i·2π·q·p/(NFFT·L)) · H_p[q mod NFFT]`.
//!
//! This equals the `NFFT·L`-point DFT of `h` evaluated at bin `q` (proved
//! by substituting `h[k·L+p] = h_p[k]` and collecting exponentials), which
//! makes the tests verifiable with a direct FFT.
//!
//! **Passband scaling:**
//! `bank.scale = L / Σh`, so `|H(0)| · scale/L = Σh · L/(Σh·L) = 1` → 0 dB.

use rustfft::{num_complex::Complex, FftPlanner};

use crate::player::convolver::{full_from_half, Bank, BLOCK};

type C = Complex<f64>;

/// `NFFT = 2·BLOCK`. Defined locally from the public `BLOCK` constant.
const NFFT: usize = 2 * BLOCK;

// ─── Bank response ───────────────────────────────────────────────────────────

/// Evaluate the frequency response of a [`Bank`] on the full output-rate grid.
///
/// Returns `NFFT·L/2 + 1` magnitude values in **dB** on the grid
/// `Ω_q = 2π·q/(NFFT·L)` for `q = 0…NFFT·L/2`.
///
/// The passband reads 0 dB: the output is scaled by `bank.scale / L` so that
/// the filter's DC gain (`Σh = L / scale`) maps to unity.
///
/// See the module doc for the full derivation.
/// Single-threaded; no global rayon pool use at runtime.
pub fn bank_response(bank: &Bank) -> Vec<f64> {
    let l = bank.l;
    let grid = NFFT * l / 2 + 1;
    let scale = bank.scale / l as f64;

    // ── Step 1: assemble branch DFTs H_p[m] for all p and m ────────────────
    // (-1)^(m·b):
    //   • b even  → (−1)^(m·b) = 1 for all m  (m·b is always even)
    //   • b odd   → (−1)^(m·b) = (−1)^m        (odd·odd = odd; odd·even = even)
    let branch_h: Vec<Vec<C>> = (0..l).map(|p| branch_dft(bank, p)).collect();

    // ── Step 2: evaluate H(q) for q = 0…grid−1 ─────────────────────────────
    let two_pi_over_nl = 2.0 * std::f64::consts::PI / (NFFT * l) as f64;
    let mut out = Vec::with_capacity(grid);
    for q in 0..grid {
        let m = q % NFFT;
        let mut h = C::new(0.0, 0.0);
        for p in 0..l {
            let phase = -(two_pi_over_nl * q as f64 * p as f64);
            let factor = C::new(phase.cos(), phase.sin());
            h += factor * branch_h[p][m];
        }
        let mag = h.norm() * scale;
        out.push(20.0 * mag.max(1e-300).log10());
    }
    out
}

/// Step 1 for branch `p`: `H_p[m] = Σ_b (−1)^{m·b} · P[p][b][m]`, NFFT bins.
fn branch_dft(bank: &Bank, p: usize) -> Vec<C> {
    let parts = bank.branch_spectra(p);
    let mut h_p = vec![C::new(0.0, 0.0); NFFT];
    for (b, half) in parts.iter().enumerate() {
        // The bank keeps half spectra; the other half is their mirror.
        let part = full_from_half(half);
        if b % 2 == 0 {
            // factor = +1 for every bin
            for m in 0..NFFT {
                h_p[m] += part[m];
            }
        } else {
            // factor = (−1)^m: +1 for even bins, −1 for odd bins
            for m in 0..NFFT {
                if m % 2 == 0 {
                    h_p[m] += part[m];
                } else {
                    h_p[m] -= part[m];
                }
            }
        }
    }
    h_p
}

// ─── Multistage response ─────────────────────────────────────────────────────

/// The response a multistage chain plays: stage 0 (`stage0`, branch 0 of the
/// blob at the source rate, `Bank::stage0`) followed by the short `chain` up
/// to ×L, on the same grid as [`bank_response`] at that L:
///
/// `|G(q)| = |H_0[q mod NFFT]| · |H_1(q)| · scale0 / L`
///
/// — branch 0 at the source rate is `H_0(e^{iΩL})`, periodic in the source
/// rate, whose DFT at `ΩL = 2π·m/NFFT` is step 1 for its one branch; `H_1` is
/// the chain's impulse response at the output rate (each stage zero-stuffed
/// by its ratio, then its taps), exact on the `NFFT·L` grid since it is far
/// shorter. The DC gain is `Σh_0 · L · scale0 / L ≈ 1` (0 dB).
///
/// Not the whole blob's response: in the passband the two agree (the blob's
/// aliases there are its stopband, ≤ −200 dB), but above the source band the
/// product shows the chain's ≥ 210 dB rejection of stage 0's images, not the
/// long filter's own floor — what is actually heard.
pub fn ms_response(stage0: &Bank, chain: &[crate::audio::converter::dsp::ms_h1::H1Stage]) -> Vec<f64> {
    let l: usize = chain.iter().map(|s| s.ratio).product();
    let grid = NFFT * l / 2 + 1;
    let scale = stage0.scale / l as f64;
    let h0 = branch_dft(stage0, 0);
    let h1 = chain_spectrum(chain, NFFT * l);
    (0..grid)
        .map(|q| {
            let mag = h0[q % NFFT].norm() * h1[q].norm() * scale;
            20.0 * mag.max(1e-300).log10()
        })
        .collect()
}

/// The short chain's impulse response at the output rate: from a unit
/// impulse at the source rate, each stage zero-stuffs by its ratio and
/// convolves with its taps.
fn chain_impulse(chain: &[crate::audio::converter::dsp::ms_h1::H1Stage]) -> Vec<f64> {
    let mut g = vec![1.0f64];
    for st in chain {
        let mut up = vec![0.0f64; (g.len() - 1) * st.ratio + 1];
        for (i, &v) in g.iter().enumerate() {
            up[i * st.ratio] = v;
        }
        let mut out = vec![0.0f64; up.len() + st.taps.len() - 1];
        for (i, &u) in up.iter().enumerate() {
            if u != 0.0 {
                for (j, &t) in st.taps.iter().enumerate() {
                    out[i + j] += u * t;
                }
            }
        }
        g = out;
    }
    g
}

/// `n`-point DFT of the chain's impulse response (`n` ≥ its length).
fn chain_spectrum(chain: &[crate::audio::converter::dsp::ms_h1::H1Stage], n: usize) -> Vec<C> {
    let g = chain_impulse(chain);
    assert!(g.len() <= n, "the chain is longer than the grid");
    let mut buf: Vec<C> = g.iter().map(|&t| C::new(t, 0.0)).collect();
    buf.resize(n, C::new(0.0, 0.0));
    FftPlanner::<f64>::new().plan_fft_forward(n).process(&mut buf);
    buf
}

/// Thinned version of [`bank_response`] for UI rendering.
/// `stride = 1` returns every bin; larger values subsample.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub fn bank_response_display(bank: &Bank, stride: usize) -> Vec<f64> {
    bank_response(bank).into_iter().step_by(stride.max(1)).collect()
}

// ─── Arbitrary FIR response ──────────────────────────────────────────────────

/// Frequency response of an arbitrary FIR filter by zero-padded FFT.
///
/// Returns `n_out/2 + 1` magnitude values in dB on the grid `Ω_k = 2πk/n_out`
/// (where `n_out = max(n, taps.len()).next_power_of_two()`), normalized so the
/// peak passband value reads 0 dB. Useful for SUB/AA/XTC filter inspection.
///
/// `n` is a hint for the desired grid resolution; the actual FFT size is
/// rounded up to the next power of two.
pub fn fir_response(taps: &[f64], n: usize) -> Vec<f64> {
    let fft_n = n.max(taps.len()).next_power_of_two().max(64);
    let mut buf: Vec<C> = taps.iter().map(|&t| C::new(t, 0.0)).collect();
    buf.resize(fft_n, C::new(0.0, 0.0));
    FftPlanner::<f64>::new().plan_fft_forward(fft_n).process(&mut buf);
    let half = fft_n / 2 + 1;
    let mags: Vec<f64> = buf[..half].iter().map(|c| c.norm()).collect();
    let peak = mags.iter().cloned().fold(0.0f64, f64::max).max(1e-300);
    mags.iter().map(|&m| 20.0 * (m / peak).max(1e-300).log10()).collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::convolver::Alignment;
    use std::f64::consts::PI;

    /// Build a windowed-sinc (Hamming) lowpass filter.
    fn windowed_sinc(taps: usize, cutoff_norm: f64) -> Vec<f64> {
        let m = taps - 1;
        (0..taps)
            .map(|i| {
                let n = i as f64 - m as f64 * 0.5;
                let sinc = if n.abs() < 1e-12 {
                    cutoff_norm
                } else {
                    (PI * cutoff_norm * n).sin() / (PI * n)
                };
                let w = 0.54 - 0.46 * (2.0 * PI * i as f64 / m as f64).cos();
                sinc * w
            })
            .collect()
    }

    /// Core correctness check: bank_response must equal the direct NFFT·L-point
    /// DFT of the full filter (scaled by bank.scale/L).
    ///
    /// Tolerance: relative error < 1e-6 for |H| > 1e-6 × peak (deep stopband excluded).
    fn check(taps_count: usize, l: usize) {
        let cutoff = 0.85 / l as f64;
        let coeffs = windowed_sinc(taps_count, cutoff);

        // Direct NFFT·L-point DFT of the full filter.
        let grid_n = NFFT * l;
        let mut buf: Vec<C> = coeffs.iter().map(|&t| C::new(t, 0.0)).collect();
        buf.resize(grid_n, C::new(0.0, 0.0));
        FftPlanner::<f64>::new().plan_fft_forward(grid_n).process(&mut buf);
        let half = grid_n / 2 + 1;
        let direct: Vec<f64> = buf[..half].iter().map(|c| c.norm()).collect();

        let bank = Bank::from_coeffs("test", &coeffs, l, Alignment::Linear, 44100 * l as u32);
        let response = bank_response(&bank);
        let s = bank.scale / bank.l as f64;

        // Peak of the direct spectrum (for relative-error threshold).
        let peak_want = direct.iter().cloned().fold(0.0f64, f64::max).max(1e-300) * s;

        assert_eq!(response.len(), half, "grid length mismatch L={l}");
        for q in 0..half {
            let got = 10f64.powf(response[q] / 20.0);
            let want = direct[q] * s;
            // Skip deep-stopband bins (>120 dB below peak): both FFTs are in
            // numerical noise there, so relative error is meaningless.
            if want > peak_want * 1e-6 {
                let rel = ((got - want) / want).abs();
                assert!(
                    rel < 1e-6,
                    "L={l} taps={taps_count} q={q}: relative error {rel:.2e} (got={got:.6e} want={want:.6e})"
                );
            }
        }
    }

    /// L=2 with 2 partitions per branch.
    #[test]
    fn bank_response_matches_direct_l2_two_partitions() {
        // NFFT = 65536; 2 branches × 2 partitions = 4 spectra of NFFT each.
        // Direct FFT grid: 2·NFFT = 131072. taps_count < grid_n avoids
        // truncation in the direct-DFT comparison buffer.
        check(NFFT * 2 - 1, 2);
    }

    /// L=4, one partition per branch.
    #[test]
    fn bank_response_matches_direct_l4() {
        check(NFFT / 4 + 1, 4);
    }

    /// L=8, one partition per branch.
    #[test]
    fn bank_response_matches_direct_l8() {
        check(NFFT / 8 + 1, 8);
    }

    /// Passband of a normalized lowpass reads 0 dB ± 0.1 dB.
    #[test]
    fn passband_near_zero_db() {
        let l = 4usize;
        let coeffs = windowed_sinc(NFFT / 2 + 1, 0.9 / l as f64);
        let bank = Bank::from_coeffs("test", &coeffs, l, Alignment::Linear, 44100 * l as u32);
        let resp = bank_response(&bank);
        // DC bin should be ≈ 0 dB.
        let dc = resp[0];
        assert!(
            dc.abs() < 0.1,
            "passband DC reads {dc:.3} dB (want ≈ 0 dB)"
        );
    }

    /// The stopband is the whole point of the |H| view (−185…−221 dB for the
    /// production filters), so the relative per-bin check above is not enough:
    /// here the ABSOLUTE complex error against the direct DFT, over every bin,
    /// must stay below −240 dB of the peak. Kaiser β = 20 gives a stopband near
    /// −190 dB, so those bins are genuinely checked.
    fn check_absolute(taps_count: usize, l: usize) {
        let cutoff = 0.85 / l as f64;
        let win = crate::player::analytics::spectra::kaiser(taps_count, 20.0);
        let m = (taps_count - 1) as f64;
        let coeffs: Vec<f64> = (0..taps_count)
            .map(|i| {
                let n = i as f64 - m * 0.5;
                let sinc = if n.abs() < 1e-12 { cutoff } else { (PI * cutoff * n).sin() / (PI * n) };
                sinc * win[i]
            })
            .collect();

        let grid_n = NFFT * l;
        let mut buf: Vec<C> = coeffs.iter().map(|&t| C::new(t, 0.0)).collect();
        buf.resize(grid_n, C::new(0.0, 0.0));
        FftPlanner::<f64>::new().plan_fft_forward(grid_n).process(&mut buf);

        let bank = Bank::from_coeffs("test", &coeffs, l, Alignment::Linear, 44100 * l as u32);
        let got_db = bank_response(&bank);
        let s = bank.scale / bank.l as f64;
        let peak = buf[..grid_n / 2 + 1].iter().map(|c| c.norm()).fold(0.0f64, f64::max) * s;

        // bank_response returns magnitudes; compare magnitudes, absolute error.
        let mut worst = 0.0f64;
        let mut deepest_db = 0.0f64;
        for q in 0..grid_n / 2 + 1 {
            let got = 10f64.powf(got_db[q] / 20.0);
            let want = buf[q].norm() * s;
            worst = worst.max((got - want).abs() / peak);
            deepest_db = deepest_db.min(20.0 * (want / peak).max(1e-300).log10());
        }
        let worst_db = 20.0 * worst.max(1e-300).log10();
        assert!(
            deepest_db < -150.0,
            "L={l}: test filter stopband only reaches {deepest_db:.1} dB — not a deep-stopband test"
        );
        assert!(
            worst_db < -240.0,
            "L={l} taps={taps_count}: absolute error {worst_db:.1} dB re peak (must be < -240 dB)"
        );
    }

    #[test]
    fn bank_response_absolute_error_deep_stopband_l2_multi_partition() {
        check_absolute(NFFT * 2 - 1, 2);
    }

    #[test]
    fn bank_response_absolute_error_deep_stopband_l8_multi_partition() {
        // 524287 taps / 8 branches = 65536 per branch → 2 partitions each.
        check_absolute(NFFT * 8 - 1, 8);
    }

    /// Timing: load a 30M-tap bank and measure bank_response() time.
    ///
    /// Run with (the folder that holds fir_30M_352800_linear_phase.npy):
    ///   set AURA_FILTER_DIR=<folder>
    ///   cargo test --profile fast bank_response_timing_30m -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bank_response_timing_30m() {
        use crate::player::convolver::{Alignment, Bank};
        let Some(dir) = std::env::var_os("AURA_FILTER_DIR") else {
            println!("AURA_FILTER_DIR is not set: no 30M filter to load");
            return;
        };
        let path_buf = std::path::Path::new(&dir).join("fir_30M_352800_linear_phase.npy");
        let path = path_buf.to_str().expect("a UTF-8 filter path");
        let l = 8usize; // 352800 / 44100 = 8
        let out_rate = 352_800u32;

        let t_load = std::time::Instant::now();
        let bank = match Bank::load(path, l, Alignment::Linear, out_rate) {
            Ok(b) => b,
            Err(e) => { println!("Bank::load failed: {e}"); return; }
        };
        let load_ms = t_load.elapsed().as_millis();
        println!("Bank loaded: l={l} in {load_ms} ms");

        let t_resp = std::time::Instant::now();
        let _ = bank_response(&bank);
        let resp_ms = t_resp.elapsed().as_millis();
        println!("|H| build (bank_response): {resp_ms} ms for 30M-tap L=8 bank @ 352.8k");
    }

    /// A Kaiser (β 20) windowed-sinc blob for ×`l`, edge at 0.85 of the
    /// source Nyquist.
    fn kaiser_blob(taps_count: usize, l: usize) -> Vec<f64> {
        let cutoff = 0.85 / l as f64;
        let win = crate::player::analytics::spectra::kaiser(taps_count, 20.0);
        let m = (taps_count - 1) as f64;
        (0..taps_count)
            .map(|i| {
                let n = i as f64 - m * 0.5;
                let sinc = if n.abs() < 1e-12 { cutoff } else { (PI * cutoff * n).sin() / (PI * n) };
                sinc * win[i]
            })
            .collect()
    }

    /// The blob's stage 0 for ×`l` through `chain`, as the player builds it.
    fn stage0_of(h: &[f64], l: usize, chain: &[crate::audio::converter::dsp::ms_h1::H1Stage]) -> Bank {
        use crate::audio::converter::dsp::multistage::MsPlan;
        let h0: Vec<f64> = h.iter().step_by(l).copied().collect();
        let plan = MsPlan::new((h.len() - 1) / 2, l, h.iter().sum(), chain);
        Bank::stage0("test", &h0, h.len(), plan)
    }

    /// `ms_response` is the response of stage 0 followed by the chain: the
    /// direct DFT of their cascade (branch 0 zero-stuffed by L, convolved
    /// with the chain's impulse response, folded onto the NFFT·L grid — the
    /// DTFT samples, however long the cascade), scaled by scale0/L. Relative
    /// error < 1e-6 down to −120 dB, absolute < −240 dB re peak on every bin;
    /// branch 0 spans two partitions.
    #[test]
    fn ms_response_is_stage0_times_the_chain() {
        for (base, l) in [(44_100u32, 8usize), (48_000, 2), (44_100, 4)] {
            let chain = crate::audio::converter::dsp::ms_h1::h1_chain(base, l).expect("a shipped chain");
            let h = kaiser_blob((NFFT + NFFT / 2) * l + 1, l);
            let bank = stage0_of(&h, l, chain);
            let got_db = ms_response(&bank, chain);

            let h0: Vec<f64> = h.iter().step_by(l).copied().collect();
            let g1 = chain_impulse(chain);
            let n = NFFT * l;
            let mut buf = vec![C::new(0.0, 0.0); n];
            for (k, &a) in h0.iter().enumerate() {
                for (j, &b) in g1.iter().enumerate() {
                    buf[(k * l + j) % n] += C::new(a * b, 0.0);
                }
            }
            FftPlanner::<f64>::new().plan_fft_forward(n).process(&mut buf);
            let s = bank.scale / l as f64;
            assert_eq!(got_db.len(), n / 2 + 1, "x{l}: grid");
            let peak = buf[..n / 2 + 1].iter().map(|c| c.norm()).fold(0.0f64, f64::max) * s;
            let mut worst = 0.0f64;
            for q in 0..n / 2 + 1 {
                let got = 10f64.powf(got_db[q] / 20.0);
                let want = buf[q].norm() * s;
                if want > peak * 1e-6 {
                    let rel = ((got - want) / want).abs();
                    assert!(rel < 1e-6, "x{l} q={q}: relative error {rel:.2e}");
                }
                worst = worst.max((got - want).abs() / peak);
            }
            let worst_db = 20.0 * worst.max(1e-300).log10();
            assert!(worst_db < -240.0, "x{l}: absolute error {worst_db:.1} dB re peak");
            assert!(got_db[0].abs() < 1e-6, "x{l}: DC reads {:.2e} dB", got_db[0]);
        }
    }

    /// In the passband (up to 0.8 of the source Nyquist) the multistage
    /// response is the whole blob's within 1e-6 dB; above the source band it
    /// is the chain's rejection of stage 0's images, which is why the
    /// analyzer shows the product, not the blob.
    #[test]
    fn ms_response_passband_is_the_blob_s() {
        let l = 8usize;
        let chain = crate::audio::converter::dsp::ms_h1::h1_chain(44_100, l).expect("the shipped x8 chain");
        let h = kaiser_blob((NFFT + NFFT / 2) * l + 1, l);
        let ms = ms_response(&stage0_of(&h, l, chain), chain);
        let single = bank_response(&Bank::from_coeffs("test", &h, l, Alignment::Linear, 44_100 * l as u32));
        assert_eq!(ms.len(), single.len());
        // The source Nyquist is bin NFFT/2.
        let edge = (0.8 * (NFFT / 2) as f64) as usize;
        let worst = (0..edge).map(|q| (ms[q] - single[q]).abs()).fold(0.0f64, f64::max);
        assert!(worst < 1e-6, "passband differs by {worst:.2e} dB");
        // The first image (around the source rate) is below −200 dB.
        let image = ms[NFFT - NFFT / 4..NFFT + NFFT / 4].iter().cloned().fold(f64::MIN, f64::max);
        assert!(image < -200.0, "the first image reads {image:.1} dB");
    }

    /// fir_response of a single tap (Dirac) is flat at 0 dB.
    #[test]
    fn fir_response_dirac_is_flat() {
        let dirac = vec![1.0f64];
        let resp = fir_response(&dirac, 256);
        for &db in &resp {
            assert!(
                db.abs() < 1e-6,
                "Dirac response reads {db:.6} dB (want 0)"
            );
        }
    }
}
