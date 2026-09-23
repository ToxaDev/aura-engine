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
5a. [Subsonic Filter (optional, 1.2.8)](#subsonic-filter)
6. [Adaptive Apodizer](#adaptive-apodizer)
7. [Auto-Snap Rate Logic](#auto-snap-rate-logic)
8. [Output Filename Convention](#output-filename-convention)
9. [Frontend ↔ Backend Communication](#frontend-backend-communication)
10. [Known Issues & Fixes Required](#known-issues)
11. [How the Polyphase Path Was Fixed](#how-to-fix-polyphase)
12. [Recent Pipeline Stabilizations (2026-04-13)](#recent-stabilizations)
13. [Extreme Hardware Optimizations (2026-04-14)](#extreme-optimizations)

---

## 1. Overview <a id="overview"></a>

The converter processes audio files through a high-fidelity DSP pipeline
(corrected — the original diagram omitted the DC-block and dither stages):

```
Input File → Decode → DC Block → [Declip] → [ISP] → [Subsonic Filter]
           → [Adaptive Headroom] → Adaptive Apodizer → [Resample + FIR]
           → [Hybrid-Phase | TFS] → [XTC] → [ISP output limiter]
           → [Subsonic guard] → True Peak → Dither → FLAC → Verify
```

Bracketed stages are user toggles. Since 1.3.0 all of them except
Hybrid-Phase, Continuous Alpha and XTC are on by default, and the subsonic
filter opens on 15 Hz. It runs twice since 1.3.3: on the source, after the
repairs, and again at the output rate after the ISP output limiter, which
would otherwise put infrasonic products back (§5a).

Two processing paths exist, selected by the **Polyphase FIR Resampling** toggle (`use_fir_resampling`):

| Setting | Path Used | Status |
|---------|-----------|--------|
| FIR Resampling = OFF | Standard Path (Rubato + FIR post-filter) | ✅ **Working** |
| FIR Resampling = ON | Polyphase FIR Path | ✅ **Working** — the default since 1.3.0 (§10 has the history) |

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
FIR Post-Filter       ← Kaiser filter from the precomputed matrix, cut at the source band edge (doc 12)
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
Polyphase Decomposition  ← The same matrix filter, split into L sub-filters
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
   - Ceiling is **−0.5 dBTP** by default (linear ≈ 0.9441), or whatever the
     Headroom control names; anything above it is scaled down

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

**What the trigger is**: an *onset* detector — the rise of percussive energy between analysis frames. It does not measure whether the reconstruction filter would ring on this material; on music the two coincide, on synthetic material they do not. See [Hybrid-Phase Proof §1](06-hybrid-phase-proof.md) for the distinction and the known blind spot in the first ~24 ms of a file.

### Algorithm:
1. **Linear-phase pass** — Full OLA convolution with symmetric filter
2. **Minimum-phase pass** — Full OLA convolution with causal filter
3. **Onset detection** — Analyze source audio for the start of percussive events
4. **Blend envelope** — ~86 Hz analysis envelope marking those regions, Catmull-Rom upsampled to the output rate
5. **Zero-crossing hard switch** — Switch between linear and minimum-phase outputs at signal zero-crossings, one plan for both channels

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

## 5a. Subsonic Filter (optional, 1.2.8) <a id="subsonic-filter"></a>

**Purpose**: protection for the speakers from infrasonic content and slow
drift a source can carry. Not a sound improvement, and nothing here claims
it is audible.

DC blocking subtracts the per-channel mean and nothing else. A slow wobble
passes through it untouched: a CD in the ASR thread had a component near 1 Hz
at about 1 % of full scale, different per channel, from which mean
subtraction removes 0.00 dB.

### Specification (one filter shape, three corners)
| | |
|---|---|
| type | linear-phase FIR high-pass: Kaiser-windowed sinc low-pass, normalised to unit DC gain, spectrally inverted (`h = δ − h_lp`) — response at DC exactly 0 |
| corner F | 20, 15 or 10 Hz (chip on the SUB row; filename tag `SUB20` / `SUB15` / `SUB10`) |
| passband | flat from F up, ≤ 0.01 dB (designed: 5·10⁻⁵ dB) |
| stopband | ≥ 100 dB from F/2 down to DC (designed: 104.4 dB) |
| −6 dB point | 3F/4 |
| length | odd, from the source rate: 29 813 taps at 44.1 kHz / 20 Hz (0.68 s); doubles with the rate and with halving F |
| where | twice. **Source pass**: `pipeline/prepare.rs`, at the source rate, after Declip and ISP (a high-pass in front of them would tilt the flat tops they read) and before the apodizer's forensics. **Guard pass** (1.3.3): `process.rs`, at the output rate, after the ISP output limiter and before the ceiling is measured — see below |
| code | `apodize.rs`: `design_subsonic_highpass`, `apply_subsonic_filter`, `apply_subsonic_guard`, `fft_convolve_linear_inplace`; `process.rs`: `run_output_subsonic` |

Why these corners: 20 Hz is the edge of the audible band and the classic
subsonic corner; 15 Hz keeps the lowest note of a 32-foot organ stop
(16.35 Hz) flat; 10 Hz takes only drift and infrasound and nothing musical.
Nothing above 20 Hz is offered — that would be cutting bass, not subsonic
protection.

Linear phase is the reason to do this offline: above F nothing moves in level
or in time, which a playback-chain IIR high-pass cannot promise. The filter's
pre- and post-ringing lies below F, at the level of what it removed.

### Convolution in bounded memory
`fft_convolve_linear_inplace` runs the same block FFT arithmetic as
`fft_convolve_ola(.., FilterPhase::Linear, ..)` — same block size, same order
of summation, bit-identical output (unit test at every block/batch boundary)
— but a few blocks at a time, writing each finished output sample back into
the input as soon as no later block reads it. `fft_convolve_ola` holds every
block's output plus two full-length buffers at once.

### Log output:
```
[CONV] Subsonic filter SUB20: 29813 taps (0.68 s), linear phase, flat from 20 Hz, ≥100 dB below 10 Hz; removed RMS L -43.5 dB / R -42.5 dB re full scale
```
(Santana, *Mal Bicho*, CD rip: the disc's broadband content below 20 Hz.)

### Measured through the whole pipeline (2026-09-15, 5k taps, FS8, CPU)
Two conversions of the same file, off and on, with the dither seeded
identically in a test build so the outputs can be subtracted:

| | ASR-case synthetic¹ | *Mal Bicho* |
|---|---|---|
| difference above 25 Hz, worst 0.34 Hz bin, relative to the signal | −100.8 dB | −98.9 dB |
| → level change above 25 Hz, bound | ≤ 0.0001 dB | ≤ 0.0001 dB |
| cross-correlation lag (30–2000 Hz) | 0 samples | 0 samples |
| 1 Hz component | −40.5 / −45.0 → −158.8 / −163.3 dBFS | — |
| content below 8 Hz (median of 1 s RMS) | — | −51.5 → −142.6 dBFS |

¹ 90 s, 44.1 kHz/24-bit: music-like noise from 30 Hz up at −20 dBFS, a
40 Hz tone at −20 dBFS, and 1 Hz at 0.94 % (L) / 0.56 % (R) of full scale.
The 40 Hz tone's amplitude and phase are the same to five decimals with the
filter on and off, at all three corners.

These are with Hybrid-Phase and the Adaptive Apodizer off — see the next two
notes for why that matters.

### Level
The output ceiling (true-peak normaliser, §14.7) is measured on the finished
render, after the filter. Removing a large infrasonic component can move the
peak, and the normalising scalar with it (−0.027 dB on *Mal Bicho*); that is
the ceiling doing its job, not the filter changing the audible band.

### At the edges of a file
The filter is half a length wide on each side — 0.34 s at 20 Hz, 0.68 s at
10 Hz — and before the first sample and after the last it sees silence. A slow
wave that is already under way when the file starts (a track cut from a
continuous album) is taken out less completely there. On the ASR-case file,
Audacity's whole-file spectrum at 352.8 kHz shows the 1 Hz hump at −40.7 dB
with the filter off, −131 dB with SUB20 and −93…−105 dB at 3–10 Hz with
SUB10; leaving out the first and last two seconds, all three corners read
−124…−131 dB. The edges are still more than 50 dB down.

### With Hybrid-Phase and the Adaptive Apodizer
Both decide from the signal they are given, and with the filter on that
signal has nothing below the corner. Hybrid-Phase switches between its
linear- and minimum-phase renders at zero crossings, and taking out a 1 Hz
wobble moves zero crossings: on *Mal Bicho* some switch points move, and
where one does the output there comes from the other branch. The difference
that makes above 20 Hz is Hybrid-Phase's own difference between its branches,
not the subsonic filter's response — and it is local: with Hybrid-Phase on,
89 % of 10 ms windows above 40 Hz still differ by less than −80 dB relative
to the signal, and 1.9 % by more than −40 dB. The Apodizer's pre-ring statistics are
measured on the filtered source too; on *Mal Bicho* its verdict did not
change (cutoff 19 591 Hz, 2048 taps, β 24; pre-ring counted on 32 % of
attacks instead of 28 %).

### The second pass, after the output limiter (1.3.3)
Everything between the source pass and the file is linear — headroom is a
scalar, the apodizer and the FIR are convolutions, XTC is a filter — except
two stages that multiply the signal by a curve that moves: the Hybrid-Phase
blend, and the ISP output limiter, which dips the gain for about 1.5 ms at
every over. Bass times a dipping gain is amplitude modulation, and the
sidebands land far below the bass, under a corner the filter had already
emptied and could no longer reach. Erogen found it in the Audio Science
Review thread: with SUB20 and the limiter engaged, a flat shelf from 3 to
10 Hz about 40 dB under the bass.

So the filter runs again at the end: the same design, computed for the
**output** rate, directly after the output limiter and directly before the
true-peak ceiling is measured, so the peak the normaliser reads is the peak
that is written. The source pass stays; it is what takes a disc's rumble out,
and keeping it means the limiter still decides its dips on a signal without
rumble in it. Two passes of the same high-pass cost 1×10⁻⁴ dB above the
corner.

A clipped CD with rumble added at the level of the disc in that thread
(FS×4, 1M taps, ISP · SUB20 · PFR · HP · αHP, limiter engaged), whole-file
spectrum, channels summed, Blackman-Harris 65536, dBFS per bin:

| Hz | 1.3.2 | 1.3.3 | ISP off |
|---|---|---|---|
| 2.7 | −56.5 | **−125.0** | −126.9 |
| 5.4 | −56.6 | −110.7 | −117.5 |
| 8.1 | −56.7 | −84.4 | −89.1 |
| 10.8 | −55.7 | −67.0 | −70.8 |
| 40 (bass) | −28.0 | −28.0 | −29.9 |

The ISP-off run is 1.9 dB quieter (the ceiling took 2.09 dB instead of 0.10);
level-matched it agrees with 1.3.3 at 2.7 Hz to the tenth. What is left above
8 Hz is the filter's own transition band (−6 dB at 15 Hz for SUB20), not a
leak. The standard route and 352.8 kHz read the same. Against 1.3.2, the
worst bin above 25 Hz differs by −104.5 dB relative to the signal, with no
time shift; the ceiling takes 0.018 dB more, because taking infrasound out
moves peaks.

At 352.8 kHz the guard is 238 485 taps at SUB20 and 317 979 at SUB15, 3–4 s
on a 3 min 38 s track. Its FFT batch is capped at about 1 GiB; on that track
the process peak stayed in the Hybrid-Phase blend.

The segmented route for very large files runs the same second pass, streamed,
after its own streamed output limiter. Before 1.3.4 it had neither and kept
the source pass alone. Its output stages — XTC, the ISP output limiter and this
pass, in that order — each have a streaming twin that a test holds to the
in-RAM version bit for bit.

### Off
With the option off nothing in the chain changes: `subsonic_hz == 0` skips
both passes, and the output is byte-identical to 1.2.7.

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
Mal Bicho [AE · 44.1k→352.8k · Kaiser 10M · f64 · SUB20 · AA · HP].flac
Track 01 [AE · 48k→768k · AURA 10M · f64 · HP].flac
Song [AE · 96k · Nuttall 5M · f64].flac
```

Options stand in the order the pipeline runs them.

### Tag components:
| Tag | Meaning | When shown |
|-----|---------|------------|
| `AE` | AuraEngine identifier | Always |
| `44.1k→384k` | Source → output rate | When rates differ |
| `384k` | Output rate only | When same rate (re-filter) |
| `Kaiser` / `AURA` | Filter window / custom filter | Always |
| `10M` / `500K` | Tap count (compact) | Always |
| `f64` / `f128` | Processing precision | Always |
| `SUB20` / `SUB15` / `SUB10` | Subsonic filter, corner in Hz (§5a) | When enabled |
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

## 10. Known Issues <a id="known-issues"></a>

No open issues in either conversion path.

### Resolved: polyphase path — spectral images and wrong gain (fixed 2026-07)

Until this section was rewritten it called the polyphase path broken and said to
leave **FIR Resampling** off. That was true when it was written. The path took
the standard route's post-filter — designed with `fc = 0.45` as a broadband
shaper that runs after rubato has already removed the images — and used it as
the interpolation filter. With a passband seven times too wide it let the L−1
images of the source spectrum through, and the output gain came out wrong with
it.

What fixed it:

- **The cutoff.** An interpolation filter has to stop at the source's band edge,
  `source_rate / (2 · output_rate)` in normalised terms (doc 06 §9).
- **The filter.** The path takes its filter from the precomputed matrix,
  `find_precomputed_filter(taps, out_rate, "linear_phase")`, designed for the
  rate it outputs: passband to 20 050 Hz and stopband from 22 050 Hz for
  44.1 kHz sources, 22 000 / 24 000 Hz for 48 kHz ones (doc 12). Every image
  starts inside the stopband.
- **The gain.** The sub-filters are scaled by `L / dc_gain` of the filter
  actually loaded, and `polyphase_pass_alignment_and_dc_gain` holds an impulse
  at sample P to output sample P·L and a DC input to exactly 1.0.
- **Parity.** Dither, bit-perfect verification and progress, the same as the
  standard path (doc 13 §6).

It has been the default route since 1.3.0 for a fresh install. An update from
1.2.x kept the box off, because the saved settings carried the old default
forward; 1.3.4 switches it on once for any settings saved before it. Measured
2026-09-23 on a clipped CD,
44.1 → 176.4 kHz, 1M taps, the same chain through both routes from one build
(left channel, Blackman-Harris 65536, dBFS per bin):

| band | polyphase | standard |
|---|---|---|
| 0.1–20 kHz, max | −22.9 | −22.9 |
| 22.05–24 kHz, max | −153.7 | −154.5 |
| 24–44.1 kHz, max | −164.1 | −165.2 |
| 44.1–66.15 kHz, max | −176.7 | −177.3 |
| 66.15–88.2 kHz, max | −181.6 | −181.7 |

Above the source band both hold nothing but the dither floor; an image of the
music would sit near −23 dBFS. The true-peak gain differed by 0.002 dB.

---

## 11. How the Polyphase Path Was Fixed <a id="how-to-fix-polyphase"></a>

This section used to hold the plan. Three options were weighed: a dedicated
interpolation filter with its cutoff at the source's band edge, a two-stage
chain that kept the old post-filter behind a separate anti-imaging step, and
narrowing the old filter in the frequency domain before decomposing it. The
first is what was built, and the precomputed matrix is what carries it: one
filter per output rate, so the cutoff is right for every ratio without a
second convolution. See §10 for what it measures like now.

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
    pub headroom_db: f64,           // Output true-peak ceiling in dBTP
                                    // (0 = the shipped -0.5; -0.5, -1.0, -3.0)
    pub adaptive_apodizer: bool,    // Per-file ADC pre-ring detection (time-domain)
    pub hybrid_phase: bool,         // Linear + Minimum phase transient blending
    pub iir_dc_blocking: bool,      // 2 Hz IIR HPF instead of static mean removal
                                    // (no UI; always false from the app)
    pub subsonic_hz: u32,           // Subsonic filter corner: 0 = off, 10/15/20 Hz
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

> **Fixed in 1.2.9 — the i32 buffer was half a fix.** It keeps every bit of
> integer PCM, which is what it was chosen for, but Symphonia converts a float
> sample into `i32` through a **clamp** at ±1.0. MP3, AAC, Vorbis and Opus
> decoders, and float WAV, legitimately produce samples past full scale. A
> file raised by MP3Gain decodes to +7.42 dBFS with 1.76 % of its frames above
> 1.0 — and every one of them was flattened here, before DC blocking, before
> the filter, before the true-peak ceiling that exists to bring such a file
> down. Integer sources were never affected.
>
> Two things were needed. The packet is now copied straight into `f64`
> (`GenericAudioBufferRef::copy_to_vec_interleaved`): a widening for float
> sources, and for integer ones the same exact division by 2¹⁵ / 2²³ / 2³¹
> that the `i32` path performed. And Symphonia is upgraded 0.5.5 → 0.6.1,
> because 0.5.5 clamps *inside* its own MP3 and Vorbis decoders
> (`synthesis.rs`, `dsp.rs`) where no buffer choice of ours can reach;
> upstream removed both.
>
> Measured: *Mal Bicho* (CD) and a 24-bit 44.1 kHz FLAC come out byte-identical
> to 1.2.8 — SHA-256 equal, with the dither seeded in a test build. The MP3
> above now reaches the ceiling at 7.27 dBTP instead of 3.30 and is scaled by
> −10.27 dB instead of −6.30; level-matched, the two outputs differ by a
> residual 23 dB below the signal, which is what the clipping was. `decode.rs`
> now logs the source peak whenever a file arrives above full scale.

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
**Solution**: Before DSP algorithms hit the buffer, the per-channel means `dc_l` and `dc_r` are computed over the whole file and subtracted — a static offset per channel, not real-time tracking (an earlier version of this note said "real-time signal average tracking"; the code never did that). It removes a constant and nothing else; slow drift and infrasound are what the optional subsonic filter (§5a) is for.

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
- **GPU mode** is locked to a maximum of `2` simultaneous threads. Modern GPU compute nodes can digest 2 concurrent full-resolution convolution streams instantaneously. Locking the pool to 2 secures safe VRAM overhead while keeping conversion well above real time (15× on a measured 12-track album, RTX 4090). *(Since 1.2.6: 3 on machines with 16 or more logical cores, 2 elsewhere — measured, see the comment above `default_workers` in `manager.rs`.)*
- **CPU mode** dynamically scales to `clamp(1, cores / 2)` but maxes out at `4` threads to avoid locking up background OS services.

> **Updated 2026-08 — where the VRAM budget is enforced.** The thread cap above
> still stands, but the *memory* half of this throttle no longer lives in
> `manager.rs`. It used to: `recommended_gpu_workers(settings.taps)` estimated
> per-worker demand once, at batch start, and downgraded the pool to one worker
> when two would not fit.
>
> That estimate was structurally wrong on the default path. The polyphase route
> builds a convolver from **one sub-filter** (`taps / L`), not the whole filter,
> so a 30M-tap ×8 job was sized as if each worker needed 3136 MB when the
> allocator actually reported 416 MB — and every such batch ran single-threaded
> for the rest of the run. It was also unfixable in place: the worker count is
> chosen before any file is decoded, and `L` depends on each file's source rate,
> which legitimately varies inside one batch.
>
> The budget now applies at the point where the real number is known — buffer
> allocation — through `gpu::vram_admission`, a Condvar gate with two rules:
> an idle device admits unconditionally (a convolver larger than the whole
> budget must still be able to run), and a thread already holding a reservation
> is re-entrant (the segmented giant path holds a bank of `L` convolvers at
> once). Together these guarantee at least one thread always makes progress.
> Two workers that cannot be resident simultaneously simply take turns on the
> device while their CPU stages — decode, true-peak, dither, encode, verify —
> overlap. The worst case is the old serialised behaviour.
>
> What the gate cannot prevent is the device refusing outright. By its own
> first rule an idle device admits any single demand however large, and the
> re-entrant bank of `L` convolvers is admitted the same way; the budget it
> checks against is `max_storage_buffer_binding_size × 0.7`, which is a proxy
> and not a measurement. That hole is closed on the other side instead: since
> 1.2.5 a refusal comes back as an error and the affected filter is convolved
> on the CPU, per phase on the polyphase path, rather than taking the process
> down.
>
> The reservation is a field on `GpuDspProcessor`, released on `Drop`, so an
> early-return or a cancelled construction cannot leak budget.

> **Updated 1.2.9 — the budget is now a measurement.** `max_storage_buffer_binding_size × 0.7`
> is a limit on one buffer, not on the card: it reads ~1433 MB on an 8 GB card
> and on a 24 GB one alike, and a 30M ×8 convolver takes 736 MB, so two never
> fitted and every batch ran one convolver at a time however many workers it
> had. On Windows the real number is available: DXGI's per-process video
> memory budget, and the `GPU Adapter Memory` performance counter for what all
> processes together hold on the adapter. `gpu::dxgi_memory` reads both, the
> gate plans against three quarters of the smaller, and the old proxy remains
> the floor for integrated adapters, for a reading that fails, and off Windows.
>
> A refusal from the device is now read for what it says. Refused while other
> workers held convolvers, it lowers this batch's budget to what they held —
> one fewer at a time; only a refusal on an idle device is a ceiling on that
> size.
>
> Measured, 13 files at 30M ×8 with three workers on an RTX 4090: 221.6 s under
> the old floor, 179.5 s under the reading (two runs each), with 160 s of
> worker time spent queued at the gate per run in the first case and none in
> the second. Output byte-identical.

### 15.1a Cross-File Filter Spectrum Cache (2026-08)

Two artefacts of convolver construction are pure functions of their inputs and
were nevertheless rebuilt every single time:

- the **DS twiddle table** — `N/2` f64 `sin`/`cos` evaluations plus hi/lo
  splits, depending on nothing but the FFT size;
- the **partitioned filter spectrum `H[ω]`** — `K` rustfft transforms of length
  `N` in `Complex<f64>`, depending on nothing but the coefficients.

On the polyphase path a convolver is built per sub-filter — `L` per pass, `2L`
per file with Hybrid-Phase — and every file in a batch rebuilt the *same*
spectra again. `gpu::filter_cache` keys them on the filter's provenance and
hands out immutable `Arc`s, so a hit uploads byte-for-byte what a miss would
have computed. Measured on the 30M matrix filter: **−74 % per construction**,
64 of 96 constructions served from cache in a six-file batch.

**The key must carry `L`.** `find_precomputed_filter` resolves on the OUTPUT
rate, so 44.1 kHz and 88.2 kHz sources at FS×8 both load
`fir_30M_352800_linear_phase.npy` and then decompose it at different strides
(L = 8 vs L = 4). Because `block_size()` clamps at `GPU_MAX_BLOCK_SIZE`, both
land on the same `N = 4194304` — a key of `(path, phase, N)` collides across
them and would serve one file the other's filter, silently. `FilterId` therefore
carries `l`, and `get_spectrum` additionally refuses any hit whose length
disagrees with the caller's `h_freq` buffer, so an incomplete key degrades into
a recomputation and a loud log line rather than corrupt audio.

Cache policy is *fill to a byte cap, then stop inserting* — deliberately not
LRU. The access pattern is strictly cyclic (phase `0..L`, then min-phase
`0..L`, repeat next file), and LRU is pathological on cyclic access: it evicts
precisely the entry needed next. Filling and stopping gives a 100 % hit rate
when the working set fits and partial hits when it does not. `clear()` runs at
batch start (so a filter regenerated by `fir-optimizer` between runs is picked
up) and at batch end (so an idle app does not sit on gigabytes of spectra).

### 15.2 In-Flight Decode Channel Clamping
**Issue**: The CPU decode logic was eagerly buffering decoded track contents into massive 64-bit float arrays (`100M+` samples) faster than the GPU could process them, ballooning active RAM footprint.
**Solution**: The `tx_prep_bound` queue has been shrunk to exactly match the active thread worker size. The processing buffer stays fully flushed.

### 15.3 Dynamic RAM Guard for Synchronous Pausing
**Issue**: Pushing 100 long FLACs queued standard allocations that outpaced OS Garbage Collection, running out of RAM completely.
**Solution**: Enhanced `sysinfo` integration via `await_free_ram` inside `memory.rs`. The decoding pre-processor now synchronously halts memory allocation and idles if the OS registers less than ~3GB free payload. The application waits securely rather than causing unhandled pointer allocation failures.
