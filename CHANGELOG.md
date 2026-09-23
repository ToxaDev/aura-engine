# Changelog

## 1.3.4 — An update from 1.2.x no longer keeps the old defaults

Mostly a release about settings. Everything in it came out of the Audio
Science Review thread on one day: one problem turned up while checking the
documentation against a reply there, the others when formdissolve asked why an
18-minute track needed 28 GB of RAM, and what it would take to answer that
properly.

**Polyphase FIR stayed off for anyone who updated from 1.2.x.** Until 1.3.0
the `PFR` box shipped unticked; since 1.3.0 it ships ticked. Settings store
that box and restore it, so an update from 1.2.x kept it off without anyone
having chosen that, and those files took the standard route instead of the
polyphase one. That had three consequences:

- TFS stands down on the standard route, so a TFS row lit in the rack did
  nothing.
- The standard route holds the whole track in RAM; only the polyphase route has
  the segmented pass for very long files. An 18-minute track at FS×8 with
  Hybrid-Phase was estimated at 28–29 GB and refused on a 32 GB machine.
- The file went through a different resampler from the one the documentation
  calls the default. In the audio band the two routes measure the same
  ([docs/05 §10](docs/05-converter-pipeline.md#known-issues)).

Settings now carry a revision number. Anything saved without one, which means
1.3.3 or earlier, gets `PFR` switched on, once. Switch it off again if you
like; from now on that choice is kept. The same 18-minute file through the
polyphase route, at FS×8 with 30M taps and Hybrid-Phase, Declip, ISP and SUB
on, took the segmented route: 6.3 GB peak working set, under three minutes on
an RTX 4090, 0 of 381 024 000 samples off in verification.

**Long files get the whole output chain.** A file too long to convert in
memory takes the segmented route, which writes the render through temp files.
After the phase blend that route ran none of the output stages: XTC was
silently skipped, every over the ISP output limiter would have trimmed locally
went to one global gain for the whole file instead, and the subsonic filter
got no second pass. Switching `PFR` on for older settings sends more tracks
down that route, because it starts at a quarter of your RAM: with Hybrid-Phase
at FS8, from about 5 minutes on a 16 GB machine, 10–11 on 32 GB and 16 on
64 GB (roughly twice that without Hybrid-Phase).

All three now run there, streamed, in the same order and with the same
arithmetic as in memory. Each has a streaming twin, and a test holds each twin
to the in-RAM version bit for bit, plus one end-to-end test through a temp
file. On 17–18 minute files the limiter makes the same call it makes in memory.
On two clipped masters it steps aside, because the overs aren't sparse. On a
dynamic master raised 1.2 dB it trims 33 overs locally, and the file loses
0.01 dB instead of 0.33. Verification finds 0 samples off, the conversion time
doesn't change, and peak memory stays about 6 GB.

**Hybrid-Phase and TFS, settled in any settings.** Settings from 1.2.x with
Hybrid-Phase on came up with TFS on as well. The rack showed `✕ TFS` and
`✕ HP`, and the engine ran TFS in place of Hybrid-Phase. Fixing only settings
nobody had touched since 1.2.x would not have been enough: the first change
made in 1.3.0–1.3.3 saved the pair as it stood. So the check runs on every
load. The rack never leaves both on by itself, so both on is always inherited,
and Hybrid-Phase, the one that was actually chosen, wins.

**Long files are named after everything that ran.** Since 1.3.0 the segmented
route built the file name before adding `PFR` and `HP` to the chain:
`… DC·ISP·SUB15].flac`, while the `AURA_CHAIN` tag inside the same file said
`DC,ISP,SUB15,PFR,HP`. The name now comes from the finished chain, as it does
on the in-RAM route.

**The out-of-memory message mentions PFR first.** It used to offer only a lower
FS multiplier or Hybrid-Phase off, which means paying in quality. With PFR off,
it now says to turn PFR on first.

**The version is on screen.** It's small, above the left end of the rule under
the heading, and read from the app's manifest. The web converter shows the
version of the engine it runs, in the same place.

**Who it affects.** Anyone who updated from 1.2.x: `PFR` comes up on, and TFS,
if its row is lit, now actually runs. Files from those setups change with it:
the name gains `PFR`, and `TFS` where it runs. A fresh install since 1.3.0
converts exactly as before, unless `PFR` was switched off by hand; then it
comes up on once, like everyone else's. Long files on the segmented route
change for everyone with XTC, ISP or SUB on, as described above; files that
fit in memory don't.

## 1.3.3 — The subsonic filter keeps its promise past the limiter

One fix, and once again Erogen found it: spectra of three versions of one
track, posted in the Audio Science Review thread, and ours had a flat shelf from
3 to 10 Hz, about 40 dB under the bass — with SUB20 on, where the filter
promises at least 100 dB down.

**Where it came from.** The subsonic filter runs on the source, after the
repairs. The ISP output limiter runs at the very end, after the convolution
and the phase engine, and trims each over with a gain dip about 1.5 ms long,
thousands of times in a track. Bass multiplied by a gain that keeps dipping is
amplitude modulation, and its sidebands land below 20 Hz, where a filter that
has already run cannot reach them. The filter itself was fine: between 13 and
20 Hz the curve in Erogen's plot follows its design to within a decibel.

Reproduced on a clipped CD with rumble added at the level of that disc (FS×4,
1M taps, the same chain: ISP · SUB20 · PFR · HP · αHP). At 2.7 Hz the filter alone
leaves −125.5 dBFS; with the limiter engaged, −56.6. Against the same run
without ISP, the difference touches 4.46 % of samples, the limiter's own output
below 12 Hz is −62.3 dBFS RMS against −83.6 for everything else, and it tracks
the limiter's activity second by second (r = 0.67).

**The fix.** The filter now runs twice. The source pass stays where it was and
still takes the disc's rumble out. A second pass — the same design, computed for
the output rate — runs straight after the output limiter and before the
true-peak ceiling is measured, so the normaliser reads the peak that is
actually written. Moving the filter to the end instead would have fed the
limiter infrasound it was about to lose, spending its dips on peaks the file
would not have. Two passes of the same high-pass cost 1×10⁻⁴ dB above the
corner.

Same file, same chain, one build, dBFS per bin (Blackman-Harris, 65536):

| Hz | 1.3.2 | 1.3.3 | ISP off |
|---|---|---|---|
| 2.7 | −56.5 | **−125.0** | −126.9 |
| 5.4 | −56.6 | −110.7 | −117.5 |
| 8.1 | −56.7 | −84.4 | −89.1 |
| 40 (bass) | −28.0 | −28.0 | −29.9 |

The ISP-off run came out 1.9 dB quieter; level-matched, it agrees with 1.3.3 at
2.7 Hz to the tenth. What remains above 8 Hz is the filter's own transition
band (−6 dB at 15 Hz for SUB20), not a leak. Both conversion routes and
352.8 kHz read the same.

**What it costs.** Above 25 Hz nothing moves: the worst bin of the difference
from 1.3.2 is −104.5 dB relative to the signal, with no time shift. The ceiling
takes 0.018 dB more, because taking infrasound out moves peaks. At 352.8 kHz
the second pass is 238 485 taps at SUB20 and 317 979 at SUB15, three to four
seconds on a 3–4 minute track; its FFT batch is capped at about 1 GiB.

**Who it affects.** Everyone with SUB on — which, like ISP, is the default since
1.3.0. The shelf appeared only when the output limiter actually engaged; the
second pass runs whenever SUB is on. The segmented route for very large files
keeps the single source pass: it has no output limiter, so there is nothing to
clean up, and the log says so.

The soak test harness also takes `AE_SOAK_PFR` now, so the standard route can
be reached from it at all.

## 1.3.2 — Hybrid-Phase stops clicking

One fix, and someone else found it. Erogen, in the Audio Science Review
thread, posted a spectrogram of a 1.3.1 file and asked what the thin vertical
lines above the apodizer's 18 kHz ceiling were. They were Hybrid-Phase: every
switch between its two branches left a tick.

**Why it ticked.** Hybrid-Phase renders the track twice, in linear and in
minimum phase, and changes branch around each attack. The switch sample is
snapped to a zero of the *mid* difference between the branches, so both
channels change phase at the same instant — snapping each channel on its own
used to let L and R drift up to ±5 ms apart. What that leaves is a step in
each channel, its share of the side difference, and the 0.083 ms fade meant to
hide it could not: 83 µs is too short for anything but a broadband tick. On a
clipped CD at 176.4 kHz / 30M taps, content above 24 kHz peaked at −42.9 dBFS.
Subtracting a pure linear-phase render recovers exactly the 1070 switch points
the log reports, and every one of the 895 ticks above −80 dBFS peaks within
0.1 ms of one of them.

**The fix.** The fade is now 1 ms, on the running integral of a Hann window
instead of a raised cosine. Its slope and its curvature are both zero at the
ends, so its own spectrum falls away much faster. The same file now peaks at
−103.0 dBFS above 24 kHz (−100.7 at 352.8 kHz / 1M), with the same switch
points and the same true-peak gain. The switching logic itself is untouched.

**What it costs.** For that millisecond both branches sound together, and at
the very top their phases disagree. Against a click-free splice, 14–21 kHz in
the 3 ms around a switch comes out 0.32 dB lower (median), 8–14 kHz 0.05 dB
lower, and nothing changes below 8 kHz. A 2 ms raised cosine cost three times
as much at the top; a Blackman-Harris integral left the ticks at −95.7 dBFS.

**Who it affects.** Only files converted with HP on. A fresh install has
opened on TFS Phase, with HP off, since 1.3.0; HP stays on for anyone who
carried their settings over from 1.2.x. Conversions without HP, with αHP or
with TFS are the same arithmetic as in 1.3.1.

The soak test harness also takes `AE_SOAK_FS` now, for the sample-rate
multiplier.

## 1.3.1 — Two stages stop running on one core

Nothing here changes what comes out. It changes how long you wait for it: the
two most expensive stages in the pipeline were each using a single thread.

**Declip now splits by channel.** The two channels are independent in every
respect that matters — their own buffers, their own rails, nothing shared —
so they run together. What does *not* split is the window loop inside a
channel: windows advance by 2048 with a 4096 span, and each one crossfades
into what the one before it wrote. That order is load-bearing and is
untouched, so the output is the same sample for sample; a test holds the pair
against two solitary passes to keep it that way. On material with real
clipping this stage was about a quarter of a conversion.

**XTC now splits by transform.** Four FFTs per block — two forward, two
inverse — pairwise independent. The block loop stays sequential, and that is
deliberate: the filter runs to some 28 000 taps against an 8192-sample block,
so one output sample collects contributions from three or four blocks through
overlap-add, and floating-point addition is not associative. Any other order
would move the last bit of every sample in the overlap. Only the transforms
moved; the accumulation is the same loop in the same order.

**A seam for an alternate polyphase pass.** `dsp/polyphase_engine.rs` lets
whoever embeds the engine supply their own convolution. Nothing installs one
here, and with none installed the pass runs exactly the code it always has.
It exists because the convolution is the one stage whose result depends only
on the input, the filter and the ratio — which makes it the one stage that
can be computed somewhere else without a second implementation of the DSP.

**CI builds again.** The repository compiles with `target-cpu=native`, and
the hosted runners are not one machine: a cache filled on a CPU with
instructions the next runner lacks brought rustc down with
`STATUS_ILLEGAL_INSTRUCTION`. CI now pins the same portable baseline the
release workflow has always used.

## 1.3.0 — Six stages come out of the lab

Six DSP stages that have existed for a while behind a private build ship in
this release, and the Advanced DSP rack is rebuilt around them.

| | | |
|---|---|---|
| `DC` | Declip | rebuilds the peaks a master flattened at full scale |
| `ISP` | Intersample Peak Correction | clears true-peak overs the samples themselves never show |
| `AHR` | Adaptive Headroom | keeps the shipped ceiling when lowering it would cost level for nothing |
| `TFS` | TFS Phase | linear phase low, minimum phase high, blended between |
| `αHP` | Continuous Alpha | a per-sample crossfade in place of Hybrid-Phase's hard switch |
| `XTC` | Crosstalk Cancellation | cancels what each speaker sends to the wrong ear |

Each one is described, with measurements, in
[docs/18 — The Advanced DSP Stages](docs/18-dsp-stages.md).

### The rack is three zones now

Source, Conversion, Output — and inside each, the stages stand in the order the
pipeline runs them:

```
┌ 1 SOURCE ─────────────┐ ┌ 2 CONVERSION ─────────┐
│ DC   Declip           │ │ AA   Adaptive Apodizer│
│ ISP  Intersample Peak │ │ PFR  Polyphase FIR    │
│ SUB  Subsonic   15 Hz │ │ HP   Hybrid-Phase   2×│
│ AHR  Adaptive Headroom│ │ αHP  Continuous Alpha │
└───────────────────────┘ │ TFS  TFS Phase        │
┌ 3 OUTPUT ─────────────────────────────────────┐
│ XTC  Crosstalk Cancellation          SET… ⚙   │
└───────────────────────────────────────────────┘
```

Dependencies and conflicts are written in words in the state column — `← HP`,
`✕ TFS` — and they propagate along the chain rather than one level deep:
switching on Continuous Alpha pulls Hybrid-Phase up with it, and Hybrid-Phase is
the one that rules out TFS.

The row under each file in the queue draws the same chain, one badge per stage,
in three states: it ran, it was reached and declined — the tooltip says why — or
it has not been reached yet. The bit-perfect check sits last, behind a divider,
because it is a verdict on the output rather than a stage. The tokens also go
into the output filename and into the `AURA_CHAIN` Vorbis comment, so a file
says what was done to it.

### What a fresh install opens on

`DC`, `ISP`, `SUB` (15 Hz), `AHR`, `AA`, `PFR` and `TFS` are on. Headroom opens
on −0.5 dB.

The three that stay off are the three that need a decision from you:
Hybrid-Phase and Continuous Alpha, because TFS occupies the same slot and costs
twice the processing time; and XTC, because it cannot run at all until you have
measured your listening triangle.

Settings you had already saved are untouched — this is what a new installation
starts from, not a migration.

### Adaptive Headroom depends on Headroom rather than replacing it

The Headroom control names the ceiling the output is normalised to. Adaptive
Headroom decides whether to honour it on this file: it keeps the shipped
−0.5 dBTP when the source peak already sits below the target, or when ENOB says
the master is a clean high-bit-depth one, and otherwise lowers the ceiling as
asked.

So with Headroom on **Off** there is nothing for it to decide, and the stage
does not run at all. The rack says so — the badge reads `← Headroom` and will
not come up — and turning Headroom back to Off puts an enabled one down again.

### Crosstalk cancellation needs a measured room

This is the one stage in the product that deliberately colours the signal, and
the only setting that describes the room rather than the file. It will not
switch on until the listening triangle is entered, in a window of its own, and
the same check is repeated on the Rust side so a hand-edited setting cannot slip
past.

A canceller built from guessed distances does not cancel less — it cancels the
wrong thing and reinforces what it exists to remove. It is worth reading the
head-displacement panel in the docs before using it: 20 mm of movement costs
about half the cancellation. Speakers only, never headphones.

### Also

- The filter pre-flight only ever asked for the minimum-phase blob when
  Hybrid-Phase was on. TFS derives from the same linear + minimum pair and now
  ships on, so an installation carrying only the linear halves would have passed
  the check and then failed part-way through a batch. Both the pre-flight and
  the opening-state logic ask for it now.
- The subsonic chip opens on 15 Hz rather than 20 Hz on a new install.

## 1.2.9 — The decoder was clipping the input

A lossy format does not promise to keep its peaks below full scale. MP3, AAC,
Vorbis and Opus decoders, and float WAV, legitimately produce samples above
1.0; a file that has been through MP3Gain with a positive gain does it on a
percent of its frames. This converter flattened every one of them at the door —
before DC blocking, before the filter, before the true-peak ceiling whose whole
job is to bring such a file down. CD rips, and any integer source, were never
affected: there is nothing above full scale in an integer sample.

### Where the clipping was

In two places, and fixing one was not enough.

Ours: every packet was copied into an `i32` buffer. That buffer was chosen for
a good reason — an `f32` one loses the 24th bit of a 24-bit master — and it
does keep every integer bit. But converting a float sample into `i32` goes
through a clamp at ±1.0.

Symphonia's: its own MP3 and Vorbis decoders clamped their output to ±1.0
inside the decoder, where no buffer choice of ours could reach.

### What changed

A packet now goes straight into `f64`. Integer PCM divides by its own exact
power of two — 2¹⁵, 2²³, 2³¹ — which is what the `i32` path did, to the bit. A
float sample is widened, not clamped.

Symphonia moves from 0.5.5 to 0.6.1, where both decoder clamps are gone. That
is a new API for probing, decoding and metadata; gapless trimming still uses
the container's own priming and padding counts, so nothing about that changed.

When a file arrives above full scale the log says so:

```
Source peaks at +7.42 dBFS — 210992 frames (1.764%) above full scale,
kept as decoded; the output ceiling scales them down
```

### What was measured

With the dither seeded in a test build, so two runs can be compared bit for
bit:

- A CD rip (Santana, *Mal Bicho*) and a 24-bit 44.1 kHz FLAC: **outputs
  SHA-256 identical to 1.2.8.** Integer sources do not move.
- An MP3 raised by MP3Gain by 7.5 dB: it decodes to +7.42 dBFS with 1.764 % of
  frames above full scale — the same count `ffmpeg` reports. The ceiling now
  sees 7.27 dBTP where it used to see 3.30, and scales the file by −10.27 dB
  instead of −6.30. Level-matched, the old and new outputs differ by a residual
  23 dB below the signal. That residual was the clipping.
- Unit tests: floats at 1.5 and −1.25 arrive unchanged, where the old path
  returned exactly ±1.0; 16-, 24- and 32-bit integers come out bit for bit as
  before, the 24th bit included.

Anyone can check the old behaviour for themselves: decode a lossy file to
float, count the samples above 1.0, then decode it through the app. They used
to flatten. They no longer do.

## Video memory: measured, not assumed

The engine decides how many convolvers may sit on the graphics card at once.
It was deciding against `max_storage_buffer_binding_size × 0.7` — a Vulkan
limit on a single buffer, not on the card. It reads about 1433 MB on an 8 GB
card and on a 24 GB card alike. One convolver at 30M taps and 8× takes 736 MB,
so two never fitted: however many files a batch had in flight, one was on the
card and the rest were queued behind it.

Windows can answer the question properly, for every graphics API at once. The
engine now reads two numbers — the per-process video memory budget from DXGI,
and what all programs together hold on the adapter, from the same performance
counter Task Manager shows — and plans against three quarters of the smaller of
the two. DXGI alone is not enough: on a 24 GB card it reported 23 374 MB while
other programs held 3.7 GB.

The old figure stays as a floor where there is no reading to be had: an
integrated adapter, whose "video memory" is system RAM and is rationed
separately; a query that fails; two identical cards the query cannot tell
apart; anything that is not Windows.

And if the card refuses an allocation anyway, that refusal is now read for what
it says. Refused while other files held convolvers, it means *fewer at once*,
and the batch plans for one fewer. Only a refusal on an idle card means *this
size will not fit*, which is what it used to mean in both cases — and which
sent every later convolver of that size to the processor.

### Four files at once

The limit of three concurrent files was measured under the old floor, where a
fourth could only lengthen the queue to the card. Re-measured with the card's
own reading, on 13 files, 2997 seconds of audio, 30M taps at 8× with the
Adaptive Apodizer and Hybrid-Phase:

| | 3 files | 4 files |
|---|---|---|
| 32 logical cores | 177.9 / 177.4 s | **146.3 / 147.6 / 147.3 s** |
| 20 cores | 156.5 / 147.2 s | **137.8 / 140.5 s** |

Four is the default now where there are sixteen or more logical cores, two
below that, the same threshold as before. Neither number is a memory decision:
a machine short of RAM or video memory holds the extra file back rather than
swapping or failing.

### What this is worth

Same 13 files, three workers, 30M taps at 8×, RTX 4090, two runs each:
**219.9 / 223.3 s before, 184.4 / 174.6 s after** — about a fifth off, from
scheduling alone. The outputs are SHA-256 identical to the ones the old
scheduler produced. At 10M taps, where two convolvers already fitted under the
old floor, the difference is inside the run-to-run spread.

Blocks do slow each other down when they share the card — 59 ms alone against
about 127 ms beside another — and the batch still finishes sooner, because what
the floor serialised was never only the convolution: it was every convolver's
construction, filter upload and flush, and the processor stages of the file
behind them.


## 1.2.8 — Subsonic filter

In the Audio Science Review thread a listener showed a CD whose spectrum rises
into a hump below 20 Hz. It is not an offset: at 65 536 bins it sits at about
1 Hz, at close to 1 % of full scale, and differs between the channels. Below
20 Hz the converter did one thing — it subtracted each channel's mean — and
that takes 0.00 dB out of a wave like this one. 1.2.8 adds a switch for it.

It is there for speakers, not for sound. Nothing here claims it is audible.

### What it does

A linear-phase FIR high-pass at the source rate, straight after DC blocking,
before the apodizer's forensics and before the filter. Off by default.

| | |
|---|---|
| corner | 20, 15 or 10 Hz — the chip on the SUB row |
| passband | flat within 0.01 dB from the corner up (designed: 5·10⁻⁵ dB) |
| stopband | at least 100 dB down from half the corner to DC (designed: 104.4 dB) |
| −6 dB | at three quarters of the corner |
| length | from the source rate: 29 813 taps at 44.1 kHz and 20 Hz, twice that at 88.2 kHz or at 10 Hz |
| filename | `SUB20` / `SUB15` / `SUB10`, before `AA` and `HP` |

Three corners, each for a reason: 20 Hz is the edge of the audible band and
the classic subsonic corner; 15 Hz keeps the lowest note of a 32-foot organ
stop, 16.35 Hz, flat; 10 Hz takes drift and infrasound and nothing musical.
There is no corner above 20 Hz — that would be cutting bass.

Linear phase is the reason to do this offline. Above the corner nothing moves
in level or in time, which a high-pass in the playback chain cannot promise.

### What was measured

Through the whole app, 5k taps at FS8, each file converted with the filter off
and on, with the dither seeded identically in a test build so the two outputs
can be subtracted.

A synthetic file built like the one in the thread — 1 Hz at −40.5 / −45.0 dBFS
under music-like noise from 30 Hz up and a 40 Hz tone: with SUB20 the 1 Hz
component comes out at −158.8 / −163.3 dBFS. Above 25 Hz the difference between
the two outputs stays at least 100.8 dB below the signal in every 0.34 Hz bin,
the cross-correlation lag is zero, and the 40 Hz tone has the same amplitude
and phase to five decimals.

A CD rip (Santana, *Mal Bicho*): above 25 Hz the difference stays at least
98.9 dB below the signal; below 8 Hz the level falls from −51.5 to −142.6 dBFS.

**With the switch off, the output is byte-identical to 1.2.7** — SHA-256 equal
on that track and three synthetic files, on CPU and GPU, with the Adaptive
Apodizer and Hybrid-Phase on and off.

### What to know

- The output ceiling is measured on the finished render, after the filter.
  Taking out a large infrasonic component can move the peak, and the ceiling's
  scalar with it: −0.027 dB on *Mal Bicho*.
- With Hybrid-Phase on, the output above 20 Hz is not identical. Hybrid-Phase
  switches between its linear- and minimum-phase renders at zero crossings, and
  removing a slow wave moves zero crossings, so some switch points move. Where
  one does, that stretch comes from the other branch. The difference is
  Hybrid-Phase's own and it is local: on *Mal Bicho* 89 % of 10 ms windows still
  differ by less than −80 dB relative to the signal.

### Advanced DSP, as a rack

The three checkboxes and the new stage are lit rows now, top to bottom in the
order the pipeline runs them: SUB, AA, PFR, HP. A row lights in its stage's
colour when it is on. Your saved settings carry over.

The line under the heading used to append every option that was on and wrap
wherever it happened to, so the panel jumped each time one was switched. It is
two fixed lines now: the filter on top, the output below, with the tap count
written as the filename writes it — `30M`, not `30.0M`.


## 1.2.7 — Five thousand taps

In the Audio Science Review thread someone asked what thirty million taps buy,
when a windowed sinc of five thousand at 8× already rejects images by more than
230 dB and stays flat to 21 kHz. We measured it on our own filter and on music.
Below 20 kHz: nothing that can be measured. The whole difference lives in the
last kilohertz of the CD band, and it is a question of where the wall at the
cutoff stands and how steep it is — not of how many coefficients it takes.

So the tap slider now starts at 5k.

### One rung, then straight to a million

The ladder is 5k | 1M 5M 10M 30M. Nothing in between, on purpose. Five
thousand taps with the window fitted to the length already reach the image
rejection of the long filters; what more length adds is the steepness of the
wall, and a rung between the two would not be a step anyone could find.

### The window is fitted to the length

Every filter from a million taps up uses a Kaiser window with β = 14. At that
length the window is not what limits the stopband. At five thousand taps it
is: the same β stops at −145 dB. So the 5k filter keeps everything the long
ones have — the band, the cutoff in its middle, the minimum-phase twin — and
changes only the window. For each output rate it takes the smallest β whose
worst stopband, from the source's Nyquist frequency up, measures −220 dB:
what the 30M filter reaches.

| output rate | flat (±0.01 dB) to | stopband | 1M at the same rate |
|---|---|---|---|
| 88.2 kHz | 20 975 Hz | −220.0 dB | −203.4 dB |
| 176.4 kHz | 20 897 Hz | −220.0 dB | −197.4 dB |
| 352.8 kHz | 20 738 Hz | −220.0 dB | −191.0 dB |
| 705.6 kHz | 20 435 Hz | −208.2 dB | −185.2 dB |
| 96 kHz | 22 918 Hz | −220.1 dB | −202.3 dB |
| 192 kHz | 22 833 Hz | −220.0 dB | −196.4 dB |
| 384 kHz | 22 660 Hz | −220.1 dB | −190.5 dB |
| 768 kHz | 22 362 Hz | −192.6 dB | −184.6 dB |

At FS16 five thousand taps cannot get to −220 dB — the same length is spread
over twice the output rate — so the filter takes the deepest stopband that
length allows. It is still deeper than the 1M filter at the same rate.

What it gives up is steepness. The 30M filter is flat to 21 049.9 Hz; the 5k
one is flat to between 20.4 and 21.0 kHz, depending on the multiplier, and
takes about another kilohertz to reach its stopband. Every figure in the table
was measured twice, the second time by code that shares nothing with the
generator; the two agree to 0.05 dB. The full account is in
[docs/12 §9](docs/12-precomputed-fir-matrix.md#short).

### Through the app

The same track converted at 5k and at 30M, everything else equal. In linear
phase the two files agree to the dither below 20.7 kHz, once a 0.002 dB
difference in level is taken out — the true-peak ceiling scaled them slightly
differently, because the gentler wall overshoots less. Above that they differ
at −89 dBFS: the top of the CD band on that recording.

With Hybrid-Phase on they differ more, at −52 dBFS. That is the minimum-phase
branch, and it is what minimum phase is: its phase follows from the whole
magnitude response, so two filters with different walls have different phase
well below them — a third of a sample under 6 kHz, six samples at 20 kHz.

One 3½-minute file, apodizer and Hybrid-Phase on, i9-14900K and RTX 4090:

| | GPU | CPU |
|---|---|---|
| 30M | 28.3 s | 56.1 s |
| 5k | 22.4 s | about 21 s |

At 5k the convolution stops being the cost on either device. What is left is
decoding, the apodizer, encoding and verification.

### A new bundle

**Compact**: the app and all sixteen 5k filters — every multiplier, both
phases, both rate families — in a few megabytes. The filters on their own are
`aura-filters-5k-all-rates.zip`, beside the other packs on the 1.0.0 release.

### Your tap setting survives the update

The app used to remember the slider's position rather than the tap count. With
a rung added below the old bottom, position 3 — 30M — would have come back as
10M for everyone who updated. It now remembers the tap count, and a position
saved by an older version is read against the ladder it was saved with.


## 1.2.6 — The card was idle two thirds of the time

A user with a new RTX 5060 Ti measured his conversions, asked whether to buy
more system memory or a card with more of its own, and sent four session logs
to go with the question. The logs answered something nobody had asked: neither
purchase was the thing holding him back.

Video memory never ran out — no refusals, no fallbacks, and the engine's own
budget for the card read 1433 MB, a figure that comes from a Vulkan binding
limit rather than from how much memory the card has. And the card itself was
working only about a third of the batch. The bottleneck was somewhere nobody
had looked.

### The output file was being encoded four times over

FLAC compresses better when the predictor order is chosen per block rather
than fixed, so every block is encoded with four candidate orders and the
smallest result is kept. That search is worth having and it stays. It was
simply being done one candidate after another, on one thread, while the rest
of the machine waited — on that user's profile it was 35% of the time spent on
a file, more than either convolution pass.

The four candidates never depended on each other, so they are now fitted at the
same time. Measured over six runs on one machine, with nothing changing but
that:

| | runs | mean |
|---|---|---|
| one after another | 91.92 · 93.99 · 93.28 | 93.06 s |
| all four at once | 70.39 · 72.76 · 70.55 | 71.23 s |

Just under a quarter off, on any machine with cores to spare — including the
ones with no discrete graphics at all.

The file that comes out is the same file. A tie between two candidates still
resolves to the same one it always did, and a test encodes the same audio twice
and compares the bytes, because a faster encoder that quietly writes something
else is not a faster encoder.

### Two files at a time was a guess

The engine converted at most two files simultaneously. That number was chosen
cautiously years ago and never measured. Measured now, on six files at 30M taps
with Hybrid-Phase on:

| files at once | batch |
|---|---|
| 1 | 105.2 s |
| 2 | 74.3 / 74.9 s |
| **3** | **66.2 / 65.6 s** |
| 4 | 71.0 / 65.8 s |

Three is about 12% better than two. Four buys nothing and makes the result less
predictable.

So three it is, on machines with enough processor cores to keep three fed —
which is where the measurement was taken, and nowhere else. A small machine
keeps two, because what a small machine wants has not been measured. Neither
number risks anything: a converter that cannot fit waits its turn instead of
taking memory the system needs.

### Still to come

The peak memory a single file needs is still dominated by holding two complete
copies of the output — one for each phase branch — before they are blended.
On a 16 GB machine that is what stops a second file from starting, and it is
the next thing to fix.


## 1.2.5 — A card out of room is an answer, not a dead process

Seventeen crash reports over two days, all the same one: 1.2.4, the 10M bundle,
`Not enough memory left` out of `Device::create_buffer` and
`Queue::write_buffer`, against the buffers a single GPU convolver owns. That is
already the version where the app was supposed to move to the CPU and finish
the file.

1.2.4 made the *absence* of a device a value. It did not make the device's
refusal one. The decision is taken once, before the batch starts, and never
revisited; an allocation that failed part-way through went to wgpu's handler
for errors nobody caught, and that handler panics. The worker died where the
card had merely said no.

What is actually being asked for: 10M taps at ×8 is a 1.25M-tap polyphase
sub-filter, the FFT block rounds up to 2²¹, and a double-single complex value
is 16 bytes. One convolver is 544 MB — the filter spectrum and the two delay
lines at 192 MB, scratch at 256 MB, twiddles at 32 MB, the read-back buffer at
64 MB. A file that fits in memory builds those one at a time. A file long
enough to be processed in segments holds all eight phases at once. The only
thing standing in the way of that was an 11 GB budget written into the source,
a number that never had anything to do with the card in the machine.

Now a refusal is recorded rather than fatal. The convolver reports it, and that
filter is built on the CPU instead. Every place that builds one goes through
the same door, so the policy cannot drift between them. Cancelling still
cancels: it is not a reason to compute on the CPU.

The bank falls back per phase rather than as a whole. The sub-filters are
independent and the two paths are verified equivalent, so a file can finish
with some phases on the GPU and the rest on the CPU without that showing in the
samples that come out.

The refusal is remembered for the rest of the batch as well. Without that, a
card too small for the job would be asked, and would refuse, once per file. It
is forgotten when the next batch starts, because whatever else was holding the
card may be gone by then.

Falling back costs time and nothing else: the CPU path is the f64 reference the
GPU path is verified against.

Still a guess: that 11 GB. Asking the driver what it actually has would spare
the first file a doomed attempt instead of learning the ceiling from a refusal.
The cost today is one failed allocation per batch.


## 1.2.4 — There was no CPU fallback. There was a panic.

The GPU checkbox ships switched on, and acquiring the device ended in
`.expect("No GPU adapter found")`. Put those together and every machine with
no Vulkan and no DX12 adapter — a driver that failed to install or fell over,
a virtual machine, a remote session — died on its first conversion at default
settings. Meanwhile the README promised the opposite: that the app falls back
to the CPU reference path on its own.

The same failure as 1.2.3's silent start, found the same way: while working
out what stands between this engine and a macOS build. A Mac never has that
adapter. The fix was not for the Mac's sake.

Now the absence of a device is a value rather than a death. The reason comes
back as a sentence, and it is cached alongside the success — a machine with no
adapter will not grow one part-way through a batch, and standing up a GPU
instance per file has already cost this project a driver-handle exhaustion
bug.

The decision is made **once**, when a batch starts, and written back into the
settings the whole batch is built from. The worker count, the peak-memory
estimate (it assumes the convolver lives in video memory, so none of it counts
against RAM) and all five places that build a convolver therefore read one
answer instead of finding out separately, per file, after allocating. The same
check covers SPIR-V passthrough: an adapter without it is not a card this
engine can convolve on, and that is a reason to move to the CPU, not to fail
the file with "uncheck GPU in settings".

Falling back costs time and nothing else. The CPU path is the f64 reference
the GPU path is verified against, so what is given up is the wait, not the
result. The wait is real, though, so the app asks once per session before the
batch: the GPU is switched on, this machine has none, run on the CPU? The
badge switches to CPU FALLBACK so the answer outlives the dialog.

## 1.2.3 — The program that said nothing when it would not start

A user downloaded the standard pack, double-clicked the exe on Windows 10, saw
nothing happen, and deleted the folder. Nothing is exactly what they were meant
to see: this is a console application, Windows closes that console the moment
the process exits, and the panic message goes with it. A black rectangle flashes
and the program is gone.

Every failure before there is a window now ends in two things a person can act
on — a message box, and a file they can send back. Three causes are named by
hand, because "install this" is worth more than a Rust error string:

- **No WebView2.** Windows 11 ships the runtime; Windows 10 often does not, and
  the LTSC and N editions never do. Without it the window cannot be made.
  Checked against the registry before it comes to that.
- **A CPU without AVX2.** The released binaries are built for `x86-64-v3` —
  Haswell and newer. On anything older the process died on an illegal
  instruction with no message at all. What is checked is what *this* binary was
  compiled to need, so a baseline build of the same source still runs on the
  same old machine. A vectored exception handler catches the hardware fault
  anyway, in case something reached an AVX2 instruction first.
- **Anything else** — a panic on any thread, or the window failing to open. The
  `expect` that used to end the process silently is gone.

The report goes next to the executable — a portable zip is already open in that
folder — and falls back to the log directory when it is not writable. It carries
the version, what the build needs from a CPU against what the CPU has, the
WebView2 version or `NOT INSTALLED`, and the path of the exe.

Where the cure is known, the box offers it rather than describing it. Yes opens
Microsoft's permanent address for the WebView2 bootstrapper — 1.8 MB, no
restart — or, for a CPU these binaries are not built for, the issue tracker
where a baseline build can be asked for. Deliberately not the product page:
that page offers five downloads and asks the visitor to pick the right one, and
picking wrong is how somebody who has already failed to start this app gives up
for the second time. The same address goes into the report under `next step`,
because a box gets dismissed unread and the file is read later — often by
somebody else. With no browser to open it with, a second box holds the address
as text rather than a button that quietly did nothing.

The hardware-fault handler is the one place that offers no button and prints
the address as text: `ShellExecuteW` starts COM and a process, and doing that
from inside a fault risks a hang, which is worse than the crash it is there to
report.

`aura-engine.exe --selftest` prints the same block and writes the report without
needing a window, a WebView2, or a working program. If it will not start on your
machine, that one command produces the file that explains why.

### Headroom now does what the control says

Headroom has done nothing on any loud master since the first commit. It applied
a gain before the filter, and the true-peak normaliser at the far end sets an
**absolute** ceiling — so whatever came off at the front, it took off that much
less at the back. Two conversions of one file at −3.0 dB and −0.5 dB came out at
the same level: −0.50 dBFS and −15.69 LUFS both.

The control names the ceiling now. `Off` leaves the shipped −0.5 dBTP; −3 dB
puts the output peak on −3.0 dBTP. The normaliser still never raises a quiet
file, so it is a ceiling and not a target — a source already below it passes
through with a gain of exactly 1.0. Nothing is scaled before the filter any
more; the source reaches the convolution as it was decoded.

**This changes delivered files for anyone who had Headroom set to something
other than `Off`.** That is the point, but it is worth knowing before you
re-run a library.

### Also in this release

- **A source wider than stereo is no longer narrowed in silence.** A 5.1 or 7.1
  file used to be accepted, converted as its front pair, and the rest discarded
  with one line in a console nobody reads. The pre-flight now reports channel
  counts, the app asks before converting anything and converts nothing if the
  answer is no, and the output name carries `6ch→2.0`.
- **The envelope release is documented as the frame it actually is.** 8 ms is
  0.69 frames at 44.1 kHz, the `.max(1.0)` clamp binds, and the real release is
  one frame — 11.6 ms. Values unchanged; the documentation was wrong, not the
  code.

## 1.2.2 — The icon the webview was asking for

Someone installed 1.2.1 and the first thing the console said was
``Asset `favicon.ico` not found; fallback to favicon.ico.html``.

WebView2 requests `/favicon.ico` for any document it loads, on its own, whether
or not the markup asks for one. `index.html` never did, and no such file had
ever been in the repository, so Tauri could not resolve the asset and said so.
Nothing was broken by it, but it was the first line a new user read after
unzipping a bundle.

`desktop-app/src/favicon.ico` now exists — six sizes from 16 to 128, built from
`icons/icon.png` — and `index.html` links it explicitly, so it does not look
unused to the next person tidying the tree.

Also: `decide_apodizer` is now `#[cfg(test)]`. It is a thin wrapper that takes
only the plan out of `decide_apodizer_verdict`; all eighteen of its callers are
tests, while the conversion path calls the verdict directly because the queue UI
shows the refusal note. In a release build the wrapper was dead code, and every
build said so. The decision ladder moved to the docstring of the function that
actually implements it.

The console window stays on purpose: it is the live audit log, and the README
describes it as one. Every line it prints also goes to the session log file, so
nothing is lost by ignoring it.

## 1.2.1 — The onset cache stops trusting a file name, and the stage is described as what it is

An outside test on a synthetic file (square → sine → square) turned up two
separate things, and they were worth keeping apart.

**The sidecar cache identified a file by its name.**
`hpss_native::generate_and_save` writes its analysis to
`<stem>.onset_envelope.json` next to the source and returned early whenever
that file was already there and carried the current detector version. Length,
timestamp, content — none of it was checked. So: edit a source in place, keep
the name, convert again, and the converter silently applied the previous
audio's switch points to the new audio. That is exactly what happened to the
outside tester: the file had grown by 694 samples and differed throughout, and
the log said `Envelope cached`. It reproduces end to end — convert a file, swap
different audio in under the same name, convert again.

The sidecar now carries `"source_fingerprint"`, an FNV-1a hash of the samples
handed to the detector, and the cache counts only when both the version and the
fingerprint match. Hashing samples rather than size and mtime buys two things:
the fingerprint survives a copy or a restore from backup, and it changes when
an earlier stage — headroom, apodizer — alters what the detector actually sees.
Cost is bounded by construction: both ends of each channel verbatim plus a
fixed number of strided probes, so a 38-minute 192 kHz source costs the same
microseconds as a 3-minute one. A mismatch is logged with its reason. Five unit
tests cover the fingerprint and the JSON key order — `envelope` has to stay
last, because `load_external_envelope` finds the end of the array with
`rfind(']')`.

**The second one is not a bug but a wrong description, and it matters more.**
The stage was documented as putting minimum phase "where the filter would
ring". It does not do that. The detector measures the rise of percussive energy
between analysis frames — the *start of a sound*. It knows nothing about the
filter's transition band or how much signal energy lands in it. On music the
two criteria almost always coincide, because a drum hit is both a beginning and
a broadband event; separate them deliberately and a steady square wave with
arbitrarily steep edges produces no trigger at all, because nothing begins.

The wording is corrected in the README, `01-architecture.md`,
`05-converter-pipeline.md`, `07-audiophile-features.md`, the checkbox tooltip
and the module header. [`06-hybrid-phase-proof.md`](docs/06-hybrid-phase-proof.md)
§1 gains a table of where the two criteria diverge, plus the known blind spot
that follows from the gate itself: the threshold is `1.5 × causal RMS` with
divisor `min(i+1, context)`, so for a lone spike at frame *i* the gate-to-flux
ratio is `1.5/√(i+1)` — above one at frames 0 and 1. An attack at the very
start of a file raises the bar above its own value and does not fire, even when
it is the largest event in that file.

Figures that had drifted from the code were corrected in the same pass:
pre-roll is 1–2 analysis frames (~12–23 ms), not a 15 ms cos² fade; the
envelope grid is ~86 Hz at 44.1 kHz, not 100 Hz; hold is 25 ms and release
8 ms, while 20 ms is `min_cooldown`, not a fade length.

A criterion based on measured pre-ringing is possible, and offline it is even
cheap: both branches are rendered in full before the blend, so the ringing does
not need predicting — it can be measured as their difference. It is not
implemented in this release. Until it is, the stage is named after what it
does.

## 1.2.0 — Ready-to-run bundles, and a build that knows what it has

Getting started used to cost four steps and a guess. You downloaded a 6 MB app,
hit "missing filter", worked out which of five packs you needed, downloaded a
gigabyte of it, and extracted it into the right subfolder. Meanwhile the
interface always opened on 30M · FS8, hardcoded in the markup — so the first
thing anyone with a smaller pack saw was an error.

- **Three bundles: the app with its filters already inside.** Starter (1M,
  ~134 MB, every multiplier FS2–FS16), Standard (10M, ~326 MB, FS8) and
  Reference (30M, ~966 MB, FS8). Unzip, run, drop a track. Each carries **both
  phase types** — Hybrid-Phase renders the minimum-phase branch in full, and
  half a pair would fail — and **both rate families**, so 44.1 and 48 kHz
  sources behave alike. Unzip more than one into the same folder and the filter
  files merge: one app, every tap count you have, which is the honest way to
  hear what a tap count buys. Built by
  [`tools/make-bundles.ps1`](tools/make-bundles.ps1).
- **The app opens on the filter that shipped with it.** At startup it asks the
  backend which blobs are on disk and starts on the largest tap count it found,
  at FS8. Settings you chose yourself are left alone; they are only moved if
  the blobs behind them are gone, and then the status line says so and the
  correction is saved, so it happens once. Slider positions with nothing behind
  them are struck through — the slider still reaches them, and selecting one
  names the download that would add it and opens it on click. With no filters
  at all, a panel says so before you queue anything.
- **Availability is asked of the resolver, not guessed.** The inventory walks
  the tap-count × output-rate grid through `find_precomputed_filter` itself.
  A directory scan would have been a second, subtly different definition of
  "available", and the two would drift apart at the first naming change —
  leaving the interface offering a setting that then fails. A test pins them
  together cell for cell, and 14 more cover the decision table
  ([docs](docs/12-precomputed-fir-matrix.md#inventory)).
- **The marks stay advisory.** Whether a conversion may run is still decided by
  the per-file pre-flight check, from real source rates — only it knows whether
  a queue is 44.1 or 48 kHz material. Someone holding just the 30M 44.1 kHz
  pack sees FS8 as available, which is true for their CDs and false for their
  48 kHz files; the per-file check is what draws that line. And a missing
  filter still stops the conversion rather than substituting another one.

## 1.1.0 — Self-contained builds, and an engine that stops instead of substituting

Full technical record for the encoder: [`docs/17-flac-encoding.md`](docs/17-flac-encoding.md).

- **FLAC is now encoded natively — no ffmpeg, no subprocess.** Encoding was
  the last stage that shelled out to an external binary, which made a
  genuinely self-contained build impossible: a portable package either carried
  a ~220 MB GPL ffmpeg just to write FLAC, or refused to produce output. The
  new encoder ([`flacenc`](https://crates.io/crates/flacenc), Apache-2.0)
  writes frames to disk as they fill, so the segmented pipeline keeps its
  bounded-RAM guarantee — neither the PCM nor the encoded frames are ever held
  whole.
- **High rates required two things the crate could not do.** It refuses
  sample rates above 96 kHz — a limit of the crate, not of the format, which
  allows up to 655350 Hz — so every frame header is rebuilt with the true rate
  and STREAMINFO is patched. And it fits one fixed LPC order per file where
  the reference encoder searches per block; since frames are driven from here,
  that search now happens here.
- **Compression settings were measured, not assumed.** Raising the LPC order
  turned out to make files *larger* — the autocorrelation of 8× oversampled
  audio is ill-conditioned. Benchmarking on real converted material set the
  candidate orders and moved the block size from 4096 (a 44.1 kHz choice,
  11.6 ms at 352.8 kHz) to 8192, cutting output by 14.5 % against the crate
  defaults. Lossless throughout: this is a size difference, never a sample
  difference.
- **A missing filter now stops the conversion.** Two paths used to degrade
  silently: the post-FIR stage let the plain resampler's output go downstream,
  and Hybrid-Phase skipped its blending — while the interface still said
  "30M Taps" and the filename still carried `Kaiser 30M · HP`. The file
  appeared, looked right, and misdescribed itself. Both are now hard errors,
  reported per file, naming the exact blob and every directory searched for it.
- **And it says so before you wait.** Adding files runs a pre-flight check:
  source headers are read (no decode), target rates worked out per rate
  family, and every needed blob confirmed present. If one is missing the batch
  does not start, a native dialog explains which file and why, and offers the
  download of the release pack that contains it.
- **Portable layouts resolve filters correctly.** Blobs next to the executable
  were only found when the working directory happened to be right, so
  launching from a shortcut silently skipped the million-tap stage — the one
  failure mode a listener cannot see.
- **The status line is visible again.** The window height had been pinned to a
  hardcoded value from JavaScript, overriding the Tauri config; as the
  interface grew, the status line was pushed past the bottom edge of a panel
  that clips without scrolling, and looked as though it had been removed. The
  window is now measured against its own content at startup and fitted to it.

## July 2026: Adaptive Apodizer v3 — Source Forensics

Full technical record: [`docs/14-adaptive-apodizer-v3.md`](docs/14-adaptive-apodizer-v3.md).

- **The detector now measures instead of guessing.** The pre-ring burst's
  dominant frequency is estimated per attack (FFT of the −9…−3 ms window,
  median across attacks with an agreement gate); the corrective cutoff lands
  just below the measured source-filter edge instead of one of three preset
  buckets. Severity selects filter depth (β=24 strong / β=14 mild, shorter
  time-domain signature); taps scale with the container rate.
- **Fake hi-res is unmasked.** A Welch spectral-cliff detector finds the
  brick-wall signature (≥20 dB inside 1/12 octave with only a noise floor
  above); a cliff well below a hi-res container's Nyquist means an upsampled
  44.1/48 kHz master, and all analysis then runs against the *original*
  Nyquist. Previously all >48 kHz sources were skipped outright.
- **Mirror-image alias probe.** A bad upstream resampler leaves images of the
  content above the original Nyquist (a tone at f gets a twin at 2·Ny−f); the
  spectral shape below each candidate legacy Nyquist is compared, per segment,
  with the mirrored band above it. Images correlate bin-for-bin — honest
  hi-res never does — and are removed regardless of the pre-ring verdict.
- **Low-transient material** (ambient, legato strings) is now handled by the
  spectral evidence alone, gently — and never inside a true hi-res
  container's own ADC band.
- **Honest refusals.** A cliff without pre-ring on transient-rich material
  means a minimum-phase or already-apodized source: diagnosed in the log,
  audio untouched. Direct post-ring detection is deliberately not attempted
  (it is ill-posed — post-ring hides inside each attack's own HF decay).
- **Field-calibrated on real material**: quorum accepts strong evidence from
  fewer attacks (album consistency), ring readings below 0.86×Nyquist are
  distrusted as lossy pre-echo/spectral tilt (preset-bucket fallback), and
  the cutoff floor is fixed at 0.816×Nyquist (18 kHz @ 44.1k). Verified in
  both toggle states with bit-perfect output checks; seven new tests pin the
  detectors and the decision logic.
- Expected side effect, documented: the minimum-phase apodizer can raise
  inter-sample peaks on heavily limited masters; the −0.50 dBTP output
  normalizer holds the target.

## July 2026: First Public Release

Repository opened at [github.com/ToxaDev/aura-engine](https://github.com/ToxaDev/aura-engine).

- **Filter blob resolution fixed for cloned checkouts.** `find_precomputed_filter`
  previously looked one directory level too shallow relative to the exe and
  compensated with a hard-coded developer path; it now resolves
  `fir-optimizer/output/` at the repo root, supports an `AURA_FILTER_DIR`
  environment-variable override, and falls back to the working directory.
- **Specs-line tap presets corrected** (`ui.js`): the header now displays
  1M/5M/10M/30M, matching what the backend actually loads (previously showed
  stale 4M/16M labels for the middle presets).
- **Legacy batch mode presets corrected** (`optimize.py`): generates
  1M/5M/10M/30M — names the runtime can actually resolve.
- **Documentation translated to English** (changelog, DSP manifesto,
  hybrid-phase proof, audiophile features, auditor guide, fir-optimizer README)
  and a visual signal-path reference added at `docs/index.html`.
- **Project metadata**: PolyForm Noncommercial 1.0.0 license (free for
  noncommercial use; commercial rights reserved by the author), CI (cargo
  check + test on Windows),
  contributing guide, trimmed `fir-optimizer/requirements.txt` to the packages
  the scripts actually import.

## July 2026: Hardening Pass — Correctness, Quality, Polyphase, Branch Cleanup

Full technical record: [`docs/13-pipeline-hardening-2026-07.md`](docs/13-pipeline-hardening-2026-07.md).

### DSP Correctness
- **OLA latency alignment.** Added `output_latency()` to the `DspProcessor` trait (CPU: 2× block size, GPU: 1× block size); trim/flush now derives latency from this method instead of hard-coding a single block. Fixes ~85 ms of leading silence and tail truncation on the CPU path, and desynchronisation between hybrid-phase branches.
- **Dither** — independent RNG per channel; output clamped to ±(1 − q_step) after quantisation.
- **`to_minimum_phase`** — DC-gain renormalisation after the tail fade.
- **Decoder** — gapless trim for MP3/AAC (delay/padding metadata), error logging instead of a silent abort, warning when more than 2 channels are present.
- **Encoder** — broken FLAC file is deleted on ffmpeg cancellation or failure.
- **GPU** — returns `Err` instead of panicking when `SPIRV_SHADER_PASSTHROUGH` is absent; spurious placeholder-filter generation removed.

### Audio Quality
- **Stereo-linked hybrid-phase switching** — a single zero-crossing point on the mid-difference signal governs both channels; L and R now switch synchronously with no inter-channel phase offset.
- **Onset envelope** computed via Catmull-Rom interpolation; the `.onset_envelope.json` cache is versioned.
- **Adaptive apodizer** — pre-ringing detector operates in the time domain (Nyquist-band burst *before* an attack transient) rather than using a band-energy heuristic; a "clean" verdict applies a static preset.

### Polyphase Path
- Brought to parity with the standard path: filter-matrix resolver, dither, bitwise verification, parallel phase processing (rayon on CPU), shared `run_polyphase_pass` helper.
- Exposed via the **Polyphase FIR Resampling** checkbox in Advanced DSP (locked during conversion, consistent with other options).
- Free of the ~0.4 s trailing silence inherent in the OLA approach (by construction, output length = input length × L).

### UI and Accuracy
- Tap presets corrected to **1M / 5M / 10M / 30M** (aligned with the actual matrix files; the previous 4M/16M labels were loading the 5M/10M matrices).
- `AA` / `Apod` tags in output filenames now reflect the processing that was actually applied, not the UI setting.
- Final job status distinguishes `converted` from `skipped`.

### Build and Branch Structure
- `Cargo.toml` — `[profile.release]` configured with fat LTO and `codegen-units=1`.
- `build.rs` compares SPIR-V bytes before copying; `start.bat` skips cargo when the binary is newer than its sources (instant launch without a rebuild).
- Branch `converter-only` reduced to the converter: removed the legacy standalone engine (`Rust/`), `Py/`, `chrome-extension/`, plotting scripts, root report generators, and outdated documentation describing a non-existent player. All docs rewritten to match the actual codebase.

---

## April 2026: Major Architectural Update — DSP Converter

### Audio Core and Filters (FIR Optimizer)
- Introduced an ideal 128-bit (quad-precision IEEE 754) math engine for Windows. Due to MSVC compiler limitations with `np.longdouble`, the ideal sinc-impulse and Kaiser-window calculation logic was rewritten using the `mpmath` library at maximum precision (`dps = 38`).
- Introduced a multiprocessing system that distributes the workload of generating millions of taps across all CPU threads. The largest reference filter at **30 million taps** is generated in a matter of minutes.
- Filter presets rebuilt to professional high-end industry standards: **1M, 4M, 16M, and Maximum 30M** taps.

### Rust Backend
- Improved automatic output-file naming: when the source track already has a sample rate above 48 kHz (Hi-Res class), adaptive apodizing (AA) is skipped in hardware, and the AA tag is no longer incorrectly appended to the output FLAC filename.
- Enforced strict 24-bit encoding for FLAC output (no experimental flags).

### User Interface
- The converter specs line at the top now reads the preset array correctly. Instead of the buggy positional display (e.g. `0.003K`), it shows human-readable values such as `1.0M Taps`, `4.0M Taps`, etc.
- Added color-coded **Smart Badges** to the job queue: purple `[HP]` for hybrid phase and green `[AA]` for adaptive apodizing.
- The remove/cancel button redesigned from round to rectangular (matching the badge border-radius), with a vector SVG cross icon. While a job is pending, the button is nearly transparent (15% opacity), brightening to a bold red accent on hover.
- A semi-transparent vertical divider line (`.file-item-divider`) added between the remove button and the badge block.
- The default Chrome/WebKit scrollbar replaced with a custom one: fully transparent track, 6 px teal thumb — consistent with AuraEngine's premium dark theme.
