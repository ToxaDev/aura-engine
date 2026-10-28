<div align="center">

<img src="docs/media/aura-icon.png" width="112" alt="AuraEngine icon"/>

# AuraEngine

**A no-compromise audio upsampler and player for audiophiles.**

**[Website](https://aura-engine.app/)** &nbsp;·&nbsp; **[Documentation](https://aura-engine.app/docs.html)** &nbsp;·&nbsp; **[Web converter](https://huggingface.co/spaces/toxadev/aura-engine-audio-upscaler)**

Precomputed FIR filters, 5k to 1G taps, multistage — several times less CPU · output to 1.536 MHz · DSD64/128/256 output and DSF/DFF playback
· Hybrid-Phase transient engine · runs on an ordinary CPU, no graphics card needed — optional GPU convolution in double-single precision · true-peak protection · bit-perfect output verification
· real-time playback, internet radio and an ASIO input through the same chain

[![CI](https://github.com/ToxaDev/aura-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/ToxaDev/aura-engine/actions/workflows/ci.yml)
[![License: PolyForm NC](https://img.shields.io/badge/License-PolyForm%20Noncommercial-teal.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-0078D6)
![Rust](https://img.shields.io/badge/backend-Rust-orange)
![DSP](https://img.shields.io/badge/DSP-f64%20end--to--end-blueviolet)

[![Try it on Hugging Face](docs/media/try-on-hf.svg)](https://huggingface.co/spaces/toxadev/aura-engine-audio-upscaler)

*The same engine, in your browser — up to 30 million taps, eight tracks at a time, nothing to install.*

### ⬇ Download 1.9.4

[**Windows**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-lite-windows-x64.zip) &nbsp;·&nbsp; [**Linux**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-linux-x86_64.AppImage) &nbsp;·&nbsp; [**macOS**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-macos-universal.dmg)

*[All options and install notes](#download) — Windows bundles carry the filters, the rest downloads in the app.*

</div>

---

<img src="docs/media/shot-hero-680.webp" align="right" width="300" alt="AuraEngine 1.9.2: the settings, the Advanced DSP rack, the player and the list — a track playing at FS×2 (88.2 kHz) with 5M taps and Hybrid-Phase, through the chain it would be converted with"/>

**AuraEngine** takes ordinary 44.1/48 kHz FLAC/WAV/MP3 files and re-renders
them at up to **768 kHz / 24-bit FLAC** (1.536 MHz for DSD output and the
player), using FIR filters of **5 thousand to a billion taps** whose
coefficients are designed in **128-bit precision**. It spends the math your DAC's built-in interpolation filter never
could: the DAC then receives an already-reconstructed, oversampled waveform and
only has to play it.

**It converts, and it plays — one engine, one chain.** Every file in the list
has ▶ beside **C**, its convert button, and internet radio plays through the
same rack. The converter takes as long as it needs and writes a file; the
player prepares ahead what needs the whole track and switches each stage in as
soon as it is ready, and neither trades sound for speed without saying so —
[one engine, two ways to use it](#one-engine-two-ways-to-use-it). A live
analyzer and scenes that draw the music come with it.

**No powerful PC or graphics card needed.** The long filter runs once, at the
source rate, and a short exact interpolator does the rest — multistage
(cascade) upsampling. On four efficiency cores of an i9-14900K, standing in for
a modest laptop, the player runs a 30-million-tap filter at ×8 on 27 ms of CPU
per second of music, and the first sound of a 30M track is ready 1.2–1.5 s
after the press, without waiting for a conversion. The output matches the single-stage
30M filter to −204…−220 dBFS —
[performance on modest hardware](#performance-on-modest-hardware).

Everything the engine does is **verifiable by design**: the full DSP trace is
logged to the console, every output file is re-decoded and compared
sample-by-sample against the internal f64 buffer, and files that fail
verification are renamed `_UNVERIFIED` instead of being silently kept.

**New in 1.9.0 — Linux and macOS versions**
1. **Linux (x86_64)** — AppImage and .deb; playback straight to the DAC through
   ALSA `hw:`, bit for bit (your user needs to be in the `audio` group).
2. **macOS** — one universal .dmg (Apple Silicon and Intel), CoreAudio in
   exclusive (hog) mode, checked bit for bit through a loopback device.
3. **One copy per system** — on Linux and macOS too, a second launch brings
   the open window to the front.
4. **Fixed:** a stuck USB device no longer hangs the app on Linux.
   The sound on Windows is unchanged; renders made by 1.8.x stay valid.

**1.9.4 — no clicks, analyzer fixes:** a short fade where the sound stops or starts again (seek, settings change, pause, Stop); the O spectrum shows for a converted file played from disk; smooth spectrum curves; Live / Instant follows the playhead on a file. Renders stay valid.

**1.9.3 — fixes:** the αHP badge lights up with very large filters (segmented render); a billion taps is `1G` in file names, not `1000M`. The sound is unchanged.

**1.9.2 — updates itself on Linux and macOS**, as on Windows (AppImage, .deb and .dmg). From 1.9.0 or 1.9.1 on Linux and macOS, install 1.9.2 by hand once.

**1.9.1 — fixes:** a track started by hand at 1G taps no longer stutters; album level no longer stops on PCs with little free RAM; MP3 titles no longer show as Chinese characters; the playlist's border stays inside the window; the "filters ready" bar goes away after a download.

**1.8.4 — fix:** the in-app filter download now also fetches the minimum-phase filter that TFS Phase (on by default) needs, so playback no longer stops with "Missing FIR filter".

**1.8.3 — fix:** conversion now works with about 1 GB of free RAM on 8 GB PCs (slower, through temporary files) instead of refusing the file.

**1.8.2 — fix:** conversion now starts on PCs with 8 GB of RAM; it no longer waits forever for 4.4 GB of free memory.

**New in 1.8.1 — filters download inside the app, a Storage tab, bundles by output rate**
1. **In-app filter download.** Missing filters show as one compact line with
   their size; a dialog downloads them from the signed `filters` release, with
   resume, a free-space check and progress in the bar.
2. **Storage tab** — filter files this version no longer uses, to the Recycle Bin.
3. **Four bundles by output rate** (Lite, ×8, ×16, ×32) replace the 5k/1M/10M/30M ones.
4. **Fixed:** the updater gave up on slow connections after five minutes.

**New in 1.8.0 — filters up to 1G taps, ×32 rates, shaped noise above 48 kHz**
([changelog](CHANGELOG.md))

1. **The tap ladder reaches 1G** — 5k · 1M · 5M · 10M · 30M · **100M · 180M ·
   300M · 500M · 1G** (counted at the output rate). The new rungs are stored as
   branch 0 at the source rate — 32 times smaller than the whole filter at
   ×32 — with minimum phase available, and play through the multistage
   upsampler. 30M is marked *optimal*: its stop band, −220.9 dB, is the
   deepest of the ladder (100M −216, 300M −207); longer filters make the
   cutoff steeper, not the floor deeper —
   [docs/12 §11](docs/12-precomputed-fir-matrix.md#branch0).
2. **×32: 1411.2 and 1536 kHz** — in the player where the device takes it,
   and in the converter for DSD256 and faster. FLAC cannot hold the rate, so
   the converter refuses FLAC at ×32 up front.
3. **Noise shaping above 48 kHz** — each output rate from 88.2 to 1536 kHz has
   its own minimum-phase shaper that takes the 24-bit rounding noise out of the
   audio band: 13–15 dB at ×2 down to 63–64 dB at ×32 against plain TPDF.
   On by default; a blind A/B found no difference. A **16-bit device** in the
   player now gets dither and shaping on its own grid —
   [docs/18](docs/18-dsp-stages.md#noise-shaping).
4. **New filter packs** on the [1.0.0 release](https://github.com/ToxaDev/aura-engine/releases/tag/v1.0.0):
   the ×32 rates of the five old rungs and the five new rungs (per rate family
   and phase). The bundles are unchanged — [download](#download).
5. **Interface** — the FS and taps sliders redrawn (sockets in one groove,
   thin outline), volume as "−5.5 dB", and the custom filter button, held
   while Hybrid-Phase is on, says why. **Fixed:** the window's close button
   needed a second click while a popup was open.

After updating, renders made by **1.7.x** show as not converted ("Made by 1.7.x
— convert again") — the noise shaping changed every render above 48 kHz, so
the render generation is raised. The source plays meanwhile and the old file
is kept.

**1.7.1** — the list's **Convert** button writes DSF as well as FLAC (its arrow
picks the format), and the rack's Output pane opens again
([changelog](CHANGELOG.md)).

**New in 1.7.0 — DSD, multistage upsampling, faster starts**
([changelog](CHANGELOG.md))

1. **DSD output** — any source to DSD64, DSD128 or DSD256 over DoP (WASAPI
   exclusive), with twelve modulators up to 9th order including look-ahead:
   noise plus distortion in the audio band from −121 dB (SDM5) to −158.8 dB
   (SDM7 LA+) at DSD64, −180 dB at DSD128. The list shows what this PC can
   run and steps down if it falls behind. **DSF and DFF files** play as they
   are (bit-perfect over DoP) or through the full chain; the converter writes
   DSF from DSD64 to DSD1024. Verified on a Chord Mojo 2 — [DSD](#dsd).
2. **Multistage upsampling, now the default** — the long filter runs once at
   the source rate, a short exact interpolator (≥ 210 dB) does the rest. With
   5M filters and longer the player needs 3.1× (×4) to 7.2× (×16) less CPU with
   30M — up to 9.5× on four efficiency cores — the converter 2.2–3.5× less, and
   30M ×8 on the GPU about 170 MiB instead of 591. The difference to the
   single-stage output is −204…−220 dBFS; in a blind A/B no difference was
   heard — [docs/21](docs/21-multistage-upsampling.md).
3. **Faster starts, same output to the bit** — the next track in 62 ms instead
   of 260, an analysed track at ×4 in 609 ms instead of 937, a converted file
   in 0.36 s instead of 1.23. The whole-track analysis is cached and the list
   is analysed ahead.
4. **CPU | GPU load** — a badge beside the version, and a window with the last
   hour of load.
5. **A reworked top bar** — the output format dropdown in one housing with the
   device button, *high CPU* / *too heavy for this PC* marks, tips above
   popovers, one badge style.

After updating to 1.7.0, renders made by earlier versions showed as not
converted — each render carries a version stamp; 1.8.0 raised its generation
(see above).

**Highlights**

- **⭐ The Hybrid-Phase engine — the signature invention of this project.**
  The track is rendered **twice in full** (linear phase + minimum phase) and an
  onset detector switches between the two renders per-attack, stereo-linked, at
  zero crossings. No pre-ringing ahead of the attacks it fires on, stereo image
  intact everywhere else —
  [see it animated below](#the-hybrid-phase-engine--why-this-exists).
- **A player on the same chain** — ▶ on any file plays it through the rack it
  would be converted with; a seek, the next track or a change in the rack never
  cuts the sound, and badges under the player show which stages are already in
  it. What it sends to the device and the converted file differ by less than
  −120 dB once every stage is in — [the player](#the-player).
- **Internet radio through the rack** — featured stations, a catalog of tens
  of thousands to search, or any stream's address: MP3, AAC, HE-AAC, FLAC,
  Vorbis, Opus and HLS, upsampled live. A dropped connection comes back
  without a seam, and long filters do not make a stream wait —
  [internet radio](#internet-radio).
- **Massive FIR upsampling** — per-ratio Kaiser filters from 5k to 1G taps
  (β = 14 from 1M up, fitted to the length at 5k), measured stopband down to
  **−221 dB** (at 30M; longer filters make the wall steeper), designed offline
  in 128-bit precision
  and applied in end-to-end **f64** with Kahan-compensated summation.
- **Adaptive apodizer v3 — source forensics** — measures the exact *frequency*
  of the ADC/SRC pre-ring baked into a master and places a minimum-phase
  corrective lowpass just below it; unmasks fake hi-res (upsampled masters,
  including mirror-image aliasing from bad resamplers) at any container rate;
  leaves clean and minimum-phase sources untouched.
- **Optional GPU acceleration with no precision loss** — not required: the
  CPU path plays and converts everything on its own. With a card, convolution runs on Vulkan
  compute in **double-single (DS) arithmetic** (~48-bit effective mantissa,
  ~−260 dB residual vs f64), enforced at the SPIR-V level with
  `NoContraction` so the driver cannot fold it back to f32.
- **True-peak safety, album by album** — 4× Lanczos-4 intersample peak scan
  with a −0.5 dBTP ceiling, applied only when needed; quiet material passes
  bit-exact. With **Album Level** the tracks of one album share one gain, so
  the album keeps the balance it was mastered with.
- **Honest output** — 24-bit TPDF dither with noise shaping at every rate
  (Wannamaker-9 at ≤48 kHz; a per-rate shaper from 88.2 to 1536 kHz that takes
  13–64 dB of noise out of the audio band), then a **bit-perfect re-decode verification** of every FLAC.
- **No external tools** — decoding and FLAC encoding are both pure Rust, and
  no subprocess is started during a conversion. The encoder streams frames to
  disk as they fill, so a 90-minute file costs the same peak RAM as a
  three-minute one.
- **It stops rather than substituting** — if the filter a setting needs is not
  on disk, the conversion fails and names the exact file, before it starts.
  It will not quietly fall back to an ordinary resampler while the interface
  still says "30M Taps".
- **An analyzer, and pictures** — loudness, LRA, true peak and DR of the
  source and the output side by side, with spectrograms that compare them; and
  19 GLSL scenes that draw the music, three of them each instrument on its own
  — [the analyzer](#the-analyzer), [visualizations](#visualizations).

<br clear="right"/>

## One engine, two ways to use it

The question is not offline against real time. There is one engine and one
chain, and it does not quietly give up sound for speed. The converter has no
deadline. The player prepares ahead what a track needs whole and switches each
stage in as soon as it is ready — the badges show which are in — and once every
stage is in, what it sends to the device matches the converted file to −120 dB
(measured on 30M taps, FS×8, TFS, XTC). When the machine cannot keep up, the
player moves the convolution to the graphics card, then steps the tap count
down, and says so. A live stream has no future to read ahead into: only the two
stages that need the whole track, Declip and the Adaptive Apodizer, stand down
there, and the interface shows it.

| | Converter | Player — a file | Player — a stream |
|---|---|---|---|
| **Chain** | The whole rack, every stage at full length | The same rack; the source stages prepared ahead over the whole track | The same rack, as the stream arrives; linear phase looks 50 ms ahead |
| **Whole-track stages** | All of them, before anything is written | All of them — switched in as they get ready, or all before the first sound with Instant start off | Declip and the Adaptive Apodizer stand down; the level follows a slow gain under the same ceiling |
| **Album Level** | One level for the tracks of an album | Each track at its own level | — |
| **Hardware** | Any machine: 10M or 30M taps at a high FS just take longer | Real time on the CPU — since 1.7.0 a 30M filter at ×4 takes about a quarter of one core — or on a discrete graphics card; if neither keeps up, fewer taps for that track, and the taps chip says so | As for a file |
| **Output** | A 24-bit FLAC, re-decoded and checked against the f64 render; or DSF, DSD64 to DSD1024 | WASAPI exclusive at the output rate, DSD64/128/256 over DoP, or BIT-PERFECT | The same |
| **Best for** | Music to keep and play anywhere | Hearing a setting before converting; listening at once | Internet radio |

**Why convert, when the player matches it?** Any filter runs on any machine —
10M or 30M taps at a high FS just take longer. Album Level gives an album one
level, and that needs every track at once; the player plays each track at its
own. The file plays anywhere — a player, a streamer, a phone, no PC needed.
Listening to it costs no CPU and has nothing to keep up with. And the file is
checked against the f64 render it came from.

**Why play?** To hear a setting before an evening goes into converting with it;
to listen at once, without disk space spent on hi-res copies; and for radio —
a stream cannot be converted, so the player is the one way to put it through
the chain.

## Performance on modest hardware

**30-million-tap upsampling on a CPU, without a graphics card.** Since 1.7.0
the long filter — the same 5M to 30M file as before, and every rung up to 1G —
runs once at the source rate; a short linear-phase interpolator, designed to
218 dB with a stop band of at least 210 dB, takes the result the rest of the way
to ×2…×32. The long filter still decides the wall, the phase and the ringing;
the work per output sample falls several times. How it works and how it was
checked: [docs/21](docs/21-multistage-upsampling.md).

All numbers below were measured on an Intel Core i9-14900K. The *four
efficiency cores* rows pin the process to four of its E-cores, as a stand-in for
a modest laptop CPU — not a real laptop. Source: 44.1 kHz CD material; filter:
30M taps (counted at the output rate), linear phase.

**Player — CPU per second of music** (milliseconds of CPU time, convolution)

| | ×4 | ×8 | ×16 |
|---|---|---|---|
| Four efficiency cores — multistage | 50 | **27** | 23 |
| Four efficiency cores — single-stage | ~160 | ~160 | ~160 |
| All cores — multistage | 11 | **6.9** | 4.7 |
| All cores — single-stage | 34 | 34 | 34 |

On four efficiency cores a 30M ×8 stream now runs about 35 times faster than
real time (about 7 times single-stage). In a real window, 30M ×4 went from one
full core to a quarter of one; the CPU badge read *CPU 3–4* before and *CPU 1*
after.

**Player — from the press to the first sound** (30M, a four-minute track,
CPU only, XTC and the subsonic filter on; the time until the first 0.3 s of
output is ready, opening the device not included; median of three)

| | ×4 | ×16 |
|---|---|---|
| Four efficiency cores — multistage | **1.51 s** | **1.15 s** |
| Four efficiency cores — single-stage | 1.86 s | 1.80 s |
| All cores — multistage | **0.40 s** | **0.62 s** |
| All cores — single-stage | 0.79 s | 0.77 s |

Playback does not wait for a conversion: the track is decoded and the source
stages run ahead of what you hear (10–16 s of it is ready by the time the chain
is built), and stages that need the whole track switch in as they get ready.
Measured on 1.8.1 code, 2026-10-24, with the `first_block_cost` bench in
`player/first_block_bench.rs`. In the window, a track the player has already
analysed starts in about 0.6 s at ×4 and a converted file in 0.36 s
([docs/19](docs/19-player-and-radio.md)).

**Converter — a 3:43 track, 30M, all cores, CPU only**

| | ×4 | ×8 | ×16 |
|---|---|---|---|
| Multistage, wall time | **13 s** | **20 s** | **33 s** |
| Single-stage, wall time | 23 s | 29 s | 45 s |
| Multistage, times faster than real time | 17× | 11× | 6.8× |

On the graphics card a 30M ×8 conversion takes 16.7 s instead of 23.5, in
about 170 MiB of video memory instead of 591.

**Multistage against single-stage — the difference in the output** (null, dBFS)

| 30M ×8 | Difference |
|---|---|
| Linear phase | −212 … −220 |
| Minimum phase | −212 … −219 |
| TFS | −204 … −210.5 |
| Hybrid-Phase | −219.8 |

The last bit of a 24-bit file is at −144 dBFS, so the difference sits about
66 dB below it. It is not bit-identical: rounded to 24 bits, one sample in
20 000 to 90 000 differs by one LSB. The wall (−6 dB at 21 050.01 Hz), the
phase, the ringing and the delay are unchanged, and in a blind A/B the
listener could not tell single-stage and multistage renders apart.

What multistage does not change: how long a filter should be is still a
setting, not a quality tier — on music a 5k and a 30M render agree below
20 kHz ([docs/12](docs/12-precomputed-fir-matrix.md#short)). Multistage makes
the long filters cheap enough to choose on an ordinary computer. It applies
from 5M taps up; 1M (the default on a fresh install), 5k, custom filters and
×3/×6 stay single-stage.

## Download

| | Download | Also |
|---|---|---|
| **Windows 10/11** (x64) | [**Windows bundle (Lite)**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-lite-windows-x64.zip) | bigger bundles below · [app only](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-windows-x64.zip) |
| **Linux** (x86_64, ALSA) | [**AppImage**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-linux-x86_64.AppImage) | [.deb](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-linux-amd64.deb) |
| **macOS** (Apple Silicon + Intel, CoreAudio) | [**Universal DMG**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-macos-universal.dmg) | |

[Checksums](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/SHA256SUMS.txt) · [all releases](https://github.com/ToxaDev/aura-engine/releases)

- **Linux:** `chmod +x aura-engine-v1.9.4-linux-x86_64.AppImage` and run it (or install the `.deb`). For direct bit-perfect
  `hw:` output your user must be in the `audio` group (`sudo usermod -aG audio $USER`, then log in again).
- **macOS:** the app is not signed by Apple. Right-click it → **Open**, or run
  `xattr -cr "/Applications/Aura Engine.app"` once. Output goes to CoreAudio in exclusive (hog) mode.
- On Linux and macOS the filters download in the app on first use. Results match Windows to rounding.

### Windows bundles

**The filters are already inside.** Unzip, run `aura-engine.exe`, drop a track
on the window: ▶ plays it, **C** converts it. No Python, no Rust, no folder to
create — the app opens on a filter that shipped with it and works immediately.

| Bundle | What is inside | Size | |
|---|---|---|---|
| **Lite** — *start here* | The app + 5k and 1M taps at every multiplier, FS2 to FS32 | ~0.17 GB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-lite-windows-x64.zip) |
| **×8** | 5k–30M taps at ×8 (352.8 / 384 kHz) + 5k–10M at ×4 | ~2.0 GB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-x8-windows-x64.zip) |
| **×16** | 5k–30M taps at ×16 (705.6 / 768 kHz) + 5k–10M at ×8 | ~2.0 GB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-x16-windows-x64.zip) |
| **×32** | 5k–30M taps at ×32 (1411.2 / 1536 kHz) + 5k–10M at ×16 | ~2.0 GB | [**Download**](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-bundle-x32-windows-x64.zip) |

Pick the bundle for the output rate your DAC runs at. Anything a setting needs
that the bundle does not carry — the 100M to 1G rungs, other multipliers —
**the app downloads itself**: a one-line bar above the filter rack names what is
missing (with its size), and a dialog fetches it from the public `filters`
release, every file checked by size and SHA-256 against a signed manifest. A
cancelled download resumes where it stopped. The **Storage** tab lists filter
files the current version no longer uses and moves them to the Recycle Bin.

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
| Audio output *(for playback)* | Opened in WASAPI exclusive mode at the output rate: nothing else can play through the device at the same time, and it must accept that rate — FS×8 is 352.8 / 384 kHz, FS×32 (1.8.0) is 1411.2 / 1536 kHz, and a DAC that does not take the rate needs a lower FS multiplier. DSD output needs a DAC that accepts DoP (DSD64 opens the device at 176.4 kHz, DSD128 at 352.8, DSD256 at 705.6). Converting needs no audio device. |
| Internet *(for radio)* | Any connection a stream plays on. The station catalog is [Radio Browser](https://www.radio-browser.info/). |
| Graphics card *(optional)* | Vulkan with SPIR-V passthrough. Conversion uses any such card; playback uses a discrete one, and only when the CPU cannot keep up — since 1.7.0 that is rare: with multistage upsampling a CPU does 30M ×8 at a fraction of its former cost. Without a card, everything runs on the CPU. |
| Disk *(optional)* | For the visualizations with instruments: a 282 MB model pack, downloaded once, when such a scene is first chosen — 374 MB installed, 724 MB free while it installs. |

**Updates install themselves from 1.5.0 on.** At start the app looks for a
newer release and offers it; the download is checked against a signature over
its version tag, file name and SHA-256 before anything is written, and the app
restarts into the new version. It will not update while a conversion is
running. 1.3.4 and earlier have no updater — install 1.5.0 from this page once;
your settings are kept.

**If nothing happens when you run it**, open a command prompt in the folder and
run `aura-engine.exe --selftest`. It prints what the build needs against what
your machine has, and writes `AuraEngine-crash.txt` next to the exe. That file
is enough to tell us why — [open an issue](https://github.com/ToxaDev/aura-engine/issues)
and attach it. From 1.2.3 the program also says so in a window of its own
rather than closing without a word, and where the cure is a download it offers
to open it.

Which one? The tap count sets how steep the wall at the cutoff is, and how long
the conversion takes. It is a setting with a ceiling, not a quality tier: on the
magnitude response there is nothing in thirty million taps that five thousand
with a wider window cannot do, and on music a 5k and a 30M render agree below
20 kHz — what differs is the last kilohertz of the CD band
([measured](docs/12-precomputed-fir-matrix.md#short)). 10M at FS8 is where to
start. 5k is the smallest download: its filters for every multiplier take under
a megabyte. 1M carries every multiplier in a download small enough for a phone
tether, and 30M has the deepest stop band of the ladder (−220.9 dB). The
rungs above it — 100M to 1G — make the wall steeper, not the floor deeper;
they come as separate [filter packs](#download), as do
the ×32 rates.

> **Comparing them is the point.** Unzip more than one bundle into the same
> folder: the filter files merge, one copy of the app serves all of them, and
> the tap slider then offers every size you have. Convert the same track at 5k
> and at 30M and listen to what a tap count actually buys.

<details>
<summary><b>Just the app, or just the filters</b> — a manual way, if you prefer files to the in-app download</summary>

<br>

| Asset | What it is | Size |
|---|---|---|
| [`aura-engine-v1.9.4-windows-x64.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.9.4/aura-engine-v1.9.4-windows-x64.zip) | The app alone. Needs filters from somewhere. | ~13 MB |
| [`aura-filters-5k-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-5k-all-rates.zip) | 5k taps, all 8 output rates, both phases | 645 KB |
| [`aura-filters-1M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1M-all-rates.zip) | 1M taps, all 8 output rates, both phases | 128 MB |
| [`aura-filters-5M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-5M-all-rates.zip) | 5M taps, all 8 output rates, both phases | 640 MB |
| [`aura-filters-10M-all-rates.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-10M-all-rates.zip) | 10M taps, all 8 output rates, both phases | 1.28 GB |
| [`aura-filters-30M-44k-family.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-30M-44k-family.zip) | 30M taps, 88.2 / 176.4 / 352.8 / 705.6 kHz | 1.92 GB |
| [`aura-filters-30M-48k-family.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-30M-48k-family.zip) | 30M taps, 96 / 192 / 384 / 768 kHz | 1.92 GB |

**New in 1.8.0 — ×32 and the rungs above 30M.** Same release, same layout
(extract beside `aura-engine.exe`); the packs above are unchanged.

| Asset | What it is | Size |
|---|---|---|
| [`aura-filters-5k-x32.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-5k-x32.zip) | 5k taps, the ×32 rates — 1411.2 and 1536 kHz, both phases | ~1 MB |
| [`aura-filters-1M-x32.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1M-x32.zip) | 1M taps, the ×32 rates — 1411.2 and 1536 kHz, both phases | 0.03 GB |
| [`aura-filters-5M-x32.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-5M-x32.zip) | 5M taps, the ×32 rates — 1411.2 and 1536 kHz, both phases | 0.16 GB |
| [`aura-filters-10M-x32.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-10M-x32.zip) | 10M taps, the ×32 rates — 1411.2 and 1536 kHz, both phases | 0.32 GB |
| [`aura-filters-30M-x32.zip`](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-30M-x32.zip) | 30M taps, the ×32 rates — 1411.2 and 1536 kHz, both phases | 0.96 GB |

| Rung | Pack: rate family and phase | Size |
|---|---|---|
| **100M** | [44k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-100M-44k-family-linear.zip) · [44k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-100M-44k-family-minimum.zip) · [48k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-100M-48k-family-linear.zip) · [48k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-100M-48k-family-minimum.zip) | 0.38 GB each |
| **180M** | [44k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-180M-44k-family-linear.zip) · [44k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-180M-44k-family-minimum.zip) · [48k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-180M-48k-family-linear.zip) · [48k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-180M-48k-family-minimum.zip) | 0.68 GB each |
| **300M** | [44k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-300M-44k-family-linear.zip) · [44k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-300M-44k-family-minimum.zip) · [48k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-300M-48k-family-linear.zip) · [48k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-300M-48k-family-minimum.zip) | 1.13 GB each |
| **500M** | [44k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-500M-44k-family-linear.zip) · [44k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-500M-44k-family-minimum.zip) · [48k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-500M-48k-family-linear.zip) · [48k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-500M-48k-family-minimum.zip) | 0.88 GB each |
| **1G** | [44k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1G-44k-family-linear.zip) · [44k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1G-44k-family-minimum.zip) · [48k linear](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1G-48k-family-linear.zip) · [48k minimum](https://github.com/ToxaDev/aura-engine/releases/download/v1.0.0/aura-filters-1G-48k-family-minimum.zip) | 1.75 GB each |

Each big-rung pack holds the branch-0 files (`.b0.npy`, `.b0.ends.npy`,
`.b0.json`) of one rung at every multiplier it is offered at — FS4, FS8, FS16
and FS32 for 100M–300M, FS8, FS16 and FS32 for 500M and 1G — for one rate
family (44k: 44.1 kHz sources, 48k: 48 kHz) and one phase. Linear phase is what
most people need; take the minimum-phase pack only for minimum-phase or
Hybrid-Phase use. The bundles carry none of these; the app can download them itself.

Filter packs contain only `.npy` blobs (and, for the big rungs, a `.json` beside each) in a `fir-optimizer/output/` folder —
extract one next to `aura-engine.exe` and it is found. `AURA_FILTER_DIR` points
the app at a folder anywhere else. The 30M blobs are split by rate family
because one archive for all of them would exceed GitHub's per-file limit.

Whatever you end up with, the app adapts: it scans for blobs at startup, opens
on the largest filter it found at FS8, strikes through the slider positions it
has nothing for, and names the exact download for the one you select.

</details>

## The player

Each row of the list has four buttons: **▶** plays the file, **C** converts it
with the rack as it is now, **M** gives the track a memory of its own, and **✕**
takes it off the list. The player — the strip above the list, with previous,
play/pause, stop, next, repeat, a seek bar, the output device, **BIT-PERFECT**,
the volume and the analyzer — runs the chain the file would be converted with:
the same filter, the same phase engine, the same rack and output stages. A
batch can run while it plays. A row that has been converted with the current
rack plays its file from disk, and its ▶ turns green. Measured on one chain
(30M taps, FS×8, TFS, XTC), the samples the player sends to the device and the
converted file differ by less than −120 dB once every stage is in.

<p align="center">
<img src="docs/media/aura-player-bars.png" width="400" alt="The big player in listening mode with the scene Bars: the spectrum as bars with falling peak caps"/>
<img src="docs/media/aura-player-speaker.png" width="400" alt="The same with the scene Speaker: one woofer seen close, its cone moving with the bass and the kicks"/>
</p>

**Nothing cuts the sound.** A seek keeps the old place playing until the new
one is ready; the sound then fades out and back in over about 250 ms — so
do pause, Stop and a change in the rack, with no click. The next track is prepared while the
current one plays and follows it without a gap; when it belongs to the other
rate family (44.1 against 48 kHz), the only pause is the device reopening at
the new rate, about 0.6 s. A change in the rack goes out 1.5 s after the last
click, so ten quick clicks make one rebuild, and the new chain is built while
the old one plays.

**Starts are fast.** Since 1.7.0 the next track in the list is prepared whole
while the current one plays — when 2 GB of memory are free; it holds about
370–630 MB at ×4 and lets go when memory runs low — and starts in 62 ms
instead of 260. The whole-track analysis is kept on disk (up to 256 MB) and
the list is analysed ahead in the background, so an analysed track at ×4
starts in 609 ms instead of 937, a clipped one in 1.42 s instead of 2.65, a
converted file in 0.36 s instead of 1.23, and a seek right after a start takes
463 ms instead of 657. What you hear is the same to the bit.

**What is in the sound, and when.** Some stages need the whole track before
they can decide anything. By default the sound starts at once with most of the
rack already in it — Declip, ISP, the Subsonic Filter and Adaptive Headroom run
over the track as it is decoded, ahead of what you hear — and the badges under
the player light as the rest switch in, while a thin bar beneath them fills
until everything is on. **Instant start**, in the player's right-click menu,
is the switch: off, nothing plays until every stage is ready, and the first
sound after a start, a seek, a new track or a rack change already has all of
them.

**On the graphics card.** The player measures, on your machine and for the
chosen filter, whether the CPU can convolve in real time with room to spare,
and moves the convolution to a discrete graphics card when it cannot. With
multistage upsampling (1.7.0) that is rare: a 30M filter at ×4 takes about a
quarter of one core. On the card 30M ×8 takes about 170 MiB of video memory,
and the result matches the CPU's to below −280 dBFS. If neither can keep up,
the tap count steps down once for that track, and the taps chip says so.

**The output.** The device is opened in WASAPI exclusive mode at the output
rate. What goes to it is checked after the volume control, and a block more
than 12 dB louder than the volume allows is replaced by silence.
**BIT-PERFECT** sends the decoded file instead — at its own rate and depth (a
lossy one as the widest integer format the device takes), without the rack,
the volume or dither — and switches on and off while a track plays. A DSF or
DFF file in BIT-PERFECT goes out as it is, over DoP — see [DSD](#dsd).

**The top bar.** The output format is a dropdown in one housing with the
device button: **PCM**, **DSD64**, **DSD128** or **DSD256**; open, it names the
modulator of each DSD entry, marks a heavy one *· high CPU* and one this PC
cannot run *· too heavy for this PC*, and the keyboard works in it. Tips show
above every popover. The stage badges have one style everywhere — the player
line, the big player over the cover, the list.

**CPU | GPU load.** A faint *CPU n · GPU n · RAM n* badge beside the version shows how
much of the machine the app is using, every second: CPU is its CPU time
across all logical cores, GPU the time the card is busy with AuraEngine's own
work (an upper bound; the spatial scenes' ONNX models and the window's drawing
are not counted), RAM the memory the app holds. Click it for a window with the last hour — 10 minutes,
30 minutes or 1 hour — with playing and converting marked in colour.

| Control | What it does |
|---|---|
| **F / R / I** | One narrow button at the head of the list: it shows the letter of the current view (your files, internet radio or the ASIO input) and three pips below. A click goes to the next view, a right-click or Shift+click to the one before, the arrow keys step. What is playing goes on; the pip of another view breathes while something plays there. |
| **▶ C M ✕** on a row | Play the file through the rack, convert it, give it a memory, take it off the list. A double-click plays it too. |
| **Convert N** | Converts every row not yet converted with the current rack; N is how many that is. Its arrow (**FLAC ▾**) picks the output — FLAC, or DSF from DSD1024 down to DSD64, with the modulators of each rate — the same setting as the rack's Output cell. It fills as the batch runs and ends with `✓ ×N`, how many times faster than real time it went; its tooltip has the audio length and the first twelve files by name. Hold it for two seconds to cancel. A finished row shows its own ×N beside its ✓. Changing the DSF rate or modulator makes the rows converted before count as not converted again. |
| **M** | Track memory: the rack, Headroom and Apodizing for this file — Declip, ISP, the Subsonic Filter and its corner, Adaptive Headroom, the Adaptive Apodizer, Hybrid-Phase, Continuous Alpha, TFS, XTC and its geometry. They come back when the track plays or converts. FS multiplier, resolution and GPU stay global. |
| **BIT-PERFECT** | The decoded file goes to the device untouched, without the rack or the volume. Switches while a track plays; works on radio too. |
| **Badges under the player** | One per stage of the rack. Lit means it is in the sound now; a thin bar under them fills while the rack switches in and goes when every stage is on. Chips beside them name the taps playing, the graphics card, M, and DISK for a converted file played from disk. |
| **Instant start** *(right-click menu)* | On (default): the sound starts at once and the stages switch in as they get ready. Off: every stage is prepared first. |
| **L** | Listening mode: the settings fold away, and the player and the list take the window. When the pointer leaves a playing player, the album cover shows with the artist, the album and the year. |
| **Ctrl+Shift+A** | The live analyzer: what you hear now. A row's right-click menu opens it on the file, or on its converted copy. |
| **Visualization** *(right-click menu in listening mode)* | Scenes by name, the album cover, or off; Spectrum bars, Full screen, Picture delay (per output device), and the Studio for writing your own scene. |
| **?** *(below the version number)* | A short tour of the window, zone by zone. It is offered once, at first start. |
| **Format** *(beside the device button)* | PCM, or DSD64 / DSD128 / DSD256 over DoP with the modulator in brackets. *Re-check this PC…* checks again what this machine can run. |
| **CPU n · GPU n · RAM n** *(beside the version)* | The app's own load and memory, every second; click for the last hour. |

How a chain is prepared, what each badge state means and how the graphics card
is chosen: [docs/19 — The Player and Internet Radio](docs/19-player-and-radio.md).

## DSD

From 1.7.0 the player sends DSD to a DAC that takes it. Pick the device in
exclusive mode, open the format dropdown beside it and choose **DSD64**,
**DSD128** or **DSD256**. The chain runs as for PCM, up to the DoP carrier
rate (176.4, 352.8 or 705.6 kHz), and a sigma-delta modulator turns it into
the DSD stream, which goes out as **DoP** — DSD packed into 24-bit PCM —
through WASAPI exclusive. Your DAC must accept DoP at that rate.

**Which modulator.** The list is ordered best first and opens on the best one
this PC can run; leave it there unless the machine is busy. Noise plus
distortion in the 20 kHz band for a −6 dB tone:

| Modulator | DSD64 | DSD128 |
|---|---|---|
| SDM7 LA+ *(look-ahead, deeper)* | −158.8 dB | −180.2 dB |
| SDM7 LA *(look-ahead)* | −151.1 dB | −172.7 dB |
| SDM7 | −138.9 dB | −160 dB |
| SDM5 | −121 dB | −138 dB |

A modulator marked *· high CPU* runs but takes a large share of a core; one
marked *· too heavy for this PC* is greyed out with the reason. If the PC
falls behind while playing, the encoder steps down to a lighter modulator and
says so. As rough guidance, SDM7 LA at DSD64 takes about a tenth of one core
per channel.

**DSD files.** DSF and DFF play through the chain like any other file. In
BIT-PERFECT they go out as they are, over DoP at their own rate, bytes
untouched, and start at once. The converter writes **DSF from DSD64 to
DSD1024** with any modulator, including the slow *Trellis* kinds the player
cannot run.

**Limits.** Only WASAPI exclusive and DoP — no ASIO, no native DSD; the player
goes up to DSD256 (DSD512 and faster files play as PCM); no DFF export.
Verified on a Chord Mojo 2. Everything else, with the full modulator table:
[docs/20 — DSD](docs/20-dsd.md).

## Internet radio

<img src="docs/media/shot-radio.webp" align="right" width="300" alt="The radio in 1.9.2: Featured, Favorites, Recent and Search above the station list — Radio Paradise, Rondo Classic, FIP"/>

**F / R** at the head of the list switches it to the radio: featured stations,
your favorites and the ones you played recently, a search of the
[Radio Browser](https://www.radio-browser.info/) catalog — tens of thousands of
stations by name, genre, country, language and format, sorted by name, votes,
plays or quality, and filtered down to lossless or a minimum bitrate — and a
field for any stream's address. On a stream the player keeps play and stop
only — another station is a click in the list — and the title of what is
playing changes when you hear the new song, not when the server announces it.

It plays MP3, AAC and HE-AAC, FLAC (also in Ogg), Vorbis and Opus, from plain
HTTP streams, .pls and .m3u playlists and HLS. HE-AAC and Opus are decoded
inside the program, so they play on every edition of Windows. Streams at 44.1
or 48 kHz and their multiples are upsampled; an encrypted HLS stream does not
play. A service that does not allow third-party players is left out of the
catalog.

A live stream has no whole track, so the two stages that need one stand down:
Declip and the Adaptive Apodizer are grey on a stream. Everything else runs as
the stream arrives — DC removal (as a 2 Hz high-pass), ISP, the Subsonic
Filter, the phase engine (TFS, Hybrid-Phase and Continuous Alpha from the first
sound), XTC and the output limiter — and Adaptive Headroom decides on the
stream's first ten seconds. **Long filters do not make a stream wait.** A
linear-phase filter needs half its length of signal ahead of it — 42 s at 30M
taps and FS×8, which no broadcast can give — so a stream plays a version of
the same filter that looks 50 ms ahead: the same magnitude and stopband, linear
phase up to 20 kHz and minimum phase in the last kilohertz under the wall. It
is derived from the shipped filters the first time a length plays on a stream,
and kept beside them.

When the connection drops, the player reconnects, finds where the new
connection repeats what it already has, and skips the repeat, so the music goes
on without a seam. The buffer starts at 8 seconds and grows when the network
leaves it empty. The server's clock and the DAC's never quite agree; the player
keeps the buffer within a second of where it should be by dropping or repeating
a few milliseconds in a quiet place, with nothing resampled. **BIT-PERFECT**
works on a stream too: a FLAC station reaches the DAC at its own depth, a lossy
one as the widest integer format the device takes.

Upsampling cannot add what the codec removed: a 128 kbps MP3 stream stays a
128 kbps MP3, played through a better reconstruction filter. Stations that
stream FLAC are where the rack has the most to work with. Details:
[docs/19 §2](docs/19-player-and-radio.md#2-internet-radio).

## The ASIO input

<img src="docs/media/aura-input-listening.png" align="right" width="300" alt="The input view: VBCABLE_AsioBridge playing 48 kHz, 32-bit, 2 channels into Aura Player, with the switch to the input and Remove driver below"/>

**F / R / I** — the view button at the head of the list has a third view. **I** is the
input: **Aura Player ASIO**, a virtual ASIO device. In any player that can send
its sound to an ASIO device — foobar2000, JRiver, HQPlayer, Roon — choose it as
the output, press play, and the music goes through Aura Player's rack to your
DAC, at the output rate and with BIT-PERFECT if you switch it on. Windows only.

The input view is one card, from what is happening:

- **Install driver** — the driver is installed once, with one elevated command
  (Windows asks for permission). **Update driver** appears when this Aura Player
  brings a newer one; **Remove driver** is at the foot of the view.
- **Waiting for a player** — the card says where to choose the device in
  foobar2000, JRiver, HQPlayer and Roon.
- **Listen** — a player is playing and Aura Player is not listening to it yet.
  The switch **Switch to the input when a player starts playing** does it for you.
- **Listening** — the player's name, what comes in (`48 kHz · 32-bit · 2 ch`),
  *Playing* or *Paused*; the strip says the player's name and INPUT, and the
  rate the rack plays it at.

The driver takes two channels at 44.1 to 768 kHz and works with 64- and 32-bit
players. A change of rate in the player is followed. The sound comes out of
Aura Player about 2.3–2.6 s after the player that plays into it (1.5 s without
the Subsonic Filter), so a pause or a seek in the player is heard that much later.

<img src="docs/media/aura-input-waiting.png" align="right" width="300" alt="The input view when a player plays but Aura Player is not listening: 'VBCABLE_AsioBridge is playing — Aura Player isn't listening to it', with the Listen button"/>

### Spotify, or any Windows program

A program with no ASIO output of its own can reach the input through two
third-party programs from VB-Audio, a *Hi-Fi Cable* and an *ASIO Bridge*
([VB-Audio Hi-Fi Cable & ASIO Bridge, download](https://vb-audio.com/Cable/index.htm#DownloadASIOBridge) —
donationware, Windows; they are not part of AuraEngine, and what they do and
how they behave is theirs). The way it runs:

1. Install it: run the setup as administrator, then restart Windows.
2. Install the driver in Aura Player's **I** view, if you have not.
3. In Windows sound settings, or in the program (Spotify and others), play to
   **Hi-Fi Cable Input**.
4. In ASIO Bridge, choose **Aura Player ASIO** and press **ASIO ON**.
5. Play. The view shows *VBCABLE_AsioBridge*; press **Listen** if it does not
   switch by itself. Aura Player picks the sound up there and plays it through
   the rack.

<img src="docs/media/aura-asio-bridge.png" width="460" alt="VB-Audio ASIO Bridge's control panel: input 48000 Hz 24-bit, ASIO device Aura Player ASIO, buffer 2048"/>

The panel above is one working setup: the Cable's input at 48 kHz and a buffer of
2048. Spotify → Hi-Fi Cable (48 kHz) → ASIO Bridge (32-bit) → Aura Player ASIO
→ the rack → the DAC is the chain this was heard on.

The driver is licensed GPL-3.0-only, the protocol between it and Aura Player
MIT; the player stays under PolyForm Noncommercial. Details:
[docs/19 §6](docs/19-player-and-radio.md#6-the-asio-input).

## The analyzer

A window of its own. **Ctrl+Shift+A** opens it on what you hear now; a row's
right-click menu opens it on the file as it is, or on its converted copy. It
measures three signals side by side — **S**, the source; **B**, the source
after the rack's repair stages, before the filter; **O**, the output of the
whole chain, from a pass over the entire track — with integrated, short-term
and momentary loudness, LRA, true peak (BS.1770 and the engine's own 4×
reading), DR, RMS, clipping runs and overs, DC offset, and what lies above and
below the audio band.

![The analyzer on a radio stream: a FLAC station as it came in (S, above) and the output (O, below) on one spectrogram, following the live edge, with the numbers of the song playing and of the session](docs/media/aura-analyzer.png)

![The analyzer on a file in 1.9.2: live loudness, true peak, PLR and DR, and the spectrum with the filter's own response (Kaiser) drawn over it](docs/media/shot-analyzer.webp)

The spectrogram — a 4096-point FFT at 44.1/48 kHz with a Kaiser window, 512
logarithmic bands from 5 Hz to Nyquist (below about 40 Hz from a 3 s window),
three resolutions blended; Log, Lin, Mel and Bark scales, four colour maps —
compares the signals directly: S | O, O − S (blue quieter, red louder) and
B − S, with a readout in Hz, note name, level and time. Shift-drag on it
selects a stretch, and the table measures that stretch alone. The spectrum, the
loudness history, the waveform (down to single samples, following the playback,
with the peaks of S and O read under the pointer), a histogram and a stereo
view with a vectorscope and correlation have tabs of their own. The numbers
copy as text in the formats forums use for DR and true-peak reports, or save as
TSV; the loudness history saves as CSV, and any view as PNG. The whole-track
passes run only while the analyzer is open, so playback without it costs no
extra CPU. On a radio stream it measures the stream as it came in (S) and the
output (O) live, totals them for the song playing and for the session (↺ starts
it afresh), lists the songs heard, and shows how far the stream's spectrum
really reaches — an MP3 at 128 kbps ends near 16 kHz, a lossless stream reaches
half its sample rate. Its views keep what has played — about three minutes of
the spectrogram, 30 minutes of the loudness and of the waveform, with a minimap
— and follow the live edge until you zoom in or look back; the histogram and
the stereo view give the song's and the session's, and the stream's loudness
saves as CSV. In BIT-PERFECT one set of numbers stands for both: the output is
the source.

## Visualizations

In listening mode the player can draw the music. A scene is a GLSL shader in an
`.aura-vis` file: 19 come with the app, and the **studio** (Visualization ›
Studio…) edits them, tunes their settings with sliders and saves new ones. It
takes Shadertoy's multipass shaders — one pasted in keeps its author, link and
licence — with the album cover as one of the inputs. Because the player renders
a few seconds ahead of what you hear, a scene can see what is coming: it can
show a kick on its way and have it land as it sounds. How to write one is set
out in the specification that comes with the app,
[`AURA-VIS-SPEC.md`](desktop-app/src/vis/AURA-VIS-SPEC.md).

Most scenes draw the whole mix — its spectrum, beats and loudness. Three, the
ones **with instruments** (Stage, Lineup, Diamond Dust Stage), draw each
instrument of the song on its own: the kit piece by piece, the bass, the voice
and the rest, with their hits and notes. They need a pack of models the app
does not carry — 282 MB, downloaded once when such a scene is first chosen,
and checked file by file before anything in it is used — and each track is
analysed once, in the background, and kept; on an RTX 4090 a four-minute track
takes 15–25 seconds. The separation wants a graphics card with 3 GB of video
memory free; without one the CPU does it, and the drum kit is then told apart
only by its bands. On a radio stream the instruments are found as the stream
plays: it is taken apart on the graphics card a little ahead of what you hear,
so they come a few seconds after a station starts, and each song's picture
gains instruments as they join in. The kit comes piece by piece — the toms and
the cymbals once they play — where the card has room for the drum network
beside the separation (about 1.5 GB more of free memory); otherwise it is kick,
snare and hi-hat taken from the drums' sound. Every instrument but the drums
has its notes about two seconds before it sounds, and on a file and a stream
alike it flashes on its strong notes. At first the notes of the guitars, the
keys and the rest go to their places by pan; once their source has played some
forty strong notes in the song, it is split into its instruments by their
notes, as on a file, and each place is taken over by the instrument nearest to
it, which keeps its slot and changes its colour once. Until the instruments are
there, the scene draws the mix and says so ("Separating instruments…"); without
a graphics card, or without enough of its memory free, it shows the mix and
says why.

<p align="center">
<img src="docs/media/aura-scene-lineup.png" width="400" alt="Lineup on a radio stream: each instrument of the song a light of its own, left to right as it sits in the mix"/>
<img src="docs/media/aura-scene-dust.png" width="400" alt="Diamond Dust Stage on the same stream"/>
</p>

**Picture delay**, per output device, holds the bars and the scenes back by the
time a DAC's own filter or active speakers add and Windows does not report.

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
- 1 ms fade on a smooth curve + 20 ms anti-chatter hold;
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
    A["Decode<br/><i>Symphonia · FDK AAC for HE-AAC · f64, no clamp</i>"] --> B["DC block<br/><i>static mean or 2 Hz IIR</i>"]
    B --> C["Declip · ISP (optional)<br/><i>flattened peaks rebuilt ·<br/>intersample overs corrected</i>"]
    C --> S["Subsonic filter (optional)<br/><i>linear-phase FIR · 20 / 15 / 10 Hz</i>"]
    S --> D["Apodizer (optional)<br/><i>source forensics: measured ring cutoff ·<br/>fake-hi-res unmasking · min-phase Kaiser</i>"]
    D --> E{Path}
    E -->|Standard| F["Rubato sinc resampler<br/><i>512-tap sinc · ~−180 dB</i>"]
    F --> G["FIR post-filter<br/><i>5k–1G taps · partitioned OLS ·<br/>CPU f64+Kahan or GPU DS</i>"]
    E -->|"Polyphase FIR<br/>(integer ratio)"| H["Multistage interpolation (default, 5M+)<br/><i>the long filter at the source rate ·<br/>short exact ×2…×32 interpolator, ≥210 dB<br/>(1M, custom filters, ×3/×6: single-stage polyphase)</i>"]
    G --> I["Phase engine (optional)<br/><i>Hybrid-Phase · Continuous Alpha ·<br/>TFS (polyphase path)</i>"]
    H --> I
    I --> X["XTC (optional)<br/><i>crosstalk cancellation · speakers only</i>"]
    X --> L["Output limiter · subsonic guard<br/><i>~1.5 ms dips at overs ·<br/>second subsonic pass</i>"]
    L --> J["True-peak ceiling<br/><i>4× Lanczos-4 · −0.5 dBTP or Headroom ·<br/>one gain per album with Album Level</i>"]
    J --> K["Dither<br/><i>24-bit TPDF · noise shaped:<br/>Wannamaker-9 ≤48 kHz, per-rate shaper above</i>"]
    K --> M["FLAC encode<br/><i>native · 24-bit · streaming</i>"]
    M --> V["Bit-perfect verification<br/><i>re-decode · compare ±2 LSB</i>"]
    K -.->|"DSD output / DSF"| Q["Sigma-delta modulator<br/><i>SDM5/7/9 · look-ahead ·<br/>DoP to the DAC or DSF file</i>"]
```

Two conversion paths share the same preparation and output stages:

| | Standard path | Polyphase FIR path |
|---|---|---|
| Resampler | Rubato `SincFixedIn` (512-tap sinc), then the big FIR as a post-filter | The big FIR **is** the resampler. Since 1.7.0, from 5M taps up, it runs once at the source rate and a short exact interpolator (≥ 210 dB) finishes the ratio — [multistage](docs/21-multistage-upsampling.md); 1M, custom filters and ×3/×6 run as L sub-filters in parallel |
| Ratios | Any | Integer only (non-integer targets snap down: 44.1 kHz → FS8 gives 352.8 kHz) |
| Trailing padding | ~0.4 s of resampler zero-pad | None — output length is exactly input × L |
| TFS Phase | Stands down, and the row says so | Runs |
| Filter blobs missing | Post-filter skipped with a warning | Hard error (by design — no silent quality downgrade) |

The player runs this chain live: the source stages prepared ahead over the
whole track, then the polyphase FIR (multistage), the phase engine and the
output stages as it plays — and, for DSD output, the modulator after them. A detailed, beautifully rendered walkthrough of every stage lives at
**[aura-engine.app/docs.html](https://aura-engine.app/docs.html#path)**
(source: [docs/docs.html](docs/docs.html)), and the same material as plain
markdown starts at **[docs/README.md](docs/README.md)**. The engineering laws the DSP core is
audited against are in **[DSP_MANIFESTO.md](DSP_MANIFESTO.md)**.

## Build from source (Windows)

> Only needed if you want to change the engine. To *use* it, take a
> [bundle](#download) — there is nothing to install and nothing to compile.

> AuraEngine is developed and tested on **Windows 11** (Windows 10 should
> work but is untested). Linux/macOS are currently not supported — the build
> uses the MSVC toolchain and Win32 APIs for audio output (WASAPI), the
> video-memory budget, thread priority and timer resolution.

### 1. Prerequisites

| Requirement | Why | Notes |
|---|---|---|
| [Rust](https://rustup.rs/) 1.88 or newer (MSVC) | builds the app | fat LTO |
| [CMake](https://cmake.org/) 3.16 or newer | builds libopus from source | for the radio's Opus streams |
| [Python 3.10+](https://www.python.org/) with `numpy scipy mpmath soundfile` | generates the FIR filters | one-time step |
| WebView2 runtime | Tauri UI | ships with Windows 11 |
| Vulkan-capable GPU *(optional)* | GPU DS convolution path | falls back to CPU automatically — no adapter, or out of video memory; playback uses a discrete card only |

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
5 tap sizes × 8 output rates × 2 phase types = 80 files, roughly **6 GB**,
and it can take a while for the 30M presets. (The ×32 rates and the rungs from
100M to 1G are not part of it: they come as packs, or from
`fir-optimizer/gen_branch0.py` — [docs/12 §11](docs/12-precomputed-fir-matrix.md#branch0).) It skips files that already
exist, so you can interrupt and resume. If you only care about one preset,
see [fir-optimizer/README.md](fir-optimizer/README.md) for generating a
subset. Store the blobs anywhere by setting the `AURA_FILTER_DIR`
environment variable to the folder that contains them.

Whatever subset you end up with, the app works out what it has at startup:
it opens on the largest tap count it found at FS8, strikes through the
slider positions with no blob behind them, and names the download for one
you select anyway. [`tools/make-bundles.ps1`](tools/make-bundles.ps1) builds
the app-plus-filters packages that appear on the release page.

### 4. Convert, or play

1. Launch the app (`start.bat`).
2. In the strip at the top of the window, set the **FS Multiplier** (FS2–FS32)
   and the **Resolution** (5k–1G taps).
3. Optionally switch stages on or off in **Advanced DSP**, below the list —
   Subsonic Filter, Adaptive Apodizer, Polyphase FIR Resampling, Hybrid-Phase,
   XTC, Album Level and the rest — or **GPU** in the strip.
4. Drop files onto the window — they join the list. **C** on a row converts
   it, **Convert N** converts every row not converted yet, and **▶** plays a
   row through the same chain.
5. The output FLAC appears **next to the source file**, named like:

```
Track [AE · 44.1k→352.8k · Kaiser 10M · f64 · SUB15·AA·PFR·TFS].flac
```

The bracket lists the stages that ran, in the order they ran. A
`✓ VERIFIED` badge means the written FLAC was re-decoded and matched the
internal DSP buffer within ±2 LSB. A console window runs alongside the UI on
purpose — it is the engine's full audit log (filter resolution, hybrid-phase
coverage, true-peak decisions, verification results). It starts minimized;
the taskbar brings it up.

## Controls reference

The five main settings — FS Multiplier, Resolution, GPU, Apodizing,
Headroom — sit in a strip at the top of the window; a click on a cell opens
its control, and GPU switches at once. Advanced DSP is below the list.

| Control | What it does |
|---|---|
| **FS Multiplier** (FS2/4/8/16/32) | Output rate = source family base × multiplier. 44.1 kHz family → 88.2/176.4/352.8/705.6/1411.2 kHz; 48 kHz family → 96/192/384/768/1536 kHz. FS32 (1.8.0) is for the player and for DSD output in the converter — a FLAC cannot hold it, so the converter refuses FLAC there. A source above 48 kHz keeps what it holds above the CD band: 96 kHz at FS8 is upsampled ×4, with the filter made for ×4. |
| **Resolution** (5k/1M/5M/10M/30M/100M/180M/300M/500M/1G) | Tap count of the main FIR, counted at the output rate. 100M–300M are offered from FS4 up, 500M and 1G from FS8 up; the app never moves to them by itself. More taps → a narrower transition band, at the cost of compute time (since 1.7.0 much less of it from 5M up — multistage). The stopband is −185 dB or deeper at every size — the 5k window is fitted to its length — so what length buys is how close to the cutoff the response stays flat. A struck-through mark means this build has no filter file for that size; selecting it names the download that would add it. |
| **Custom filter (.npy)** | In the Resolution cell. Load your own 1-D float64 coefficient file instead of the built-in matrix. |
| **GPU** (Hardware GPU Acceleration) | Runs convolution on Vulkan compute in DS precision. Automatically falls back to CPU (f64) when the adapter lacks `SPIRV_SHADER_PASSTHROUGH` (e.g. DX12-only), and when the card runs out of memory during a batch — per phase on the polyphase path, so the phases already on the GPU stay there. The same switch covers playback, which uses a discrete card, and only when the CPU cannot keep up. |
| **Apodizing** (Off/Gentle/Moderate/Strong) | Static minimum-phase corrective lowpass at 20/19/18 kHz for CD-era sources. Locked off while the Adaptive Apodizer is on. |
| **Headroom** (Off/−0.5/−1/−3 dB) | The true-peak ceiling the finished render is normalised to. `Off` keeps the shipped −0.5 dBTP; −3 dB puts the output peak on −3.0 dBTP. A file already below it is left alone. Opens on −0.5 dB. |
| **Declip** | On by default. Rebuilds peaks a master flattened at full scale, by constrained AR interpolation with the rail as a lower bound. Stands down on runs shorter than 17 samples — that is a limiter, not a clipper — and, since 1.5.0, on MP3, AAC, Vorbis and Opus sources, whose overs are the codec's (tag `DC`). |
| **Intersample Peak Correction** | On by default. Finds true-peak overs above 0 dBTP with a 4× Lanczos scan and applies the smallest correction that clears them, skipping spans Declip has just rebuilt. At the end of the chain a limiter with ~1.5 ms dips trims the overs the reconstruction itself makes, so the whole file doesn't come down for a few milliseconds of peaks (tag `ISP`). |
| **Subsonic Filter** (20/15/10 Hz) | On by default, opening on 15 Hz. A linear-phase FIR high-pass at the source rate, running after the source repairs so a high-pass cannot tilt the flat tops they read: flat within 0.01 dB from the corner up, at least 100 dB down from half the corner to DC. Click the chip on the row to change the corner. It is there for speakers — infrasound and slow drift a disc can carry, which removing the DC offset does not touch — not as a sound improvement (tag `SUB20` / `SUB15` / `SUB10`). With Hybrid-Phase on, the switch points follow the filtered signal. Since 1.3.3 it runs a second time at the output rate, after the ISP output limiter, whose gain dips would otherwise leave a shelf of infrasound about 40 dB under the bass. |
| **Adaptive Headroom** | On by default. Decides whether to honour the Headroom setting on this file: keeps the shipped −0.5 dBTP when the source peak already sits below the target, or when ENOB says the master is a clean high-bit-depth one. Needs Headroom set to something other than `Off` — with nothing requested there is nothing to decide (tag `AHR`). |
| **Adaptive Apodizer** | Per-track source forensics: detects pre-ring and measures its exact frequency, unmasks fake hi-res via spectral-cliff detection and a mirror-image alias probe, and applies a corrective filter only on real evidence. Since 1.5.0 it stands down where its cutoff would sit at or above a lossy codec's wall, where it could only turn the phase (tag `AA`). |
| **Polyphase FIR Resampling** | On by default. The direct path: FIR-as-resampler at integer ratios, exact output length (tag `PFR`). Since 1.7.0 it is multistage from 5M taps up — the long filter at the source rate, a short exact interpolator for the rest; several times less CPU, the same sound (difference −204…−220 dBFS); the rungs of 100M and more play only this way. 1M, custom filters and ×3/×6 stay single-stage. For the old single-stage path everywhere: `"upsampler": "single"` in the settings file, or `AURA_MULTISTAGE=0` — [docs/21](docs/21-multistage-upsampling.md). |
| **Hybrid-Phase Blending** | Off by default. Dual linear+minimum-phase convolution with transient-driven switching (tag `HP`, ~2× processing time). Cannot run together with TFS Phase. |
| **Continuous Alpha** | Off by default. Replaces Hybrid-Phase's hard switch with a per-sample crossfade driven by the HPSS envelope. Needs Hybrid-Phase (tag `aHP`). |
| **TFS Phase** | On by default. Linear phase below 1.5 kHz, minimum phase above 4 kHz, blended between; magnitude matches the linear-phase render to ±0.000003 dB. Derived from the linear + minimum pair on first use and cached — at 30M taps about 12 s and 4.5 GB of RAM, once. Since 1.5.0 it is derived as designed (stopband −193 to −231 dB; see the CHANGELOG). Replaces Hybrid-Phase for the file (tag `TFS`). |
| **Crosstalk Cancellation** | Off by default, and the one stage that deliberately colours the signal. Cancels what each speaker sends to the wrong ear, built from a listening triangle you measure. Will not switch on until that geometry is entered. Speakers only, never headphones (tag `XTC`). |
| **Album Level** | On by default. The queued files that share a folder **and** an Album tag are converted as one album: before the first one is written every track is prepared and its true-peak cut estimated, and each takes the deepest of them, so the album keeps the balance it was mastered with. A file without the tag, or the only queued track of its album, converts exactly as before. The queue row shows `ALB`; no filename token. The player plays each track at its own level. |

The rack these live in is three zones — Source, Conversion, Output — with the
stages in each standing in the order the pipeline runs them. Each stage is
described, with measurements, in
[docs/18 — The Advanced DSP Stages](docs/18-dsp-stages.md).

Sources at or below 48 kHz get the full treatment. Hi-res containers skip the
static apodizing presets, but the Adaptive Apodizer analyzes them too: if a
"hi-res" file is really an upsampled 44.1/48 kHz master, the baked-in brickwall
is detected and treated against the *original* Nyquist. Input formats: WAV,
FLAC, MP3, OGG, AAC, M4A (HE-AAC included), DSF and DFF. Output is 24-bit FLAC,
or DSF, DSD64 to DSD1024.
Files with non-standard rates are rejected (`BAD`), files already at or above
the target are skipped (`SKIP`).

## How the quality claims are enforced

This project treats sound-quality claims as **testable invariants**, not
marketing. The rules live in [DSP_MANIFESTO.md](DSP_MANIFESTO.md); the
mechanics, briefly:

- **Unity gain**: every filter is DC-normalized (`sum(h) == 1.0`) at design
  time; the converter never changes loudness unless true-peak protection has
  to act.
- **f64 everywhere**: decode writes f64 directly — integer PCM exactly, and
  lossy decoders' samples above full scale kept rather than clipped; there is
  no f32 truncation anywhere in the CPU sample path. The GPU path uses double-single
  f32 pairs (~48-bit mantissa) specifically because plain f32 would not meet
  the noise floor. A radio stream is f64 from the decoder to the filter.
- **Latency-exact alignment**: OLS convolver latency (2 blocks CPU, 1 block
  GPU) and FIR group delay are trimmed analytically — tested by unit tests
  (`cargo test`), not tuned by ear.
- **Bit-perfect verification**: every output file is re-decoded and compared
  against the DSP buffer. More than 900 unit tests cover convolver latency,
  unity gain, polyphase reconstruction, phase alignment, true-peak and dither
  behaviour — and hold the player's streaming stages to the converter's
  whole-file code.

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
| [User documentation (website)](https://aura-engine.app/docs.html) | Installing on Windows, Linux and macOS, every converter setting, the player, the ASIO input, filters, FAQ and troubleshooting |
| [The Signal Path (website)](https://aura-engine.app/docs.html#path) | The full signal path, visually — every stage with its parameters and rationale |
| [docs/20-dsd.md](docs/20-dsd.md) | DSD output over DoP, the modulators and their measured noise, DSF/DFF files, DSF export, limits |
| [docs/21-multistage-upsampling.md](docs/21-multistage-upsampling.md) | How multistage upsampling works, its accuracy against single-stage, CPU and GPU savings, how to switch back |
| [docs/19-player-and-radio.md](docs/19-player-and-radio.md) | The player, internet radio, the analyzer and the visualizations — how the conversion chain plays live, and what a stream can and cannot run |
| [docs/18-dsp-stages.md](docs/18-dsp-stages.md) | Every stage in the Advanced DSP rack — declip, intersample peaks, adaptive headroom, TFS phase, crosstalk cancellation, album level — with the measurements behind each |
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

**Is a billion-tap filter better than 30M?**
Not by depth. The worst stop band (44.1 kHz × 8, linear phase) is −191 dB at
1M, −205 at 5M, −211 at 10M, **−220.9 dB at 30M**, and then −216 dB at 100M,
−211 at 180M and −207 at 300M: 30M already sits at the floor of double-precision
arithmetic, which is why the slider marks it *optimal*. What 100M to 1G buy is
a steeper wall at the cutoff, at the price of a download of up to 1.75 GB per
pack and of time. They are there for those who want that wall, not a deeper
floor. 1G at FS8 is a 47-minute kernel; at FS32, under 12 minutes —
[docs/12 §11](docs/12-precomputed-fir-matrix.md#branch0).

**Why will the converter not write FLAC at ×32?**
A FLAC header stores the sample rate in 20 bits — at most 1 048 575 Hz — and
×32 is 1411.2 or 1536 kHz. The converter says so before it starts. ×32 works
for the player (if the device takes the rate) and for DSD256 and faster in the
converter, where the chain runs at ×32 and the result is a DSF.

**Offline conversion or real-time playback?**
Both, through one chain — see
[One engine, two ways to use it](#one-engine-two-ways-to-use-it).

**Which radio stations can it play?**
Any stream at 44.1 or 48 kHz (or a multiple) in MP3, AAC, HE-AAC, FLAC,
Vorbis or Opus — over plain HTTP, a .pls or .m3u playlist, or HLS. Encrypted
HLS does not play. Upsampling cannot add what the codec removed: a 128 kbps
MP3 stream stays a 128 kbps MP3, played through a better reconstruction
filter. Stations that stream FLAC are where the rack has the most to work
with.

**Do I need a powerful PC or a graphics card?**
No. Pinned to four efficiency cores of an i9-14900K — a stand-in for a modest
laptop — the player runs a 30M-tap filter at ×8 on 27 ms of CPU per second of
music, about 35 times faster than real time, with no graphics card. Any filter
also converts on any machine; a long one just takes longer —
[performance on modest hardware](#performance-on-modest-hardware).

**Do I have to wait for the conversion before I can listen?**
No. ▶ plays the source through the same chain at once: on those four
efficiency cores the first sound of a 30M track is ready 1.2–1.5 s after the
press (0.4–0.6 s on all cores), and the stages that need the whole track switch
in as they get ready. Converting is for keeping a file, not for listening.

**Is multistage (cascade) upsampling lossy?**
It is not bit-identical, but the difference to the single-stage output of the
same 30M filter is −204 to −220 dBFS, about 66 dB below the last bit of a
24-bit file; the wall, the phase and the delay are unchanged, and in a blind
A/B no difference was heard — [docs/21](docs/21-multistage-upsampling.md).

**Do I really need the GPU?**
No, and since 1.7.0 even less than before. The CPU path is the reference
implementation (f64, Kahan-compensated), and multistage upsampling cut its
cost several times with 5M filters and longer: a 3:43 track at 30M ×4
converts in 13 s instead of 23 on the CPU, and the player runs 30M ×4 on about
a quarter of one core. The GPU path stays within ~−260 dB of the CPU result —
far below audibility — and still helps most with hi-res sources (176.4/192 kHz)
and Hybrid-Phase pairs on a weak CPU. For playback it matters only when the
CPU cannot run the chosen filter in real time: the player measures that on
your machine, per filter, and uses the card only then.

**Can I run two copies side by side?**
Not since 1.5.0. A second launch brings the open window forward instead of
starting another copy: the player needs the output device to itself, so one
copy runs per machine. Running two copies in parallel to convert faster, as
earlier versions allowed, is no longer possible; within one copy a batch still
converts several files at once.

**Why is there a console window next to the app?**
It is the audit log, and it is intentional. AuraEngine's core promise is that
you can *see* what it did to your audio — filter selection, hybrid-phase
switch coverage, true-peak action, verification verdicts. It starts minimized,
so it stays out of the way until you want it; the same log is written to
`%LOCALAPPDATA%\AuraEngine\logs`.

**Does it need ffmpeg or any other external tool?**
No. Since 1.1.0 both decoding and encoding are pure Rust — Symphonia (and,
since 1.5.0, the FDK AAC decoder for HE-AAC) in, `flacenc` out — and no
subprocess is started during a conversion. The one
remaining exception is *verifying* 705.6/768 kHz output: those rates exceed
the FLAC specification's own 655350 Hz cap, so no pure-Rust decoder will read
them back. Without ffmpeg installed the log says plainly that verification
could not run at that rate, instead of marking a good file `_UNVERIFIED`. See
[docs/17-flac-encoding.md](docs/17-flac-encoding.md).

**Can it damage loudness or dynamics?**
No. The engine applies one gain to a whole file, in one documented place: the
true-peak normaliser, when the reconstructed waveform would exceed the ceiling
— −0.5 dBTP, or whatever Headroom names. With Album Level on, the tracks of one
album share that gain — each takes the deepest cut any of them needs — so their
balance stays as mastered. The only other is the ISP stage's output limiter,
which holds an over with a dip of about 1.5 ms instead of taking the whole file
down. It never raises a quiet file. Everything else is unity-gain by
construction, and the verification step proves the file on disk matches the
math.

## Project structure

```
aura-engine/
├── desktop-app/               # The app (Tauri): converter and player
│   ├── src/                   #   Frontend: static HTML/CSS/JS (no build step)
│   │   └── vis/               #     Visualization scenes (.aura-vis) and their specification
│   ├── src-tauri/             #   Rust backend
│   │   ├── src/audio/         #     DSP core: converter/, gpu/, dsp_core.rs,
│   │   │                      #     hybrid_phase.rs, hpss_native.rs
│   │   ├── src/player/        #     Player: WASAPI output, real-time chain, GPU policy, analyzer
│   │   ├── src/player/radio/  #     Internet radio: network, codecs, joining across reconnects
│   │   ├── src/radio_catalog/ #     Station catalog (Radio Browser), favorites and recent stations
│   │   └── src/spatial/       #     Instrument map for the visualizations (models come as a pack)
│   └── start.bat              #   Build-and-run launcher
├── fir-optimizer/             # Python filter designer (generates .npy blobs)
├── docs/                      # Technical documentation + docs/index.html
├── DSP_MANIFESTO.md           # Engineering laws of the DSP core
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
[realfft](https://crates.io/crates/realfft) ·
[rubato](https://crates.io/crates/rubato) ·
[Symphonia](https://crates.io/crates/symphonia) ·
[libopus](https://opus-codec.org/) ·
[FDK AAC](https://android.googlesource.com/platform/external/aac) ·
[wgpu](https://wgpu.rs/) · [rayon](https://crates.io/crates/rayon) ·
[flacenc](https://crates.io/crates/flacenc) · [wasapi](https://crates.io/crates/wasapi) ·
[ort](https://crates.io/crates/ort) · NumPy/SciPy/mpmath. The full list, with
every licence, is in [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

The radio catalog is [Radio Browser](https://www.radio-browser.info/), a free
and open database of stations, made to be used by players like this one.

Several visualization scenes are adaptations of work published on
[Shadertoy](https://www.shadertoy.com/) — by Tonny Espeset, mrange, Virgill,
espeon, ixmibrahim after Martijn Steinrucken (BigWings), weyland, El_Sargo,
Jan Mróz (jaszunio15), patrickjaillet and Stephane Cuillerdier (Aiekick). Each
scene names its source, its author and its licence in its own header.

The visualizations with instruments use HTDemucs (Meta), DrumSep (an MDX23C
model by aufr33 and jarredou), Basic Pitch (Spotify) and ONNX Runtime with
DirectML (Microsoft), downloaded as a separate pack with their licences.

## License

**[PolyForm Noncommercial 1.0.0](LICENSE)** © 2026 ToxaDev

In plain words: the source is open to read, build, use, modify and share
**for any noncommercial purpose** — personal listening, hobby projects,
research, education. **Commercial use of any kind requires a separate
license from the author** — all commercial rights are reserved. If you want
to use AuraEngine (or a derivative of it) in a product or service, write to
<auraengine.dev@gmail.com> to discuss commercial licensing.

Three parts carry licences of their own. The ASIO driver is GPL-3.0-only and the protocol it speaks to Aura Player is MIT. The visualization scenes adapted from
other authors keep theirs — CC BY-NC-SA 3.0, CC BY 3.0 or CC0, named in each
scene file. The instruments pack keeps the licences of its models and runtime
(MIT, Apache 2.0, CC BY-NC-SA 4.0 for DrumSep, Microsoft's for DirectML),
listed in `LICENSES.txt` inside it. The third-party libraries the program is
built with keep their own licences;
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) lists them all, with their
texts, and ships in every download.

## Contact

| | |
|---|---|
| Something is broken | [open an issue](https://github.com/ToxaDev/aura-engine/issues) |
| Questions, ideas, results you want to share | [Discussions](https://github.com/ToxaDev/aura-engine/discussions) |
| Commercial licensing, or anything private | <auraengine.dev@gmail.com> |

GitHub has no private messaging, so the address above is the way to reach the
author directly.
