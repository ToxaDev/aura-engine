# AuraEngine — Documentation

Technical documentation for the AuraEngine desktop app (`desktop-app/`): the
offline converter and, since 1.5.0, the player that runs the same chain in
real time — on files, on internet radio streams and, since 1.7.0, as DSD.

> **Scope note.** Documents 01–18 describe the conversion chain, which the
> player shares stage for stage. What the player, internet radio, the analyzer
> and the visualizations add on top of it is in
> [19 — The Player and Internet Radio](19-player-and-radio.md). Earlier
> revisions of these docs described a real-time WASAPI/ASIO player
> (`engine.rs`) and a separate standalone Rust engine from an older codebase —
> neither exists here; the player of 1.5.0 is a different, new component.

## Index

> 🌐 **Start here:** [**aura-engine.app**](https://aura-engine.app/) and its
> [**user documentation**](https://aura-engine.app/docs.html) — installing, every setting, and
> [the full signal path](https://aura-engine.app/docs.html#path), beautifully rendered: every DSP stage
> with its parameters and rationale. (Source: [index.html](index.html),
> [docs.html](docs.html).)

> ⬇ **Just want to run it?** Nothing here needs to be built. The
> [ready-to-run bundles](../README.md#download) are the app with its
> million-tap filters already inside — unzip, run, drop a track on the window:
> ▶ plays it, ⤓ converts it.

| # | Document | Description |
|---|----------|-------------|
| 1  | [Architecture Overview](01-architecture.md) | Data flow, module map, technology stack. |
| 4  | [Developer Guide](04-developer-guide.md) | Module-by-module walkthrough, build/run, key types. |
| 5  | [Converter Pipeline](05-converter-pipeline.md) | Standard vs Polyphase paths, multistage, DSF export, every stage in detail. |
| 6  | [Hybrid-Phase Proof](06-hybrid-phase-proof.md) | The Hybrid-Phase engine: HPSS onset detection, stereo-linked switch, verification. |
| 7  | [Audiophile Features](07-audiophile-features.md) | Plain-language description of each sound-quality feature. |
| 8  | [Project Structure](08-project-structure.md) | The file tree, regenerated from the actual source. |
| 9  | [Audio Auditor Guide](09-audio-auditor-guide.md) | Step-by-step of what the engine does to the signal, for reviewers. |
| 11 | [Group-Delay Truncation Trade-off](11-group-delay-truncation-tradeoff.md) | Why `estimate_band_weighted_group_delay` analyses only the first 131 k taps. |
| 12 | [Pre-computed FIR Matrix](12-precomputed-fir-matrix.md) | Per-ratio FIR blobs — naming, lookup, generation; the 100M–1G rungs stored as branch-0 files (1.8.0). |
| 13 | [Pipeline Hardening (2026-07)](13-pipeline-hardening-2026-07.md) | The correctness/quality/perf changes made in the 2026-07 audit pass. |
| 14 | [Adaptive Apodizer v3 — Source Forensics](14-adaptive-apodizer-v3.md) | Spectral-cliff detector, validated ring-frequency measurement, ring-vs-wall cross-check, fake-hi-res unmasking, mirror-image alias probe, album pooling, per-file verdict, field calibration. |
| 15 | [Measurements](15-measurements.md) | Frequency/impulse responses measured from the production filter blobs, with reproduction script. |
| 16 | [Filter Length & Rate](16-filter-length-and-rate.md) | What a tap count means and at which rate it is defined; kernel span per preset and rate (5k to 1G, up to ×32), group delay, measured energy concentration. |
| 17 | [FLAC Encoding](17-flac-encoding.md) | The native encoder that replaced the ffmpeg subprocess: streaming design, high-rate handling and where FLAC stops (768 kHz), per-block predictor search, measured compression settings. |
| 18 | [The Advanced DSP Stages](18-dsp-stages.md) | Every stage in the rack — declip, intersample peaks, adaptive headroom, TFS phase, crosstalk cancellation, album level — what each does, what it refuses to do, and what ships switched on; dither and noise shaping above 48 kHz. |
| 19 | [The Player and Internet Radio](19-player-and-radio.md) | The conversion chain played live: preparation, Instant start and the badges, playback on the graphics card, BIT-PERFECT; internet radio and what a stream can and cannot run; the analyzer; the visualizations. |
| 20 | [DSD](20-dsd.md) | DSD64/128/256 output over DoP, the twelve modulators with their measured noise and CPU marks, DSF/DFF files played as they are, DSF export to DSD1024, limits and troubleshooting. |
| 21 | [Multistage Upsampling](21-multistage-upsampling.md) | The default since 1.7.0: the long filter at the source rate, a short exact interpolator for the rest (up to ×32 since 1.8.0, and the only way the 100M–1G rungs play); accuracy against single-stage, CPU and GPU savings, where it applies, how to switch back. |

- [DSP Manifesto](../DSP_MANIFESTO.md) — the project's laws on phase, clipping and precision.

## Where the actual code lives

| Concern | File |
|---------|------|
| Tauri commands / entry point | `desktop-app/src-tauri/src/main.rs`, `player_commands.rs` |
| Batch orchestration, progress, cancel | `desktop-app/src-tauri/src/audio/converter/manager.rs` |
| Segmented (bounded-RAM) path for long files | `converter/pipeline/segmented.rs` |
| Per-file pipeline (both paths) | `desktop-app/src-tauri/src/audio/converter/process.rs` |
| Decode / prepare (DC block, apodize) | `converter/decode.rs`, `converter/pipeline/prepare.rs` |
| Advanced DSP stages (Declip, ISP, Adaptive Headroom, TFS, XTC) | `converter/dsp/lab/` |
| Album Level | `converter/album.rs` |
| CPU partitioned OLS convolver | `desktop-app/src-tauri/src/audio/dsp_core.rs` |
| GPU DS-precision convolver | `desktop-app/src-tauri/src/audio/gpu/` |
| Hybrid-Phase blend + HPSS | `audio/hybrid_phase.rs`, `audio/hpss_native.rs`, `converter/pipeline/hybrid_mixer.rs` |
| Apodizer | `desktop-app/src-tauri/src/audio/converter/apodize.rs` |
| Dither / true-peak / filter resolver | `converter/dsp/{dither,true_peak,filter,polyphase}.rs` |
| Multistage upsampler | `converter/dsp/multistage.rs`, `converter/pipeline/ms_route.rs`, `player/ms_player.rs` |
| DSD: readers, DoP, modulators, DSF writer | `desktop-app/src-tauri/src/audio/dsd/`, `player/dsd_worker.rs`, `player/dsd_direct.rs` |
| CPU / GPU load sampler | `desktop-app/src-tauri/src/perf.rs` |
| Encode (native FLAC) + verify | `converter/encode.rs`, `converter/utils/verify.rs` |
| Player: live chain, WASAPI output, GPU policy | `desktop-app/src-tauri/src/player/` (`chain.rs`, `output.rs`, `policy.rs`, `gpu/`) |
| Internet radio: network, codecs, reconnects, drift | `player/radio/` |
| Station catalog, favorites, recent stations | `desktop-app/src-tauri/src/radio_catalog/` |
| Analyzer | `player/analytics/`, `desktop-app/src/analytics.html`, `desktop-app/src/js/analytics/` |
| Visualizations | `desktop-app/src/vis/` (scenes and their specification), `desktop-app/src/js/vis/`, `src-tauri/src/vis_store.rs` |
| Instruments for the visualizations | `desktop-app/src-tauri/src/spatial/` |
| Updater | `desktop-app/src-tauri/src/updater.rs` |
| Frontend | `desktop-app/src/` (`index.html`, `components/`, `js/`) |
| Filter generator (Python) | `fir-optimizer/optimize.py` |
