# AuraEngine — FLAC Encoding

**Status:** current as of 1.1.0 · replaces the ffmpeg subprocess that every
earlier version used.

## Why this changed

Encoding was the last stage of the pipeline that shelled out to an external
binary. Decoding had been pure Rust (Symphonia) for a long time; output went
through `ffmpeg -c:a flac`.

That single dependency made a genuinely self-contained build impossible. A
portable package either had to carry a ~220 MB GPL ffmpeg build — for the sole
purpose of writing FLAC — or refuse to produce output on a machine that did
not already have one installed. Neither is a good answer for a tool whose
whole point is that you unpack it and it works.

Since 1.1.0 the converter writes FLAC itself, using the
[`flacenc`](https://crates.io/crates/flacenc) crate (Apache-2.0). No external
process is started at any point in a conversion.

## Streaming, not buffering

The encoder is incremental. Frames are encoded and written to disk as they
fill, so the segmented (bounded-RAM) pipeline keeps its guarantee: neither the
PCM nor the encoded frames are ever held whole in memory. A 90-minute file at
768 kHz encodes with the same peak RAM as a three-minute one.

Mechanically:

```
fLaC | STREAMINFO (placeholder) | frame | frame | frame | …
       ↑                                                  
       rewritten at the end, once the MD5, the sample count
       and the frame-size extremes are known
```

The STREAMINFO block sits at a fixed offset (byte 8, 34 bytes long), so the
final pass seeks back and overwrites it rather than holding the stream open.

## Two things the crate could not do unaided

### Sample rates above 96 kHz

`flacenc` refuses to construct a `StreamInfo` above 96 kHz. That is a limit of
the crate, not of the format: FLAC stores the sample rate in a 20-bit field
and tops out at 655350 Hz, and the crate's own public `FrameHeader::new`
accepts the high rates without complaint.

Inside the encoder, `StreamInfo`'s rate is read in exactly one place — to pick
the rate spec written into each frame header. Nothing about the actual coding
(subframes, LPC, Rice parameters, stereo decorrelation) depends on it. So the
encoder is driven with a placeholder rate, and then:

- every frame header is rebuilt with the true rate before the frame is
  written, using the crate's public API; and
- the true rate is patched into the 20-bit field of the STREAMINFO payload.

The output is an ordinary FLAC file. Independent decoders read 352.8 kHz and
384 kHz files with the correct rate, channel count, bit depth and duration,
and validate every frame CRC.

### Per-block predictor search

`flacenc` fits one fixed LPC order for an entire file. The reference encoder
searches the order for every block, and that difference is worth a great deal
on this material — 8× oversampled audio is extremely predictable, and the best
order varies from block to block.

Because frames are driven from AuraEngine's side, the search happens there:
each block is encoded with every candidate order and the smallest result is
kept. `count_bits()` reports what the frame will actually occupy, so this is a
measurement rather than an estimate.

## Settings were measured, not reasoned about

Every parameter below was chosen by benchmarking real converted material
(`cargo test --release encode_compression_ratio -- --ignored --nocapture`),
because the intuitive answers turned out to be wrong.

**Longer predictors are not better here.** The autocorrelation of heavily
oversampled audio is ill-conditioned — the signal occupies an eighth of the
Nyquist band — so beyond a point the quantized coefficients predict *worse*
than a shorter fit. Measured on a 203 s 44.1 → 352.8 kHz track:

| Fixed LPC order | Output |
|---|---|
| 4 | 140.2 MB |
| 8 | 125.0 MB |
| 10 (crate default) | 129.8 MB |
| 16 | 136.2 MB |
| 24 (crate maximum) | 140.6 MB |

**Window choice barely matters.** Tukey α = 0.4 is optimal; rectangular is far
worse (149.2 MB), and mixing several windows into the candidate set buys 0.6 %
for double the encoding time.

**Block size does matter, and 4096 is the wrong default here.** 4096 frames is
a 44.1 kHz choice, where it spans 93 ms; at 352.8 kHz the same count is
11.6 ms, which re-sends the predictor coefficients eight times more often than
necessary. The FLAC streamable subset allows up to 16384 above 48 kHz, and
8192 measured best.

Final configuration and the result of each step:

| Configuration | Output | vs. previous |
|---|---|---|
| Crate defaults | 129.8 MB | — |
| Best fixed order (8) | 125.0 MB | −3.7 % |
| Per-block order search [6, 8, 10, 12] | 114.3 MB | −11.9 % |
| Block size 8192 | **110.9 MB** | **−14.5 %** |

For reference, `ffmpeg -compression_level 8` writes the same audio as 96.0 MB.
The remaining ~15 % gap is structural: the reference encoder also searches
`partial_tukey` and `punchout_tukey` windows, which fit the predictor over
sub-regions of a block, and `flacenc` implements neither.

**This is a size difference, not a quality difference.** FLAC is lossless; the
decoded samples are identical either way. The measurement that proves it: take
a file written by AuraEngine, re-encode its audio with `ffmpeg
-compression_level 8`, and you get 96.0 MB — the same as the old pipeline
produced, from the same samples.

## Verification is unchanged

Every output file is still re-decoded and compared sample-by-sample against
the internal f64 buffer, and anything that fails is renamed `_UNVERIFIED`
rather than silently kept. See [09-audio-auditor-guide.md](09-audio-auditor-guide.md).

One honest caveat: above 655350 Hz — the FLAC specification's own cap —
Symphonia refuses to decode, so it cannot read AuraEngine's own 705.6/768 kHz
output. ffmpeg was the only decoder available for that case. When it is not
installed, the log now says plainly that verification could not run, instead
of marking a perfectly good file `_UNVERIFIED`, which reads as data
corruption.

## Tests

| Test | What it pins |
|---|---|
| `round_trip_is_lossless_at_352k8` / `_at_384k` | Encode → decode → sample-exact equality, with MD5 verification enabled. The production verifier runs with MD5 checking *off*, so a wrong STREAMINFO digest would otherwise slip past. |
| `full_scale_rails_do_not_wrap` | Full-scale input clamps to the extreme 24-bit codes instead of wrapping. |
| `abort_removes_the_partial_file` | A cancelled encode leaves no half-written file that looks playable. |
| `encode_compression_ratio` (ignored) | Manual benchmark against a real file; the settings above are its output. |

## Licensing

`flacenc` is Apache-2.0 and Symphonia is MPL-2.0. Both are pure Rust and
statically linked. No GPL-licensed component is distributed with AuraEngine.
