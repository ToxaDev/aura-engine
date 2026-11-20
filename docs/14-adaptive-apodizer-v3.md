# 14 — Adaptive Apodizer v3: Source Forensics

**Status:** implemented 2026-07-13, field-calibrated and validated on live material 2026-07-13/14 · v3.1 (burst validation, quality benchmark, album pooling, post-ring opt-in) 2026-07-14 · `desktop-app/src-tauri/src/audio/converter/apodize.rs`, wired in `pipeline/prepare.rs` + album pre-scan in `manager.rs`

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
milder → β=14 (~140 dB). Taps scale with the container rate
(2048 @ ≤48k … 16384 cap since v3.1) so the transition stays narrow in Hz.

*Correction (v3.1 benchmark):* the original rationale "β=14 has a shorter
time-domain signature" is wrong — at fixed taps a HIGHER β tapers the window
harder and its −80 dB tail is slightly SHORTER (24 vs 27 ms @ 2048 taps).
Both depths clean identically in measurement; the two-level ladder is kept
for field continuity, not audibility.

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

---

## v3.1 — 2026-07-14: precision, benchmark, album pooling, post-ring opt-in

### 1. Burst validation for the frequency measurement

The 2026-07-13 field case (18.1 kHz junk readings) was patched in v3.0 with a
frequency trust floor (0.86×Ny). v3.1 attacks the root cause: every burst that
`is_ring` accepts is additionally validated **for the frequency median only**
(fraction/severity still count all bursts — the field-tuned treatment
thresholds are untouched) against the two physical signatures of a real
filter pre-ring:

* **crescendo** — a pre-ring is the time-reversed decay of the source filter's
  impulse response, so its envelope GROWS toward the attack
  (−5…−3 ms RMS > 1.3 × −9…−7 ms RMS);
* **narrowband** — a damped oscillation at one frequency. The analysis band's
  dB spectrum is linearly detrended (kills the spectral-tilt bias the field
  case exposed) and the residual peak must stand ≥10 dB above the residual
  median. Broadband junk (lossy pre-echo, HF slope) peaks ~6–9 dB.

Surviving bursts get **parabolic sub-bin interpolation** on the detrended
mainlobe. Measured accuracy on the synthetic 21 kHz fixture: **±13 Hz**
(v3.0 bound was ±600 Hz). Rejected-burst counts appear in the
`Source analysis v3:` log line. If too few bursts survive, the verdict falls
back to the v2 buckets — same as distrust before, but physics-based.

### 2. Quality benchmark — "cleaning vs air" matrix

`bench_apodizer_quality_matrix` (ignored test):
`cargo test --release -- --ignored bench_apodizer --nocapture`.
Grid of (β ∈ 10–24, taps ∈ 2048/4096, margin ∈ 0.91–0.97) against a 21 kHz
linear-phase wall; measures suppression at the ring frequency, residual
pre-ring through the analyzer's own HP glasses, 16–20 kHz air loss on a
bright bed, and the apodizer's own tail length. Findings that drove v3.1:

| Finding | Consequence |
|---|---|
| Cleaning is binary: every config ≥60 dB at the ring reaches the −99.5 dBFS floor | β=10 would already suffice; β stays 24/14 (harmless, field-known) |
| Air loss depends ONLY on fc: −1.05 dB @ 0.91 → 0.00 dB @ 0.97 (16–20k) | margins stay field-calibrated; precision (see §1) is what will let them tighten later |
| taps 4096→2048: identical cleaning AND air, half the tail (33→19 ms @ −60 dB), half the CPU | **applied** — adaptive taps now 2048 @ ≤48k (transition ~330 Hz ≪ any margin) |
| higher β = slightly shorter −80 dB tail at fixed taps | doc correction above; "β=14 = shorter" was folklore |

### 3. Album pooling (`pool_analyses` + manager pre-scan)

When Adaptive Apodizer is on and a folder holds ≥2 files of the batch, the
prep thread first decodes and analyzes ALL siblings (~0.2 s analysis + one
extra decode per track, overlapped with the previous files' GPU work), pools
the measurements, and every sibling receives **one album verdict** — the
quiet interlude inherits the album's 51-attack statistics instead of
guessing from its own 3. fc depends only on the pooled analysis, so
mixed-container folders (44.1k + fake-hi-res 88.2k of the same master) still
get identical cutoffs, with taps scaled per container.

Pooling refuses heterogeneous folders (compilations): different suspected
origins, effective Nyquists >1% apart, or cliffs >5% apart → per-track
verdicts as before. Alias evidence needs ≥half the tracks agreeing on the
same origin. Log: `[CONV] Adaptive Apodizer: album pool for '<folder>' …`.

### 4. Post-ring opt-in — added and REMOVED the same day

A `+ treat post-ring walls` toggle (gentle β=14 treatment for "cliff + no
pre-ring" minimum-phase sources) shipped in v3.1 and was removed hours
later at Anton's request. Rationale for removal: post-masking (~100 ms)
makes post-ring practically inaudible — unlike pre-ring, whose pre-masking
window is 1–2 ms — while re-apodizing a master already apodized at the
studio just loses treble. A knob that rarely helps, can hurt, and demands
the user understand forensic diagnoses is a bad knob. The diagnosis log
line stays; if the need ever returns, this section documents the design
(flag `apodize_postring` through main.rs → ConvertSettings → decide_apodizer,
fc = 0.96×cliff clamped, β=14, hi-res ADC-band guard applies).

### v3.2 — 2026-07-14 (same day): ring-vs-wall cross-check

First live run of v3.1 on MP3 sources exposed a new failure: the validated
burst estimator measured rings 0.8–1.2 kHz BELOW each track's spectral wall
(Phil Collins: ring 19617 vs wall 20790) — physically impossible, pre-ring
oscillates AT the wall. Codec pre-echo inside the burst window pulls the
FFT peak down. Blindly trusting it cut ~1 kHz of audible air vs the v3.0
buckets (measured on converted files: −13.7 dB @ 19–20 kHz), heard
immediately as "less transparency, more low-mid weight".

Fix: when a cliff exists and the trusted ring sits >3% below it, the two
witnesses disagree → placement anchors to the wall (`fc = margin × cliff`),
reason names the contamination. Ring at/above the cliff keeps precise ring
placement (the cliff marks the steepest drop, which may sit slightly below
the true content edge). Pinned by `v32_ring_below_wall_anchors_to_cliff`
with the exact field numbers; verified on the live batch — fires on exactly
the two contaminated tracks, all agreeing verdicts unchanged.

Post-fix A/B/C measurement (old vs new vs original mp3): tonal-balance
shape deviation of BOTH conversions vs the original is 0.000–0.002 dB
everywhere below 16 kHz — the pipeline is tonally transparent; the only
audible differences between builds are the apodizer band itself and the
true-peak normalizer trim (shorter v3.1 filter rotates phase less → less
ISP overshoot → up to ~0.4 dB less trim, i.e. output level closer to the
source). For A/B listening, level-match to cancel that trim delta.

### v3.1 tests

`v3_broadband_preecho_yields_no_ring_frequency` (codec pre-echo reads as ring
but yields no frequency → bucket), `v31_album_pool_rescues_starved_sibling`
(field pair + 3-attack interlude → one moderate-bucket verdict for all),
`v31_album_pool_refuses_heterogeneous_folders`, `v31_album_pool_alias_quorum`,
`v32_ring_below_wall_anchors_to_cliff` (field case 2026-07-14). The 21 kHz
fixture's frequency assertion tightened 600 → 150 Hz.
