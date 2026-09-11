# Changelog

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
