# AuraEngine DSP Architecture: Audit & Validation Manifesto

**For developers, DSP engineers, and code auditors**

## Purpose

This manifesto codifies the mathematical and numerical invariants that govern
AuraEngine's DSP core. Every commit, refactor, or optimization must pass a
strict audit against each item in this list.

Every rule below is stated as a property of the signal that can be measured
and tested. Where a rule is *motivated* by a perceptual argument, that
argument is labelled as a rationale and kept separate from the requirement —
because the requirement is enforceable and the rationale is not. This project
does not claim that every invariant it holds is audible; it claims they are
true, and that they are the right things to be true.

---

## SECTION 1. Signal Energy and Gain Staging

*Physical principle: conservation of energy (Parseval's theorem) and protection against digital destruction.*

**1.1. DC Gain Normalization**

- **Axiom:** No filter may alter the original loudness of a track unless the user explicitly requests it.
- **Requirement:** The sum of all coefficients of any filter — after windowing, cepstrum computation, and fade application — must equal exactly 1.0, to the precision of a 64-bit float.
- **How to verify in code:** Locate the final stage of filter generation. A normalization step of the form `h = h / sum(h)` must be present.

**1.2. Headroom Is an Output Ceiling**

- **Axiom:** Reconstruction puts inter-sample peaks above the source's sample peaks, and the minimum-phase branch gathers energy into sharper peaks still. Whatever limits them has to act on the finished render, where those peaks exist, and must change nothing else.
- **Requirement:** The Headroom setting names the output ceiling in dBTP: Off keeps the shipped −0.5 dBTP; −0.5, −1.0 and −3.0 dB put the ceiling there. Nothing is scaled before the filter — the source reaches the convolution engine exactly as decoded. If the finished render's true peak is above the ceiling, the whole file is brought down to it by one scalar (§1.3); a file already below it passes through untouched. Until 1.2.3 this section required an attenuation before the FIR instead; on any loud master the true-peak ceiling then undid it, so the control changed nothing.
- **How to verify in code:** `converter/pipeline/prepare.rs` resolves the ceiling from `headroom_db` and logs `Headroom: output ceiling … dBTP`; no gain is applied there. The scalar is computed and applied by `converter/dsp/true_peak.rs` at the end of the chain.

**1.3. True Peak / Inter-Sample Peak Limiting**

- **Axiom:** Any reconstruction filter, whatever its length, can create excursions between samples above the sample peaks — and above 0 dBFS — when the analog waveform is reconstructed.
- **Requirement:** Before the final export, the signal must pass through an inter-sample peak scanner (Catmull-Rom or Sinc interpolation). If the true peak is above the ceiling set in §1.2, the entire file is scaled down by one scalar — no compression, no limiting — to prevent DAC clipping.
- **How to verify in code:** Confirm that the True Peak scanner is positioned at the very end of the processing chain, immediately before quantization to 24/32-bit output.

---

## SECTION 2. Phase-Transform Mathematics (Cepstrum & Aliasing)

*Physical principle: converting symmetric energy (linear phase) into asymmetric energy (minimum phase) without information loss and without introducing spurious noise.*

**2.1. Time-Aliasing Isolation**

- **Axiom:** Computing minimum phase via the complex cepstrum requires an FFT and a log-magnitude operation. If the FFT window equals the impulse length, the computational tails wrap back to the beginning — time-domain aliasing.
- **Requirement:** Zero-padding is mandatory. For filters shorter than 1 million taps, the cepstrum FFT size N\_fft must be at least 16× the filter length, rounded up to the nearest power of two. For larger filters, the multiplier scales down gracefully (8× for 1–5 M taps, 4× for 5–20 M taps, 2× beyond 20 M taps) to prevent memory exhaustion while still providing adequate isolation.
- **How to verify in code:** Audit `get_n_fft()` in `fir-optimizer/optimize.py`. Verify the formula: `n_fft = max((mult * N).next_power_of_two(), 2^23)` with the multiplier table above. The 2^23 floor is there for short filters: at 2^20 the minimum-phase twin of the 5k filter at 96 kHz still carried 1.7·10⁻⁴ dB of aliasing ripple.

**2.2. Truncation-Ripple Protection (Hann Tail Fade)**

- **Axiom:** Abruptly truncating a minimum-phase impulse at tap N causes rectangular truncation, which produces Gibbs-phenomenon ripple in the frequency domain.
- **Requirement:** A smooth fade window (Hann tail) must be applied to the last 5–10% of the generated impulse, driving it to an absolute mathematical zero.
- **How to verify in code:** Locate the application of a Hann (or cosine-rolloff) window applied strictly to the right-hand tail of the coefficient array.
- **Current status:** Not required. From 1 M taps up the impulse decays naturally to below −300 dB before truncation. The 5 k rung added in 1.2.7 is below the ~100 K line this requirement was written against, and was checked: its Kaiser window (β ≈ 20–23, fitted to the length) takes the edge coefficients to −212…−249 dB below the peak — no higher than the 1 M filter's own edge (−208 dB) — and the measured stopband of the finished blobs, which includes any truncation effect, is −193…−220 dB.

---

## SECTION 3. Impulse Response and Spectral Purity (Apodizing & Windows)

*What this section constrains: the distribution of impulse-response energy
around a transient, and the depth of the stopband. Both are measurable
properties of the filter.*

**3.1. Zero Pre-Ringing**

- **What is true:** A linear-phase FIR distributes its impulse-response
  energy symmetrically about the main peak, so a transient in the output is
  preceded by energy the source did not have there. A minimum-phase filter of
  the same magnitude response places that energy after the peak instead. This
  is a redistribution in time, not a removal of anything.
- **Rationale, not a claim of audibility:** the argument for preferring the
  minimum-phase arrangement on transients is that a precursor has no physical
  counterpart in the recorded event. Whether it is audible on properly
  bandlimited material is contested, and this project has not settled it — see
  the discussion linked from `docs/06-hybrid-phase-proof.md`. The requirement
  below is enforced because it is a well-defined property of the impulse
  response, not because an audible benefit has been demonstrated.
- **Requirement:** The final impulse of both the apodizing pre-filter and the main filter (in minimum-phase mode) must have an absolute zero to the left of the main peak.
- **How to verify in code/tests:** Plot the impulse response in the time domain. Index `[0]` must hold the maximum peak; all indices before it must equal `0.0`.

**3.2. Stopband Attenuation**

- **What is true:** aliasing folds content back below Nyquist as components
  that were never in the source and bear no harmonic relation to it. Unlike
  the pre-ring question this is not a matter of taste — the folded energy is
  measurable in the output spectrum and is unambiguously an error. The
  requirement is to put it far enough down that no listening argument about it
  can arise.
- **Requirement:** The stopband attenuation must fall below −140 dB with no ripple whatsoever.
- **How to verify in code:** The pre-filter must use a high-order window function — Kaiser with β ≥ 14.0, or Blackman-Harris. Gaussian or Hamming windows are unacceptable; they produce ripple in the −50 to −90 dB range.

**3.3. Correct Apodizing Zone**

- **What is true:** the apodizing filter must cover the transition band of
  the source's own anti-alias filter (20–22.05 kHz for CD-rate material) and
  must not reach down into the passband. A cutoff placed too low removes
  recorded content; that is a real, measurable loss, and it is the failure
  mode this rule exists to prevent.
- **Requirement:** The roll-off must begin at a precisely defined boundary (e.g., 18 kHz, 19 kHz, or 20 kHz) and reach the noise floor strictly before the Nyquist frequency of the source file (22.05 kHz for 44.1 kHz input).
- **How to verify in code:** Audit the sinc generator. There must be no erroneous frequency-scaling factors (such as a stray `2.0 * π` that shifts the cutoff frequency).

---

## SECTION 4. Computational Architecture and Numerical Precision

*What this section constrains: arithmetic precision along the whole path, so
that accumulated rounding error stays far below the noise floor of any source
material rather than becoming something to argue about.*

**4.1. End-to-End FP64 (or Higher)**

- **Axiom:** A 10-million-tap filter requires billions of multiply-accumulate (MAC) operations per second of audio. Using 32-bit arithmetic (FP32) accumulates rounding error that destroys micro-detail and raises the noise floor.
- **Requirement:** All NumPy arrays, Rust vectors, and GPU compute kernels must be strictly initialized as float64 (`f64`). The standalone offline converter uses TwoFloat (double-double, ~106-bit) arithmetic throughout the convolution chain for even greater headroom.
- **How to verify in code:** Perform a global search. If any audio buffer or convolution kernel is cast to float32 — even temporarily for GPU transfer — it is a critical blocker bug.

**4.2. FFT Convolution (Overlap-Save / Overlap-Add)**

- **Axiom:** Direct time-domain convolution of a 10-million-tap filter would take years per track.
- **Requirement:** The engine must use Overlap-Save or Overlap-Add partitioned convolution in the frequency domain. Block sizes must be tuned to fit CPU L1/L2/L3 cache or GPU VRAM efficiently.
- **How to verify in code:** The CPU path uses an OLA block size of 32 768 samples (optimally fitting L3 cache), implemented in `audio/dsp_core.rs` with Kahan compensated summation. The GPU path uses a partitioned Overlap-Save shader (`audio/gpu/wola.rs`, `audio/shaders/gpu_ola.wgsl`).

---

## Auditor / QA Checklist

Before every release, or when merging a pull request, the developer must check off each item:

- [x] Magnitude response plots generated. Stopband attenuation reaches −140 dB. No ripple visible.
- [x] Impulse response plotted. In minimum-phase mode, pre-ringing equals zero.
- [x] A test audio file has been passed through the system end-to-end. The output file contains no clipping (True Peak scanner reports no overloads).
- [x] `sum(h) == 1.0` verified by unit tests for every filter mode.
- [x] All GPU computations confirmed as FP64 by profiler.

---

## Implementation Status (audit 2026-07-05)

> **Note on file references:** `filter_design.rs` and `fir.rs` appeared in an earlier monorepo layout and are not present in this converter-only branch. The equivalent logic now lives in `audio/dsp_core.rs` (CPU OLA engine) and `audio/gpu/` (GPU OLA/OLS pipeline). The realtime Player mentioned in §1.2 history is also not part of this branch; the converter is the sole shipping component.

| §   | Requirement                      | Status    | Implementation (file / note)                                                                               |
| --- | -------------------------------- | --------- | ---------------------------------------------------------------------------------------------------------- |
| 1.1 | DC Gain `sum(h) == 1.0`          | ✅ PASS  | `fir-optimizer/optimize.py` — filter normalized at each generation stage; `process.rs:1045` confirms sum-normalization in Rust |
| 1.2 | Headroom = output ceiling        | ✅ PASS  | `converter/pipeline/prepare.rs` resolves the dBTP ceiling, nothing is scaled before the FIR; `converter/dsp/true_peak.rs` applies one scalar at the end |
| 1.3 | True Peak Scanner                | ✅ PASS  | `converter/dsp/true_peak.rs` — 4× polyphase sinc (Lanczos-4) inter-sample scanner, one scalar per file |
| 2.1 | Zero-Padding ≥ 16× (adaptive)   | ✅ PASS  | `fir-optimizer/optimize.py:76-86` — `get_n_fft()` with multiplier 2–16× depending on filter size         |
| 2.2 | Hann Tail Fade                   | ⚠️ N/A  | Not required: natural decay below −300 dB from 1 M taps up; at 5 k the fitted Kaiser window takes the edges to −212 dB or lower |
| 3.1 | Zero Pre-Ringing                 | ✅ PASS  | Cepstral min-phase transform places peak at index [0]; `utils/verify.rs` (`analyze_filter()`) confirms     |
| 3.2 | Stopband ≥ −140 dB               | ✅ PASS  | 5 k: **−193…−220 dB** (β fitted per rate); 1 M: −185…−203 dB; 30 M: −204…−221 dB (β = 14) — per blob `_meta.json`, docs/12 §9 |
| 3.3 | Apodizing Zone (20–24 kHz)       | ✅ PASS  | `fir-optimizer/optimize.py` cutoff logic; `converter/apodize.rs` — cutoff at 22 kHz, transition 20–24 kHz |
| 4.1 | End-to-End FP64+                 | ✅ PASS  | TwoFloat (~106-bit double-double) throughout the offline converter chain (`audio/dsp_core.rs`)             |
| 4.2 | FFT Convolution (OLA/OLS)        | ✅ PASS  | CPU: `audio/dsp_core.rs` (OLA, block = 32 768, Kahan summation); GPU: `audio/gpu/wola.rs` + `gpu_ola.wgsl` |

**Summary: 9/9 core requirements satisfied. §2.2 (Hann Tail Fade) is not applicable: re-evaluated in 1.2.7, when the minimum tap count fell to 5 K, and still not needed.**
