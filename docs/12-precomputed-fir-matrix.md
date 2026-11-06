# Pre-computed FIR Matrix — One Filter Per Conversion Ratio

> **Files**:
> * `fir-optimizer/optimize.py`         — generator (Python)
> * `fir-optimizer/output/fir_*_*.npy`  — generated artefacts
> * `desktop-app/src-tauri/src/audio/converter/dsp/filter.rs::find_precomputed_filter` — runtime resolver
> **Last updated**: 2026-10-23 (1.8.0: rungs to 1G, ×32, branch-0 files — [section 11](#branch0))
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
9. [The 5k rung — a window fitted to the length](#short)
10. [Multistage upsampling and the blobs (1.7.0)](#multistage)
11. [Rungs from 100M to 1G — branch-0 files (1.8.0)](#branch0)

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
| TAG        | `5k` · `1M` · `5M` · `10M` · `30M` · `100M` · `180M` · `300M` · `500M` · `1G` (matches Rust tap thresholds) |
| TARGET_HZ  | output sample rate as integer Hz (e.g. `88200`, `352800`, `1411200`) |
| phase      | `linear_phase` · `minimum_phase`                          |

Examples:
```
fir_30M_352800_linear_phase.npy   ← 44.1 kHz × 8
fir_1M_88200_minimum_phase.npy    ← 44.1 kHz × 2
fir_5M_768000_linear_phase.npy    ← 48 kHz × 16
fir_5k_352800_minimum_phase.npy   ← 44.1 kHz × 8
fir_30M_1411200_linear_phase.npy  ← 44.1 kHz × 32
```

From 100M up (1.8.0) a rung is not one whole `.npy` but three files with the
same stem plus `.b0.` — see [section 11](#branch0):

```
fir_300M_352800_linear_phase.b0.npy        ← branch 0 of the 300M filter, 44.1 kHz × 8
fir_300M_352800_linear_phase.b0.ends.npy
fir_300M_352800_linear_phase.b0.json
```

The `k` is lowercase, the `M` uppercase — exactly what the generator writes
and the resolver asks for.

**The runtime keys directly off this format.** Any rename breaks `find_precomputed_filter`.

---

## 3. Lookup algorithm in Rust <a id="lookup"></a>

`desktop-app/src-tauri/src/audio/converter/dsp/filter.rs::find_precomputed_filter(taps, src_rate, out_rate, phase_type)`:

```
input:  taps, src_rate, out_rate, phase_type
output: Option<full_path_to_npy>

0. design_rate = family base (44 100 / 48 000) × L, where L = out_rate / src_rate
       — the rate the blob was designed for; a CD-rate source gives
       design_rate = out_rate, as before (an out-of-family or non-integer
       ratio also falls back to out_rate). Below, target_rate_hz = design_rate.
1. derive TAG from taps:
       taps ≥ 750 M  → "1G"
       taps ≥ 400 M  → "500M"
       taps ≥ 240 M  → "300M"
       taps ≥ 140 M  → "180M"
       taps ≥ 65 M   → "100M"
       taps ≥ 25 M   → "30M"
       taps ≥ 7.5 M  → "10M"
       taps ≥ 2.5 M  → "5M"
       taps ≥ 500 k  → "1M"
       taps ≥ 2.5 k  → "5k"
       else          → return None
2. for each candidate dir (relative-to-exe and hardcoded workspace path):
   a. try   fir_<TAG>_<target_rate_hz>_<phase>.npy            ← preferred
   a2. else fir_<TAG>_<target_rate_hz>_<phase>.b0.npy         ← branch-0 rung (1.8.0)
   b. else, when target_rate_hz ∈ {352800, 384000} only,
      try   fir_<TAG>_<phase>.npy                             ← legacy fallback
3. return None if nothing matched
```

Step 0 arrived in 1.5.0. Before it the blob was chosen by the output rate
alone, and every blob is designed for a 44.1 or 48 kHz source — so a 96 kHz
file going to 384 kHz got the filter made for 48 kHz going to 384 kHz, with its
wall at 23 kHz, and lost what it held above that. Now 96 → 384 kHz is ×4 and
takes the ×4 blob (`fir_<TAG>_192000_…`): run at 384 kHz, its passband
reaches 44 kHz and its stopband starts at the source's own Nyquist, 48 kHz.

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
* taps     ∈ {5 000, 1 000 000, 5 000 000, 10 000 000, 30 000 000}
* source   ∈ {44 100, 48 000} Hz
* multiplier ∈ {2, 4, 8, 16}
* phase    ∈ {linear, minimum}

= **80 .npy files** total. (The ×32 rates added in 1.8.0 — 1411.2 and 1536 kHz —
are not part of `--all-ratios`; they ship as packs, see section 5.) Each (taps × source × multiplier) triple is
generated once, producing a linear+minimum phase pair.  Existing files
are skipped, so the command is restartable.

Pass-band / stop-band per source rate (set in `_band_for_source`):

```
44.1 kHz source → f_passband = 20 050 Hz, f_stopband = 22 050 Hz
48   kHz source → f_passband = 22 000 Hz, f_stopband = 24 000 Hz
```

(Both leave 2 kHz of transition band; tighter than the legacy 4 kHz, but
still clean for the 1M-tap case and gives a much sharper brick wall on
30M-tap.) Every size puts its cutoff in the middle of that band. From 1M up
the window is Kaiser β = 14; the 5k filter's β is fitted per output rate —
[section 9](#short).

> **Time budget**: the 30M-tap rows take ≈ 5–10 minutes each on a desktop
> CPU (it's a 600M-point f128 FFT internally). Plan an overnight run for
> a full matrix on one machine; the smaller (1M / 5M) rows finish in
> minutes, and all sixteen 5k blobs in about a minute, most of it spent
> fitting the window.

---

## 5. What gets generated and how big it is <a id="size"></a>

| TAG | One file size | Files / TAG | Per TAG total |
|-----|---------------|-------------|---------------|
| 5k  | ~40 KB        | 16          | 642 KB        |
| 1M  | ~8 MB         | 16          | 128 MB        |
| 5M  | ~40 MB        | 16          | 640 MB        |
| 10M | ~80 MB        | 16          | 1.3 GB        |
| 30M | ~240 MB       | 16          | 3.8 GB        |
| **Total** |          | **80**      | **≈ 6 GB**    |

Per TAG: 2 sources × 4 multipliers × 2 phases = 16 files.

Since 1.8.0 each TAG also has its two ×32 rates (1 411 200 and 1 536 000 Hz) —
four more files per TAG (two rates × two phases), the same size as any other
file of that TAG — and five more TAGs exist above 30M, stored differently:
[section 11](#branch0).

Generating only the TAG sizes you actually use is fine — see section 8 for
what the app does with a partial matrix. What it will *not* do is stand in a
different filter for a missing one (section 7).

Two pre-built shapes exist on the release page, both extracting to this same
`fir-optimizer/output/` layout:

* **Filter packs** — blobs only, one per TAG (`aura-filters-10M-all-rates.zip`
  and friends). The 30M blobs are split into `-44k-family` and `-48k-family`
  because a single archive of all 16 would be 3.8 GB, past GitHub's 2 GB
  per-asset limit.
* **×32 packs** (1.8.0) — `aura-filters-<5k|1M|5M|10M|30M>-x32.zip`, the two
  ×32 rates of one old TAG each (about 0.001, 0.03, 0.16, 0.32 and 0.96 GB).
* **Big-rung packs** (1.8.0) — `aura-filters-<100M|180M|300M|500M|1G>-<44k|48k>-family-<linear|minimum>.zip`,
  the branch-0 files of one rung, one rate family and one phase (about 0.38,
  0.68, 1.13, 0.88 and 1.75 GB). See [section 11](#branch0).
* **Bundles** (1.8.1: by output rate) — the app plus filters in one zip, built by
  `tools/make-bundles.ps1`: **Lite** (5k and 1M at every ×2..×32, ~0.17 GB),
  **×8** (5k–30M at ×8 and 5k–10M at ×4), **×16** (5k–30M at ×16 and 5k–10M at ×8),
  **×32** (5k–30M at ×32 and 5k–10M at ×16), ~2.0 GB each. 100M and longer come
  only from the in-app download.
* **In-app download** (1.8.1) — the app lists what a setting needs but the disk
  lacks and downloads it from the public release `filters`: one asset per file
  plus the signed `filters-manifest.json` and `.sig`; a file is accepted only at
  the listed size and SHA-256, `.part` files resume after Cancel. The Storage tab
  lists filter files the current version no longer uses and moves them to the
  Recycle Bin.

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
multi-gigabyte download, and the bundles ship only a slice of it (since 1.8.1
the rest is one click away in the app). A
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
| Returning user, settings still valid | Nothing. Their choice stands. Settings hold the tap count itself; a position saved before 1.2.7 — when position 0 meant 1M — is read against the old ladder, so nobody drops a rung when 5k appears below it. |
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

---

## 9. The 5k rung — a window fitted to the length <a id="short"></a>

Added in 1.2.7. The ladder is `5k | 1M 5M 10M 30M`: five thousand taps, then
straight to a million.

### Why not β = 14

Every filter from 1M up is Kaiser β = 14. At that length the window is not
what limits the stopband. At 5 000 taps it is: the same β gives −145 dB at
FS8, far short of the long filters. So the 5k filter keeps everything else —
the band, the cutoff in its middle (21 050 Hz for 44.1 kHz sources, 23 000 Hz
for 48 kHz), the even length, the cepstral minimum-phase twin — and changes
only the window.

### How β is chosen

`short_filter_beta()` in `optimize.py`, for every output rate separately:
the **smallest** β whose worst stopband, from the band's stop edge (22 050 /
24 000 Hz) up, is at or below **−220 dB** — what the 30M filter measures.
Smallest, because a wider window than the target needs only softens the wall
further. The worst stopband is unimodal in β (sidelobes set it until the
widening main lobe reaches the stop edge), so the search finds the deepest
point first and bisects down from it; where even that misses −220 dB, the
deepest point is used. β is measured, not taken from the Kaiser design
formula, which at 220 dB overstates what a given β achieves by about 13 dB.

| output rate | β | flat (±0.01 dB) to | worst stopband | 1M at the same rate |
|---|---|---|---|---|
| 88 200 | 21.44 | 20 975 Hz | −220.0 dB | −203.4 dB |
| 176 400 | 22.11 | 20 897 Hz | −220.0 dB | −197.4 dB |
| 352 800 | 22.82 | 20 738 Hz | −220.0 dB | −191.0 dB |
| 705 600 | 22.26 | 20 435 Hz | **−208.2 dB** (deepest reachable) | −185.2 dB |
| 96 000 | 21.48 | 22 918 Hz | −220.1 dB | −202.3 dB |
| 192 000 | 22.26 | 22 833 Hz | −220.0 dB | −196.4 dB |
| 384 000 | 22.96 | 22 660 Hz | −220.1 dB | −190.5 dB |
| 768 000 | 20.45 | 22 362 Hz | **−192.6 dB** (deepest reachable) | −184.6 dB |

Each value is in the blob's `_meta.json` (`design.beta`, `stats.flat_to_hz`,
`stats.stopband_atten_db`). They were re-measured by direct summation of
`Σ h[n]·e^(−jωn)` on a grid of 64 points per sidelobe — no FFT, no code shared
with the generator — and agree to 0.05 dB and 0.5 Hz, for both phases.

### What it gives up

Where the wall is steep. A 30M filter is flat to 21 049.9 Hz; the 5k filter
is flat to 20.4–21.0 kHz depending on the multiplier and falls to −220 dB
over the next kilohertz or so. The cutoff is in the same place, so the part of
the spectrum where the two differ is the last kilohertz below the source's
Nyquist — nothing below 20 kHz. Its image rejection is at least that of the
1M filter at every rate, and at FS2–FS8 that of the 30M one.

At FS16 five thousand taps cannot reach −220 dB inside a 2 kHz band: that
multiplier spreads the same length over twice the output rate. It gets
−208 dB (44.1 kHz family) and −193 dB (48 kHz family) — still deeper than 1M
at that rate.

### Measured through the app

Mal Bicho (Santana, *Corazón*), 44.1 → 352.8 kHz, converted by the real
pipeline (`mem_soak_convert_batch`) at 5k and at 30M with everything else
equal. Left channel, two seconds trimmed from each end.

**Linear phase only** (no AA, no Hybrid-Phase). No offset between the two
files. The true-peak ceiling scaled them by 0.596836 and 0.596708 — the 5k
output peaks 0.002 dB lower, its gentler wall overshooting less. With that
single gain taken out:

| band | 30M output | 5k output | difference |
|---|---|---|---|
| 0–20 kHz | −13.8 dBFS | −13.8 dBFS | **−150.6 dBFS** (the two dithers) |
| 20–20.7 kHz | −59.8 | −59.8 | −154.9 |
| 20.7–22.05 kHz | | | **−88.6** |
| above 22.05 kHz | −145.1 | −145.1 | −142.1 (dither) |

The whole difference is the top of the CD band, where the wall is.

**With Hybrid-Phase** the outputs differ more: −52 dBFS RMS in 0–20 kHz,
peaking at attacks. That is the minimum-phase branch, and it is a property of
minimum phase, not of this rung: a minimum-phase filter's phase follows from
its whole magnitude response, so two filters with different walls have
different phase well below the wall. Each branch is aligned to its own linear
render (49 samples for 5k, 51 for 30M); after that the two branches still
differ by 0.3 samples below 6 kHz, 1.2 at 15 kHz and 6 at 20 kHz. The
alignment itself behaves the same for both: the band-weighted estimate sits
1.5–1.8 samples above the true low-frequency phase delay (47.2 and 49.5).

GPU and CPU renders of the same 5k conversion differ by −151 dBFS — dither.

**Time**, one file (3 min 38 s), AA and Hybrid-Phase on, i9-14900K + RTX 4090,
nothing else running:

| | GPU | CPU |
|---|---|---|
| 30M | 28.3 s (four runs; one outlier at 34.4) | 56.1 s |
| 5k | 22.4 s | 20.0–22.0 s |

At 5k the convolution stops being the cost, on either device: what is left
is decoding, the apodizer, encoding and verification. On this machine the
CPU is as fast as the GPU at that length, so the GPU checkbox is left to mean
what it says rather than being overridden below some tap count.

### What else had to hold at this length

* **Polyphase split.** 5 000 taps split into 625 per branch at FS8 and
  312–313 at FS16 (5 000 is not a multiple of 16); `polyphase_decompose`
  pads the short branches with zeros, which is exact.
* **Hybrid-Phase alignment.** `estimate_band_weighted_group_delay` pads a
  5k filter to a 16 384-point FFT. Against a 4 194 304-point reference the
  estimate moves by at most 0.02 samples; one rate (705.6 kHz, 93.51 vs 93.49)
  rounds to the neighbouring integer — 1.4 µs, below anything the blend can
  resolve. `GROUP_DELAY_ANALYSIS_TAPS` needs no change.
* **Minimum phase.** The cepstral conversion aliases when the FFT is too
  short: at 2^20 points the 96 kHz twin carried 1.7·10⁻⁴ dB of ripple at the
  band edge. `get_n_fft` now never goes below 2^23, which brings it to
  4·10⁻⁷ dB. From 524 288 taps up that floor was already met, so no 1M–30M
  blob is affected.
* **Convolution.** Both convolvers are block-FFT, so their cost does not fall
  in proportion to the tap count: a 5k filter still pays for the block
  transforms. At this length the conversion time is set by the stages around
  the convolution — the apodizer, encoding and verification.

---

## 10. Multistage upsampling and the blobs (1.7.0) <a id="multistage"></a>

The blobs are unchanged: the same files, the same names, nothing regenerated.
With multistage upsampling (the default since 1.7.0, for 5M taps and longer at
×2/×4/×8/×16, and ×32 since 1.8.0) only **branch 0** of the blob's polyphase split is used — the
branch that lands on the source's own sample times. It runs at the **source
rate** as stage 0; the short stages after it are computed in code and need no
file. A filter of 1M taps or fewer, and a custom filter, still use all branches
as before. See [21 — Multistage upsampling](21-multistage-upsampling.md).

---

## 11. Rungs from 100M to 1G — branch-0 files (1.8.0) <a id="branch0"></a>

The ladder has five more rungs above 30M — **100M, 180M, 300M, 500M and 1G**
(`TAP_LADDER` in `filter.rs`) — offered from FS4 (100M–300M) or FS8 (500M, 1G)
up to FS32 (`min_fs`). A whole 1G-tap filter at ×32 would be 8 GB, and the
multistage upsampler reads only branch 0 of it, so these rungs are **stored as
branch 0 alone**. For a filter of `N` taps designed for ×`L`:

| File | Content |
|---|---|
| `<stem>.b0.npy` | `h[k·L]`, k = 0 … ⌈N/L⌉−1 — branch 0 at the **source** rate, f64. `L` times smaller than the whole filter |
| `<stem>.b0.ends.npy` | the first 131 072 taps of the whole filter, then its last 64 |
| `<stem>.b0.json` | format `aura-branch0` v1, `taps`, `l`, `source_rate`, `target_rate`, `phase`, `sum_bits` and `sum` — Σh of the whole filter, added left to right as the runtime sums a whole file — `head_taps`, `tail_taps`, and the generator's settings |

The ends and the sum give the runtime what it used to read off the whole file:
the gain of stage 0, the group delay and the symmetry check. The result is bit
for bit what the whole filter gives: a test (ignored by default, it needs the
large blobs) loads whole 30M–200M blobs and their branch-0 files, linear and
minimum phase, and finds identical stage-0 banks.

`find_precomputed_filter` takes the `.b0.npy` after the whole file in each
folder (step a2 above). `load_npy_f64_branch(path, L, 0)` reads it;
`load_npy_f64` refuses it, so the single-stage path and the other routes that
need the whole filter never play branch 0 as if it were one. The hidden switch
back to single-stage ([21](21-multistage-upsampling.md#switching-back-to-single-stage))
therefore does nothing for these rungs.

**Generating them.** `fir-optimizer/gen_branch0.py <out_dir> <multipliers> <taps>
[linear|minimum|both] [44100,48000]` (taps as numbers or tags, e.g.
`100M,180M,300M,500M,1G`) writes the three files without ever holding the
filter:

* *Linear phase* evaluates the Kaiser closed form piece by piece at `k·L`, with
  the same element-wise calls and the same pairwise normalising sum
  `optimize.py` makes over the whole array.
* *Minimum phase* up to 268M taps is the full-rate cepstrum with fewer arrays
  alive at once; above it (300M, 500M, 1G) `minphase_branch0.py` does the same
  in one half-size array with a packed four-step real FFT (1G taps: 16 GB of
  memory).

Both reproduce the whole 30M–200M files bit for bit.

**Where they come from.** Since 1.8.1 the app downloads them itself (the
"missing filter" line above the rack opens the download dialog); the big-rung
packs of section 5 are the manual way. The bundles carry 5k–30M only, so
everything from 100M up is a download. The
packs split by rate family and phase: linear phase is what most people need,
and the minimum-phase pack only matters for minimum-phase and Hybrid-Phase use.

**Does a longer filter sound better?** Not by depth: the worst stop band
(44.1 kHz × 8, linear phase) is −191 dB at 1M, −205 at 5M, −211 at 10M,
**−220.9 dB at 30M**, then −216 dB at 100M, −211 at 180M and −207 at 300M — the
float64 floor is reached at 30M and rounding eats into longer filters. What the
extra taps buy is a steeper wall at the cutoff. The taps slider marks 30M
*optimal* for that reason.
