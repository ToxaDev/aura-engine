# Contributing to AuraEngine

Thanks for your interest! AuraEngine is a small, focused project — the rules
below keep the DSP core trustworthy.

## Ground rules

1. **Read [`DSP_MANIFESTO.md`](DSP_MANIFESTO.md) first.** Every change to the
   audio path must comply with it — gain staging, phase behaviour, precision.
   A PR that violates a manifesto axiom (e.g. introduces an f32 truncation in
   the sample path, or breaks `sum(h) == 1.0` normalization) will not be
   merged, no matter how much faster it is.
2. **No silent behaviour changes.** If your change alters the rendered
   samples, say so in the PR and explain why the new output is more correct.
   Bit-exactness regressions need a justification, not a shrug.
3. **Tests are part of the change.** DSP fixes ship with a test that fails
   before and passes after (`cargo test` in `desktop-app/src-tauri`).

## Dev setup (Windows)

```bat
git clone https://github.com/ToxaDev/aura-engine.git
cd aura-engine\desktop-app
start.bat
```

- **Rust** 1.88 or newer (MSVC toolchain) and **CMake** 3.16 or newer are the
  hard build requirements; CMake builds libopus from source, for Opus streams.
- **glslangValidator is optional.** The GPU compute shaders are GLSL; the
  repo ships pre-compiled SPIR-V blobs (`src/audio/shaders/precompiled/`),
  and `build.rs` uses them automatically when no compiler is found. You only
  need the [Vulkan SDK](https://vulkan.lunarg.com/) (or a standalone
  `glslangValidator` on `PATH`) if you **edit a shader**. To refresh the
  committed blobs after a shader change, build once with
  `AURA_REFRESH_PRECOMPILED=1` and commit the updated `.spv` files.
- **Python 3.10+** with `numpy`/`scipy`/`mpmath` is needed only to regenerate
  FIR filter blobs: `python fir-optimizer/optimize.py --all-ratios`.

## Before you open a PR

```bat
cd desktop-app\src-tauri
cargo check --locked
cargo test --locked --release
```

CI runs exactly these two commands on `windows-latest`.

## Commit style

Conventional-commit-ish prefixes are appreciated:
`fix(dsp): …`, `feat(ui): …`, `docs: …`, `test(dsp): …`, `chore: …`.

## Licensing of contributions

AuraEngine is licensed under [PolyForm Noncommercial 1.0.0](LICENSE), and the
author additionally licenses the project commercially at his sole discretion
(dual licensing). By submitting a contribution you agree that:

1. your contribution is provided under the same PolyForm Noncommercial
   1.0.0 terms, and
2. you grant the project author a perpetual, irrevocable right to relicense
   your contribution as part of the project, including under commercial
   terms.

If you are not comfortable with this, please open an issue describing the
change instead of a PR — bug reports and ideas need no license at all.

## Reporting bugs

Please include: OS + GPU model, the input file's format/sample rate, the
exact converter settings (the two lines under the heading at the top of the
window and the stages lit in Advanced DSP — or the name of a converted file,
which lists every stage that ran), and the log output. If the GPU path is
involved, note whether the issue reproduces with **Hardware GPU Acceleration**
unchecked (CPU fallback).

For a problem in the player, say which output device and FS multiplier it
was, whether BIT-PERFECT and Instant start were on, and whether the GPU chip
in the player was lit; for a radio stream, its address or the station's name
in the catalog. The session log (the console window, or
`%LOCALAPPDATA%\AuraEngine\logs`) says most of the rest.
