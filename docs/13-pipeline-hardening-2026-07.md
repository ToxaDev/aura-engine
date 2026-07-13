# Pipeline Hardening — 2026-07 Audit Pass

A multi-agent audit of the converter surfaced ~40 confirmed issues (correctness
bugs, quality gaps, perf wins). This document records what was changed. It is
the authoritative "what the pipeline actually does now" reference; older docs
predate these fixes.

## 1 — OLA latency alignment (correctness)

**Bug.** The CPU partitioned overlap-save convolver has a true algorithmic
latency of **2 × b_size** (65 536 samples): one block inherent to overlap-save,
plus one from the deferred `out_buf` read in `process_audio_internal`. The
pipeline trimmed only **1 × b_size**, so every standard-path CPU output carried
~32 768 samples (~85 ms @ 384 kHz) of leading silence and had an equal amount
of real audio truncated from the tail. On Hybrid-Phase this also desynchronised
the minimum-phase branch against the linear branch, smearing every crossfade.
The GPU path (1 × b_size latency) was correct.

**Fix.** The `DspProcessor` trait gained `output_latency()` — `2*b_size` for
`CpuDspProcessor`, `1*b_size` for `GpuDspProcessor`. All trim/flush arithmetic
(standard path, both polyphase passes, `hybrid_mixer.rs`) now queries it instead
of hardcoding a block, and the polyphase group-delay trim uses the exact
`(N−1)/2` rather than a per-phase-rounded `sub_delay*L`. Regression tests
`convolver_latency_is_two_blocks_and_unity_gain`,
`process_rs_standard_trim_alignment` (previously `#[ignore]`d) and
`polyphase_pass_alignment_and_dc_gain` guard it.

## 2 — Output stage (correctness)

- **Dither made independent per channel.** Left and right now draw from
  separate `SmallRng` streams (`dither.rs`); the shared stream previously
  correlated the two channels' dither floor toward the phantom centre.
- **Post-quantization clamp.** Output is clamped to ±(1 − q_step) after the
  Wannamaker-9 feedback, so a shaper/dither excursion can never exceed the
  largest 24-bit code (ffmpeg's f64→s32 handling of >1.0 is build-dependent).
  Error feedback still uses the unclamped value so the shaper stays stable.
- **`to_minimum_phase` DC renormalization.** After the cepstral tail fade, the
  impulse is rescaled back to the source DC gain, preserving 0 Hz magnitude.

## 3 — Decode / encode robustness

- **Gapless trim.** MP3/AAC encoder-delay and padding frames
  (`codec_params.delay`/`padding`) are now stripped, so LAME priming silence
  never enters the FIR chain.
- **Errors surfaced, not swallowed.** Mid-stream decode errors and corrupt
  packets are logged (and counted) instead of silently truncating the file;
  `>2`-channel sources log a warning that only L/R are kept; the decode buffers
  pre-reserve `n_frames`.
- **No partial FLAC left behind.** A cancelled or failed encode deletes the
  half-written output file instead of leaving a corrupt FLAC at the target path.

## 4 — Stereo-linked Hybrid-Phase (imaging)

The zero-crossing switch that selects linear- vs minimum-phase per transient
used to be computed **independently per channel**, letting L and R switch up to
the full ±5 ms search window apart — an interchannel timing artifact orders of
magnitude above the ~10–20 µs ITD audibility threshold, which smeared the
phantom image on exactly the transients Hybrid-Phase targets.

`blend_outputs_stereo` now computes **one** switch plan from the *mid*
difference signal `((dL + dR)/2)` and applies it to both channels, so L/R
always switch at the same sample. The mid signal is used only for analysis —
no M/S transform touches the audio; each channel's output is still its own
linear or minimum sample. See [Hybrid-Phase Proof](06-hybrid-phase-proof.md).

Also: the ~86 Hz onset envelope is upsampled to the output rate with
**Catmull-Rom** (C1-continuous) instead of linear interpolation, removing the
derivative kinks that made the 0.3 switch threshold jitter; and the
`.onset_envelope.json` cache is now **version-tagged** (`hpss_native_rust_v4`)
so stale caches from older detectors are regenerated automatically.

## 5 — Adaptive apodizer: real pre-ring detector

The old detector compared **average spectral power** in 15–18 / 18–20 /
20–22 kHz bands, so it fired on any bright material and said nothing about
ringing. The v2 detector is **time-domain**: it isolates the near-Nyquist band
(127-tap Kaiser highpass @ 0.78×Nyquist), finds the strongest broadband attacks
across the whole track, and for each measures near-Nyquist energy *just before*
the attack against the local background and the attack itself. A burst that
precedes the attack, rises clearly above background, and correlates with (but is
smaller than) the attack is pre-ringing. The decision is by the fraction of
ringing attacks; cutoffs are expressed relative to Nyquist. If no consistent
signature is found the source is left untouched — and (fix) a clean verdict now
falls through to the user's static preset instead of silently skipping it.

## 6 — Polyphase path brought to parity

The integrated polyphase path (FIR *is* the resampler — no `rubato` in the
chain) was previously unreachable from the UI and, when run, skipped dither and
verification and ignored the filter matrix. Now it:

- resolves filters through `find_precomputed_filter` (per-ratio matrix, keyed on
  output rate) with a custom-path override;
- runs the L sub-filters **in parallel on CPU** (rayon), sequentially on GPU,
  via the shared `run_polyphase_pass` helper;
- applies the same dither and bit-perfect FLAC verification as the standard path;
- is exposed by the **Polyphase FIR Resampling** checkbox (Advanced DSP).

Because the output length is exactly `input × L` by construction, this path is
also free of the ~0.4 s of trailing zero-padding the standard (rubato) path
leaves at the end of each file.

## 7 — Build / robustness / perf

- **No spurious recompiles.** `build.rs` now byte-compares SPIR-V blobs before
  copying them into `OUT_DIR` (an unconditional copy bumped mtimes and forced a
  full ~13 s relink every launch). `start.bat` skips cargo entirely when the
  binary is newer than all sources; `--build` forces a rebuild.
- **Release profile.** `Cargo.toml` gained `[profile.release]` with `lto="fat"`
  and `codegen-units=1` so the hot MAC/DS loops inline across `rustfft`/`bytemuck`.
- **GPU no longer panics** on a device without `SPIRV_SHADER_PASSTHROUGH`
  (returns `Err`, surfaces as a normal per-file failure); its constructor no
  longer generates and FFTs a throwaway placeholder filter before the real one.
- **Atomic run-guard.** `convert_files` uses `compare_exchange` so two IPC calls
  can't both start a batch and corrupt shared state.

## 8 — Honest reporting

- The `AA`/`Apod` filename tag reflects the apodizing that **actually ran**
  (`PreparedAudio.apod_tag`), not merely the setting — clean-verdict files are
  no longer mislabelled `AA`.
- The final status distinguishes converted from skipped:
  `All done! N converted, M skipped`.
- Tap presets (1M / 5M / 10M / 30M) match the filter-file tags; the old
  4M / 16M presets silently loaded the 5M / 10M blobs.

## Still open (not done in this pass)

- **XTC** (`dsp/xtc.rs`) — a complete, mathematically honest transaural
  crosstalk canceller, but it compensates the listening geometry, not the
  recording; left dead-code and unwired by design.
- Standard-path trailing zero-pad (~0.4 s) from rubato chunking — cosmetic;
  the polyphase path avoids it entirely.
- The Hybrid-Phase onset detector's stated 15 ms lookahead is effectively
  ~11.6 ms; STFT frames are start-stamped rather than centre-stamped. Both are
  benign for this use (they widen, not narrow, pre-ring coverage).
