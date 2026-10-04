# Project Structure

Regenerated from the actual source tree of this branch (1.5.0). `desktop-app/`
is the Tauri app — the converter and, since 1.5.0, the player that runs the
same chain in real time, on files and on internet radio; `fir-optimizer/` is
the Python filter generator.

```
AuraEngine/
├── README.md                     Project overview + build/run
├── CHANGELOG.md
├── DSP_MANIFESTO.md              Rules on phase / clipping / precision
├── THIRD-PARTY-NOTICES.txt       Libraries the program is built with, and their licences
├── LICENSE
├── .github/workflows/            ci.yml (cargo check + test), release.yml
│
├── desktop-app/                  ── THE APP (Tauri): converter and player ──
│   ├── start.bat                 Build-if-changed + launch (fast path)
│   ├── tests/                    Node tests of the frontend logic (*.test.mjs)
│   ├── src/                      Frontend (vanilla JS, no framework, no build step)
│   │   ├── index.html            Main window
│   │   ├── components/converter.html
│   │   ├── analytics.html        The analyzer's window
│   │   ├── vis-studio.html       The visualization studio's window
│   │   ├── xtc-geometry.html     The listening-triangle window for XTC
│   │   ├── css/                  One sheet per part (list, player, rack, radio, …)
│   │   ├── vis/                  The scenes (*.aura-vis) and AURA-VIS-SPEC*.md,
│   │   │                        the specification they are written against
│   │   └── js/
│   │       ├── main, converter, settings, state, ui, helpers
│   │       ├── dropzone, listview, list-drag, library      the list
│   │       ├── rackstrip, dsprack, inventory               settings strip, Advanced DSP
│   │       ├── player, arming, listening, fullview, memory the player
│   │       ├── radio                                       the radio view
│   │       ├── batchstats, update, tour*, tooltip, tip-*   batch summary, updater, tour
│   │       ├── analytics/        The analyzer's views (spectrogram, spectrum,
│   │       │                     loudness, waveform, histogram, stereo), export
│   │       └── vis/              Scene renderer, music features, studio, model pack
│   └── src-tauri/                Rust backend
│       ├── Cargo.toml            deps + [profile.release] (fat LTO); rust-version 1.88
│       ├── build.rs              GLSL → SPIR-V via glslangValidator (precompiled fallback)
│       ├── .cargo/config.toml    target-cpu=native; the vendored FDK AAC source
│       ├── vendor/aac/           FDK AAC decoder (Rust, from AOSP external/aac), vendored
│       ├── tauri.conf.json
│       ├── icons/
│       └── src/
│           ├── main.rs           Tauri command handlers
│           ├── player_commands.rs  The player's commands
│           ├── startup.rs        Why the app did not start (--selftest, crash file)
│           ├── single_instance.rs  One copy of the app per machine
│           ├── updater.rs        Signed in-app updates from GitHub Releases
│           ├── window_state.rs   Where each window was, put back at the next start
│           ├── app_dir.rs        Where the app keeps its own files (logs, calibration, …)
│           ├── vis_store.rs      The studio's window and the user's own scenes
│           ├── webview_png.rs    A picture of a window's page, as PNG
│           ├── audio/
│           │   ├── processor.rs      DspProcessor trait (process_audio,
│           │   │                    block_size, output_latency)
│           │   ├── dsp_core.rs       CpuDspProcessor: partitioned OLS on half
│           │   │                    spectra, Kahan MAC, to_minimum_phase, group delay
│           │   ├── hybrid_phase.rs   Stereo-linked zero-crossing blend,
│           │   │                    envelope loading (Catmull-Rom upsample)
│           │   ├── hpss_native.rs    Native HPSS onset envelope (STFT)
│           │   ├── memory.rs         RAM-aware allocation gate
│           │   ├── disk_space.rs     Room for the segmented route's temporary files
│           │   ├── tag_text.rs       Tag text as it was written (ID3 code pages)
│           │   ├── logging.rs        aelog! + heartbeat
│           │   ├── cancel_flag.rs
│           │   ├── gpu/              DS-precision GPU convolver (wgpu/Vulkan):
│           │   │                    context, setup, processor, wola, fft_math,
│           │   │                    bind_groups, ds_preflight, filter_cache,
│           │   │                    vram_admission, dxgi_memory, matches_cpu, profile
│           │   ├── shaders/
│           │   │   ├── *.comp.glsl       ACTIVE — compiled by build.rs
│           │   │   ├── precompiled/*.spv fallback blobs (no glslang needed)
│           │   │   └── *.wgsl            legacy reference (NOT compiled)
│           │   └── converter/
│           │       ├── mod.rs, types.rs, state.rs
│           │       ├── manager.rs       batch orchestration, progress, cancel
│           │       ├── album.rs         Album Level: one true-peak gain per album
│           │       ├── decode.rs        symphonia decode → f64 (no clamp)
│           │       ├── process.rs       per-file pipeline (both paths)
│           │       ├── apodize.rs       adaptive + static apodizer, subsonic filter
│           │       ├── encode.rs        native FLAC (flacenc) + filename builder
│           │       ├── pipeline/
│           │       │   ├── prepare.rs       source stages: DC, Declip, ISP, subsonic, apodize
│           │       │   ├── segmented.rs     bounded-RAM route for very long files
│           │       │   ├── hybrid_mixer.rs  standard-path Hybrid-Phase
│           │       │   ├── resample_logic.rs  integer-ratio snapping
│           │       │   └── mod.rs
│           │       ├── dsp/
│           │       │   ├── dither.rs      TPDF + Wannamaker-9 shaping
│           │       │   ├── true_peak.rs   Lanczos-4 4× intersample peak
│           │       │   ├── filter.rs      per-ratio blob resolver
│           │       │   ├── polyphase.rs, polyphase_engine.rs  polyphase decomposition
│           │       │   └── lab/           the Advanced DSP stages: declip, isp,
│           │       │                      headroom, tfs, xtc, stream_linear, stats
│           │       └── utils/verify.rs   bit-perfect FLAC re-decode check
│           ├── player/           The converter's chain, live
│           │   ├── controller.rs, controller/  the transport: queue, play, seek, power
│           │   ├── chain.rs          building a playable chain from the rack
│           │   ├── convolver.rs      streaming polyphase convolution (the converter's CPU arithmetic)
│           │   ├── gpu/              the player's GPU streaming convolver
│           │   ├── policy.rs         CPU or GPU for this filter on this machine
│           │   ├── calibration.rs    the CPU convolver's measured block cost
│           │   ├── source_stages.rs  the source stages done as the audio arrives
│           │   ├── stages.rs, blend.rs, slow_gain.rs  output-rate stages, Hybrid-Phase, level
│           │   ├── arming.rs         how far the rack is from being fully in the sound
│           │   ├── render.rs, timeline.rs  the render thread and the output timeline
│           │   ├── output.rs         WASAPI output
│           │   ├── analytics/        the analyzer's measurements (loudness, LRA, DR,
│           │   │                    true peak, spectra, …)
│           │   ├── radio/            internet radio: network, ICY, HLS, codecs
│           │   │                    (FDK AAC, Opus), joining across reconnects, drift
│           │   ├── equivalence.rs, file_vs_live.rs, e2e.rs  player = converter tests
│           │   └── selftest.rs       --player-selftest: the player without a window
│           ├── radio_catalog/    Radio Browser catalog, listing rules, icons,
│           │                    favorites and recent stations
│           └── spatial/          Instruments for the visualizations: stems, drum
│                                kit, notes, placement; the model pack (pack.rs)
│
├── fir-optimizer/                ── FILTER GENERATOR (Python) ──
│   ├── optimize.py               Kaiser-sinc FIR design (--all-ratios)
│   ├── generate_envelope.py      (legacy Python HPSS, superseded by Rust)
│   ├── verify_hybrid_phase.py    independent Hybrid-Phase verification
│   ├── analyze_source.py, audiophile_analysis.py, compare.py
│   ├── xtc_analyze*.py, xtc_make_testfile.py   XTC analysis scripts
│   ├── config.json, requirements.txt, *.bat
│   └── output/                   generated .npy filter blobs (git-ignored,
│                                RUNTIME dependency of the app)
│
├── tools/make-bundles.ps1        builds the app-plus-filters bundles
│
└── docs/                         this documentation + the website (index.html)
```

### Notes

- **`output/` is git-ignored** because a single 30M-tap f64 filter is ~240 MB.
  The app resolves blobs there via `converter/dsp/filter.rs`
  (`find_precomputed_filter`); generate them with
  `python fir-optimizer/optimize.py --all-ratios`. The first TFS conversion
  and the first stream at a tap length derive their own filters from these
  blobs once and keep them beside them.
- **Active GPU shaders are the `.comp.glsl` files.** The `.wgsl` files are
  kept only as historical reference of the pre-DS f32 design and are not
  compiled or loaded by the running pipeline.
- **The instruments pack is not in the repository.** The visualizations with
  instruments download their models (HTDemucs, DrumSep, Basic Pitch, ONNX
  Runtime with DirectML) as a separate pack from the releases page;
  `spatial/pack.rs` checks it file by file.
- There is **no** `engine.rs`, `gpu_core.rs`, `converter.rs`, or standalone
  `Rust/` crate in this branch — those belonged to an older merged codebase.
  The player in `src-tauri/src/player/` is new in 1.5.0 and shares the
  converter's code rather than keeping its own.
