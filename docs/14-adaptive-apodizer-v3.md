# 14 — Adaptive Apodizer v3: Source Forensics

**Status:** implemented 2026-07-13, field-calibrated and validated on live material 2026-07-13/14 · `desktop-app/src-tauri/src/audio/converter/apodize.rs`, wired in `pipeline/prepare.rs`

## Why v3

v2 answered one question — *"do attacks carry near-Nyquist pre-ringing?"* — and mapped
the answer to one of three preset cutoffs (0.816/0.862/0.907 × Nyquist). It worked, but:

1. **Hi-res containers were skipped entirely** (`sample_rate > 48000 → skip`), yet
   upsampled 44.1/48k masters sold as hi-res ("fake hi-res") are exactly the files
   where apodizing helps most — the original ADC brick-wall ring at ~22 kHz is still
   baked into the 96k container.
2. The detector answered **whether** there is ringing, not **at what frequency** —
   the three buckets were guesses around the true filter edge.
3. Measured severity (`med_sev`) was computed, logged… and discarded.
4. Material without transients (ambient, legato strings) returned `None` even when
   the source filter was demonstrably a brick wall.

## What v3 measures

All detectors share one analysis pass (`analyze_source`) and feed one decision
function (`decide_apodizer`). The v2 time-domain core is preserved verbatim
(thresholds untouched — they survived field tuning); it is parameterized by band
and surrounded by new measurements.

### 1. Spectral-cliff detector (`analyze_spectral_cliff`)

Welch long-term spectrum (16384-point Hann, ≤128 segments spread over the whole
track, ~120 Hz smoothing). A **cliff** = ≥20 dB dropped inside 1/12 octave
(≥240 dB/oct sustained — no natural source does this) between max(8 kHz, 0.2×Ny)
and 0.995×Ny. For cliffs below 0.90×Ny the region above must be floor-like
(≥25 dB below the passband shoulder), which protects steep-but-natural spectra.

* Cliff **well below** a hi-res container's Nyquist ⇒ fake hi-res. The origin rate
  is snapped to the legacy grid (44.1/48/88.2/96/176.4/192k) and all further
  analysis runs against the **effective (original) Nyquist**.
* Cliff **near** Nyquist on a 44.1/48k source ⇒ ordinary mastering brick wall.

### 2. Ring-frequency estimation

Pre-ring oscillates at the source filter's transition frequency. For every attack
classified as ringing, the isolated HF band over the −9…−3 ms window is
Hann-windowed and FFT'd (8192 bins); the median peak across attacks — accepted only
when ≥60 % of bursts agree within ±8 % — is the measured filter edge. The apodizer
cutoff lands at `f_ring × margin` (margin 0.97/0.94/0.91 by the v2 fraction ladder,
clamped to [0.80, 0.93]×effective-Nyquist) instead of a preset bucket.

### 3. Severity → filter depth

Median ring-over-background (dB) selects β: ≥8 dB → β=24 (~240 dB stopband),
milder → β=14 (~140 dB — still inaudible rejection, but with a visibly shorter
time-domain signature of the apodizer itself). Taps scale with the container rate
(4096 @ ≤48k … 32768 cap) so the transition stays narrow in Hz.

### 4. Low-transient (spectral-only) path

When fewer than 8 attacks are judgeable, a detected cliff alone triggers **gentle**
treatment: `fc = min(0.96 × cliff, 0.93 × Ny_eff)`, β=14. Guard: never fires on a
true hi-res container's own ADC band (cliff ≥ 0.90 × container Nyquist at >48k) —
every honest 96k recording has its ADC filter near 43–47 kHz and that is not a defect.

### 5. Mirror-image alias probe (`probe_mirror_aliasing`)

A bad upstream SRC (linear interpolation, leaky filters, ZOH) leaves **mirror
images** of the original content above the original Nyquist: a tone at *f* gets a
twin at *2·Ny−f*. Per 16384-sample segment, the dB spectrum just below each
candidate legacy Nyquist is compared — after linear detrending — with the mirrored
band just above it. Images correlate **bin-for-bin in spectral shape**; honest
hi-res content and dither floors do not (energy-only correlation would
false-positive on ordinary loudness co-variation, shape does not). Accepted at
mean r ≥ 0.55 over ≥12 segments.

This matters because strong images raise the above-cliff region and defeat the
cliff detector's floor check — exactly the badly-upsampled case. The probe then
pins the origin on its own, and the images justify cutting below the original
Nyquist **regardless of the pre-ring verdict** (a minimum-phase upstream SRC
leaves no pre-ring, but its images are just as audible). Probed origins are
≥22.05 kHz, so this branch can never cut into the midrange.

### 6. Diagnosis without action

* **Cliff + no pre-ring on transient-rich material** ⇒ the source filter is
  minimum-phase or the master was already apodized. Logged, audio untouched.
  (Direct post-ring detection is deliberately NOT attempted: post-ring hides inside
  each attack's own HF decay and cannot be separated on real music without the
  clean reference — an ill-posed problem. This indirect verdict follows from
  measurements we trust.)
* **Cliff below 17 kHz** ⇒ lossy or dark source; below the apodizer's range,
  logged, untouched.
* Nothing detected ⇒ `None`; `prepare.rs` falls back to the user's static preset
  exactly as before (never silently swallowing a manually selected strength).

## Decision ladder (most → least evidence)

| Evidence | Action |
|---|---|
| Quorum + ≥25 % ringing, ring freq in trust zone (≥0.86×Ny) | fc = f_ring × margin(fraction), β by severity |
| Same, ring freq below trust zone or indeterminate | v2 bucket × effective Nyquist (field-tested fallback) |
| Mirror-image aliasing (any ring verdict) | fc = 0.93 × origin Nyquist, β=24 — images are junk regardless |
| Cliff + ring refuted (≥8 attacks, <25 %) | none — min-phase/pre-apodized diagnosis |
| Cliff ≥17 kHz + too few attacks | gentle: fc ≈ 0.96 × cliff, β=14 |
| Cliff <17 kHz | none — lossy/dark diagnosis |
| Nothing | none → static preset fallback |

Quorum: 8 judged attacks, or 5 when the evidence is strong (≥40 % ringing at
≥12 dB). Cutoff clamp: [0.816, 0.93] × effective Nyquist.

## Field calibration — 2026-07-13, first live run

Two sibling tracks of one CD-rip album measured near-identically
(43 % of 7 attacks vs 49 % of 41; ring 18206 vs 18126 Hz; sev ~20 dB) yet diverged:
one fell under the flat 8-attack quorum (no treatment), the other trusted the
18.1 kHz "ring" and cut at 17 640 Hz — audibly deep. Root causes and fixes:

1. **Flat quorum broke album consistency** → strong evidence from ≥5 attacks now
   qualifies.
2. **The ring-frequency estimator is biased toward the analysis-band edge** on
   tilted spectra: an 18.1 kHz reading is not a plausible ADC transition (real
   brick walls live at 19–22 kHz) — it is lossy pre-echo or the music's own HF
   slope. Readings below 0.86×Ny are no longer used for precise placement; the
   verdict falls back to v2's buckets and the log names the suspicion.
3. **Floor raised 0.80 → 0.816×Ny** (= v2's strongest preset, 18 kHz @ 44.1).

Both tracks now land on the identical moderate bucket (19 007 Hz, β=24). Pinned by
`v3_album_consistency_and_ring_trust_window` with the exact log numbers.
Note: junk *below* the cutoff (e.g. 18.1 kHz pre-echo) is deliberately NOT chased —
cutting under 18 kHz to remove it would cost audible treble; that is a manual
decision, not an automatic one.

### Validation re-run (both toggle states)

* **AA on** — both sibling tracks receive the identical verdict (19 007 Hz, β=24,
  `AA` filename tag) and the distrust reason is spelled out in the
  `[CONV] Adaptive Apodizer v3:` log line. The analysis pass costs ~0.2 s per
  4-minute 44.1 kHz track and overlaps the GPU convolution of the previous file
  (prep-thread pipelining) — no wall-clock cost in batches.
* **AA off** — no analysis runs, nothing is applied, no tag; the output true
  peak matches the pre-v3 run exactly (deterministic control, zero hidden state).
* All four conversions passed STAGE-5 bit-perfect verification with 0 mismatches.

### Expected side effect: true-peak growth under apodizing

The apodizer is minimum-phase; its phase rotation near the cutoff can RAISE
inter-sample peaks on heavily limited masters (observed: −1.02 → −0.15 dBTP on
one track). The output true-peak normalizer catches this and holds the
−0.50 dBTP target (a −0.35 dB trim in that case). AA-treated versions of loud
masters may therefore sit a fraction of a dB quieter — by design, not a defect.

## Tests

`apodize.rs::tests::v3_*` — six synthetic fixtures with known filter history:
linear-phase brick wall (ring measured to ±600 Hz, deep treatment), the same
magnitude minimum-phase (refused), 44.1-in-88.2 fake hi-res (origin snapped,
treated in the original band, taps scaled), true hi-res with smooth rolloff
(refused, no false alias positives), steady brick-walled noise without transients
(gentle spectral path), and a linear-interpolation ×2 upsample of a minimum-phase
master — no pre-ring, cliff floor-check defeated by images — rescued by the
mirror probe alone.
Fixture physics note: a click's sample width scales with container rate — a
1-sample click at 88.2k carries half the analog area of one at 44.1k and
under-drives the ring by exactly that ratio.

Synthetic tests prove the mechanics. Real-world threshold behavior should still be
sanity-checked on a live library (log lines `[CONV] Source analysis v3:` and
`[CONV] Adaptive Apodizer v3:` carry every measured number for that purpose).
