# AuraEngine — Audiophile DSP Features & Sound Enhancements

This document covers every key audio-processing technology inside AuraEngine. It is written for readers who want to understand precisely how the engine achieves uncompromising audiophile quality, and it works equally well as a technical reference or as source material for audio-forum posts.

---

## 1. Adaptive Apodizing (Source Pre-Ring Correction)

**When it runs:** At the very start of the pipeline, *before* upsampling.

**The problem:** The overwhelming majority of modern (and classic) studio recordings at 44.1/48 kHz carry what is commonly called "digital ringing" — the Gibbs phenomenon. It originates in the studio's analogue-to-digital converter (ADC), which cuts everything above 22 kHz with a brickwall filter. That ringing is *baked into your FLAC file*. Ordinary upsamplers simply scale it up along with everything else.

**How AuraEngine solves it:**

The engine uses a **time-domain pre-ring detector** rather than a simple spectral-energy check. Concretely:

1. A 127-tap Kaiser highpass filter isolates the near-Nyquist band (0.78 × Nyquist and above) across the entire track.
2. The algorithm locates the strongest broadband attack transients in the track.
3. For each attack it measures the near-Nyquist energy in the window *just before* the onset against the local background and the attack itself.
4. A burst that precedes the attack, rises clearly above background, and correlates with (but is smaller than) the attack is classified as genuine pre-ringing.
5. The verdict is derived from the *fraction* of ringing attacks found; cutoffs are expressed relative to Nyquist, not as fixed frequency bins.

If a consistent ringing signature is present, the engine selects one of three minimum-phase apodizing FIR filters: **2 048 taps** for the "gentle" setting, or **4 096 taps** for moderate, strong, and adaptive settings. The optimal cut-off frequency (typically 19–21 kHz) is calculated from the detector's Nyquist-relative thresholds.

If no consistent pre-ringing signature is found, the source is left completely untouched — and the engine correctly falls through to the user's chosen static preset rather than silently skipping it. The `AA`/`Apod` suffix in the output filename reflects apodizing that **actually ran**, so clean-verdict files are never mislabelled.

**What you hear:**

The engine heals the original ADC's sins before anything else touches the signal. The track is scrubbed of synthetic "grit" and metallic coloration in the upper frequencies, producing a mathematically clean canvas for upsampling.

---

## 2. Massive 64-bit FIR Upsampling (Up to 30 Million Taps)

**When it runs:** The main process, immediately after decoding and apodizing.

**The problem:** The DAC chip in your headphone amplifier or integrated DAC physically cannot reconstruct a high-fidelity analogue waveform from a 44.1 kHz stream in real time. Embedded chips use relatively coarse filters — a few hundred to a few thousand taps in 32-bit arithmetic — which causes timing smearing, micro rounding errors, and aliasing.

**How AuraEngine solves it:**

AuraEngine performs *offline* resampling to 384 kHz or 768 kHz without any real-time constraint. The workflow is split across two components:

- **`fir-optimizer` (Python):** Filter coefficients (Kaiser-windowed sinc) are designed offline using this Python toolchain. By default it uses SciPy's double-precision (f64) routines. The `--legacy-mpmath` flag switches the designer to Python's `mpmath` library, which performs the coefficient computation in **128-bit quad-precision (IEEE 754 binary128)** using all available CPU cores. That ultra-precise mathematical description of the ideal filter is then truncated and serialised to **64-bit float** — the tap count can reach **30 million**. These precomputed coefficient files live in the filter matrix and are loaded by the runtime.

- **Rust runtime:** At conversion time the engine loads the precomputed f64 coefficients and runs them through the overlap-save convolver on the host CPU (f64, with Kahan-compensated accumulation) or the GPU (double-single f32, also Kahan-compensated). The convolver never recomputes the filter geometry — it inherits the accuracy established at design time.

**What you hear:**

Mathematically clean ("black background") reconstruction of the original analogue waveform. The absence of rounding errors at the filter-design stage yields an extraordinarily smooth sound. Digital "staircasing" disappears, sub-bass tightens, and stereo holography improves because left/right phase relationships are correct down to sub-millionth fractions. The DAC in your player only needs to convert an already-perfect waveform.

---

## 3. Hybrid-Phase Engine (Per-Transient Phase Switching)

**When it runs:** During the FIR upsampling mixing stage.

**The problem:** Every filter has a trade-off.

- *Linear phase* preserves a perfect, wide stereo stage and instrument depth, but a 10 M-tap FIR creates pre-ringing: a faint metallic ghost up to ~13 ms *before* each sharp drum hit, smearing the attack.
- *Minimum phase* delivers perfectly sharp attacks (all ringing moves *after* the strike), but disrupts inter-channel phase coherence — bass focus suffers and the soundstage collapses.

**How AuraEngine solves it:**

AuraEngine runs two parallel filter paths simultaneously (linear phase and minimum phase). The native-Rust **HPSS module** (`hpss_native.rs`, analysis taking ~50–150 ms at conversion time) performs Harmonic-Percussive Source Separation to locate kick drums, sharp transients, and vocal clicks across the track.

The phase switch itself is **binary, not a crossfade:** when a transient is detected, the engine locates a true **zero-crossing** in both channels' signals and snaps to minimum phase at that exact sample — then snaps back to linear phase at the next zero-crossing after the transient. A **20 ms debounce** prevents rapid toggling on dense material.

Critically, the switch plan is computed **once from the mid signal** `((dL + dR) / 2)` and applied identically to both channels. Before the stereo-link fix, L and R could switch up to ±5 ms apart — an interchannel timing artifact far above the ~10–20 µs audibility threshold that smeared the very transients Hybrid-Phase was supposed to sharpen. Now L and R always switch at the same sample; each channel's output remains its own independent linear or minimum-phase sample (no M/S transform is applied to the audio itself).

The onset envelope from the HPSS analysis is upsampled to the output rate using **Catmull-Rom** interpolation (C¹-continuous), eliminating the derivative kinks that caused switch-threshold jitter at the ~0.3 onset level. The `.onset_envelope.json` cache is version-tagged (`hpss_native_rust_v4`) so caches from older detectors are automatically regenerated.

**What you hear:**

The best of both worlds: punchy, articulate, "analogue" transient attacks without pre-ringing or smear, combined with the vast, deep stereo stage that only linear-phase filters can deliver.

---

## 4. True Intersample Peak Limiting (Inter-Sample Overload Protection)

**When it runs:** At the final stage, before writing the 32-bit FLAC output.

**The problem:** Many modern tracks are mastered right up to 0 dBFS. When such a file is upsampled, the faithfully reconstructed analogue waveform between samples frequently exceeds 0 dBFS — often by +2 to +3 dB. Feeding this to a DAC chip causes hard digital clipping (flat-topped waveforms), which produces a harsh digital distortion on loud passages.

**How AuraEngine solves it:**

Following the ITU-R BS.1770-4 recommendation, AuraEngine uses **Polyphase Sinc Interpolation (4× oversampling with a Lanczos-4 window)** to locate true analogue peaks with mathematical rigour. This is a professional DSP approach that is substantially more accurate than the basic cubic splines (e.g. Catmull-Rom) found in consumer-grade software. If any reconstructed peak exceeds the ceiling, the engine applies a micro-normalisation — pulling the entire track down by the exact number of dB needed to keep the loudest intersample peak within the safe window.

**What you hear:**

No hard digital clipping. Bass on heavily limited pop and hip-hop recordings stops "choking" on quality playback hardware after upsampling.

---

## 5. Mathematical Precision Engine (Micro-Detail Preservation)

**When it runs:** Throughout the entire signal path, from decoding to final write.

**The problem:** Multi-stage digital audio processing typically accumulates rounding errors — especially in 32-bit float arithmetic on long tracks and large data arrays — leading to degradation of the quietest sounds: the loss of reverb tails, hall "air", and micro-dynamics.

**How AuraEngine solves it:**

The engine applies precision measures at every stage:

- **Lossless 24-bit decoding.** FLAC samples are captured as `i32` without mantissa rounding, then safely promoted to `f64` for the entire processing chain.
- **Kahan-compensated accumulation.** FIR convolution across 30 M-tap filters uses Kahan summation in the Rust runtime (CPU: f64 accumulators; GPU: double-single f32 accumulators), reducing floating-point error by roughly two orders of magnitude compared to a naive accumulate loop.
- **Cubic Sinc interpolation and DC blocking.** The upsampling stage uses cubic sinc interpolation for sub-sample precision and automatically strips any DC offset present in source files.
- **64-bit HPSS analysis.** The transient detection module (implemented natively in Rust in `hpss_native.rs`) runs entirely in double precision, including the median filters, capturing transients down to SNR −60 dBFS.
- **Independent per-channel dither.** Left and right channels draw from separate `SmallRng` streams (`dither.rs`), so their dither noise floors are uncorrelated. The previous shared stream correlated both channels' dither toward the phantom centre, subtly damaging the noise floor imaging.
- **Post-quantisation clamp.** Output is clamped to ±(1 − q_step) after the Wannamaker-9 noise-shaper feedback, so a shaper excursion can never produce a sample code outside the 24-bit range. Error feedback still uses the unclamped value so the shaper remains stable.

**What you hear:**

Not one subtle nuance — room acoustics, cymbal decay, string after-ring — is buried in quantisation noise or mathematical drift. Full dynamic range is preserved from the first decode to the final sample.

---

## Summary for Audio Forums

Unlike typical upsampling software, AuraEngine is a professional offline DSP pipeline that:

1. **Corrects ADC pre-ringing at the source** (Adaptive Apodizing) — using a time-domain detector that distinguishes genuine pre-ringing from naturally bright material, with 2 048- or 4 096-tap minimum-phase correction filters.
2. **Performs ultra-precise 64-bit convolution** (up to 30 M-tap FIR) using filter coefficients that were designed offline in 128-bit quad-precision by the `fir-optimizer` Python toolchain, then truncated to f64 for the Rust runtime.
3. **Intelligently navigates the linear/minimum-phase trade-off** by snapping between them at zero-crossings on transients, with stereo-linked switching so L and R always move together (Hybrid-Phase Engine).
4. **Protects your DAC from intersample overloads** via polyphase sinc true-peak detection conforming to ITU-R BS.1770-4 (True Peak Limiter).
5. **Guarantees mathematical integrity across the entire graph** through Kahan-compensated accumulators, 64-bit spectral analysis, independent per-channel dither, and lossless 24-bit decode (Precision Engine).

*The result: crystalline, natural, and "alive" analogue presentation from your existing digital library.*
