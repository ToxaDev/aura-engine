# 20. DSD — output, files and export

> **Files**: `desktop-app/src-tauri/src/audio/dsd/` (readers, DoP, encoder, DSF writer),
> `player/dsd_worker.rs`, `player/dsd_direct.rs`, `player/dsd_cpu.rs`, `audio/converter/dsd_out.rs`
> **Audience**: anyone with a DAC that takes DSD
> **Since**: 1.7.0

---

## TL;DR

Aura Player can send DSD to your DAC: **DSD64, DSD128 or DSD256 over DoP**,
through WASAPI exclusive. The source goes through the usual chain at the DoP carrier rate and a sigma-delta
modulator turns it into a DSD bit stream. **DSF and DFF files** play through
the same chain, or, in BIT-PERFECT, go out as they are. The converter writes
**DSF from DSD64 to DSD1024**.

To turn it on: in the player, pick the device in exclusive mode, open the
format dropdown beside the device button and choose **DSD64**, **DSD128** or
**DSD256**. The best modulator this PC can run is already selected.

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

| Format | DSD rate | DoP carrier (PCM rate the device opens at) |
|---|---|---|
| DSD64 | 2.8224 MHz | 176.4 kHz, 24-bit |
| DSD128 | 5.6448 MHz | 352.8 kHz, 24-bit |
| DSD256 | 11.2896 MHz | 705.6 kHz, 24-bit |

Your DAC must accept DoP at that carrier rate. If the device has no 24-bit
format at that rate, or refuses it, the player falls back to PCM and says so.
Pauses, starts, the end of a track and the gaps around a seek are filled with
DSD silence.

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
  so the loop can run with more gain and stay stable. **LA+** looks deeper; at
  DSD256 it is the same as LA, so the list shows LA only.
- **Trellis** — the deepest search. Too slow for real time; converter only.

The list in the player is ordered best first. The measured noise plus
distortion in the 20 kHz band, for a −6 dB tone, relative to PCM full scale:

| Rank | Modulator | Rates | Where | N+D, DSD64 | N+D, DSD128 |
|---|---|---|---|---|---|
| 1 | SDM9 Trellis | DSD512–1024 | converter | at the measurement floor | |
| 2 | SDM9 LA | DSD512–1024 | converter | at the measurement floor | |
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
- The SDM9 kinds run only at DSD512 and DSD1024, which the player does not
  offer, so in the player the list starts with SDM7. At those rates they reach
  −202…−207 dB, which is the floor of the measurement itself, so they are
  ranked by stability and search depth.
- Look-ahead is what makes the difference at DSD64: SDM7 −138.9 → SDM7 LA
  −151.1 → SDM7 LA+ −158.8 dB.
- No modulator overloaded anywhere in the test matrix.

**Which to pick.** Leave the default: it is the best modulator this PC can
run. If the CPU is busy with other things, SDM7 LA is a good step down; plain
SDM7 costs very little and is still 18 dB quieter than SDM5 at DSD64.

### CPU

Each channel is encoded on its own core; the stream cannot be split into
blocks, because a seam between blocks leaves an artefact at −35…−68 dB. As
rough guidance, SDM7 LA at DSD64 takes about a tenth of one core per channel,
and plain SDM7 much less; LA+ and higher rates take several times more. These
figures are preliminary and will be measured again.

The player does not leave this to guesswork. When the format list opens it
knows what this PC managed in a self-check:

- a modulator marked **· high CPU** (amber) runs, but takes a large share of a
  core;
- one marked **· too heavy for this PC** (grey) is disabled, with the reason;
- **Re-check this PC…** runs the check again, for example after closing a
  heavy program.

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
with its bytes untouched: no volume, no fades, no dither. The DSD stream
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
player's DSD output is unchanged: its carriers are 176.4, 352.8 and 705.6 kHz.

---

## Limits

- **Output only through WASAPI exclusive, over DoP.** Not in shared mode, not
  over ASIO; no native DSD.
- **DSD512 and DSD1024 only in the converter.** The player plays up to DSD256,
  and plays faster files as PCM.
- **DSF export only**, no DFF.
- The format pill is narrow in a small window: a long name such as
  *DSD256 (SDM7 Wide LA)* is cut with an ellipsis.
- Tested on a Chord Mojo 2. Other DoP DACs should work the same way but have
  not been tried.
- Windows only, like the rest of the player for now.

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
