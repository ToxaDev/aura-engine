# 21. Multistage upsampling

> **Files**: `desktop-app/src-tauri/src/audio/converter/dsp/multistage.rs`, `dsp/ms_h1.rs`, `dsp/ms_text.rs`,
> `converter/pipeline/ms_route.rs`, `player/ms_player.rs`, `player/ms_hybrid.rs`, `player/ms_quality.rs`
> **Audience**: anyone who wants to know why 1.7.0 needs several times less CPU for the same filter
> **Since**: 1.7.0 (the default); ×32 and the 100M–1G rungs: 1.8.0

---

## TL;DR

The long filter — the same 10M or 30M file as before, nothing in the filter
files changed — now runs **once, at the source rate**. A short, exact,
linear-phase interpolator then takes the result the rest of the way to ×2,
×4, ×8, ×16 or ×32. With 5M filters and longer this costs **3–7 times less CPU in
the player** at ×4–×16 (up to 9.5× on four efficiency cores), **2.2–3.5 times
less in the converter**, and about **a third of the graphics memory**. The
difference to the old single-stage output sits at **−204…−220 dBFS**, about
66 dB under the last bit of a 24-bit file; in a blind A/B nobody could tell
them apart.

Since 1.8.0 this is also the only way the new rungs of **100M to 1G taps**
play: their files hold branch 0 and nothing else (see
[12 — section 11](12-precomputed-fir-matrix.md#branch0)), and the output rate
goes up to **×32** (1411.2 / 1536 kHz).

---

## How it works

Upsampling by L with a polyphase FIR works out every output sample from the
long filter: L output samples per input sample, each one a dot product with
one of the filter's L branches. Every output sample costs the full branch.

But one branch in L — the one that lands exactly on the input samples — is
special: it is what the long filter says the signal is *at the source's own
sample times*, with all of its wall, phase and apodization. Multistage keeps
only that branch of the long filter (**stage 0**, with gain L over its sum),
runs it at the source rate, and lets a short interpolator fill in the samples
between:

```
source ──► stage 0: the long filter (5M … 1G), branch 0, at the source rate
       ──► half-band ×2 (hb1)
       ──► one polyphase stage for the rest of the ratio (×2, ×4, ×8 or ×16)
       ──► output at ×2 … ×32
```

Each short stage is designed to 218 dB, with a stop band of at least 210 dB.
The long filter decides the sound — the wall, the phase, the ringing, the
delay; the short stages only have to interpolate a signal that is already
band-limited, and they do it far below anything the long filter leaves.

---

## Accuracy

Difference to the single-stage output (null), dBFS:

| Case | Difference |
|---|---|
| 30M ×8, linear phase | −212 … −220 |
| 30M ×8, minimum phase | −212 … −219 |
| 30M ×8, TFS | −204 … −210.5 |
| Hybrid-Phase 30M ×8 | −219.8 |
| α-HP 30M ×8 | −220.3 |
| 30M ×32, linear phase (1.8.0) | −213.6 … −216.0 |
| 100M ×8, linear phase (1.8.0) | −215.7 … −217.2 |
| 1M (for comparison; 1M itself stays single-stage) | −199 … −200 |
| GPU vs CPU, multistage | −283 … −306 (RMS) |

What this means:

- The 24-bit LSB is at −144 dBFS. The difference is about **66 dB below it**.
- It is **not bit-identical**. Rounded to 24 bits, one sample in 20 000 to
  90 000 comes out one LSB different — a value sitting exactly on the edge
  between two codes. With dither or any volume change this is lost in the
  dither itself.
- Unchanged: the wall (−6 dB at 21 050.01 Hz, same as before), the phase, the
  ringing, the delay, the inter-sample peak.
- Image rejection above 23 kHz for 30M: −227 dB single-stage, −210 dB
  multistage. Both are far below the 24-bit floor.
- **Listening.** Pairs of single-stage and multistage renders were compared
  blind; the listener could not tell them apart. That is one listener — the
  measurements above are the main evidence.

---

## CPU

**Player**, 30M filter, 44.1 kHz source, CPU milliseconds per second of music:

| | ×4 | ×8 | ×16 |
|---|---|---|---|
| Multistage, all cores | 11 | 6.9 | 4.7 |
| Times less than single-stage, all cores | 3.1× | 4.9× | 7.2× |
| Times less, four efficiency cores | 3.8× | 5.9× | 9.5× |

At ×2 on four efficiency cores it is 2.1× less. Four efficiency cores stand
for a modest laptop: there the single-stage path costs about 160 ms per second
at any ratio, multistage 50/27/23 at ×4/×8/×16. (Single-stage polyphase cost
hardly depends on the ratio; multistage gets cheaper as the ratio grows.) On
those cores a 30M ×8 stream started with a margin of about 7× real time; now
it is about 35×.

Shorter filters gain less, because less of the work was in the long filter:

| Filter | ×4 | ×8 | ×16 |
|---|---|---|---|
| 30M | 3.1× | 4.9× | 7.2× |
| 10M | 2.6× | 3.6× | 4.2× |
| 5M | 2.1× | 2.8× | 2.4× |

In a real window, 30M ×4 went from one full core to a quarter of one (×4.2);
the CPU badge read *CPU 3–4* before and *CPU 1* after.

**Hybrid-Phase** 30M ×8 on four efficiency cores: 298.8 → 49.0 ms per second,
6.1 times less.

**From the press to the first sound**, 30M, a four-minute 44.1 kHz track, CPU
only, XTC and the subsonic filter on — the time until the first 0.3 s of output
is ready (decode and source stages, the chain's build, the pre-roll; opening
the device not included), median of three, 1.8.1 code (2026-10-24, the
`first_block_cost` bench in `player/first_block_bench.rs`):

| | ×4 | ×16 |
|---|---|---|
| Four efficiency cores — multistage | 1.51 s | 1.15 s |
| Four efficiency cores — single-stage | 1.86 s | 1.80 s |
| All cores — multistage | 0.40 s | 0.62 s |
| All cores — single-stage | 0.79 s | 0.77 s |

The gain is in the pre-roll — the first output the chain has to compute before
the device starts: on four efficiency cores at ×16 it went from 1.00 to 0.32 s.
A summary of these numbers for readers of the main page:
[README — performance on modest hardware](../README.md#performance-on-modest-hardware).

**Converter**, a 3:43 track, 30M:

| | ×4 | ×8 | ×16 |
|---|---|---|---|
| CPU time, s | 258 → 74 | 326 → 106 | 403 → 184 |
| Times less | 3.5× | 3.1× | 2.2× |
| Wall time, s | 23 → 13 | 29 → 20 | 45 → 33 |

The converter gains less than the player because decoding, the inter-sample
peak stage, the subsonic filter, FLAC encoding and the read-back check did not
change. With 10M at ×4: 117 → 57 CPU-seconds; 5M at ×4: 84 → 53.

---

## GPU

On the graphics card, stage 0 runs on the card at the source rate and the
short chain on the CPU. Graphics memory for 30M ×8: **about 170 MiB**
(measured 168–173) instead of **591 MiB**. GPU conversion with 30M: ×8
23.5 → 16.7 s, ×16 37 → 31 s.

The GPU is now needed much less. It still helps most with hi-res sources
(176.4/192 kHz) and with Hybrid-Phase pairs on a weak CPU.

---

## Where it applies

Multistage is the default (and, from 100M taps, the only way) in the player, the internet radio, the ASIO input,
the converter (in memory and on the giant-file route), Hybrid-Phase and α-HP
(each half has its own stage 0 and short chain, blended as before), and DSD
output (its ×4 to ×32 PCM part). Live sources — the radio and the ASIO input —
get about 4 ms more delay at 44.1 kHz (2 ms at 96 kHz, 1 ms at 192 kHz).

**What stays single-stage**, by design:

- the **1M** filter and the short 5k filter — **1M is the default on a fresh
  install**, so the savings start when you pick a 5M, 10M or 30M filter
  (the Standard bundle has 10M, the Reference bundle 30M);
- a custom filter of your own;
- ×3 and ×6, and any non-integer ratio;
- the peak probe.

The analyzer's response shows what was actually played — stage 0 and the
short chain. The player and converter rows, the render's tag and its file name
name the chain too. A FLAC render carries
`AURA_UPSAMPLER=multistage x2(N1)+x4(N2)…`.

---

## Switching back to single-stage

The single-stage path is still in the app for the rungs of 30M and shorter,
and its output is unchanged bit for bit. It has no switch in the interface;
either

- add `"upsampler": "single"` to the settings file, or
- start the app with the environment variable `AURA_MULTISTAGE=0`.

A single-stage render is tagged `AURA_UPSAMPLER=single` and named
*… Kaiser 30M Single …*; a multistage render has no mode mark in its name.

Rungs of 100M taps and more cannot go back: their files are branch 0 only and
the single-stage path refuses them.

---

## ×32 and the rungs above 30M (1.8.0)

**×32.** The last stage for 1411.2 kHz (44.1 × 32) and 1536 kHz (48 × 32) is a
new short chain: the half-band stage and one ×16 polyphase stage, 218 dB each.
The chain's stop band is −219.6 dB for the 44.1 kHz family and −218.8 dB for
the 48 kHz family; the chains of ×2 to ×16 are byte-identical to 1.7.x. The player outputs ×32 where
the device accepts the rate; the converter runs the chain at ×32 for DSD256 and
faster, and refuses FLAC there, because a FLAC header cannot hold a rate above
1 048 575 Hz.

**The rungs.** The ladder is 5k · 1M · 5M · 10M · 30M · 100M · 180M · 300M ·
500M · 1G; 100M–300M are offered from FS4 up and 500M and 1G from FS8 up. In
multistage the long filter runs once at the source rate whatever its length, so
the extra work of a longer rung is the branch-0 filter itself: at the source
rate, a 1G-tap filter at ×8 has 125 million coefficients (1 GB in memory as f64),
at ×32 31 million. Measured against the single-stage output, the multistage
null is −215.7 … −217.2 dBFS at 100M ×8 and −213.6 … −216.0 dBFS at 30M ×32,
the same as at 30M ×8.
