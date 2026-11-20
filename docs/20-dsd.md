# 20. DSD — output, files and export

> **Files**: `desktop-app/src-tauri/src/audio/dsd/` (readers, DoP, encoder, DSF writer),
> `player/dsd_worker.rs`, `player/dsd_direct.rs`, `player/dsd_cpu.rs`, `audio/converter/dsd_out.rs`
> **Audience**: anyone with a DAC that takes DSD
> **Since**: 1.7.0

---

## TL;DR

Aura Player can send DSD to your DAC: **DSD64, DSD128 or DSD256 over DoP**,
through WASAPI exclusive — or, on Windows from 2.0.3, **natively up to DSD1024**
through your DAC's own ASIO driver ([Native DSD through ASIO](#native-dsd-through-asio-203)). The source goes through the usual chain at the DoP carrier rate and a sigma-delta
modulator turns it into a DSD bit stream. **DSF and DFF files** play through
the same chain, or, in BIT-PERFECT, go out as they are. The converter writes
**DSF from DSD64 to DSD1024**.

To turn it on: in the player, pick the device in exclusive mode, open the
output format panel beside the device button, take the **DSD** tab and set
the rate — **DSD64**, **DSD128** or **DSD256**. The **Auto** preset picks the
best modulator that runs freely on this PC; the panel also locks the rates
above what your DAC plays ([The output format panel](#the-output-format-panel-204)).

---

## What DSD output is

DSD is a 1-bit stream at a very high rate: DSD64 is 2.8224 MHz, DSD128 twice
that, DSD256 four times. A 1-bit stream has a lot of quantization noise; a
*modulator* shapes that noise out of the audio band, up into the ultrasonic
range, where the DAC's analogue filter removes it. How well a modulator does
this — how deep the noise is under 20 kHz, and whether it stays stable on loud
music — is most of what separates one DSD encoder from another.

### DoP

Windows has no standard way to send DSD to a DAC. **DoP** (DSD over PCM) packs
16 DSD bits into each 24-bit PCM sample and marks it with a header byte; a
DAC that understands DoP recognises the marker and plays DSD, any other DAC
would play it as noise — which is why Aura Player sends DoP only in
WASAPI exclusive, where nothing in Windows mixes or resamples it.

### Native DSD through ASIO (2.0.3)

On Windows the device list also offers the DAC's own ASIO driver (Chord ASIO,
for example). Through a driver with a DSD mode the DSD stream goes out
natively — the DSD bytes themselves, not DoP words — and two more rates open:
**DSD512** and **DSD1024**, offered for an ASIO device only. For them the chain
runs at ×16 and the modulator interpolates the rest. A driver without a DSD
mode gets DSD64–DSD256 as DoP over its PCM stream; `AURA_ASIO_DOP=1` asks for
DoP even when the driver has a DSD mode. PCM goes through the ASIO driver as
well. If the driver opens but never asks for sound — Windows holds the DAC as
its default device, another program uses it, or it is unplugged — the player
says so: make another device the default output in Windows, then press Play
again.

| Format | DSD rate | DoP carrier (PCM rate the device opens at) |
|---|---|---|
| DSD64 | 2.8224 MHz | 176.4 kHz, 24-bit |
| DSD128 | 5.6448 MHz | 352.8 kHz, 24-bit |
| DSD256 | 11.2896 MHz | 705.6 kHz, 24-bit |

Your DAC must accept DoP at that carrier rate. If the device has no 24-bit
format at that rate, or refuses it, the player falls back to PCM and says so.
Pauses, starts, the end of a track and the gaps around a seek are filled with
DSD silence.

### The output format panel (2.0.4)

The format button beside the device button opens a panel with two tabs, **PCM**
and **DSD**. On DSD:

- **Auto / Max / Light** presets (anything else shows as *Manual*);
- a **DSD rate** slider, DSD64 to DSD1024 (DSD512 and DSD1024 on an ASIO
  device only), and a **Modulator** slider from *lighter on the CPU* to
  *cleaner*, its stops coloured by how each fares on this PC; **Wide** is its
  own switch;
- **Your DAC** under the rate slider: a list of 617 known DACs gives the
  highest DSD rate the DAC plays, and the rates above it are locked — a driver
  may accept a rate its DAC stays silent at, so the driver is not asked. The
  DAC is found by the driver's name or, when the driver names no model
  (Chord's one driver serves every Chord), by the USB name of the plugged
  device. The field searches the list; choose your model, or give only the top
  rate. Clicking a locked rate allows it;
- **CPU** and **Chain** meters (the chain's own FS for this output) and a hint
  line;
- **All modes** — the table of every modulator at every rate (see [CPU](#cpu));
- **48 kHz DSD for 48 kHz tracks**, and **Re-check this PC** in the foot.

The panel stays inside the window: if it does not fit under the button it
opens upward and its middle scrolls. In the device list ASIO drivers have their
own heading, and a known DAC model is highlighted.

### How the chain feeds the modulator

The PCM part of the chain runs exactly as it does for PCM output: the
upsampling filter (multistage — see [21](21-multistage-upsampling.md)) takes the
source to the DoP carrier rate (×4, ×8 or ×16 from 44.1 kHz), the rack runs on
it, and then the modulator's own short interpolator takes it the rest of the
way to the DSD rate. The encoder runs off the audio thread, one worker per
channel, with a look-ahead buffer of 80–250 ms depending on the modulator.

---

## Which modulator

There are twelve, in three orders (5, 7, 9) and several kinds:

- **plain** — a classic feedback loop.
- **Wide** — moves the noise further from the audio band: at DSD256 about
  14 dB less noise at 100 kHz, for about 5 dB more inside the band. It needs
  room above the band, so it starts at DSD128.
- **LA** (look-ahead) — chooses each next bit by looking several paths ahead,
  so the loop can run with more gain and stay stable. **LA+** looks deeper
  (16 paths); from 2.0.4 it runs at DSD256 too, where it is 4 dB quieter than
  LA in silence and 3.5 dB lower in N+D at −60 dBFS.
- **Trellis** — the deepest search. Too slow for real time; converter only.

The table is ordered best first; the panel's modulator slider runs the other
way, from lighter to cleaner. The measured noise plus distortion in the 20 kHz
band, relative to PCM full scale:

| Rank | Modulator | Rates | Where | N+D, DSD64 | N+D, DSD128 |
|---|---|---|---|---|---|
| 1 | SDM9 Trellis | DSD512–1024 | converter | at the measurement floor | |
| 2 | **SDM9 LA** | DSD64–1024 | player (DSD64–256 from 2.0.4), converter | **−170.5 dB** | **−190.2 dB** |
| 3 | SDM7 Trellis | DSD64–1024 | converter | | |
| 4 | SDM9 | DSD512–1024 | converter | near the measurement floor | |
| 5 | **SDM7 LA+** | DSD64–1024 | player, converter | **−158.8 dB** | **−180.2 dB** |
| 6 | SDM7 LA | DSD64–1024 | player, converter | −151.1 dB | −172.7 dB |
| 7 | SDM7 Wide LA | DSD128–1024 | player, converter | — | −171.4 dB |
| 8 | SDM7 | DSD64–1024 | player, converter | −138.9 dB | −160 dB |
| 9 | SDM7 Wide | DSD128–1024 | player, converter | — | −155.0 dB |
| 10 | SDM5 LA | DSD64–1024 | player, converter | −133.7 dB | −151.0 dB |
| 11 | SDM5 | DSD64–256 | player, converter | −121 dB | −138 dB |
| 12 | SDM5 Wide | DSD128–256 | player, converter | — | — |

How to read it:

- The order is the same at DSD64, 128 and 256. At DSD256, SDM7 measures
  −175 dB against −148 for SDM5; seventh order is 17–27 dB quieter than fifth.
- SDM9 LA runs in the player at DSD64, DSD128 and DSD256 since 2.0.4: at
  −60 dBFS it measures −170.5 dB at DSD64 (SDM7 LA −151.2), −190.2 at DSD128
  (−172.6) and −201.3 at DSD256 (−187.4). It is heavier than SDM7 LA; if the
  CPU cannot keep up, the player falls back to SDM7 LA. The other SDM9 kinds
  run only at DSD512 and DSD1024, where they reach −202…−207 dB, the floor of
  the measurement itself, so they are ranked by stability and search depth.
- Look-ahead is what makes the difference at DSD64: SDM7 −138.9 → SDM7 LA
  −151.1 → SDM7 LA+ −158.8 → SDM9 LA −170.5 dB.
- No modulator overloaded anywhere in the test matrix.

**Which to pick.** Leave the **Auto** preset: it is the best modulator that
runs freely on this PC at the rate. **Max** takes the best one it can run even
if heavy, **Light** the lightest. If the CPU is busy with other things, SDM7 LA is a good step down; plain
SDM7 costs very little and is still 18 dB quieter than SDM5 at DSD64.

### CPU

Each channel is encoded on its own core; the stream cannot be split into
blocks, because a seam between blocks leaves an artefact at −35…−68 dB. As
rough guidance, SDM7 LA at DSD64 takes about a tenth of one core per channel,
and plain SDM7 much less; SDM9 LA and higher rates take several times more.
Since 2.0.4 SDM7 LA+ runs its 16 search paths on a new single-precision kernel
and is about 3× lighter at DSD64/128 (needs AVX2 and FMA; without them, and in
the converter, it runs as before).

The player does not leave this to guesswork. It knows what this PC managed in
a self-check, which brings the core up to full clock, keeps the best result
between starts and waits 10 s after the app starts:

- the panel's **CPU** meter shows the load of the chosen mode, and **All
  modes** shows every modulator at every rate in % of a core per channel —
  green runs freely, amber is heavy, red is too heavy for this PC (from 90 %
  of a core per channel);
- a too-heavy mode can still be picked to try anyway; a mode that has played
  2 minutes without falling back is no longer marked heavy;
- **Re-check this PC** in the panel's foot runs the check again, for example
  after closing a heavy program.

A saved choice that this PC can no longer run falls back to the best one it
can. If the encoder falls behind while playing, it steps down to a lighter
modulator and says so on screen.

---

## DSD files

DSF and DFF (DSDIFF) files open like any other file, in the list and in
playlists. There are two ways they play.

**Through the chain** (the default). The file is decoded to PCM — the
decimating filter is flat to 20 kHz at DSD64 and to 40 kHz at DSD128 and
above, on all cores — and then goes through the rack like any other source.
With DSD output it is encoded again by the modulator you chose.

**As it is**, in BIT-PERFECT. A DSF or DFF goes out over DoP at its own rate
(natively on an ASIO device) with its bytes untouched: no volume, no fades, no dither. The DSD stream
starts at once, while the PCM copy that the analyzer reads is decoded behind
it. On a Chord Mojo 2 a DSD64 file started in 233 ms and the next track in
63 ms. Transitions get DSD silence, and a guard silences anything that stops
looking like DSD, fresh for each track. A DSD512 or faster file, or a device
that refuses DoP at that rate, falls back to PCM with a note.

A file converted from PCM plays at its own rate, and its DSD rate follows it —
a 352.8 kHz render goes out as DSD128.

---

## DSF export

The converter writes DSF (not DFF) from **DSD64 to DSD1024**, in the source's
rate family, with any modulator — the best one by default, including the
Trellis kinds that are too slow for the player. Trellis is slow: four minutes
of DSD256 take about five minutes to encode. The file is streamed to disk, the
encoder's delay is trimmed, and it is read back and checked after writing.
File names carry the same filter and upsampler tags as FLAC renders, and the
file carries the version stamp in an ID3v2 tag (see the
[CHANGELOG](../CHANGELOG.md) for 1.7.0).

**The chain's rate (FS) for DSD, and FS32 (1.8.0).** The PCM part of the chain
runs at the FS multiplier you set, capped so that the DSD bit rate stays at
least 8 times the PCM rate: DSD64 at FS8 at most, DSD128 at FS16, DSD256 and up
at FS32 (1411.2 or 1536 kHz). FS32 is therefore only ever reached by DSD256,
DSD512 and DSD1024 export — FLAC cannot hold a ×32 rate, and the converter
refuses FLAC at FS32 before it starts ([17](17-flac-encoding.md)). The
player's DSD output over DoP is unchanged: its carriers are 176.4, 352.8 and
705.6 kHz; native DSD512 and DSD1024 (ASIO) run the chain at ×16.

---

## Limits

- **WASAPI exclusive carries DSD over DoP only**, up to DSD256; not in shared
  mode. Native DSD and DSD512/DSD1024 in the player need the DAC's ASIO driver
  (Windows only).
- **DSD512 and faster files** play as PCM.
- **DSF export only**, no DFF.
- The format pill is narrow in a small window: a long name such as
  *DSD256 (SDM7 Wide LA)* is cut with an ellipsis.
- Tested on a Chord Mojo 2. Other DoP DACs should work the same way but have
  not been tried.
- DoP goes out on Windows (WASAPI), Linux (ALSA) and macOS (Core Audio); only
  Windows has been tried with a DAC.

---

## Troubleshooting

**Noise instead of music.** The DAC does not recognise DoP at this rate.
Check that the device is in exclusive mode and that the DAC supports DoP up to
the format you chose; try DSD64.

**Sand, hiss or crackle on DSD256 with a Chord DAC.** Chord's Windows driver
1.0.70 sometimes breaks a stream after a 705.6 kHz stream — about one start in
five, in our tests. If it happens, start playback again, or use DSD128. Driver 1.0.69 could
not open 705.6/768 kHz at all; 1.0.70 fixed that. After reinstalling the
driver, select the output in Aura Player again.

**A note says the encoder stepped down.** The PC fell behind. Close heavy programs,
pick a lighter modulator, or let it stay on the one it chose.
