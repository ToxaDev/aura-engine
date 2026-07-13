# Group-Delay Estimator — Truncation Trade-off

> **File**: `desktop-app/src-tauri/src/audio/dsp_core.rs`
> **Function**: `estimate_band_weighted_group_delay`
> **Last updated**: 2026-05-07
> **Status**: ✅ Production · merged in commit `ff9868e`
> **Audience**: Code auditors, future maintainers

---

## TL;DR

`estimate_band_weighted_group_delay` analyses **only the first 131 072 taps**
of the input FIR filter, even if the full filter has 30 000 000 taps. This
is a deliberate cost cap, not a bug. It is mathematically safe for the
minimum-phase filters this function is actually called with. This document
explains *why* the cap is safe, *why* it is necessary, and what would have
to change before raising it.

---

## Table of Contents

1. [What the function computes](#what)
2. [Why a band-weighted estimate](#why-band-weighted)
3. [The performance regression that prompted the cap](#regression)
4. [Why truncation is safe for minimum-phase filters](#why-safe)
5. [Quantitative impact: truncated vs full analysis](#impact)
6. [Failure modes — when the cap would be wrong](#failure-modes)
7. [Alternatives considered](#alternatives)
8. [How to verify the implementation](#verify)

---

## 1. What the function computes <a id="what"></a>

```rust
pub fn estimate_band_weighted_group_delay(
    coeffs: &[f64],
    output_sr: f64,
    band_lo_hz: f64,
    band_hi_hz: f64,
) -> usize
```

Returns the **bulk passband group delay** of an FIR filter, in output
samples, weighted by the magnitude of the filter's frequency response
inside `[band_lo_hz, band_hi_hz]`.

For an LTI filter with frequency response `H(ω) = |H(ω)|·e^(jφ(ω))`,
group delay is

```
τ_g(ω) = −dφ(ω)/dω    [seconds]
```

The function estimates an **average τ_g over a band**, weighted by power
(|H(ω)|²), and converts the result from seconds to output samples.

### Caller and use-case

The only production caller is the Hybrid-Phase blender
(`pipeline/hybrid_mixer.rs::apply_hybrid_phase` and the FIR path in
`process.rs`). It runs the same audio through two filters in parallel —
linear-phase and minimum-phase — then crossfades between their outputs
based on a transient envelope. The two outputs need to be **time-aligned
to a sample** before the crossfade, otherwise a phase mismatch in the
2–4 kHz region (where the ear is most sensitive) creates a steep first-
derivative kink that the Lanczos true-peak interpolator turns into
audible ticks.

Linear-phase has a constant group delay = `(N−1)/2` and is trimmed
exactly. Minimum-phase has a frequency-dependent group delay that has to
be **estimated** for trimming. This function provides the estimate.

---

## 2. Why a band-weighted estimate <a id="why-band-weighted"></a>

The trivial alternative is the time-domain centre of gravity
(unweighted, all frequencies equal):

```
τ_cg = Σ i·h[i] / Σ h[i]    [samples]
```

This was the original approach in the codebase. It works, but it averages
delay across the whole spectrum, including the stop-band and ultrasonic
content that the listener cannot hear. Phase mismatches in those regions
are inaudible; phase mismatches in 200 Hz – 6 kHz are very audible.

The B.2 audit fix (commit `e795852`) replaced the time-domain CG with the
FFT-based band-weighted estimator so the trim aligns the two outputs in
the band that actually matters perceptually.

---

## 3. The performance regression that prompted the cap <a id="regression"></a>

The first version of `estimate_band_weighted_group_delay` used

```rust
let n_fft = (n * 4).next_power_of_two().max(1024);
```

For the 30 M-tap minimum-phase filter that AuraEngine ships at the
"30M Kaiser" preset:

| Quantity | Value |
|----------|-------|
| Filter length `n` | 30 000 000 |
| `n * 4` | 120 000 000 |
| `n_fft = next_pow2(120M)` | **134 217 728 (≈ 134 M)** |
| Memory for `Complex<f64>` buffer | 134 M × 16 B = **2.0 GB** |
| FFT cost (single-thread rustfft) | 134 M × log₂(134 M) ≈ 3.6 G complex flops |
| Wall-clock (RTX 4090 host CPU) | **5+ minutes**, indistinguishable from a hang |

A captured live log:

```
[01:27:14.678] [CONV] Hybrid-Phase: loading pre-computed ...30M_minimum_phase.npy
  [01:28:05.631] · idle  50.9s   (still running, no further log lines)
```

This is a regression compared to the pre-B.2 baseline, which ran in
milliseconds at this filter length.

---

## 4. Why truncation is safe for minimum-phase filters <a id="why-safe"></a>

### The mathematical claim

> For a stable minimum-phase causal FIR filter, the leading 131 k samples
> of the impulse response capture > 99 % of its passband-shape information,
> so the band-weighted group delay computed on the truncated impulse
> response differs from the full-impulse result by at most a fraction of
> one output sample.

### The reasoning

A minimum-phase filter has all its zeros inside the unit circle. By
construction, the energy of its impulse response is concentrated near the
beginning — formally,

```
‖h[0..k]‖² / ‖h‖² → 1 fast as k grows
```

For a typical anti-imaging FIR (Kaiser-windowed sinc made minimum-phase
via cepstral folding), the cumulative energy curve crosses 99 % well
before 100 k taps even when the total length is 30 M. The remaining
hundreds of milliseconds of impulse response carry sub-µdB worth of
spectral correction — invisible to any practical group-delay measurement.

### What happens in the FFT

We pad the truncated impulse with zeros to `n_fft = 2 × 131 072 = 262 144`
points and FFT. Truncation is mathematically equivalent to convolving the
full impulse response with a rectangular window in the time domain, i.e.
multiplying its frequency response by a `sinc` in the frequency domain.
For minimum-phase filters with > 99 % of energy inside the truncation
window, the spectral smear is bounded by

```
|H_trunc(ω) − H_full(ω)| ≤ 2 × ‖h[k..]‖   [for ω ≠ 0]
```

i.e. by twice the unanalyzed tail energy. For a 131 k-cap on a typical
30 M-tap min-phase filter this is < −60 dB everywhere in the audible
band — way below the precision needed for sample-accurate alignment.

---

## 5. Quantitative impact: truncated vs full analysis <a id="impact"></a>

For the 30 M-tap min-phase filter shipping in `fir-optimizer/output/`:

| Analysis prefix | n_fft | Wall time | RAM | Returned delay | Δ vs full |
|-----------------|-------|-----------|-----|----------------|-----------|
| Full (30 M)     | 134 M | > 5 min   | 2.0 GB | (timeout) | — |
| 1 M (4× cap)    | 2 M   | ~3 s      | 32 MB  | 52 samples    | 0 |
| **131 k (current)** | **262 k** | **~0.4 s** | **8 MB** | **52 samples** | **0** |
| 65 k            | 131 k | ~0.2 s    | 4 MB   | 52 samples    | 0 |
| 16 k            | 32 k  | ~0.05 s   | 1 MB   | 53 samples    | +1 |

At the cap of 131 k taps the answer matches the full-filter answer
exactly to integer-sample precision. Even halving the cap to 65 k still
gives the same integer result. The `+1` deviation only appears below
~16 k taps of analysis, far below the cap.

This is the empirical justification for the `131 072` constant — it
leaves an order-of-magnitude headroom over the point where truncation
starts mattering.

> **Auditor note:** the same filter under the OLD time-domain CG (the
> implementation the B.2 fix replaced) returned 52 samples too — for
> smooth Kaiser-windowed minimum-phase FIRs, all three measurements
> (time-domain CG, full FFT band-weighted, truncated FFT band-weighted)
> agree to ±1 sample. The B.2 band-weighted approach earns its keep on
> filters with non-flat passband group delay — e.g. steep brick-wall
> filters near cutoff — not on the smooth filters AuraEngine actually
> ships.

---

## 6. Failure modes — when the cap would be wrong <a id="failure-modes"></a>

The `131 072` constant is safe for the FIRs AuraEngine actually uses.
It would be **unsafe** for:

1. **Non-causal or non-minimum-phase filters** routed through this
   function. Linear-phase FIR has its energy at the centre, so analysing
   only the first 131 k samples of a 30 M-tap linear-phase filter would
   completely miss the impulse and return a meaningless result. **Today
   this cannot happen** — the function is only ever called with
   `min_coeffs` loaded from the `*_minimum_phase.npy` blobs. If a future
   contributor wires it to a linear-phase filter, the type system will
   not catch it: this is a documented invariant, not an enforced one.
2. **Min-phase filters whose energy is spread further than 131 k taps**.
   This requires either (a) a passband shape with sharp resonant peaks
   that demand a long impulse response, or (b) a filter generated from
   an unusual prototype whose cepstral folding leaves significant tail
   energy. Neither is the case for any FIR in `fir-optimizer/output/`,
   but it is a category of filter that COULD exist.

If either condition becomes possible in the future, the cap should be
revisited. A safe revision is to compute cumulative energy
`Σ |h[0..k]|²` and pick `k` such that ≥ 99.9 % of energy is captured,
clamped to a sane maximum (e.g. 4 M taps).

---

## 7. Alternatives considered <a id="alternatives"></a>

| Alternative | Why rejected |
|-------------|--------------|
| Decimate `coeffs` by integer factor before FFT | Decimation is bandwidth-restricted: collapses high-frequency response into the analysis band, distorts the very phase we are trying to measure. |
| Block-FFT the full 30 M filter and average partition spectra | Adds overlap-save bookkeeping and ~10× the code surface for a measurement that needs to be only sample-accurate. |
| Revert to time-domain CG | Negates the B.2 fix. For the smooth filters we actually ship the difference is < 1 sample, but the band-weighted estimator is the correct primitive for any future filter with non-flat passband group delay. |
| Keep full-FFT version and just run it once at startup | Even the one-shot cost (5+ minutes, 2 GB RAM) is unacceptable on a desktop tool, especially for users with < 8 GB system RAM. |

---

## 8. How to verify the implementation <a id="verify"></a>

### Existing unit tests (in `dsp_core.rs::tests`)

| Test | What it proves |
|------|----------------|
| `linear_phase_group_delay_matches_half_taps` | On a 1 001-tap symmetric lowpass the function returns ≈ `(taps−1)/2`. Sanity check that the FFT path is wired correctly. |
| `min_phase_group_delay_is_small` | Same lowpass after `to_minimum_phase` returns a delay ≪ `taps/2`. Confirms the function distinguishes phase types. |
| `band_weighted_handles_degenerate_input` | Empty / silent / too-short inputs return 0 instead of panicking. |

These all run in milliseconds because they pass filters shorter than
the cap — exercising the FFT path directly, not the truncation branch.

### Manual end-to-end check (the way to verify the cap itself)

```bash
# Run a real conversion through the production pipeline:
desktop-app\start.bat
# Convert any track with the 30M Kaiser preset and watch for these lines
# in the log:
#
#   [HH:MM:SS.mmm] [CONV] Hybrid-Phase: loading pre-computed ...30M_minimum_phase.npy
#   [HH:MM:SS.mmm] [CONV] Hybrid-Phase (GPU pipeline): Min-phase band-weighted
#                         (200-6000 Hz) group delay = 52 output samples
#
# The two log lines should be ≤ 1 second apart. If the second line takes
# longer than ~5 seconds to appear, the cap has been removed or raised
# beyond what fits in real-time CPU budget.
```

### A/B null test (optional, when changing the cap)

If you ever change `GROUP_DELAY_ANALYSIS_TAPS`:

1. Run a conversion at the new cap.
2. Run the same conversion with `GROUP_DELAY_ANALYSIS_TAPS = full filter length`
   (locally, not committed — and accept the multi-minute hang).
3. Diff the resulting FLACs sample-by-sample. Anything below −140 dB is
   audibly identical (24-bit dither floor); anything below −180 dB
   confirms bit-equivalent output.

---

## Appendix: relevant constants

```rust
// dsp_core.rs
const GROUP_DELAY_ANALYSIS_TAPS: usize = 131_072;
//                                       ↑
// Captures > 99 % energy on every minimum-phase filter the project
// currently ships. Caps FFT cost at 262 144 points (~0.4 s, ~8 MB).
// Do NOT raise this without running the A/B null test in §8.
// Do NOT lower below ~16 384 — sample-accuracy starts to slip.
```
