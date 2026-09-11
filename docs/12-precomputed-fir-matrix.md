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
8. [How a build adapts to the blobs it has](#inventory)

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
the caller (`process.rs` / `hybrid_mixer.rs`) fails that file with a message
naming the blob it wanted — see [section 7](#missing).

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

Generating only the TAG sizes you actually use is fine — see section 8 for
what the app does with a partial matrix. What it will *not* do is stand in a
different filter for a missing one (section 7).

Two pre-built shapes exist on the release page, both extracting to this same
`fir-optimizer/output/` layout:

* **Filter packs** — blobs only, one per TAG (`aura-filters-10M-all-rates.zip`
  and friends). The 30M blobs are split into `-44k-family` and `-48k-family`
  because a single archive of all 16 would be 3.8 GB, past GitHub's 2 GB
  per-asset limit.
* **Bundles** — the app plus one TAG's blobs in one zip, built by
  `tools/make-bundles.ps1`. The 1M bundle carries all 8 rates; the 10M and
  30M ones carry the FS8 pair only, which is what keeps them at 326 MB and
  966 MB instead of 1.3 GB and 3.8 GB.

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

If `find_precomputed_filter` returns `None`, **the conversion of that file
fails.** It is not a warning and there is no fallback path.

Earlier versions skipped the post-FIR stage and let the rubato resampler's
output go downstream, with Hybrid-Phase skipped alongside it. That was the
worst possible outcome: the file still appeared, the interface still said
"30M Taps", the filename still carried `Kaiser 30M`, and only a line in the
console said otherwise. A file that misdescribes itself is worse than no file.

The error names the exact blob, lists every directory searched for it, and
points at the release pack that contains it:

```
[CONV] Missing FIR filter - conversion stopped.

Needed: fir_30M_705600_linear_phase.npy
For: 30M taps at 705600 Hz output (linear-phase).

Searched:
    <exe dir>/fir-optimizer/output
    <exe dir>/filters
    ...
```

The failure is per file - the row is marked and the batch continues - and the
same check also runs *before* a batch starts, so a missing filter is reported
without the user waiting through a conversion that cannot happen.


---

## 8. How a build adapts to the blobs it has <a id="inventory"></a>

Section 7 is what happens when the user asks for a filter that is not there.
This section is about not letting them ask.

No two installations hold the same blobs: the matrix is a separate,
multi-gigabyte download, and the bundles deliberately ship one TAG each. A
build whose interface always opened on 30M · FS8 would greet most users with
an error before they had done anything — which is exactly what the bundles
exist to prevent.

So the app asks at startup. `filter::inventory()` walks the full
`TAP_LADDER × TARGET_RATES` grid and reports, per cell, whether the linear
and minimum-phase blobs resolve. It answers by calling
`find_precomputed_filter` rather than by listing directories: a directory
walk would be a second, subtly different definition of "available", and the
two would drift apart at the first naming change — leaving the interface
offering a setting that then fails. `inventory_agrees_with_the_resolver_cell_for_cell`
pins them together.

The `filter_inventory` command hands that grid to the frontend along with the
pack that would fill each TAG in. From it, `src/js/inventory.js` decides:

| Situation | What the app does |
|---|---|
| First run | Opens on the largest TAG present, at FS8 if that TAG has it. FS8 is preferred across the whole ladder before any other multiplier is tried. |
| Returning user, settings still valid | Nothing. Their choice stands. |
| Returning user, blobs gone | Moves to the nearest working pair — TAG held, multiplier moved first — and says so in the status line. Persists the correction, so it happens once. |
| A TAG or multiplier with no blobs | Its slider mark is struck through. The slider still reaches it. |
| That position selected anyway | A panel names the download that would add it, and opens it on click. |
| Nothing installed at all | A red panel says the converter cannot run yet, and points at a pack. No setting is moved. |
| The probe itself failed | Every combination reads as available and nothing is moved. A wrong "you have this" costs one pre-flight error; a wrong "you don't" would hide filters the user really has. |

The marks are advisory. Nothing here decides whether a conversion may run —
the pre-flight `check_filters` does, from the real source rates, because only
it knows whether a queue is 44.1 or 48 kHz material. A user holding just
`aura-filters-30M-44k-family.zip` sees FS8 as available, which is true for
their CDs and false for their 48 kHz files; the per-file check is what draws
that line.

The decision table above is covered by `desktop-app/tests/inventory.test.mjs`
(`node --test "desktop-app/tests/**/*.test.mjs"`), one case per row.