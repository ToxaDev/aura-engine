# Offline Converter Pipeline — Technical Reference

> **Code**: `desktop-app/src-tauri/src/audio/converter/` (package, not a single file)
> **Status**: Standard Path ✅ stable | Polyphase FIR Path ✅ stable
>
> ⚠️ **Read [13-pipeline-hardening-2026-07.md](13-pipeline-hardening-2026-07.md) first.**
> The 2026-07 audit changed several things this document predates:
> the polyphase path is fixed and at full parity (dither + verification), OLA
> latency trimming is corrected, the Hybrid-Phase switch is stereo-linked, the
> apodizer uses a time-domain pre-ring detector, and the post-filter cutoff is
> per-ratio (not a fixed `fc=0.45`). Where this doc and doc 13 disagree, doc 13
> is current. A few specific corrections are inlined below; the dated
> "Recent…"/"Extreme…" sections (§12–15) are kept as historical changelog.

---

## Table of Contents

1. [Overview](#overview)
2. [Two Processing Paths](#two-processing-paths)
3. [Standard Path (Rubato + FIR Post-Filter)](#standard-path)
4. [Polyphase FIR Path (Experimental)](#polyphase-fir-path)
5. [Hybrid-Phase Engine](#hybrid-phase-engine)
6. [Adaptive Apodizer](#adaptive-apodizer)
7. [Auto-Snap Rate Logic](#auto-snap-rate-logic)
8. [Output Filename Convention](#output-filename-convention)
9. [Frontend ↔ Backend Communication](#frontend-backend-communication)
10. [Known Issues & Fixes Required](#known-issues)
11. [How to Fix the Polyphase Path](#how-to-fix-polyphase)
12. [Recent Pipeline Stabilizations (2026-04-13)](#recent-stabilizations)
13. [Extreme Hardware Optimizations (2026-04-14)](#extreme-optimizations)

---

## 1. Overview <a id="overview"></a>

The converter processes audio files through a high-fidelity DSP pipeline
(corrected — the original diagram omitted the DC-block and dither stages):

```
Input File → Decode → DC Block → Headroom → Adaptive Apodizer
           → [Resample + FIR] → Hybrid-Phase → True Peak → Dither → FLAC → Verify
```

Two processing paths exist, selected by the **FIR Resampling** checkbox (`use_fir_resampling`):

| Setting | Path Used | Status |
|---------|-----------|--------|
| FIR Resampling = OFF | Standard Path (Rubato + FIR post-filter) | ✅ **Working** |
| FIR Resampling = ON | Polyphase FIR Path | ⚠️ **Broken** (see §10) |

---

## 2. Two Processing Paths <a id="two-processing-paths"></a>

### Architecture comparison:

```
STANDARD PATH (FIR Resampling OFF):
────────────────────────────────────
Input 44.1kHz
    │
    ▼
Rubato SincFixedIn    ← Anti-imaging built into resampler
    │  44.1k → 384k
    ▼
FIR Post-Filter       ← Our 10M-tap Kaiser filter (fc=0.45)
    │  Spectral shaping only (images already removed)
    ▼
Hybrid-Phase Blend    ← Linear + Min-phase OLA, zero-crossing switch
    │
    ▼
True Peak → FLAC


POLYPHASE FIR PATH (FIR Resampling ON):
────────────────────────────────────────
Input 44.1kHz
    │
    ▼
Polyphase Decomposition  ← Same 10M-tap filter split into L sub-filters
    │  h_k[m] = h[m·L + k]
    ▼
L × OLA Convolution      ← Each sub-filter processes input at SOURCE rate
    │  Outputs interleaved → 352.8kHz
    ▼
Hybrid-Phase Blend        ← Same as standard, but with polyphase min-phase
    │
    ▼
True Peak → FLAC
```

---

## 3. Standard Path (Rubato + FIR Post-Filter) <a id="standard-path"></a>

### Signal Flow (converter.rs, lines ~1077-1440):

1. **Rubato Resampling** — `SincFixedIn` polyphase resampler
   - Chunk-based processing (**32768** frames/chunk; sinc_len 512, oversampling 512, Cubic)
   - Internal sinc anti-imaging filter (~−180 dB stop-band)
   - Any ratio supported (44.1k→384k, 48k→768k, etc.)

2. **FIR Post-Filter** — overlap-save convolution (CPU or GPU)
   - Pre-computed Kaiser filter selected **per output rate** from the filter
     matrix (`find_precomputed_filter`), not a fixed `fc=0.45` — its cutoff is
     correct for the current ratio. (The old single-design-point `fc=0.45` was
     the pre-`ca1af01` behaviour.)
   - Applied at OUTPUT rate
   - Latency trim = `output_latency()` (2×b_size on CPU, 1×b_size on GPU) +
     `(N−1)/2` group delay — see doc 13 §1.

3. **Hybrid-Phase** (if enabled) — Lines ~1285-1421
   - Loads `fir_10M_minimum_phase.npy` from `fir-optimizer/output/`
   - Runs second OLA convolution with minimum-phase filter
   - Computes blend envelope from source transients
   - Zero-crossing hard switch between linear and minimum-phase outputs

4. **True Peak Normalization** — 4× **Lanczos-4** (8-tap) polyphase-sinc
   intersample peak detection
   - Ceiling is **−0.5 dBTP** (linear ≈ 0.9441); anything above it is scaled down

5. **Dither** — 24-bit TPDF, independent per channel; Wannamaker-9 noise
   shaping only at ≤48 kHz output (pure TPDF above)

6. **FLAC Encoding** — **24-bit** FLAC via ffmpeg (`-sample_fmt s32
   -bits_per_raw_sample 24`), then bit-perfect re-decode verification

### Key variables:
```rust
let (resampled_l, resampled_r, out_rate) = ...;  // Rubato output
let resampled_l_saved = resampled_l.clone();      // Saved for Hybrid-Phase 2nd pass
```

---

## 4. Polyphase FIR Path (Experimental) <a id="polyphase-fir-path"></a>

### Signal Flow (converter.rs, lines ~756-1075):

1. **Auto-Snap** — If `out_rate` isn't integer multiple of source, snap DOWN
   - 44.1kHz → 384kHz snaps to **352.8kHz** (×8)
   - 48kHz → 384kHz stays **384kHz** (×8, already integer)

2. **Filter Loading** — Full 10M-tap coefficients (custom .npy or generated)

3. **Polyphase Decomposition** — `polyphase_decompose(coeffs, L)`
   ```
   Phase 0: h[0], h[L], h[2L], ...  → 1,250,000 taps (for L=8)
   Phase 1: h[1], h[L+1], h[2L+1], ...
   ...
   Phase L-1: h[L-1], h[2L-1], ...
   ```

4. **L × Sequential OLA Convolution** — Each sub-filter → OLA at INPUT rate
   ```rust
   output[n*L + phase] = sub_filter_output[n] * scale;
   ```

5. **Scale Factor** — `scale = L / dc_gain` (compensates for polyphase energy split)

6. **Group Delay Trimming** — Linear-phase: `(b_size + sub_delay) * L` samples

7. **Hybrid-Phase** (if enabled) — Same dual-pass approach but with polyphase decomposition of minimum-phase filter

### ✅ STATUS: FIXED (2026). The cutoff bug described in §10 was corrected
(`fc = source_rate / (2·output_rate)`), and the path now applies dither and
bit-perfect verification and is exposed via the *Polyphase FIR Resampling*
checkbox. §10 is retained as the historical bug write-up. See doc 13 §6.

---

## 5. Hybrid-Phase Engine <a id="hybrid-phase-engine"></a>

**Module**: `audio/hybrid_phase.rs`

**Purpose**: Preserve transient sharpness of minimum-phase filters while keeping the flat frequency response of linear-phase filters.

### Algorithm:
1. **Linear-phase pass** — Full OLA convolution with symmetric filter
2. **Minimum-phase pass** — Full OLA convolution with causal filter
3. **Onset detection** — Analyze source audio for transient energy
4. **Blend envelope** — 100Hz resolution envelope marking transient regions
5. **Zero-crossing hard switch** — Switch between linear and minimum-phase outputs at signal zero-crossings

### Key functions:
```rust
// Compute blend envelope from source audio
compute_blend_envelope(samples_l, samples_r, sample_rate, output_len, out_rate) -> BlendEnvelope

// Apply zero-crossing hard switch blend
blend_outputs(linear, minimum, envelope, channel) -> Vec<f64>
```

### Metrics logged:
```
[HYBRID-PHASE] Onset stats: 314415 frames, 262342 positive (83.4%)
[HYBRID-PHASE] After gate: 57850 active frames (18.4%)
[HYBRID-PHASE] Envelope follower: 314415 frames, 1345 transient regions
[HYBRID-PHASE] Coverage: min-phase>15.1%  active>76.8%  linear>23.2%
[HYBRID-PHASE] Hard switch: 2236 switches, min-phase 27.6% of samples
```

---

## 6. Adaptive Apodizer <a id="adaptive-apodizer"></a>

**Purpose**: Automatically detect and suppress ADC anti-aliasing filter ringing artifacts.

### Algorithm:
1. **Spectral analysis** — Measure energy in 15-18kHz, 18-20kHz, 20-22kHz bands
2. **Compare** against reference thresholds to detect ADC ringing
3. **Determine optimal cutoff** — Where ringing starts
4. **Apply apodizing filter** — 4096-tap minimum-phase lowpass at detected cutoff

### Log output:
```
[CONV] ADC Analysis: 15-18kHz: -22.6dB, 18-20kHz: -30.8dB, 20-22kHz: -43.6dB
[CONV] Adaptive Apodizer: detected ADC ringing, optimal cutoff = 19000 Hz
[CONV] Adaptive apodizing: 4096 taps, fc_norm=0.8617, cutoff=19000Hz
```

---

## 7. Auto-Snap Rate Logic <a id="auto-snap-rate-logic"></a>

When `use_fir_resampling = true`, the polyphase path requires integer ratio `L = out_rate / src_rate`.

If the requested rate isn't an integer multiple, the engine snaps **DOWN**:

| Source | Requested | Snapped To | Ratio |
|--------|-----------|------------|-------|
| 44100 | 384000 | **352800** | ×8 |
| 44100 | 768000 | **705600** | ×16 |
| 48000 | 384000 | 384000 ✓ | ×8 |
| 48000 | 768000 | 768000 ✓ | ×16 |
| 96000 | 384000 | 384000 ✓ | ×4 |

The snapped rate is broadcast to the frontend via `CONV_SNAPPED_RATE` global, and the UI dropdown updates automatically during conversion.

---

## 8. Output Filename Convention <a id="output-filename-convention"></a>

**Format**: `{source_filename} [AE · {rate} · {filter} {taps} · {precision} · {options}].flac`

### Examples:
```
Rhythm Is A Dancer [AE · 44.1k→384k · Kaiser 10M · f64 · AA · HP].flac
Track 01 [AE · 48k→768k · AURA 10M · f64 · HP].flac
Song [AE · 96k · Nuttall 5M · f64].flac
```

### Tag components:
| Tag | Meaning | When shown |
|-----|---------|------------|
| `AE` | AuraEngine identifier | Always |
| `44.1k→384k` | Source → output rate | When rates differ |
| `384k` | Output rate only | When same rate (re-filter) |
| `Kaiser` / `AURA` | Filter window / custom filter | Always |
| `10M` / `500K` | Tap count (compact) | Always |
| `f64` / `f128` | Processing precision | Always |
| `AA` | Adaptive Apodizer enabled | When active |
| `HP` | Hybrid-Phase enabled | When active |

### Implementation:
```rust
fn build_output_name(audio: &AudioFile, settings: &ConvertSettings, 
                     src_path: &Path, actual_out_rate: u32) -> String
```
- Uses **source filename** as base (not metadata tags)
- `actual_out_rate` reflects the snapped rate (for FIR path)

---

## 9. Frontend ↔ Backend Communication <a id="frontend-backend-communication"></a>

### Progress polling:
```rust
// Backend (converter.rs)
pub fn get_progress() -> (u32, u32, u32, String, String, u32) {
    // (progress_0_1000, queue_total, queue_done, status_text, output_path, snapped_rate)
}
```

```javascript
// Frontend (main.js)
const [progress, total, done, status, output, snappedRate] = 
    await invoke('get_conversion_progress');

// Auto-update rate dropdown when backend snaps
if (snappedRate > 0 && snappedRate !== currentVal) {
    rateSelect.value = snappedRate.toString();
}
```

### Global state atoms:
| Variable | Type | Purpose |
|----------|------|---------|
| `CONV_PROGRESS` | AtomicU32 | 0-1000 (0.0%-100.0%) |
| `CONV_RUNNING` | AtomicBool | Conversion active |
| `CONV_CANCEL` | AtomicBool | Cancellation flag |
| `CONV_STATUS` | Mutex<String> | Current status text |
| `CONV_OUTPUT` | Mutex<String> | Output file path |
| `CONV_QUEUE_TOTAL` | AtomicU32 | Total files in batch |
| `CONV_QUEUE_DONE` | AtomicU32 | Completed files |
| `CONV_SNAPPED_RATE` | AtomicU32 | Actual output rate after snap |

---

## 10. Known Issues (historical — FIXED, see §4) <a id="known-issues"></a>

> **This entire section is a historical bug write-up.** The polyphase cutoff
> bug described below was fixed in 2026 (`fc = source_rate / (2·output_rate)`,
> see §4 and doc 13 §6) and the path is now at full parity with the standard
> path. Kept for engineering history.

### ⚠️ CRITICAL (historical): Polyphase FIR Path — Spectral Imaging + Wrong Gain

**Symptom**: When `FIR Resampling` is ON:
- Spectral imaging visible as horizontal bands repeating at multiples of input Nyquist
- Output is ~12 dB too loud → true peak normalization crushes volume to ~50%
- Audibly quieter and with artifacts compared to Standard Path

**Root Cause**: The FIR filter has **wrong cutoff frequency** for polyphase interpolation.

```
Our filter:        fc = 0.45 (45% of Nyquist)
                   Passes frequencies up to ~80 kHz at 352.8kHz output rate

Required for ×8:   fc = 1/(2×8) = 0.0625 (6.25% of Nyquist)
                   Must cut off at ~22 kHz to suppress spectral images
```

The filter was designed as a **broadband post-filter** for spectral shaping (applied AFTER Rubato removes images). When used as a **polyphase interpolation filter**, it does NOT suppress the L-1 spectral images created by upsampling, because its passband is 7× too wide.

**Scale Factor Issue**: Likely related — `scale = L / dc_gain` may or may not be correct depending on whether OLA preserves sub-filter output levels. Empirical testing showed output peak at ~4× expected level with scale=L, and ~0.5× with scale=1. Neither is correct, suggesting the filter bandwidth issue contaminates the gain calculation.

**Current workaround**: Leave `FIR Resampling` checkbox **OFF**. The Standard Path produces correct output.

---

## 11. How to Fix the Polyphase Path <a id="how-to-fix-polyphase"></a>

### Option A: Generate a proper interpolation filter (Recommended)

The polyphase path needs a **dedicated interpolation filter** with cutoff at the input Nyquist:

```rust
// In the polyphase path, BEFORE decomposition:
let fc_interp = 0.5 / l as f64;  // e.g., 0.0625 for ×8

// Generate new filter with correct cutoff
let interp_coeffs = generate_fir_coefficients_with_cutoff(
    settings.taps, 
    settings.win_type, 
    fc_interp  // NOT the default 0.45
);

// Then decompose and process as before
let phases = polyphase_decompose(&interp_coeffs, l);
```

**Changes needed**:
1. Add `fc` parameter to `generate_fir_coefficients()` in `dsp_core.rs`
2. In polyphase path, call with `fc = 0.5 / L` instead of default `0.45`
3. For custom filters (.npy), either:
   - Trust that the user's filter has the correct cutoff, OR
   - Apply a brick-wall at `fc = 0.5 / L` in the frequency domain before decomposition

**Scale factor**: With the correct filter (DC gain ≈ 1/L per sub-filter), `scale = L` should be correct. Verify empirically after fixing the filter.

### Option B: Two-stage approach

Keep the current filter unchanged. Add a separate anti-imaging step:

```
Input → [Generate anti-imaging filter at fc=0.5/L] → Polyphase interpolation →
→ [Apply user's custom post-filter at fc=0.45] → Output
```

This gives the benefit of the user's custom spectral shaping WITHOUT spectral imaging, but doubles the processing time (two separate convolutions).

### Option C: Frequency-domain narrowing

Before polyphase decomposition, narrow the filter's bandwidth in the frequency domain:

```rust
// FFT the full filter
let H = fft(full_coeffs);
// Zero out frequencies above fc = 0.5/L
for bin in (cutoff_bin..H.len()-cutoff_bin) { H[bin] = 0.0; }
// IFFT back to time domain
let narrowed = ifft(H);
// Now decompose
let phases = polyphase_decompose(&narrowed, l);
```

**Caveat**: This changes the filter characteristics (removes the user's spectral shaping above the input Nyquist). May or may not be acceptable.

### Verification after fix

1. Convert 44.1kHz → 352.8kHz with FIR Resampling ON
2. Check log: `True peak` should be `-2...-4 dBFS` (no normalization triggered)
3. Open in iZotope RX: spectrogram should show NO horizontal band repetitions
4. A/B compare with Standard Path output — should sound identical
5. Compare with source file scaled to match — should be transparent upsampling

---

## Appendix: ConvertSettings Struct

```rust
pub struct ConvertSettings {
    pub out_rate: u32,              // COMPUTED per file: family_base * fs_multiplier
    pub fs_multiplier: u32,         // FS value: 2, 4, 8, or 16
    pub taps: usize,                // FIR filter tap count (e.g., 10_000_000)
    pub precision: u32,             // GPU DS precision selector
    pub win_type: i32,              // filename tag only — does NOT select the filter
    pub custom_filter_path: Option<String>,  // Path to .npy filter file
    pub use_gpu: bool,              // GPU DS convolution path
    pub use_fir_resampling: bool,   // Polyphase FIR path (integer ratio)
    pub apodizing: u32,             // 0=off, 1=gentle, 2=moderate, 3=strong
    pub headroom_db: f64,           // Pre-DSP gain reduction (0, -0.5, -1.0, -3.0)
    pub adaptive_apodizer: bool,    // Per-file ADC pre-ring detection (time-domain)
    pub hybrid_phase: bool,         // Linear + Minimum phase transient blending
    pub iir_dc_blocking: bool,      // 2 Hz IIR HPF instead of static mean removal
}
```

> **Note.** `out_rate` is not a direct user input — the UI sends `fs_multiplier`
> (FS2/4/8/16) and `prepare.rs` computes `out_rate = family_base × fs_multiplier`
> from the source's 44.1 or 48 kHz family. `win_type` only affects the output
> filename; the actual filter is chosen by `taps` + `out_rate`.

---

## 12. Recent Pipeline Stabilizations (2026-04-13) <a id="recent-stabilizations"></a>

This section catalogs critical bug fixes implemented to ensure pipeline stability and 100% verification pass rates.

### 12.1 Vulkan GPU `DeviceLost` Exhaustion Fix
**Issue**: When using the Polyphase FIR path or Hybrid-Phase (which computes `l` sub-filters sequentially), the rapid spinning up and dropping of `wgpu` instances (`wgpu::Instance::new` → `request_device`) caused Windows TDR/Vulkan to exhaust driver handles or hit a rate limit, resulting in `RequestDeviceError { inner: Core(DeviceLost) }`.
**Solution**: Implemented a global `std::sync::OnceLock<GpuContext>` in `gpu_core.rs`. The WGPU Device and Queue are now requested strictly **once per application lifecycle** taking full advantage of the adapter's maximum storage limits. This single GPU instance is reused across all DSP convolution contexts, reducing initialization overhead to zero and stabilizing polyphase processing.

### 12.2 Power-of-2 Sample Rate Snapping
**Issue**: The automatic rate snapping logic previously allowed non-standard integer multiples. When requesting `384kHz` with a `44.1kHz` file, it used `floor(384000/44100) = 8`. This resulted in `352.8kHz`, which was correct. However, asking for `768kHz` from `44.1kHz` resulted in `floor(768000/44100) = 17`, giving an obscure and incorrect standard `749.7kHz`.
**Solution**: Snapping logic was refactored to explicitly loop `pow2_ratio *= 2`. Output rates now strictly lock to `source_rate × 2^N` while remaining below the requested boundary. Thus `44.1kHz` always snaps to `88.2`, `176.4`, `352.8`, or `705.6kHz` depending on the max limit.

### 12.3 Verification Script Unrelated Envelope Loading
**Issue**: `verify_hybrid_phase.py` was generating false `[WARN]` logs indicating mismatched sample windows. 
**Solution**: Found that `candidates.sort(key=st_mtime)` was blindly loading the absolute latest `.hybrid_phase.json` generated in the folder, rather than the envelope associated with that specific track. It was fixed to explicitly use the `source_path`'s base stem to match the exact `[stem].hybrid_phase.json`. Warns have dropped to near zero, except for micro crossfade bounds.

---

## 13. Extreme Hardware Optimizations (2026-04-14) <a id="extreme-optimizations"></a>

This section documents the performance breakthroughs implemented to fully saturate high-end workstation hardware (e.g., RTX 4090, 24+ core CPUs).

### 13.1 Dynamic Hardware-Bound Threading
**Issue**: The converter orchestrator (`manager.rs`) previously had a hardcoded limit of 4 concurrent worker threads. This vastly underutilized multi-core processors and modern GPUs, leaving them practically idle.
**Solution**: Replaced the hardcoded magic number with `std::thread::available_parallelism()`. The application now scales its worker pool dynamically (clamped between 2 and 64 threads) to match the host hardware, allowing massively parallel batch processing.

### 13.2 Zero-Allocation Hot-Loop (GPU/RAM Bottleneck Elimination)
**Issue**: Inside `gpu_core.rs` (`process_ola_block`), the CPU was executing `vec![0.0f32; self.n * 2]` on every execution (e.g., dynamically ~8 Million zeroes). With 24 parallel streams at 25 blocks per second, the CPU attempted to allocate and zero-fill over **1.6 GB of heap memory per second**, locking the Windows RAM Allocator Mutex and creating heavy L3 cache thrashing. The GPU was starved waiting for PCIe data.
**Solution**: Moved `complex_l` and `complex_r` to the `GpuDspProcessor` persistent struct. The massive memory allocation happens only **once** upon filter initialization. The hot-loop now strictly overrides indices in place. This drops the heap allocation penalty to absolutely zero, instantly maximizing PCIe bus throughput.

### 13.3 Dynamic OLA Latency Trim Calculation
**Issue**: The dynamic scaling of GPU block sizes (from 262k up to 2M depending on filter size) desynchronized the group-delay padding. The engine continued trimming a hardcoded 32,768 samples, resulting in massive blocks of silence padding prefixing the FLAC outputs.
**Solution**: Overlap-Add latency extraction calculations in `process.rs` were properly linked to the active Convolution `b_size` property instance, guaranteeing millisecond-perfect sample alignment no matter how large the `n`-point FFT expands.

### 13.4 Hybrid Phase Transients Research
**Issue**: The `verify_hybrid_phase.py` graphing algorithm highlighted that the Minimum-to-Linear phase crossover envelope persistently hovered around intermediate coefficients (e.g., `0.5`). 50% hybrid blending is acoustically suboptimal because it retains half the pre-ringing amplitude while inducing half extreme minimum phase distortion.
**Status**: The theory for `envelope_follower` correction has been proven and is staged for the next phase. Transients must push the envelope to a hard `1.0` maximum instantaneously to successfully mask phase rings.

---

## 14. Mathematical Precision Re-Architecture (2026-04-16) <a id="precision-re-architecture"></a>

This section documents the final push to mathematical perfection, eliminating accumulated quantization noise across the DSP graph.

### 14.1 Strict 128-bit Filter Policy (Fallback Generation Removed)
**Issue**: When the converter did not find a pre-compiled `.npy` filter from `fir-optimizer`, it dynamically instantiated a 64-bit Sine-Kaiser window internally via Rust as a fallback.
**Solution**: The fallback generator routines (`generate_fir_coefficients`, `generate_filters`, `to_minimum_phase` in `dsp_core.rs`) have been completely deleted. System now strictly expects 128-bit IEEE Quad-Precision generated `.npy` filters. This enforces a no-compromise mathematical baseline.

### 14.2 Lossless 24-bit Symphonia Extraction
**Issue**: FLAC 24-bit sources were being dumped to a 32-bit floating point `SampleBuffer::<f32>`. A 32-bit float only contains 23-bits of mantissa precision, permanently snapping off the 24th least significant bit of studio source assets during extraction.
**Solution**: Target buffer architecture rotated to `SampleBuffer::<i32>`. Symphonia extracts and left-shifts any 16/24-bit PCM losslessly into the `i32` bound, which is then flawlessly converted to `f64` via `1.0/2^31` scaling, keeping 100% of spatial depth.

### 14.3 Long-Double (80-bit) Kahan FIR Normalization
**Issue**: Normalizing the massive 30-Million tap array by calculating `total = np.sum(h)` accumulated a massive float64 loss, skewing the overall DC gain of the system by approximately `1.2e-12` increments over absolute volume. 
**Solution**: Summation normalizer replaced with `np.sum(h.astype(np.longdouble))`. The internal 80-bit x87 hardware buffers virtually negate the float decay.

### 14.4 Sub-Sample Cubic Sinc Interpolation 
**Issue**: Rubato processing was technically rendering interpolation limits using `SincInterpolationType::Linear` on its fixed sinc window arrays. 
**Solution**: Promoted to `Cubic` interpolation scaling, resulting in up to 4× less interpolation error on generated wave slices.

### 14.5 Double Precision HPSS Spectrograms
**Issue**: The native Rust Harmonic-Percussion Separator computed sliding magnitude median limits holding arrays of `f32`. During ultra-quiet passages, noise-floor truncation created artifact triggers.
**Solution**: All internal matrices within `hpss_native.rs` have been elevated to raw `f64`, improving threshold dynamics at -60 dBFS structures.

### 14.6 Intelligent Component-Level DC Blocking
**Issue**: Studio inputs tracking fractional Direct-Current voltage offsets forced the gigantic FIR filter to ring infinitely, contaminating the tail threshold detection and limiting dynamic true-peak tracking headroom.
**Solution**: Real-time signal average tracking implemented. Before DSP algorithms hit the buffer, global `dc_l` and `dc_r` components are mathematically stripped.

### 14.7 True-Peak Ceiling
> **Corrected.** The `1.00116` bypass described in an earlier draft was never
> the shipping behaviour. `true_peak.rs` uses `TARGET_TRUE_PEAK_DBTP = -0.5`
> (linear ≈ 0.9441): any signal whose 4×-oversampled Lanczos-4 intersample
> peak exceeds −0.5 dBTP is scaled down to it; quieter material is left
> bit-exact (no gain applied). There is no `1.00116` threshold.

---

## 15. Batch Memory Safety & Thread Throttling (2026-04-16) <a id="batch-memory-safety"></a>

This section catalogs the critical fixes installed to prevent out-of-memory crashes when dropping batches of 100+ high-resolution tracks into the offline converter.

### 15.1 VRAM-Aware GPU Stream Throttling
**Issue**: Utilizing `std::thread::available_parallelism()` in `manager.rs` caused the Engine to uncontrollably spawn up to 32 parallel WOLA convolution pipelines. This violently overbooked the ~24GB VRAM buffer on flagship GPUs, triggering Driver TDR hangs and freezing the UI under extreme loads.
**Solution**: Hardware processing queues have been strictly throttled:
- **GPU mode** is locked to a maximum of `2` simultaneous threads. Modern GPU compute nodes can digest 2 concurrent full-resolution convolution streams instantaneously. Locking the pool to 2 secures safe VRAM overhead while keeping processing hundreds of times faster than real time.
- **CPU mode** dynamically scales to `clamp(1, cores / 2)` but maxes out at `4` threads to avoid locking up background OS services.

### 15.2 In-Flight Decode Channel Clamping
**Issue**: The CPU decode logic was eagerly buffering decoded track contents into massive 64-bit float arrays (`100M+` samples) faster than the GPU could process them, ballooning active RAM footprint.
**Solution**: The `tx_prep_bound` queue has been shrunk to exactly match the active thread worker size. The processing buffer stays fully flushed.

### 15.3 Dynamic RAM Guard for Synchronous Pausing
**Issue**: Pushing 100 long FLACs queued standard allocations that outpaced OS Garbage Collection, running out of RAM completely.
**Solution**: Enhanced `sysinfo` integration via `await_free_ram` inside `memory.rs`. The decoding pre-processor now synchronously halts memory allocation and idles if the OS registers less than ~3GB free payload. The application waits securely rather than causing unhandled pointer allocation failures.
