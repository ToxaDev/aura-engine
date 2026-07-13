# Project Structure

Regenerated from the actual source tree of this branch. The converter is the
Tauri app in `desktop-app/`; `fir-optimizer/` is the Python filter generator.

```
AuraEngine/
├── README.md                     Project overview + build/run
├── CHANGELOG.md
├── DSP_MANIFESTO.md              Rules on phase / clipping / precision
├── .gitignore
│
├── desktop-app/                  ── THE CONVERTER (Tauri app) ──
│   ├── start.bat                 Build-if-changed + launch (fast path)
│   ├── src/                      Frontend (vanilla JS, no framework)
│   │   ├── index.html
│   │   ├── components/converter.html
│   │   ├── css/style.css
│   │   └── js/                   main, converter, settings, dropzone,
│   │                            state, ui, helpers
│   └── src-tauri/                Rust backend
│       ├── Cargo.toml            deps + [profile.release] (fat LTO)
│       ├── build.rs              GLSL → SPIR-V via glslangValidator
│       ├── .cargo/config.toml    target-cpu=native
│       ├── tauri.conf.json
│       ├── icons/
│       └── src/
│           ├── main.rs           Tauri command handlers
│           └── audio/
│               ├── mod.rs
│               ├── processor.rs      DspProcessor trait (process_audio,
│               │                    block_size, output_latency)
│               ├── dsp_core.rs       CpuDspProcessor: partitioned OLS,
│               │                    Kahan MAC, to_minimum_phase, group-delay
│               ├── hybrid_phase.rs   Stereo-linked zero-crossing blend,
│               │                    envelope loading (Catmull-Rom upsample)
│               ├── hpss_native.rs    Native HPSS onset envelope (STFT)
│               ├── memory.rs         RAM-aware allocation gate
│               ├── logging.rs        aelog! + heartbeat
│               ├── cancel_flag.rs
│               ├── gpu/              DS-precision GPU convolver (wgpu/Vulkan)
│               │   ├── context.rs, setup.rs, processor.rs, wola.rs,
│               │   ├── fft_math.rs, bind_groups.rs, ds_preflight.rs
│               ├── shaders/
│               │   ├── *.comp.glsl       ACTIVE — compiled by build.rs
│               │   ├── precompiled/*.spv fallback blobs (no glslang needed)
│               │   └── *.wgsl            legacy reference (NOT compiled)
│               └── converter/
│                   ├── mod.rs, types.rs, state.rs
│                   ├── manager.rs       batch orchestration, progress, cancel
│                   ├── decode.rs        symphonia decode → f64 (via i32)
│                   ├── process.rs       per-file pipeline (both paths)
│                   ├── apodize.rs       adaptive + static apodizer
│                   ├── encode.rs        FLAC via ffmpeg + filename builder
│                   ├── pipeline/
│                   │   ├── prepare.rs       DC block, headroom, apodize
│                   │   ├── hybrid_mixer.rs  standard-path Hybrid-Phase
│                   │   ├── resample_logic.rs  integer-ratio snapping
│                   │   └── mod.rs
│                   ├── dsp/
│                   │   ├── dither.rs      TPDF + Wannamaker-9 shaping
│                   │   ├── true_peak.rs   Lanczos-4 4× intersample peak
│                   │   ├── filter.rs      per-ratio blob resolver
│                   │   └── polyphase.rs   polyphase decomposition
│                   └── utils/verify.rs   bit-perfect FLAC re-decode check
│
├── fir-optimizer/                ── FILTER GENERATOR (Python) ──
│   ├── optimize.py               Kaiser-sinc FIR design (--all-ratios)
│   ├── generate_envelope.py      (legacy Python HPSS, superseded by Rust)
│   ├── verify_hybrid_phase.py    independent Hybrid-Phase verification
│   ├── analyze_source.py, audiophile_analysis.py, compare.py
│   ├── config.json, requirements.txt, *.bat
│   └── output/                   generated .npy filter blobs (git-ignored,
│                                RUNTIME dependency of the converter)
│
└── docs/                         this documentation
```

### Notes

- **`output/` is git-ignored** because a single 30M-tap f64 filter is ~240 MB.
  The converter resolves blobs there via `converter/dsp/filter.rs`
  (`find_precomputed_filter`); generate them with
  `python fir-optimizer/optimize.py --all-ratios`.
- **Active GPU shaders are the `.comp.glsl` files.** The `.wgsl` files are
  kept only as historical reference of the pre-DS f32 design and are not
  compiled or loaded by the running pipeline.
- There is **no** `engine.rs`, `gpu_core.rs`, `converter.rs`, or standalone
  `Rust/` crate in this branch — those belonged to the old merged codebase.
