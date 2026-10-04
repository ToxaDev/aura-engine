# AuraEngine — Architecture Overview

## What Is AuraEngine?

AuraEngine is an **audio upsampler**: it re-renders ordinary
44.1/48 kHz (or higher) files as high-resolution FLAC using very long
linear-phase FIR filters, an adaptive apodizer, headroom and true-peak
protection, and the author's Hybrid-Phase transient engine. It is a Tauri
desktop app — a Rust DSP backend driven by a vanilla-JS UI. Since 1.5.0 it
also plays: `src-tauri/src/player/` runs the same chain in real time — on files
and on internet radio streams — with WASAPI exclusive output, the convolution
on the CPU or the GPU, and the source stages prepared ahead for the whole
track; the conversion is still file-in, file-out. The player's side is
described in [19 — The Player and Internet Radio](19-player-and-radio.md).

> **Law:** all DSP must obey `DSP_MANIFESTO.md` — no clipping, no unintended
> phase error, no avoidable precision loss.

## Signal flow

The whole chain runs in **f64** on the CPU; the optional GPU path carries the
convolution in double-single (DS) f32 pairs (~48-bit mantissa, ~−260 dB null
vs f64). Two conversion paths exist, chosen by the *Polyphase FIR Resampling*
toggle.

```
Decode (symphonia → f64, no clamp, gapless-trimmed; HE-AAC via FDK AAC)
   │
DC block (per-channel mean, or 2 Hz IIR HPF)
   │
Declip (on by default): flattened peaks rebuilt; stands down on lossy
   sources, whose overs are the codec's — tag DC
   │
ISP (on by default): intersample overs corrected on the source — tag ISP
   │
Subsonic filter (on by default, 15 Hz): linear-phase FIR high-pass at the
   source rate, flat from 20/15/10 Hz, ≥100 dB down from half that — tag SUB20
   (runs again at the output rate after the ISP output limiter, see below)
   │
Headroom / Adaptive Headroom: the output ceiling is decided here — tag AHR
   │
Apodizer (adaptive pre-ring forensics at any source rate, or a static preset
          for ≤48 kHz sources)
   │
   ├── STANDARD PATH ─────────────────────────────────────────────
   │      rubato SincFixedIn resample  →  FIR post-filter (OLS, CPU/GPU)
   │
   └── POLYPHASE PATH (integer ratio) ────────────────────────────
          FIR *is* the resampler: decompose into L sub-filters, convolve
          the source directly, interleave  (no library resampler in the chain)
   │
Phase: Hybrid-Phase / Continuous Alpha (optional) — second convolution with
   the minimum-phase filter, HPSS onset envelope (triggers on the start of a
   sound, not on measured pre-ringing), stereo-linked zero-crossing switch
   between branches — or TFS (on by default; polyphase path), a filter of its
   own derived from the linear + minimum pair
   │
XTC (optional): crosstalk cancellation for one measured listening triangle
   │
ISP output limiter: ~1.5 ms gain dips at overs; with Album Level on it holds
   the render at the ceiling divided by the album's gain
   │
Subsonic guard (1.3.3, when SUB is on): the same high-pass at the output
   rate, after the ISP output limiter, whose gain dips modulate the bass
   — so the ≥100 dB promise holds for the file, not only for the source
   │
True-peak normalize (4× Lanczos-4 intersample; ceiling −0.5 dBTP, or
                     whatever Headroom names; cut-only)
   │
Dither (24-bit TPDF; Wannamaker-9 noise shaping only at ≤48 kHz output;
        the album's gain and the ceiling's are applied in this pass)
   │
Encode 24-bit FLAC (native, streaming)  →  bit-perfect re-decode verification
```

## Technology stack

| Layer | Choice |
|-------|--------|
| Shell | Tauri 1.x (`api-all`), vanilla-JS frontend, `withGlobalTauri` |
| Decode | `symphonia` (FLAC/WAV/MP3/OGG/AAC), decoded straight to f64 — integer PCM exactly, float samples past full scale untouched; HE-AAC by the FDK AAC decoder (Rust, from AOSP, vendored); Opus streams by libopus |
| Resample (standard path) | `rubato` `SincFixedIn` (sinc_len 512, oversampling 512, Cubic) |
| Convolution (CPU) | `realfft` (over `rustfft`) f64 on half spectra, partitioned overlap-save, Kahan-summed, `b_size` = 32768 |
| Convolution (GPU) | `wgpu`/Vulkan, GLSL→SPIR-V passthrough, double-single f32, half spectra with L and R in one transform |
| Parallelism | `rayon` (per-channel resample, polyphase phases, partition MAC) |
| Filters | designed offline in `fir-optimizer` (Kaiser-sinc, β=14), stored as `.npy` |
| Encode | `flacenc` (pure Rust, streaming, 24-bit); no external process |
| Playback | `wasapi` (exclusive mode), the converter's convolvers run block by block |
| Analyzer, visualizations | measurements in Rust (`player/analytics/`); scenes are GLSL drawn by WebGL 2 in the WebView; the instrument models run on `ort` (ONNX Runtime, DirectML) |

## Key modules

See [Project Structure](08-project-structure.md) for the full tree and
[Developer Guide](04-developer-guide.md) for a module-by-module walkthrough.
The per-file pipeline lives in `converter/process.rs`; the two convolvers
implement a common `DspProcessor` trait (`processor.rs`).
