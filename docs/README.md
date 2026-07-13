# AuraEngine — Documentation

Technical documentation for the AuraEngine offline converter (`desktop-app/`).

> **Scope note.** This branch is converter-only. Earlier revisions of these
> docs also described a real-time WASAPI/ASIO player (`engine.rs`) and a
> separate standalone Rust engine — neither exists here. The docs below
> describe the **actual** desktop converter as it is in this branch.

## Index

> 🌐 **Start here:** [**toxadev.github.io/aura-engine**](https://toxadev.github.io/aura-engine/) —
> the full signal path, beautifully rendered: every DSP stage with its
> parameters and rationale. (Source: [index.html](index.html).)

| # | Document | Description |
|---|----------|-------------|
| 1  | [Architecture Overview](01-architecture.md) | Converter data flow, module map, technology stack. |
| 4  | [Developer Guide](04-developer-guide.md) | Module-by-module walkthrough, build/run, key types. |
| 5  | [Converter Pipeline](05-converter-pipeline.md) | Standard vs Polyphase paths, every stage in detail. |
| 6  | [Hybrid-Phase Proof](06-hybrid-phase-proof.md) | The Hybrid-Phase engine: HPSS onset detection, stereo-linked switch, verification. |
| 7  | [Audiophile Features](07-audiophile-features.md) | Plain-language description of each sound-quality feature. |
| 8  | [Project Structure](08-project-structure.md) | The file tree, regenerated from the actual source. |
| 9  | [Audio Auditor Guide](09-audio-auditor-guide.md) | Step-by-step of what the engine does to the signal, for reviewers. |
| 11 | [Group-Delay Truncation Trade-off](11-group-delay-truncation-tradeoff.md) | Why `estimate_band_weighted_group_delay` analyses only the first 131 k taps. |
| 12 | [Pre-computed FIR Matrix](12-precomputed-fir-matrix.md) | Per-ratio FIR blobs — naming, lookup, generation. |
| 13 | [Pipeline Hardening (2026-07)](13-pipeline-hardening-2026-07.md) | The correctness/quality/perf changes made in the 2026-07 audit pass. |
| 14 | [Adaptive Apodizer v3 — Source Forensics](14-adaptive-apodizer-v3.md) | Spectral-cliff detector, ring-frequency measurement, fake-hi-res unmasking, mirror-image alias probe, field calibration. |
| 15 | [Measurements](15-measurements.md) | Frequency/impulse responses measured from the production filter blobs, with reproduction script. |

- [DSP Manifesto](../DSP_MANIFESTO.md) — the project's laws on phase, clipping and precision.

## Where the actual code lives

| Concern | File |
|---------|------|
| Tauri commands / entry point | `desktop-app/src-tauri/src/main.rs` |
| Batch orchestration, progress, cancel | `desktop-app/src-tauri/src/audio/converter/manager.rs` |
| Per-file pipeline (both paths) | `desktop-app/src-tauri/src/audio/converter/process.rs` |
| Decode / prepare (DC block, headroom, apodize) | `converter/decode.rs`, `converter/pipeline/prepare.rs` |
| CPU partitioned OLS convolver | `desktop-app/src-tauri/src/audio/dsp_core.rs` |
| GPU DS-precision convolver | `desktop-app/src-tauri/src/audio/gpu/` |
| Hybrid-Phase blend + HPSS | `audio/hybrid_phase.rs`, `audio/hpss_native.rs`, `converter/pipeline/hybrid_mixer.rs` |
| Apodizer | `desktop-app/src-tauri/src/audio/converter/apodize.rs` |
| Dither / true-peak / filter resolver | `converter/dsp/{dither,true_peak,filter,polyphase}.rs` |
| Encode (FLAC via ffmpeg) + verify | `converter/encode.rs`, `converter/utils/verify.rs` |
| Frontend | `desktop-app/src/` (`index.html`, `components/`, `js/`) |
| Filter generator (Python) | `fir-optimizer/optimize.py` |
