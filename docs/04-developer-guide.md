# Developer Guide — Converter Walkthrough

This is the developer's map of the `desktop-app` converter. The player that
arrived in 1.5.0 lives in `src-tauri/src/player/` and reuses this chain — the
same source stages, filters and output stages, streamed; what it adds is
described in [19 — The Player and Internet Radio](19-player-and-radio.md).
(Earlier revisions also documented a standalone `Rust/aura-engine-rs` engine
and a real-time `engine.rs` player from an older codebase — neither exists in
this branch.)

## Build & run

```bat
cd desktop-app
start.bat            :: build-if-changed + launch; unchanged runs start instantly
start.bat --build    :: force a rebuild
start.bat --clean    :: cargo clean -p aura-engine, then rebuild
```

Or directly: `cd desktop-app/src-tauri && cargo build --release --bin aura-engine`.

**Requirements**
- Rust toolchain, 1.88 or newer (`rust-version` in `Cargo.toml`).
- CMake 3.16 or newer: `opusic-sys` builds libopus from source for the radio's
  Opus streams. The HE-AAC decoder (FDK AAC, Rust, from AOSP) is vendored in
  `src-tauri/vendor/aac` and builds offline.
- GPU path: a Vulkan adapter exposing `SPIRV_SHADER_PASSTHROUGH`. Without one —
  and also when the adapter runs out of memory mid-batch — the engine converts
  on the CPU reference path by itself; nothing has to be switched off by hand.
  `glslangValidator` is only needed to *recompile* shaders — pre-built `.spv`
  blobs are checked in.
- Filter blobs in `fir-optimizer/output/` — generate with
  `python fir-optimizer/optimize.py --all-ratios`.

There is **no** CPAL/ASIO dependency (the `CPAL_ASIO_DIR` line in old docs and
scripts is dead; the player talks to WASAPI through the `wasapi` crate), no
`libflac-sys`, no FFTW.

**Tests**

```bat
cd desktop-app\src-tauri && cargo test --release
node --test "desktop-app/tests/**/*.test.mjs"
```

The Node suite covers the frontend logic with a wrong answer that costs
something: which tap count and FS multiplier the sliders open on, given the
blobs actually installed (see
[`docs/12-precomputed-fir-matrix.md` section 8](12-precomputed-fir-matrix.md#inventory)),
and that the subsonic corners the UI sends are exactly the ones the backend
accepts (`tests/dsprack.test.mjs`). Everything else in `src/js` is DOM wiring,
exercised by running the app.

**Packaging**

`tools/make-bundles.ps1` builds the app-plus-filters zips on the release page.
It takes the executable from the *published* release rather than from
`target/release`, because the repo's `.cargo/config.toml` compiles with
`target-cpu=native` — correct for a local build, and a crash on anyone else's
machine. Run `-ListOnly` first to see the plan and the sizes.

## Request flow

```
UI (src/js/converter.js)
  └─ invoke('convert_files', { paths, fsMultiplier, taps, precision,
        customFilterPath, useGpu, useFirResampling, apodizing, headroomDb,
        adaptiveApodizer, hybridPhase, subsonicHz, …the rack's stages,
        Album Level })
        │
main.rs ─ Tauri command ─→ converter::manager::convert_files
        │
manager.rs  spawns a prep thread (decode+prepare) feeding a bounded channel to
            a worker pool (1–2 GPU workers, or 1–4 CPU); progress via CONV_*
            atomics, polled by get_conversion_progress
            GPU workers are NOT rationed by VRAM here — the budget is enforced
            at buffer allocation by gpu::vram_admission (see 05 §15.1)
        │
manager.rs  album pre-scan: the queued tracks of one album are prepared and
            their output peaks estimated before the first is written (album.rs)
        │
pipeline/prepare.rs   decode → DC block → [Declip] → [ISP] → [subsonic]
                      → Headroom / Adaptive Headroom decision → apodize → PreparedAudio
                      (the subsonic filter runs again in process.rs, after the output limiter)
        │
process.rs::process_one_prepared   the two conversion paths (below)
```

## The `DspProcessor` trait (`audio/processor.rs`)

Both convolvers implement:

```rust
fn process_audio(&mut self, in_l, in_r, out_l, out_r, num_frames);  // all &[f64]
fn block_size(&self) -> usize;
fn output_latency(&self) -> usize;   // CPU: 2*b_size, GPU: 1*b_size
```

The trait is **f64** end-to-end (no f32 bottleneck). `output_latency()` is the
single source of truth for trim/flush arithmetic — see doc 13 §1.

- `CpuDspProcessor` (`dsp_core.rs`): partitioned overlap-**save** on half
  spectra (`realfft` over `rustfft` — the spectrum of a real signal is
  symmetric, so the other half holds nothing new), `b_size`=32768, FFT=65536,
  Kahan-summed frequency-domain MAC parallelised over bins with rayon.
- `GpuDspProcessor` (`gpu/`): partitioned convolution in **double-single** f32 on
  Vulkan via SPIR-V passthrough (GLSL `precise` → `NoContraction`), h_freq/twiddles
  in f64→DS; since 1.5.0 on half spectra, with L and R carried through one
  complex transform (L + iR). `block_size(taps)` =
  `next_power_of_two(taps).clamp(262144, 2097152)`, FFT = 2 × block.

## Two conversion paths (`process.rs`)

| Toggle | Path |
|--------|------|
| Polyphase FIR **off** | **Standard**: `rubato` resample → FIR post-filter (OLS) → Hybrid-Phase → true-peak → dither → FLAC → verify. |
| Polyphase FIR **on** (integer ratio) | **Polyphase**: FIR decomposed into L sub-filters, source convolved directly (`run_polyphase_pass`, parallel on CPU), interleaved → Hybrid-Phase → true-peak → dither → FLAC → verify. |

Filter blobs are resolved by `dsp/filter.rs::find_precomputed_filter(taps, src,
out, phase)` — per-ratio matrix keyed on the design rate (family base × L), so a
source above 48 kHz gets the filter for its own ratio. Every built-in filter is
Kaiser; the filter is chosen by taps and rates.

Between the convolution and the dither both paths run the same output stages:
Hybrid-Phase / Continuous Alpha or TFS (TFS on the polyphase path only), XTC,
the ISP output limiter, the subsonic guard, and the true-peak ceiling — with an
album's shared gain folded into the limiter's target and the dither pass when
Album Level is on (doc 05 §5b).

## Output stage

- `dsp/true_peak.rs` — 4× Lanczos-4 (8-tap) intersample peak, −0.5 dBTP ceiling.
- `dsp/dither.rs` — 24-bit TPDF, independent RNG per channel, post-quant clamp;
  Wannamaker-9 noise shaping only at ≤48 kHz output.
- `encode.rs` — 24-bit FLAC, encoded natively and streamed to disk frame by
  frame; `utils/verify.rs` re-decodes and checks
  bit-perfect (±2 LSB) match. Filename built from `PreparedAudio.apod_tag` +
  settings (the tag reflects what actually ran).

## Where to look for…

| Task | Start here |
|------|-----------|
| Add a UI control | `src/components/converter.html` + `src/js/converter.js` + `settings.js`, then the `ConvertSettings` field in `converter/types.rs` and its use in `process.rs`/`prepare.rs`. |
| Change the filter design | `fir-optimizer/optimize.py`, then regenerate `--all-ratios`. |
| Hybrid-Phase behaviour | `hybrid_phase.rs` (blend), `hpss_native.rs` (onset), `pipeline/hybrid_mixer.rs` (standard-path orchestration). |
| Apodizer detection | `converter/apodize.rs::analyze_source` + `decide_apodizer` — see [doc 14](14-adaptive-apodizer-v3.md). |
| GPU correctness | `gpu/wola.rs` (OLA block), `gpu/setup.rs` (pipeline), `gpu/ds_preflight.rs` (DS math test). |

See [Pipeline Hardening (doc 13)](13-pipeline-hardening-2026-07.md) for the
current correctness/quality state and [Converter Pipeline (doc 5)](05-converter-pipeline.md)
for stage-by-stage detail.
