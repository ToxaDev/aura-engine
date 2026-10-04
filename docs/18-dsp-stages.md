# 18. The Advanced DSP stages

Everything in the **Advanced DSP** rack, what it does, and why it is or is not
switched on when you install the app.

The rack is three plates, and inside each one the stages stand in the order the
pipeline actually runs them:

| Zone | Runs on | Stages |
|---|---|---|
| **1 Source** | the file as decoded, before the filter sees it | `DC` Declip · `ISP` Intersample Peak Correction · `SUB` Subsonic Filter · `AHR` Adaptive Headroom |
| **2 Conversion** | the filter itself, and the phase it is rendered in | `AA` Adaptive Apodizer · `PFR` Polyphase FIR Resampling · `HP` Hybrid-Phase · `αHP` Continuous Alpha · `TFS` TFS Phase |
| **3 Output** | the finished render | `XTC` Crosstalk Cancellation · `ALB` Album Level |

The zones are not a second list kept in step by hand — they are slices of one
chain, and that same chain is what the row under each file in the queue draws,
and what the badges under the player show while a track plays
([19 — The Player and Internet Radio](19-player-and-radio.md)).

**A fresh install opens with `DC`, `ISP`, `SUB`, `AHR`, `AA`, `PFR`, `TFS` and,
since 1.5.0, `ALB` on.** The three that stay off are the three that need a
decision from you:
Hybrid-Phase and Continuous Alpha, because TFS occupies the same slot and costs
half the time; and XTC, because it cannot run at all until you have measured
your listening triangle.

Updating from 1.2.x lands in the same place for the stages 1.3.0 added:
settings saved before them don't mention `DC`, `ISP`, `AHR` or `TFS`, so those
four come up on, while everything you had set yourself stays as you left it.
Switching the four off gives you the 1.2.9 chain back.

Two exceptions, since 1.3.4. If you had Hybrid-Phase on, TFS stays off: the
two can't run on one file, and the one you chose wins. And `PFR` comes up on
once, even if your settings say off. It shipped off before 1.3.0, so an "off"
saved by 1.3.3 or earlier is most likely the old default rather than a choice.
Without it a file takes the standard route, where TFS stands down and a long
track has no segmented route and can be refused for memory. The two routes
measure the same in the audio band ([05 §10](05-converter-pipeline.md#known-issues)).
Switch it off again if you want; from 1.3.4 on that choice is kept.

Every stage that fires writes its token into the output filename and into the
`AURA_CHAIN` Vorbis comment, so a converted file says what was done to it
without anyone having to remember. Album Level is the one exception: it changes
no sample's shape, only the level the ceiling sets, so it has no filename token;
the file carries it in an `AURA_ALBUM_GAIN` tag instead.

---

## `DC` — Declip

A master that was pushed into the rail came back with its peaks flattened. The
flat top carries no information about how high the wave was going; the flanks
either side of it do.

![What clipping leaves behind](media/dsp-declip-idea.png)

Declip finds those flat runs and rebuilds the arc over them by constrained
autoregressive interpolation. The rail is treated as a **lower bound**, not as a
value: the reconstruction is required to come out at or above the level the
master kept, never below it, because the one thing the file does tell us is that
the wave was at least that high.

Here is the longest flat run in a real track — 57 samples, 1.29 ms — with the
arc the repair puts back:

![A rebuilt peak, +6.02 dB over the rail](media/dsp-declip-repair.png)

**It refuses more often than it fires.** A run shorter than 17 samples is a
limiter working, not a clipper, and undoing a limiter is a different job with a
different answer — so the stage stands down and says so. On a 2014 Santana
master, twelve tracks, not one run reached the threshold.

The output ceiling takes the rebuilt peaks back down to a safe level, so a
declipped file arrives quieter than the same file without the stage. That is
the reconstruction being paid for, not a bug — level-match before comparing.

**It leaves lossy sources alone (since 1.5.0).** An MP3, AAC, Vorbis or Opus
decoder puts out samples above full scale where the codec's rounding
overshoots — the Audio Science Review thread counted them in 40 of 58 MP3s.
Those are the codec's, not a clipped master's, but from 1.2.9, when the decoder
stopped clipping them, Declip could take them for one and rebuild them: on 25
lossy files it did so on four. It now asks the decoder, leaves such sources as
decoded, and the log says why. On a live radio stream Declip does not run at
all: it needs the whole track.

## `ISP` — Intersample Peak Correction

A waveform is not only its samples. Between them the reconstructed signal can go
higher than any sample does — and higher than full scale — which is where a DAC
or a downstream encoder clips a file that "never exceeds 0 dBFS".

![Intersample peaks reaching +3.68 dBTP between samples](media/dsp-intersample-peaks.png)

ISP scans at 4× with a Lanczos kernel, finds the true-peak overs, and applies
the smallest correction that clears them. Spans that Declip has just rebuilt are
left alone: they are deliberately above the old rail, and holding them down is
the output ceiling's job, not this one's. Since 1.5.1 the same goes for a
cluster whose correction would reach a sample that is itself above full scale
in the decode — an MP3 normalised hot, a loud stream. That is the source's own
level, and the output level lowers it whole; carving it, as 1.5.0 did, made a
hot MP3 rasp. Integer sources (CD, 24-bit) cannot have such a sample, so for
them nothing changed.

ISP has a second half, at the other end of the chain. A master that sits on
its limiter ceiling comes back from the reconstruction with intersample peaks
above full scale — up to +2.6 dBTP on one test track — and the output ceiling
is −0.5 dBTP. One gain for the whole file would take the track down 2–3 dB for
the sake of a few milliseconds, so instead a limiter dips the gain for about
1.5 ms around each over, both channels together, and nowhere else. It steps
aside and leaves the job to the global gain if any over needs more than 6 dB,
or if the dips would cover more than 5 % of the file. On a clipped CD it made
2,553 dips covering 4.7 % of the samples and cost the file 0.1 dB instead of
about 2; the loudness range is the same either way. Its gain dips put a little
energy back below 20 Hz, which is why, since 1.3.3, the subsonic filter runs a
second time after it.

## `SUB` — Subsonic Filter

A linear-phase FIR high-pass at the source rate, flat from its corner up and at
least 100 dB down from half that corner to DC. Click the chip on the row for
20, 15 or 10 Hz; a new install opens on **15 Hz**, which leaves the lowest organ
pipe (16.35 Hz) untouched.

It is there for the infrasonic content and slow drift a disc can carry, which
removing the DC offset does not touch — protection for speakers, not a sound
improvement. Linear phase is the point: nothing above the corner moves in level
or in time. It costs look-ahead — half the filter's length — which the
converter and the player's preparation of a file have for free, and which a
live radio stream pays as a short delay before its first sound (0.43, 0.54 and
0.77 s for 20, 15 and 10 Hz at 44.1 kHz).

Since 1.3.3 it runs twice. The pass in this zone takes the disc's rumble out;
a second pass, the same filter at the output rate, runs after the ISP output
limiter. That limiter trims overs with short gain dips, and bass times a
dipping gain puts energy back below the corner — a shelf about 40 dB under
the bass, where the filter promises 100 dB down. The second pass keeps that
promise for the file itself. The rack still shows one `SUB` row.

## `AHR` — Adaptive Headroom

The **Headroom** control names the ceiling the output is normalised to. Adaptive
Headroom decides whether to honour it on this particular file:

| What the source looks like | What AHR does |
|---|---|
| peak already sits below the requested ceiling | keeps the shipped −0.5 dBTP — the cut would cost level for nothing |
| ENOB above 20 bits and peak below −1 dBFS | keeps −0.5 dBTP — a pristine high-bit-depth master |
| anything louder | lowers the ceiling as asked |

So it is a veto on your setting, not a replacement for it — which is why the
badge reads `← Headroom` and will not light while Headroom is on **Off**. With
nothing requested there is nothing to veto, and the stage does not run at all.

Whichever way it goes, the reason lands in the `AURA_HEADROOM` tag along with
the measured peak and ENOB.

## `AA` — Adaptive Apodizer

Per-file forensics on the source: it locates the brick-wall edge, measures
ADC or resampler pre-ringing on real musical attacks, and sets the apodizing
cutoff from what it measured. Clean, minimum-phase-mastered and genuinely
hi-res sources are left untouched. Full detail in
[14 — Adaptive Apodizer v3](14-adaptive-apodizer-v3.md).

**It stands down at a codec's wall (since 1.5.0).** On a lossy file the cutoff
it would place can sit at or above the frequency where the codec has already
cut everything. A filter there reaches no ringing and only turns the phase,
while the badge said it had run; it now declines, with that reason. Of 12 test
files this changed one, a 192 kbps AAC stream, and only in phase. On a live
radio stream it does not run: its analysis needs the whole track. While it is
on, the static Apodizing presets are locked off, so a stream plays without an
apodizer; switch the Adaptive Apodizer off and a static preset runs on streams
at 44.1 and 48 kHz.

## `PFR` — Polyphase FIR Resampling

Your FIR **is** the resampler — there is no second resampler in the chain. The
whole conversion is one exact convolution of the original samples against the
designed filter. Integer ratios only.

## `HP` — Hybrid-Phase · `αHP` — Continuous Alpha

Linear phase through sustained passages, minimum phase across detected attacks,
switched at a zero crossing with a 1 ms fade. Continuous Alpha
replaces that hard switch with a per-sample crossfade driven by the HPSS
envelope, and needs Hybrid-Phase to be on.

Both are **off by default**: they double the processing time, and TFS renders
the same trade differently. Hybrid-Phase and TFS cannot both run on one file —
the rack shows that as `✕ TFS` / `✕ HP`. See
[06 — Hybrid-Phase Proof](06-hybrid-phase-proof.md).

## `TFS` — TFS Phase

Linear phase below 1.5 kHz, minimum phase above 4 kHz, blended across the band
between. The idea is to spend the phase budget where it is audible: low
frequencies keep the exact timing a linear-phase filter gives them, while the
treble is rendered with no pre-ringing ahead of a transient.

The magnitude response is not part of the trade — it matches the linear-phase
render to **±0.000003 dB** across 20 Hz–20 kHz. Only the phase differs.

TFS is derived from the linear- and minimum-phase pair of your chosen filter
size, so it needs **both** blobs on disk; the bundles ship both. The first
conversion at a given size and rate derives the TFS filter and caches it — at
30M taps about 12 s and 4.5 GB of RAM; after that the cached blob is picked up
instantly. Until 1.5.0 the derivation ran on a transform as long as the filter,
which cut its ringing short (stopband −94 to −126 dB) and, at 1M and 30M taps,
wrapped the end of it around to the start; since 1.5.0 it runs on a transform
twice as long, and the stopband is −193 to −231 dB. Files converted with TFS
from 1.3.0 on therefore come out slightly different under 1.5.0 — by at most
−120 dBFS in the audio band on music; the filename tag is still `TFS`. TFS
applies on the Polyphase FIR path; on the standard path the row reports that
it stood down.

## `XTC` — Crosstalk Cancellation

Over speakers, each ear hears both of them. XTC cancels what each speaker sends
to the *wrong* ear, so the stereo image is no longer bounded by the speakers
themselves.

**This is the one stage in the product that deliberately colours the signal.**
Everything else here is either a repair or a rendering choice; this one rewrites
the file for one geometry. It is off by default and cannot be switched on until
you have entered your listening triangle:

![The listening-geometry window](media/dsp-xtc-geometry.png)

Guessed distances do not cancel less — they cancel the wrong thing and reinforce
what the filter exists to remove, so the app refuses rather than assuming. The
same check is repeated on the Rust side, so a hand-edited setting cannot slip
past it.

What it buys, and what it costs when you move:

![Suppression across span angle, and against head displacement](media/dsp-xtc-suppression.png)

Both panels are computed from a symmetric free-field model of the triangle, not
measured in a room. Read the right-hand panel before deciding whether this is
for you: **20 mm of head movement costs about half the cancellation.** It is a
one-chair effect, for speakers, never for headphones.

## `ALB` — Album Level

The true-peak ceiling is one gain per file, and it only ever cuts. How deep it
cuts depends on what the chain did to that file: how high Declip rebuilt the
peaks, what the phase engine and XTC added, and whether the ISP output limiter
could hold the overs locally or left the whole file to the ceiling. Converted
one by one, the tracks of an album can come out with their balance changed. In
[Discussions #5](https://github.com/ToxaDev/aura-engine/discussions/5) a DR4
album lost 11.9 dB on one track and 3.2 dB on another.

Album Level converts an album as one. The queued files that share a folder
**and** an Album tag are an album. Before the first of them is written, every
track is prepared through the same source stages with the same settings, and a
short render of the chain estimates what the ceiling will find on it. Each
track would take its own cut by the usual rule; the album takes the deepest one,
and every track gets that one gain in front of its output limiter. The limiter,
the subsonic guard and the ceiling then run as before, and take whatever the
estimate missed.

Measured on a 12-track album (50.8 minutes, FS×2, 1M taps, the default rack):

| | per-track cuts | spread against the CD |
|---|---|---|
| Album Level off | 0 … −6.18 dB | 6.15 dB |
| Album Level on | one: −6.17 dB, set by the loudest track | 0.02 dB |

The same with the chain from #5 (`DC·ISP·SUB10·PFR·HP`): 6.19 → 0.02 dB; at FS×8,
6.18 → 0.03 dB.

**What it leaves alone.** A file without an Album tag, or the only queued track
of its album, converts exactly as before: with Album Level on and off such files
came out with the same peak and RMS to 0.001 dB, and differed only by their two
independent dithers. The player plays each track at its own level.

**What it costs.** A wait: no track of an album is finished before every track
of it has been prepared. The preparation is kept for each track's turn (within
a third of the memory budget), so it replaces the usual one rather than adding a
second; the estimate itself takes 0.4–0.8 s a track. On the album above the
batch took 120 s instead of 96.

**What it cannot do.** The album's level is set by the track that needs the
deepest cut, so an album where one track needs a much deeper cut than the rest
comes out quieter as a whole. That is the price of keeping the balance. It
matters most where Declip rebuilds very tall peaks: one such track can set the
level for all of them. And a track whose overs the limiter holds locally keeps
the small loss its own ceiling takes on the 4× view between samples — up to
0.45 dB at FS×2 and 0.15 dB at FS×8 on this album — unless the album's cut is
deep enough to clear it.

The level, the track that set it and each track's own estimate go into the log
(`[ALBUM]` lines) and into the `AURA_ALBUM_GAIN` tag, for example
`-6.17dB;own=+0.00dB;set_by=…;tracks=12`. Switch `ALB` off to convert track by
track, as before 1.5.0.

---

## What the row under a file means

Each stage draws one badge, in pipeline order, and each badge is one of three
things — it ran, it was reached and declined (the tooltip says why), or it has
not been reached yet. The bit-perfect check sits last, behind a divider: it is a
verdict on the output rather than a stage of the chain.

A stage that declines is not a failure. Most of these are built to refuse on
material that does not need them, and a run where `DC` and `ISP` both stand down
means the master was already clean.

The badges under the player read the same way, with one more state: while a
track starts, a lit badge means that stage is already in the sound you hear.
On a live radio stream `DC` and `AA` are grey — they need the whole track —
and the rest of the rack runs as the stream arrives.
