# Pre-computed FIR Matrix — One Filter Per Conversion Ratio

> **Files**:
> * `fir-optimizer/optimize.py`         — generator (Python)
> * `fir-optimizer/output/fir_*_*.npy`  — generated artefacts
> * `desktop-app/src-tauri/src/audio/converter/dsp/filter.rs::find_precomputed_filter` — runtime resolver
> **Last updated**: 2026-05-07
> **Status**: ✅ Production · merged in commit after `ca1af01`
> **Audience**: Code auditors, DSP developers

---

## TL;DR

The pre-computed FIR coefficient blobs in `fir-optimizer/output/` are now
keyed on **output sample rate**, not just on tap count. The runtime loads
the blob whose cutoff was designed for the rate it's actually outputting,
so FS2 / FS4 / FS8 / FS16 conversions all use a filter whose −3 dB point
sits at the correct Hz value. The previous single-blob-per-tap-count
scheme silently mis-applied the FS8-design filter at every other ratio,
which collapsed FS2 audio to a 5 kHz cutoff.

---

## Table of Contents

1. [The bug this replaces](#bug)
2. [Naming convention](#naming)
3. [Lookup algorithm in Rust](#lookup)
4. [How to generate the matrix](#generate)
5. [What gets generated and how big it is](#size)
6. [Backward compatibility with legacy blobs](#backcompat)
7. [What happens when a file is missing](#missing)

---

## 1. The bug this replaces <a id="bug"></a>

The original scheme shipped a single coefficient file per tap count:

```
fir_30M_linear_phase.npy         (designed for 48 kHz → 384 kHz)
fir_30M_minimum_phase.npy
fir_10M_linear_phase.npy
…
```

`fir-optimizer/config.json` showed the design point:
```json
{ "fs_source": 48000, "fs_target": 384000,
  "freq_params": { "f_passband_hz": 20000, "f_stopband_hz": 24000 } }
```

The blob's normalised cutoff is fixed at `22 kHz / (384 kHz / 2) ≈ 0.1146`.
Applied at any other output rate that same number lands at the wrong Hz:

| Mode  | Output rate  | Effective cutoff | Audible result            |
|-------|--------------|------------------|---------------------------|
| FS2   | 88.2 kHz     | **5 kHz**        | severely muffled          |
| FS4   | 176.4 kHz    | **10 kHz**       | dull                      |
| FS8   | 352.8 kHz    | **22 kHz**       | ✓ design point            |
| FS16  | 705.6 kHz    | 44 kHz           | inaudible (above hearing) |

A real user run at FS2 (commit `ca1af01` log) heard the audio "as if it's
11 kHz wide" because everything above ~5 kHz was filtered out.

The interim fix (`ca1af01`) detected the mismatch and skipped the post-FIR
entirely at non-FS8 rates, falling back to the rubato resampler's own
anti-imaging. That was a damage-control patch. **This document describes
the proper fix**: a per-ratio filter matrix.

---

## 2. Naming convention <a id="naming"></a>

```
fir_<TAG>_<TARGET_HZ>_<phase>.npy
```

| Field      | Values                                                    |
|------------|-----------------------------------------------------------|
| TAG        | `1M` · `5M` · `10M` · `30M` (matches Rust tap thresholds) |
| TARGET_HZ  | output sample rate as integer Hz (e.g. `88200`, `352800`) |
| phase      | `linear_phase` · `minimum_phase`                          |

Examples:
```
fir_30M_352800_linear_phase.npy   ← 44.1 kHz × 8
fir_1M_88200_minimum_phase.npy    ← 44.1 kHz × 2
fir_5M_768000_linear_phase.npy    ← 48 kHz × 16
```

**The runtime keys directly off this format.** Any rename breaks `find_precomputed_filter`.

---

## 3. Lookup algorithm in Rust <a id="lookup"></a>

`desktop-app/src-tauri/src/audio/converter/dsp/filter.rs::find_precomputed_filter(taps, target_rate_hz, phase_type)`:

```
input:  taps, target_rate_hz, phase_type
output: Option<full_path_to_npy>

1. derive TAG from taps:
       taps ≥ 25 M   → "30M"
       taps ≥ 7.5 M  → "10M"
       taps ≥ 2.5 M  → "5M"
       taps ≥ 500 k  → "1M"
       else          → return None
2. for each candidate dir (relative-to-exe and hardcoded workspace path):
   a. try   fir_<TAG>_<target_rate_hz>_<phase>.npy            ← preferred
   b. else, when target_rate_hz ∈ {352800, 384000} only,
      try   fir_<TAG>_<phase>.npy                             ← legacy fallback
3. return None if nothing matched
```

The legacy fallback (step 2b) exists so users with an old install (only
the FS8 blobs in `fir-optimizer/output/`) continue to get a working FS8
conversion. Any other ratio without the new matrix returns `None`, and
the caller (`process.rs` / `hybrid_mixer.rs`) skips the post-FIR /
Hybrid-Phase step with a clear log line.

---

## 4. How to generate the matrix <a id="generate"></a>

```bash
cd fir-optimizer
python optimize.py --all-ratios
```

The new `--all-ratios` flag iterates every combination of:
* taps     ∈ {1 000 000, 5 000 000, 10 000 000, 30 000 000}
* source   ∈ {44 100, 48 000} Hz
* multiplier ∈ {2, 4, 8, 16}
* phase    ∈ {linear, minimum}

= **64 .npy files** total. Each (taps × source × multiplier) triple is
generated once, producing a linear+minimum phase pair.  Existing files
are skipped, so the command is restartable.

Pass-band / stop-band per source rate (set in `_band_for_source`):

```
44.1 kHz source → f_passband = 20 050 Hz, f_stopband = 22 050 Hz
48   kHz source → f_passband = 22 000 Hz, f_stopband = 24 000 Hz
```

(Both leave 2 kHz of transition band; tighter than the legacy 4 kHz, but
still clean for the 1M-tap case and gives a much sharper brick wall on
30M-tap.)

> **Time budget**: the 30M-tap rows take ≈ 5–10 minutes each on a desktop
> CPU (it's a 600M-point f128 FFT internally). Plan an overnight run for
> a full matrix on one machine; the smaller (1M / 5M) rows finish in
> minutes.

---

## 5. What gets generated and how big it is <a id="size"></a>

| TAG | One file size | Files / TAG | Per TAG total |
|-----|---------------|-------------|---------------|
| 1M  | ~8 MB         | 16          | 128 MB        |
| 5M  | ~40 MB        | 16          | 640 MB        |
| 10M | ~80 MB        | 16          | 1.3 GB        |
| 30M | ~240 MB       | 16          | 3.8 GB        |
| **Total** |          | **64**      | **≈ 6 GB**    |

Per TAG: 2 sources × 4 multipliers × 2 phases = 16 files.

If disk pressure is a concern, generate only the TAG sizes you actually
use — the runtime falls back gracefully to skip post-FIR for unavailable
combinations rather than crashing.

---

## 6. Backward compatibility with legacy blobs <a id="backcompat"></a>

The pre-existing `fir_<TAG>_<phase>.npy` files (no target rate in the
name) are kept by the resolver as a fallback **only when the requested
output rate is one of the FS8 design points**:

* `352 800 Hz` (44.1 × 8)
* `384 000 Hz` (48 × 8)

For any other target the resolver ignores the legacy blob and returns
`None`, so the runtime won't silently mis-apply it the way pre-`ca1af01`
code did.

---

## 7. What happens when a file is missing <a id="missing"></a>

If `find_precomputed_filter` returns `None`:

* **post-FIR** in the standard path is skipped. Audio comes straight from
  the rubato resampler (sinc_len=512, oversampling=512 → ~−180 dB
  stop-band, more than enough as anti-imaging on its own).
* **Hybrid-Phase** is also skipped, with a separate warning, because
  the linear-phase output it would blend against was never produced.

Both warnings are emitted via `aelog!` so they appear in the timestamped
heartbeat-aware console log:

```
[01:51:14.530] [CONV] WARNING: no pre-computed FIR found for taps=30000000 target=88200Hz (linear_phase). \
                       SKIPPING post-FIR — rubato resampler (sinc_len=512, ~−180 dB stop-band) output \
                       goes straight downstream. Run `python fir-optimizer/optimize.py --all-ratios` \
                       to populate the per-rate filter matrix.
```

This is intentional: missing-file should ALWAYS be a clearly visible
runtime warning, never a silent quality regression.
