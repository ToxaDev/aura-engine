# AuraEngine — Architecture Overview

## What Is AuraEngine?

AuraEngine is an **offline audio upsampler**: it re-renders ordinary
44.1/48 kHz (or higher) files as high-resolution FLAC using very long
linear-phase FIR filters, an adaptive apodizer, headroom and true-peak
protection, and the author's Hybrid-Phase transient engine. It is a Tauri
desktop app — a Rust DSP backend driven by a vanilla-JS UI. There is no
real-time player in this branch; everything is file-in, file-out.

> **Law:** all DSP must obey `DSP_MANIFESTO.md` — no clipping, no unintended
> phase error, no avoidable precision loss.

## Signal flow

The whole chain runs in **f64** on the CPU; the optional GPU path carries the
convolution in double-single (DS) f32 pairs (~48-bit mantissa, ~−260 dB null
vs f64). Two conversion paths exist, chosen by the *Polyphase FIR Resampling*
toggle.

```
Decode (symphonia → i32 → f64, gapless-trimmed)
   │
DC block (per-channel mean, or 2 Hz IIR HPF)
   │
Headroom (optional pre-DSP attenuation)
   │
Apodizer (adaptive ADC pre-ring detector, or static preset; ≤48 kHz sources)
   │
   ├── STANDARD PATH ─────────────────────────────────────────────
   │      rubato SincFixedIn resample  →  FIR post-filter (OLS, CPU/GPU)
   │
   └── POLYPHASE PATH (integer ratio) ────────────────────────────
          FIR *is* the resampler: decompose into L sub-filters, convolve
          the source directly, interleave  (no library resampler in the chain)
   │
Hybrid-Phase (optional): second convolution with the minimum-phase filter,
   HPSS onset envelope, stereo-linked zero-crossing switch between branches
   │
True-peak normalize (4× Lanczos-4 intersample, −0.5 dBTP ceiling)
   │
Dither (24-bit TPDF; Wannamaker-9 noise shaping only at ≤48 kHz output)
   │
Encode 24-bit FLAC (ffmpeg)  →  bit-perfect re-decode verification
```

## Technology stack

| Layer | Choice |
|-------|--------|
| Shell | Tauri 1.x (`api-all`), vanilla-JS frontend, `withGlobalTauri` |
| Decode | `symphonia` (FLAC/WAV/MP3/OGG/AAC), decoded via `SampleBuffer::<i32>` → f64 |
| Resample (standard path) | `rubato` `SincFixedIn` (sinc_len 512, oversampling 512, Cubic) |
| Convolution (CPU) | `rustfft` f64 partitioned overlap-save, Kahan-summed, `b_size` = 32768 |
| Convolution (GPU) | `wgpu`/Vulkan, GLSL→SPIR-V passthrough, double-single f32 |
| Parallelism | `rayon` (per-channel resample, polyphase phases, partition MAC) |
| Filters | designed offline in `fir-optimizer` (Kaiser-sinc, β=14), stored as `.npy` |
| Encode | external `ffmpeg` (`-f f64le` → 24-bit FLAC) |

## Key modules

See [Project Structure](08-project-structure.md) for the full tree and
[Developer Guide](04-developer-guide.md) for a module-by-module walkthrough.
The per-file pipeline lives in `converter/process.rs`; the two convolvers
implement a common `DspProcessor` trait (`processor.rs`).
