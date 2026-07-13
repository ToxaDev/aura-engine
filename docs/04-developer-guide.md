# Developer Guide — Converter Walkthrough

This is the developer's map of the `desktop-app` converter. (Earlier revisions
also documented a standalone `Rust/aura-engine-rs` engine and a real-time
`engine.rs` player — neither exists in this branch.)

## Build & run

```bat
cd desktop-app
start.bat            :: build-if-changed + launch; unchanged runs start instantly
start.bat --build    :: force a rebuild
start.bat --clean    :: cargo clean -p aura-engine, then rebuild
```

Or directly: `cd desktop-app/src-tauri && cargo build --release --bin aura-engine`.

**Requirements**
- Rust toolchain (stable).
- `ffmpeg` on `PATH` — the converter shells out to it for FLAC encoding.
- GPU path: a Vulkan adapter exposing `SPIRV_SHADER_PASSTHROUGH`. Without it the
  app returns a clean error for GPU jobs; use the CPU path. `glslangValidator`
  is only needed to *recompile* shaders — pre-built `.spv` blobs are checked in.
- Filter blobs in `fir-optimizer/output/` — generate with
  `python fir-optimizer/optimize.py --all-ratios`.

There is **no** CPAL/ASIO dependency (the `CPAL_ASIO_DIR` line in old docs and
scripts is dead), no `cmake`/`libflac-sys`, no FFTW.

## Request flow

```
UI (src/js/converter.js)
  └─ invoke('convert_files', { fsMultiplier, taps, precision, winType,
        customFilterPath, useGpu, useFirResampling, apodizing, headroomDb,
        adaptiveApodizer, hybridPhase, iirDcBlocking })
        │
main.rs ─ Tauri command ─→ converter::manager::convert_files
        │
manager.rs  spawns a prep thread (decode+prepare) feeding a bounded channel to
            a worker pool (1–2 GPU workers, or 1–4 CPU); progress via CONV_*
            atomics, polled by get_conversion_progress
        │
pipeline/prepare.rs   decode → DC block → headroom → apodize  → PreparedAudio
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

- `CpuDspProcessor` (`dsp_core.rs`): partitioned overlap-**save**, `b_size`=32768,
  FFT=65536, Kahan-summed frequency-domain MAC parallelised over bins with rayon.
- `GpuDspProcessor` (`gpu/`): same OLS in **double-single** f32 on Vulkan via
  SPIR-V passthrough (GLSL `precise` → `NoContraction`), h_freq/twiddles in f64→DS.
  `block_size(taps)` = `next_power_of_two(taps).clamp(262144, 2097152)`.

## Two conversion paths (`process.rs`)

| Toggle | Path |
|--------|------|
| Polyphase FIR **off** | **Standard**: `rubato` resample → FIR post-filter (OLS) → Hybrid-Phase → true-peak → dither → FLAC → verify. |
| Polyphase FIR **on** (integer ratio) | **Polyphase**: FIR decomposed into L sub-filters, source convolved directly (`run_polyphase_pass`, parallel on CPU), interleaved → Hybrid-Phase → true-peak → dither → FLAC → verify. |

Filter blobs are resolved by `dsp/filter.rs::find_precomputed_filter(taps,
out_rate, phase)` — per-ratio matrix keyed on output rate. `win_type` does **not**
select a filter (it only tags the filename); the filter is chosen by taps+rate.

## Output stage

- `dsp/true_peak.rs` — 4× Lanczos-4 (8-tap) intersample peak, −0.5 dBTP ceiling.
- `dsp/dither.rs` — 24-bit TPDF, independent RNG per channel, post-quant clamp;
  Wannamaker-9 noise shaping only at ≤48 kHz output.
- `encode.rs` — 24-bit FLAC via ffmpeg; `utils/verify.rs` re-decodes and checks
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
