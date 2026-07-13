# Changelog

## July 2026: Adaptive Apodizer v3 — Source Forensics

Full technical record: [`docs/14-adaptive-apodizer-v3.md`](docs/14-adaptive-apodizer-v3.md).

- **The detector now measures instead of guessing.** The pre-ring burst's
  dominant frequency is estimated per attack (FFT of the −9…−3 ms window,
  median across attacks with an agreement gate); the corrective cutoff lands
  just below the measured source-filter edge instead of one of three preset
  buckets. Severity selects filter depth (β=24 strong / β=14 mild, shorter
  time-domain signature); taps scale with the container rate.
- **Fake hi-res is unmasked.** A Welch spectral-cliff detector finds the
  brick-wall signature (≥20 dB inside 1/12 octave with only a noise floor
  above); a cliff well below a hi-res container's Nyquist means an upsampled
  44.1/48 kHz master, and all analysis then runs against the *original*
  Nyquist. Previously all >48 kHz sources were skipped outright.
- **Mirror-image alias probe.** A bad upstream resampler leaves images of the
  content above the original Nyquist (a tone at f gets a twin at 2·Ny−f); the
  spectral shape below each candidate legacy Nyquist is compared, per segment,
  with the mirrored band above it. Images correlate bin-for-bin — honest
  hi-res never does — and are removed regardless of the pre-ring verdict.
- **Low-transient material** (ambient, legato strings) is now handled by the
  spectral evidence alone, gently — and never inside a true hi-res
  container's own ADC band.
- **Honest refusals.** A cliff without pre-ring on transient-rich material
  means a minimum-phase or already-apodized source: diagnosed in the log,
  audio untouched. Direct post-ring detection is deliberately not attempted
  (it is ill-posed — post-ring hides inside each attack's own HF decay).
- **Field-calibrated on real material**: quorum accepts strong evidence from
  fewer attacks (album consistency), ring readings below 0.86×Nyquist are
  distrusted as lossy pre-echo/spectral tilt (preset-bucket fallback), and
  the cutoff floor is fixed at 0.816×Nyquist (18 kHz @ 44.1k). Verified in
  both toggle states with bit-perfect output checks; seven new tests pin the
  detectors and the decision logic.
- Expected side effect, documented: the minimum-phase apodizer can raise
  inter-sample peaks on heavily limited masters; the −0.50 dBTP output
  normalizer holds the target.

## July 2026: First Public Release

Repository opened at [github.com/ToxaDev/aura-engine](https://github.com/ToxaDev/aura-engine).

- **Filter blob resolution fixed for cloned checkouts.** `find_precomputed_filter`
  previously looked one directory level too shallow relative to the exe and
  compensated with a hard-coded developer path; it now resolves
  `fir-optimizer/output/` at the repo root, supports an `AURA_FILTER_DIR`
  environment-variable override, and falls back to the working directory.
- **Specs-line tap presets corrected** (`ui.js`): the header now displays
  1M/5M/10M/30M, matching what the backend actually loads (previously showed
  stale 4M/16M labels for the middle presets).
- **Legacy batch mode presets corrected** (`optimize.py`): generates
  1M/5M/10M/30M — names the runtime can actually resolve.
- **Documentation translated to English** (changelog, DSP manifesto,
  hybrid-phase proof, audiophile features, auditor guide, fir-optimizer README)
  and a visual signal-path reference added at `docs/index.html`.
- **Project metadata**: PolyForm Noncommercial 1.0.0 license (free for
  noncommercial use; commercial rights reserved by the author), CI (cargo
  check + test on Windows),
  contributing guide, trimmed `fir-optimizer/requirements.txt` to the packages
  the scripts actually import.

## July 2026: Hardening Pass — Correctness, Quality, Polyphase, Branch Cleanup

Full technical record: [`docs/13-pipeline-hardening-2026-07.md`](docs/13-pipeline-hardening-2026-07.md).

### DSP Correctness
- **OLA latency alignment.** Added `output_latency()` to the `DspProcessor` trait (CPU: 2× block size, GPU: 1× block size); trim/flush now derives latency from this method instead of hard-coding a single block. Fixes ~85 ms of leading silence and tail truncation on the CPU path, and desynchronisation between hybrid-phase branches.
- **Dither** — independent RNG per channel; output clamped to ±(1 − q_step) after quantisation.
- **`to_minimum_phase`** — DC-gain renormalisation after the tail fade.
- **Decoder** — gapless trim for MP3/AAC (delay/padding metadata), error logging instead of a silent abort, warning when more than 2 channels are present.
- **Encoder** — broken FLAC file is deleted on ffmpeg cancellation or failure.
- **GPU** — returns `Err` instead of panicking when `SPIRV_SHADER_PASSTHROUGH` is absent; spurious placeholder-filter generation removed.

### Audio Quality
- **Stereo-linked hybrid-phase switching** — a single zero-crossing point on the mid-difference signal governs both channels; L and R now switch synchronously with no inter-channel phase offset.
- **Onset envelope** computed via Catmull-Rom interpolation; the `.onset_envelope.json` cache is versioned.
- **Adaptive apodizer** — pre-ringing detector operates in the time domain (Nyquist-band burst *before* an attack transient) rather than using a band-energy heuristic; a "clean" verdict applies a static preset.

### Polyphase Path
- Brought to parity with the standard path: filter-matrix resolver, dither, bitwise verification, parallel phase processing (rayon on CPU), shared `run_polyphase_pass` helper.
- Exposed via the **Polyphase FIR Resampling** checkbox in Advanced DSP (locked during conversion, consistent with other options).
- Free of the ~0.4 s trailing silence inherent in the OLA approach (by construction, output length = input length × L).

### UI and Accuracy
- Tap presets corrected to **1M / 5M / 10M / 30M** (aligned with the actual matrix files; the previous 4M/16M labels were loading the 5M/10M matrices).
- `AA` / `Apod` tags in output filenames now reflect the processing that was actually applied, not the UI setting.
- Final job status distinguishes `converted` from `skipped`.

### Build and Branch Structure
- `Cargo.toml` — `[profile.release]` configured with fat LTO and `codegen-units=1`.
- `build.rs` compares SPIR-V bytes before copying; `start.bat` skips cargo when the binary is newer than its sources (instant launch without a rebuild).
- Branch `converter-only` reduced to the converter: removed the legacy standalone engine (`Rust/`), `Py/`, `chrome-extension/`, plotting scripts, root report generators, and outdated documentation describing a non-existent player. All docs rewritten to match the actual codebase.

---

## April 2026: Major Architectural Update — DSP Converter

### Audio Core and Filters (FIR Optimizer)
- Introduced an ideal 128-bit (quad-precision IEEE 754) math engine for Windows. Due to MSVC compiler limitations with `np.longdouble`, the ideal sinc-impulse and Kaiser-window calculation logic was rewritten using the `mpmath` library at maximum precision (`dps = 38`).
- Introduced a multiprocessing system that distributes the workload of generating millions of taps across all CPU threads. The largest reference filter at **30 million taps** is generated in a matter of minutes.
- Filter presets rebuilt to professional high-end industry standards: **1M, 4M, 16M, and Maximum 30M** taps.

### Rust Backend
- Improved automatic output-file naming: when the source track already has a sample rate above 48 kHz (Hi-Res class), adaptive apodizing (AA) is skipped in hardware, and the AA tag is no longer incorrectly appended to the output FLAC filename.
- Enforced strict 24-bit encoding for FLAC output (no experimental flags).

### User Interface
- The converter specs line at the top now reads the preset array correctly. Instead of the buggy positional display (e.g. `0.003K`), it shows human-readable values such as `1.0M Taps`, `4.0M Taps`, etc.
- Added color-coded **Smart Badges** to the job queue: purple `[HP]` for hybrid phase and green `[AA]` for adaptive apodizing.
- The remove/cancel button redesigned from round to rectangular (matching the badge border-radius), with a vector SVG cross icon. While a job is pending, the button is nearly transparent (15% opacity), brightening to a bold red accent on hover.
- A semi-transparent vertical divider line (`.file-item-divider`) added between the remove button and the badge block.
- The default Chrome/WebKit scrollbar replaced with a custom one: fully transparent track, 6 px teal thumb — consistent with AuraEngine's premium dark theme.
