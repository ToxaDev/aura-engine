<div align="center">

<img src="desktop-app/src-tauri/icons/128x128.png" width="96" alt="AuraEngine icon"/>

# AuraEngine

**A no-compromise offline audio upsampler for audiophiles.**

Million-tap FIR filters · Hybrid-Phase transient engine · GPU double-single precision convolution
· true-peak protection · bit-perfect output verification

[![CI](https://github.com/ToxaDev/aura-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/ToxaDev/aura-engine/actions/workflows/ci.yml)
[![License: PolyForm NC](https://img.shields.io/badge/License-PolyForm%20Noncommercial-teal.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D6)
![Rust](https://img.shields.io/badge/backend-Rust-orange)
![DSP](https://img.shields.io/badge/DSP-f64%20end--to--end-blueviolet)

### [⬇ Download for Windows](#download)

*Ready-to-run bundles — the million-tap filters are already inside.*

</div>

---

<a href="docs/media/aura-demo.mp4"><img src="docs/media/aura-demo.gif" align="right" width="300" alt="AuraEngine converting a batch of tracks (4× speed) — click for the video"/></a>

**AuraEngine** takes ordinary 44.1/48 kHz FLAC/WAV/MP3 files and re-renders
them offline at up to **768 kHz / 24-bit FLAC**, using FIR filters of
**1 to 30 million taps** whose coefficients are designed in **128-bit
precision**. Because it is not bound by real-time constraints, it can spend
the math your DAC's built-in interpolation filter never could: the DAC then
receives an already-reconstructed, oversampled waveform and only has to play
it.

Everything the engine does is **verifiable by design**: the full DSP trace is
logged to the console, every output file is re-decoded and compared
sample-by-sample against the internal f64 buffer, and files that fail
verification are renamed `_UNVERIFIED` instead of being silently kept.

**Highlights**

- **⭐ The Hybrid-Phase engine — the signature invention of this project.**
  The track is rendered **twice in full** (linear phase + minimum phase) and an
  onset detector switches between the two renders per-attack, stereo-linked, at
  zero crossings. No pre-ringing ahead of the attacks it fires on, stereo image
  intact everywhere else —
  [see it animated below](#the-hybrid-phase-engine--why-this-exists).
- **Massive FIR upsampling** — per-ratio Kaiser (β = 14) filters, 1M–30M taps,
  measured stopband below **−220 dB**, designed offline in 128-bit precision
  and applied in end-to-end **f64** with Kahan-compensated summation.
- **Adaptive apodizer v3 — source forensics** — measures the exact *frequency*
  of the ADC/SRC pre-ring baked into a master and places a minimum-phase
  corrective lowpass just below it; unmasks fake hi-res (upsampled masters,
  including mirror-image aliasing from bad resamplers) at any container rate;
  leaves clean and minimum-phase sources untouched.
- **GPU acceleration with no precision loss** — convolution runs on Vulkan
  compute in **double-single (DS) arithmetic** (~48-bit effective mantissa,
  ~−260 dB residual vs f64), enforced at the SPIR-V level with
  `NoContraction` so the driver cannot fold it back to f32.
- **True-peak safety** — 4× Lanczos-4 intersample peak scan with a −0.5 dBTP
  ceiling, applied only when needed; quiet material passes bit-exact.
- **Honest output** — 24-bit TPDF dither (Wannamaker-9 noise shaping at
  ≤48 kHz), then a **bit-perfect re-decode verification** of every FLAC.
- **No external tools** — decoding and FLAC encoding are both pure Rust, and
  no subprocess is started during a conversion. The encoder streams frames to
  disk as they fill, so a 90-minute file costs the same peak RAM as a
  three-minute one.
- **It stops rather than substituting** — if the filter a setting needs is not
  on disk, the conversion fails and names the exact file, before it starts.
  It will not quietly fall back to an ordinary resampler while the interface
  still says "30M Taps".

<br clear="right"/>

## Download

**The filters are already inside.** Unzip, run `aura-engine.exe`, drop a track
on the window. No Python, no Rust, no second download, no folder to create —
the app opens on the filter that shipped with it and converts immediately.

| Bundle | Filter | Output rates | Size | |
|---|---|---|---|---|
| **Starter** | 1M taps | every multiplier — FS2 to FS16 | ~134 MB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.2.6/aura-engine-v1.2.6-bundle-1M-windows-x64.zip) |
| **Standard** — *start here* | 10M taps | FS8 — 352.8 / 384 kHz | ~326 MB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.2.6/aura-engine-v1.2.6-bundle-10M-windows-x64.zip) |
| **Reference** | 30M taps | FS8 — 352.8 / 384 kHz | ~966 MB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.2.6/aura-engine-v1.2.6-bundle-30M-windows-x64.zip) |

These point at the current release. The
[releases page](https://github.com/ToxaDev/aura-engine/releases/latest) always
has the newest one, whatever this table says.

Every bundle carries **both phase types**, so Hybrid-Phase works out of the
box, and **both rate families**, so 44.1 and 48 kHz sources convert alike.

**What it needs**

| | |
|---|---|
| Windows | 10 or 11, 64-bit |
| CPU | AVX2 — Intel Haswell (2013) or AMD Excavator (2015) and newer |
| Runtime | [Microsoft Edge WebView2](https://developer.microsoft.com/microsoft-edge/webview2/). Windows 11 has it already; Windows 10 often does not, and the LTSC and N editions never do |

**If nothing happens when you run it**, open a command prompt in the folder and
run `aura-engine.exe --selftest`. It prints what the build needs against what
your machine has, and writes `AuraEngine-crash.txt` next to the exe. That file
is enough to tell us why — [open an issue](https://github.com/ToxaDev/aura-engine/issues)
and attach it. From 1.2.3 the program also says so in a window of its own
rather than closing without a word, and where the cure is a download it offers
to open it.

Which one? The tap count sets how sharply the filter can cut, and how long the
conversion takes — [the measured difference is here](docs/15-measurements.md).
10M is the setting worth living with. 1M is there so you can try the whole
thing on a phone tether, and 30M is the ceiling the engine designs for.

> **Comparing them is the point.** Unzip more than one bundle into the same
> folder: the filter files merge, one copy of the app serves all of them, and
> the tap slider then offers every size you have. Convert the same track at 1M
> and at 30M and listen to what a tap count actually buys.

<details>
<summary><b>Just the app, or just the filters</b> — for people who already have blobs, or want a size we don't bundle</summary>

<br>

| Asset | What it is | Size |
|---|---|---|
| [`aura-engine-v1.2.6-windows-x64.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.2.6/aura-engine-v1.2.6-windows-x64.zip) | The app alone. Needs filters from somewhere. | ~6 MB |
| [`aura-filters-1M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1M-all-rates.zip) | 1M taps, all 8 output rates, both phases | 128 MB |
| [`aura-filters-5M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-5M-all-rates.zip) | 5M taps, all 8 output rates, both phases | 640 MB |
| [`aura-filters-10M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-10M-all-rates.zip) | 10M taps, all 8 output rates, both phases | 1.28 GB |
| [`aura-filters-30M-44k-family.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-30M-44k-family.zip) | 30M taps, 88.2 / 176.4 / 352.8 / 705.6 kHz | 1.92 GB |
| [`aura-filters-30M-48k-family.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-30M-48k-family.zip) | 30M taps, 96 / 192 / 384 / 768 kHz | 1.92 GB |

Filter packs contain only `.npy` blobs in a `fir-optimizer/output/` folder —
extract one next to `aura-engine.exe` and it is found. `AURA_FILTER_DIR` points
the app at a folder anywhere else. The 30M blobs are split by rate family
because one archive for all of them would exceed GitHub's per-file limit.

Whatever you end up with, the app adapts: it scans for blobs at startup, opens
on the largest filter it found at FS8, strikes through the slider positions it
has nothing for, and names the exact download for the one you select.

</details>

## The Hybrid-Phase engine — why this exists

<p align="center">
  <img src="docs/media/hybrid-phase.gif" width="780" alt="Animation: a conventional linear-phase filter pre-rings before every attack; AuraEngine's Hybrid-Phase detects the attack, switches to the minimum-phase render at a zero crossing, and the attack lands clean."/>
</p>

Every FIR filter forces a trade. **Linear phase** keeps inter-channel timing
perfect — the stereo image stays holographic — but it *pre-rings*: a faint
anticipatory smear arrives **before** every drum hit. **Minimum phase** hits
perfectly clean, but warps timing across frequencies. The industry's usual
answer is a fixed "intermediate-phase" compromise filter — which simply
carries a little of both flaws, everywhere, all the time.

**Hybrid-Phase refuses the trade.** AuraEngine renders the track **twice, in
full** — one complete linear-phase pass and one complete minimum-phase pass —
then a native-Rust HPSS transient detector decides, moment by moment, which
render you hear: linear phase through sustains and decays (imaging),
minimum phase through attacks (zero pre-ringing). The switch itself is
engineered to be inaudible:

- fires only at a **zero crossing of the mid signal**;
- **stereo-linked** — one switch plan applied to both channels at the same
  sample, so the image can never skew;
- 32-sample raised-cosine micro-fade (~0.09 ms) + 20 ms anti-chatter hold;
- both renders aligned sample-exact via band-weighted group delay before
  blending.

**What the detector keys on, precisely.** It measures the rise of percussive
energy between analysis frames — the *start of a sound*. It does not measure
whether the filter would ring on this material; it knows nothing about the
transition band. On music the two coincide, because an attack is both a
beginning and a broadband event. On synthetic material they come apart: a
steady square wave with arbitrarily steep edges produces no trigger at all,
because nothing begins. So the honest name for the stage is *minimum phase on
attacks*, and there is a known blind spot in the first ~24 ms of a file — both
are written up, with the numbers, in
[docs/06-hybrid-phase-proof.md §1](docs/06-hybrid-phase-proof.md).

This technique was invented for AuraEngine. We are not aware of any other
converter that does content-aware switching between two complete phase
renders — if you know one, open an issue: we would genuinely love to compare
notes. The verification methodology is documented in
[docs/06-hybrid-phase-proof.md](docs/06-hybrid-phase-proof.md).

## The signal path

```mermaid
flowchart TD
    A["Decode<br/><i>Symphonia · lossless i32 → f64</i>"] --> B["DC block<br/><i>static mean or 2 Hz IIR</i>"]
    B --> D["Apodizer (optional)<br/><i>source forensics: measured ring cutoff ·<br/>fake-hi-res unmasking · min-phase Kaiser</i>"]
    D --> E{Path}
    E -->|Standard| F["Rubato sinc resampler<br/><i>512-tap sinc · ~−180 dB</i>"]
    F --> G["FIR post-filter<br/><i>1M–30M taps · partitioned OLS ·<br/>CPU f64+Kahan or GPU DS</i>"]
    E -->|"Polyphase FIR<br/>(integer ratio)"| H["Polyphase interpolation<br/><i>the filter IS the resampler ·<br/>L sub-filters in parallel</i>"]
    G --> I["Hybrid-Phase (optional)<br/><i>2nd min-phase pass · HPSS onsets ·<br/>stereo-linked zero-crossing switch</i>"]
    H --> I
    I --> J["True-peak limiter<br/><i>4× Lanczos-4 · −0.5 dBTP ceiling</i>"]
    J --> K["Dither<br/><i>24-bit TPDF · Wannamaker-9 ≤48 kHz</i>"]
    K --> L["FLAC encode<br/><i>native · 24-bit · streaming</i>"]
    L --> M["Bit-perfect verification<br/><i>re-decode · compare ±2 LSB</i>"]
```

Two conversion paths share the same preparation and output stages:

| | Standard path | Polyphase FIR path |
|---|---|---|
| Resampler | Rubato `SincFixedIn` (512-tap sinc), then the big FIR as a post-filter | The big FIR **is** the resampler — decomposed into L sub-filters running in parallel |
| Ratios | Any | Integer only (non-integer targets snap down: 44.1 kHz → FS8 gives 352.8 kHz) |
| Trailing padding | ~0.4 s of resampler zero-pad | None — output length is exactly input × L |
| Filter blobs missing | Post-filter skipped with a warning | Hard error (by design — no silent quality downgrade) |

A detailed, beautifully rendered walkthrough of every stage lives at
**[toxadev.github.io/aura-engine](https://toxadev.github.io/aura-engine/)**
(source: [docs/index.html](docs/index.html)), and the same material as plain
markdown starts at **[docs/README.md](docs/README.md)**. The engineering laws the DSP core is
audited against are in **[DSP_MANIFESTO.md](DSP_MANIFESTO.md)**.

## Build from source (Windows)

> Only needed if you want to change the engine. To *use* it, take a
> [bundle](#download) — there is nothing to install and nothing to compile.

> AuraEngine is developed and tested on **Windows 11** (Windows 10 should
> work but is untested). Linux/macOS are currently not supported — the build
> uses the MSVC toolchain and a few Win32 APIs for thread priority and timer
> resolution.

### 1. Prerequisites

| Requirement | Why | Notes |
|---|---|---|
| [Rust](https://rustup.rs/) (stable, MSVC) | builds the app | recent stable recommended (fat LTO) |
| [Python 3.10+](https://www.python.org/) with `numpy scipy mpmath soundfile` | generates the FIR filters | one-time step |
| WebView2 runtime | Tauri UI | ships with Windows 11 |
| Vulkan-capable GPU *(optional)* | GPU DS convolution path | falls back to CPU automatically — no adapter, or out of video memory |

### 2. Clone and build

```bat
git clone https://github.com/ToxaDev/aura-engine.git
cd aura-engine\desktop-app
start.bat
```

`start.bat` compiles the release binary on first run and launches it.
Subsequent runs skip cargo entirely when nothing changed (instant start);
`start.bat --build` forces a rebuild, `--clean` wipes the build cache.
No Node.js, no npm, no Tauri CLI — the frontend is static HTML/JS embedded
into the binary.

### 3. Get the FIR filters (one-time)

The converter loads pre-computed filter coefficient files (`.npy`) —
it deliberately refuses to synthesize filters at runtime, because runtime
generation could not match the 128-bit design precision.

**Easiest way:** grab a [filter pack](#download) and extract it into the
repo folder. The archives already contain the `fir-optimizer/output/`
structure, so nothing has to be moved afterwards.

**Or generate them yourself:**

```bat
cd ..\fir-optimizer
pip install -r requirements.txt
python optimize.py --all-ratios
```

`--all-ratios` populates `fir-optimizer/output/` with the full matrix —
4 tap sizes × 8 output rates × 2 phase types = 64 files, roughly **10 GB**,
and it can take a while for the 30M presets. It skips files that already
exist, so you can interrupt and resume. If you only care about one preset,
see [fir-optimizer/README.md](fir-optimizer/README.md) for generating a
subset. Store the blobs anywhere by setting the `AURA_FILTER_DIR`
environment variable to the folder that contains them.

Whatever subset you end up with, the app works out what it has at startup:
it opens on the largest tap count it found at FS8, strikes through the
slider positions with no blob behind them, and names the download for one
you select anyway. [`tools/make-bundles.ps1`](tools/make-bundles.ps1) builds
the app-plus-filters packages that appear on the release page.

### 4. Convert

1. Launch the app (`start.bat`).
2. Set the **FS multiplier** (FS2–FS16) and **filter resolution** (1M–30M taps).
3. Optionally enable **Adaptive Apodizer**, **Hybrid-Phase Blending**,
   **Polyphase FIR Resampling**, or **Hardware GPU Acceleration**.
4. Drop files onto the window — conversion starts immediately.
5. The output FLAC appears **next to the source file**, named like:

```
Track [AE · 44.1k→352.8k · Kaiser 10M · f64 · AA · HP].flac
```

A `✓ VERIFIED` badge means the written FLAC was re-decoded and matched the
internal DSP buffer within ±2 LSB. A console window runs alongside the UI on
purpose — it is the engine's full audit log (filter resolution, hybrid-phase
coverage, true-peak decisions, verification results).

## Controls reference

| Control | What it does |
|---|---|
| **FS Multiplier** (FS2/4/8/16) | Output rate = source family base × multiplier. 44.1 kHz family → 88.2/176.4/352.8/705.6 kHz; 48 kHz family → 96/192/384/768 kHz. |
| **Filter Resolution** (1M/5M/10M/30M) | Tap count of the main FIR. More taps → narrower transition band and deeper stopband, at the cost of compute time. A struck-through mark means this build has no filter file for that size; selecting it names the download that would add it. |
| **Custom filter (.npy)** | Load your own 1-D float64 coefficient file instead of the built-in matrix. |
| **Window** | Filename tag of the filter family (the actual filter is selected by taps + output rate). |
| **Hardware GPU Acceleration** | Runs convolution on Vulkan compute in DS precision. Automatically falls back to CPU (f64) when the adapter lacks `SPIRV_SHADER_PASSTHROUGH` (e.g. DX12-only), and when the card runs out of memory during a batch — per phase on the polyphase path, so the phases already on the GPU stay there. |
| **Apodizing** (Off/Gentle/Moderate/Strong) | Static minimum-phase corrective lowpass at 20/19/18 kHz for CD-era sources. |
| **Adaptive Apodizer** | Per-track source forensics: detects pre-ring and measures its exact frequency, unmasks fake hi-res via spectral-cliff detection and a mirror-image alias probe, and applies a corrective filter only on real evidence (tag `AA`). |
| **Hybrid-Phase Blending** | Dual linear+minimum-phase convolution with transient-driven switching (tag `HP`, ~2× processing time). |
| **Polyphase FIR Resampling** | The direct path: FIR-as-resampler at integer ratios, exact output length. |
| **Headroom** (0/−0.5/−1/−3 dB) | The true-peak ceiling the finished render is normalised to. `Off` leaves −0.5 dBTP; −3 dB puts the output peak on −3.0 dBTP. A file already below it is left alone. |

Sources at or below 48 kHz get the full treatment. Hi-res containers skip the
static apodizing presets, but the Adaptive Apodizer analyzes them too: if a
"hi-res" file is really an upsampled 44.1/48 kHz master, the baked-in brickwall
is detected and treated against the *original* Nyquist. Input formats: WAV, FLAC,
MP3, OGG, AAC, M4A. Output is always 24-bit FLAC. Files with non-standard
rates are rejected (`BAD`), files already at or above the target are skipped
(`SKIP`).

## How the quality claims are enforced

This project treats sound-quality claims as **testable invariants**, not
marketing. The rules live in [DSP_MANIFESTO.md](DSP_MANIFESTO.md); the
mechanics, briefly:

- **Unity gain**: every filter is DC-normalized (`sum(h) == 1.0`) at design
  time; the converter never changes loudness unless true-peak protection has
  to act.
- **f64 everywhere**: decode promotes lossless i32 → f64; there is no f32
  truncation anywhere in the CPU sample path. The GPU path uses double-single
  f32 pairs (~48-bit mantissa) specifically because plain f32 would not meet
  the noise floor.
- **Latency-exact alignment**: OLS convolver latency (2 blocks CPU, 1 block
  GPU) and FIR group delay are trimmed analytically — tested by unit tests
  (`cargo test`), not tuned by ear.
- **Bit-perfect verification**: every output file is re-decoded and compared
  against the DSP buffer. 25 unit tests cover convolver latency, unity gain,
  polyphase reconstruction, phase alignment, true-peak and dither behaviour.

### Measured, not promised

These are measurements of the **actual production filter files** — not
design-tool renderings. Reproduce them with
[`fir-optimizer/plot_measurements.py`](fir-optimizer/plot_measurements.py);
full gallery with methodology in **[docs/15-measurements.md](docs/15-measurements.md)**.

| Quantity (30M taps, 44.1 → 352.8 kHz) | Design law | **Measured** |
|---|---|---|
| Stopband attenuation | ≤ −140 dB | **≤ −220.9 dB** |
| Passband ripple | flat | **± 0.09 nano-dB** |
| DC gain error | 0 | **1.1 × 10⁻¹⁵** |
| Transition width (−6 → −120 dB) | — | **0.05 Hz** (a DAC chip: 2–4 kHz) |

![Measured frequency response of the production 30M-tap filter](docs/media/m1-frequency-response.png)

![Measured transition band of all four filter sizes](docs/media/m3-resolution-ladder.png)

## Documentation

| Document | Contents |
|---|---|
| [The Signal Path (website)](https://toxadev.github.io/aura-engine/) | The full signal path, visually — every stage with its parameters and rationale |
| [docs/15-measurements.md](docs/15-measurements.md) | Measured frequency/impulse responses of the production filters + how to reproduce them |
| [docs/16-filter-length-and-rate.md](docs/16-filter-length-and-rate.md) | What a tap count means, at which rate — and why 30M taps is not a 10-minute window |
| [docs/17-flac-encoding.md](docs/17-flac-encoding.md) | The native FLAC encoder: streaming design, high-rate handling, and the measured compression settings |
| [docs/01-architecture.md](docs/01-architecture.md) | Data flow, module map, technology stack |
| [docs/05-converter-pipeline.md](docs/05-converter-pipeline.md) | Both processing paths, stage by stage |
| [docs/06-hybrid-phase-proof.md](docs/06-hybrid-phase-proof.md) | Hybrid-Phase engine: detection, switching, verification |
| [docs/07-audiophile-features.md](docs/07-audiophile-features.md) | Each sound-quality feature in plain language |
| [docs/09-audio-auditor-guide.md](docs/09-audio-auditor-guide.md) | Step-by-step signal audit for reviewers |
| [docs/12-precomputed-fir-matrix.md](docs/12-precomputed-fir-matrix.md) | Filter blob naming, lookup, generation |
| [docs/13-pipeline-hardening-2026-07.md](docs/13-pipeline-hardening-2026-07.md) | The 2026-07 correctness audit pass |
| [DSP_MANIFESTO.md](DSP_MANIFESTO.md) | The laws: gain staging, phase, precision |
| [CHANGELOG.md](CHANGELOG.md) | Release history |

## FAQ

**30 million taps ÷ 48 kHz = 10 minutes. Is the filter really that long?**
No — a tap count here is defined at the **output** rate, never at the source
rate. The 30M blob for 44.1 kHz × 8 runs at 352.8 kHz, so its kernel is 85 s
(39 s at 768 kHz), and 99.76 % of its energy lies within ±1 ms of the centre.
Full arithmetic, per-rate table and measured tail decay:
**[docs/16-filter-length-and-rate.md](docs/16-filter-length-and-rate.md)**.

**Why offline instead of real-time?**
A 30M-tap convolution at 768 kHz cannot run in real time on consumer
hardware without cutting corners. Offline rendering removes the deadline, so
every stage can use the highest-quality algorithm instead of the fastest one.

**Do I really need the GPU?**
No. The CPU path is the reference implementation (f64, Kahan-compensated).
The GPU path exists to make 10M/30M-tap conversions dramatically faster while
staying within ~−260 dB of the CPU result — far below audibility.

**Why does a console window open with the app?**
It is the audit log, and it is intentional. AuraEngine's core promise is that
you can *see* what it did to your audio — filter selection, hybrid-phase
switch coverage, true-peak action, verification verdicts.

**Does it need ffmpeg or any other external tool?**
No. Since 1.1.0 both decoding and encoding are pure Rust — Symphonia in,
`flacenc` out — and no subprocess is started during a conversion. The one
remaining exception is *verifying* 705.6/768 kHz output: those rates exceed
the FLAC specification's own 655350 Hz cap, so no pure-Rust decoder will read
them back. Without ffmpeg installed the log says plainly that verification
could not run at that rate, instead of marking a good file `_UNVERIFIED`. See
[docs/17-flac-encoding.md](docs/17-flac-encoding.md).

**Can it damage loudness or dynamics?**
No. The engine applies gain in exactly one documented place: the true-peak
normaliser, when the reconstructed waveform would exceed the ceiling — −0.5 dBTP,
or whatever Headroom names. It never raises a quiet file. Everything else is
unity-gain by construction, and the verification step proves the file on disk
matches the math.

## Project structure

```
aura-engine/
├── desktop-app/            # The converter (Tauri app)
│   ├── src/                #   Frontend: static HTML/CSS/JS (no build step)
│   ├── src-tauri/          #   Rust backend
│   │   └── src/audio/      #     DSP core: converter/, gpu/, dsp_core.rs,
│   │                       #     hybrid_phase.rs, hpss_native.rs
│   └── start.bat           #   Build-and-run launcher
├── fir-optimizer/          # Python filter designer (generates .npy blobs)
├── docs/                   # Technical documentation + docs/index.html
├── DSP_MANIFESTO.md        # Engineering laws of the DSP core
└── CHANGELOG.md
```

## Contributing

PRs are welcome — read [CONTRIBUTING.md](CONTRIBUTING.md) first, especially
the part about the [DSP manifesto](DSP_MANIFESTO.md): changes to the audio
path must keep its invariants (unity gain, f64 precision, phase behaviour)
and ship with tests. CI runs `cargo check` + `cargo test` on Windows.

## Acknowledgments

Built on excellent open-source foundations:
[Tauri](https://tauri.app/) · [rustfft](https://crates.io/crates/rustfft) ·
[rubato](https://crates.io/crates/rubato) ·
[Symphonia](https://crates.io/crates/symphonia) ·
[wgpu](https://wgpu.rs/) · [rayon](https://crates.io/crates/rayon) ·
[flacenc](https://crates.io/crates/flacenc) · NumPy/SciPy/mpmath.

## License

**[PolyForm Noncommercial 1.0.0](LICENSE)** © 2026 ToxaDev

In plain words: the source is open to read, build, use, modify and share
**for any noncommercial purpose** — personal listening, hobby projects,
research, education. **Commercial use of any kind requires a separate
license from the author** — all commercial rights are reserved. If you want
to use AuraEngine (or a derivative of it) in a product or service, write to
<auraengine.dev@gmail.com> to discuss commercial licensing.

## Contact

| | |
|---|---|
| Something is broken | [open an issue](https://github.com/ToxaDev/aura-engine/issues) |
| Questions, ideas, results you want to share | [Discussions](https://github.com/ToxaDev/aura-engine/discussions) |
| Commercial licensing, or anything private | <auraengine.dev@gmail.com> |

GitHub has no private messaging, so the address above is the way to reach the
author directly.
