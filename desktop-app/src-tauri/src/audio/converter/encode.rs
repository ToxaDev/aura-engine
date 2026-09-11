use crate::audio::converter::state::*;
use crate::audio::converter::types::{AudioFile, ConvertSettings};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, FrameHeader, FrameOffset, StreamInfo};
use flacenc::config;
use flacenc::encode_fixed_size_frame;
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};
use rayon::prelude::*;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

/// Frames per FLAC block for the high output rates this converter produces.
///
/// The usual 4096 is a 44.1 kHz choice — there it spans 93 ms. At 352.8 kHz
/// the same count is 11.6 ms, which re-sends the predictor coefficients eight
/// times more often than necessary and fits them over too short a window.
/// Measured on real converted material, 8192 beats both 4096 and 16384.
///
/// The FLAC streamable subset caps the block size at 16384 above 48 kHz, so
/// this stays inside it — hardware players will not refuse the file.
const FLAC_BLOCK_SIZE: usize = 8192;

/// Block size for output at or below 48 kHz, where the streamable subset
/// tightens the cap to 4608. Not reachable from the UI today (every offered
/// rate is higher), but the encoder should not be the thing that breaks if a
/// low-rate path ever appears.
const FLAC_BLOCK_SIZE_LOW_RATE: usize = 4096;

/// Candidate LPC orders. `flacenc` fits one fixed order per encoder rather
/// than searching, so the search happens a level up: every block is encoded
/// with each of these and the smallest result wins.
///
/// The set is chosen by the `encode_compression_ratio` benchmark against real
/// converted material, not by reasoning about it. Longer is not automatically
/// better here — the autocorrelation of heavily oversampled audio is
/// ill-conditioned, and past a point the quantized coefficients predict worse
/// than a shorter fit.
const FLAC_CANDIDATES: &[(usize, f32)] = &[(6, 0.4), (8, 0.4), (10, 0.4), (12, 0.4)];

/// Bit depth of the output stream.
const OUTPUT_BITS: usize = 24;

/// `1 / Q_STEP` from the dither stage. Samples arrive already quantized to the
/// 24-bit grid inside [-1, 1], so scaling by 2^23 lands them exactly on
/// integers — this conversion adds no second rounding of its own.
const SCALE_24: f64 = 8_388_608.0;

/// Largest positive 24-bit code. The negative rail is `-SCALE_24`.
const MAX_24: f64 = 8_388_607.0;

/// Byte offset of the STREAMINFO payload: `fLaC` (4 bytes) + the metadata
/// block header (4 bytes). The payload is written twice — once with
/// placeholders so the frames can start streaming out, and once at the end
/// when the MD5 and the sample count are finally known.
const STREAMINFO_OFFSET: u64 = 8;

/// Serialized size of a STREAMINFO payload (272 bits).
const STREAMINFO_LEN: u8 = 34;

/// Sample rate handed to `flacenc`'s `StreamInfo`, which rejects anything above
/// 96 kHz — a limit of that crate, not of the format: FLAC stores the rate in a
/// 20-bit field and tops out at 655350 Hz, and `flacenc`'s own public
/// `FrameHeader::new` accepts the high rates happily.
///
/// The placeholder never reaches the file. `StreamInfo`'s rate is read in
/// exactly one place inside the encoder — to pick the frame header's rate spec
/// — and nothing about the actual coding (subframes, LPC, rice parameters,
/// stereo decorrelation) depends on it. So every frame header is rebuilt with
/// the true rate before it is written, and the STREAMINFO payload gets the true
/// rate patched into its 20-bit field by [`patch_stream_info_rate`].
const PLACEHOLDER_RATE: usize = 96_000;

/// Bit offsets inside a STREAMINFO payload: 16 (min block) + 16 (max block)
/// + 24 (min frame) + 24 (max frame) puts the 20-bit sample rate at bit 80,
/// i.e. byte 10, byte 11, and the high nibble of byte 12. The low nibble of
/// byte 12 holds the channel count and the top bit of the sample size, so it
/// has to survive untouched.
fn patch_stream_info_rate(payload: &mut [u8], rate: u32) {
    debug_assert!(rate <= 0x000F_FFFF, "sample rate exceeds FLAC's 20-bit field");
    payload[10] = ((rate >> 12) & 0xFF) as u8;
    payload[11] = ((rate >> 4) & 0xFF) as u8;
    payload[12] = (((rate & 0x0F) as u8) << 4) | (payload[12] & 0x0F);
}

/// f64 on the 24-bit grid → 24-bit signed integer code.
#[inline]
fn to_i24(x: f64) -> i32 {
    // The clamp guards the paths that bypass dither (where nothing has
    // constrained the sample to ±(1 − Q_STEP) yet); on the normal path the
    // value is already inside the rails and the clamp is a no-op.
    (x * SCALE_24).round().clamp(-SCALE_24, MAX_24) as i32
}

/// Build output filename
pub fn build_output_name(
    audio: &AudioFile,
    settings: &ConvertSettings,
    src_path: &Path,
    actual_out_rate: u32,
    apod_tag: Option<&str>,
) -> String {
    // ── Base name from source filename (not metadata tags) ──
    let base = src_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // ── Filter window ──
    let window = match settings.win_type {
        0 => "Hamming",
        1 => "Hann",
        2 => "Blackman",
        3 => "Nuttall",
        4 => "Kaiser",
        _ => "Custom",
    };
    let filter_name = if settings.custom_filter_path.is_some() {
        "AURA".to_string()
    } else {
        window.to_string()
    };

    // ── Taps (compact) ──
    let taps_str = if settings.taps >= 1_000_000 {
        let m = settings.taps as f64 / 1_000_000.0;
        if m == m.floor() {
            format!("{:.0}M", m)
        } else {
            format!("{:.1}M", m)
        }
    } else if settings.taps >= 1000 {
        let k = settings.taps / 1000;
        format!("{}K", k)
    } else {
        format!("{}", settings.taps)
    };

    // ── Sample rate (compact) ──
    let rate_khz = actual_out_rate as f64 / 1000.0;
    let rate_str = if rate_khz == rate_khz.floor() {
        format!("{:.0}k", rate_khz)
    } else {
        format!("{:.1}k", rate_khz)
    };

    // ── Source info ──
    let src_rate_khz = audio.sample_rate as f64 / 1000.0;
    let src_str = if src_rate_khz == src_rate_khz.floor() {
        format!("{:.0}k", src_rate_khz)
    } else {
        format!("{:.1}k", src_rate_khz)
    };

    // ── Source width ──
    // A source wider than stereo is narrowed to its front pair, and the file
    // says so rather than leaving that fact in a dialog the user clicked away
    // and a log line they will not keep. Mono and stereo add nothing.
    let width_tag = if audio.channels > 2 {
        Some(format!("{}ch→2.0", audio.channels))
    } else {
        None
    };

    // ── Build tag chain: compact dot-separated ──
    // Format: [AE · 44.1k→384k · Kaiser 10M · f64 · AA · HP · 6ch→2.0]
    let mut tags: Vec<String> = Vec::new();

    // Source → Output rate
    if audio.sample_rate != actual_out_rate {
        tags.push(format!("{}→{}", src_str, rate_str));
    } else {
        tags.push(rate_str);
    }

    // Filter + taps
    tags.push(format!("{} {}", filter_name, taps_str));

    // Precision
    tags.push(format!("f{}", settings.precision));

    // Apodizing — reflects what ACTUALLY ran in prepare ("AA" adaptive,
    // "Apod[-M/-S]" static preset, nothing when the detector skipped),
    // not merely the settings the user had enabled.
    if let Some(tag) = apod_tag {
        tags.push(tag.to_string());
    }

    // Advanced DSP
    if settings.hybrid_phase {
        tags.push("HP".to_string());
    }

    // Last, because it describes what the source was rather than what the
    // engine did to it.
    if let Some(w) = width_tag {
        tags.push(w);
    }

    format!("{} [AE · {}].flac", base, tags.join(" · "))
}
/// Incremental FLAC encoder, native (no external tool). Frames are encoded
/// and written out as they fill, so the segmented (bounded-RAM) pipeline never
/// has to materialize the whole track — neither as PCM nor as encoded frames.
/// `encode_flac` is a thin wrapper around it.
pub struct StreamingFlacEncoder {
    /// `None` once the file has been aborted or handed over in `finish`.
    /// Windows will not delete a file that is still open, so `abort` has to be
    /// able to drop the handle before unlinking.
    file: Option<std::io::BufWriter<std::fs::File>>,
    /// One per candidate LPC order; every block is encoded with each and
    /// the smallest result is kept.
    configs: Vec<flacenc::error::Verified<config::Encoder>>,
    stream_info: StreamInfo,
    framebuf: FrameBuf,
    /// Tracks the running MD5 and the frame numbering that STREAMINFO needs.
    context: Context,
    /// Interleaved samples not yet consumed by a full block.
    pending: Vec<i32>,
    sink: ByteSink,
    output_path: std::path::PathBuf,
    /// The real output rate. `stream_info` carries [`PLACEHOLDER_RATE`].
    sample_rate: u32,
    block_size: usize,
}

impl StreamingFlacEncoder {
    pub fn new(sample_rate: u32, output_path: &Path) -> Result<Self, String> {
        let block_size = if sample_rate <= 48_000 {
            FLAC_BLOCK_SIZE_LOW_RATE
        } else {
            FLAC_BLOCK_SIZE
        };
        Self::with_candidates(sample_rate, output_path, block_size, FLAC_CANDIDATES)
    }

    /// `lpc_order` is exposed so the compression benchmark can sweep it; the
    /// production entry point above pins it to [`FLAC_LPC_ORDER`].
    fn with_candidates(
        sample_rate: u32,
        output_path: &Path,
        block_size: usize,
        candidates: &[(usize, f32)],
    ) -> Result<Self, String> {
        // Compression settings. FLAC is lossless, so these change file size
        // only — never a sample.
        //
        // `flacenc` fits one fixed LPC order per encoder, while the reference
        // encoder searches the order for every block. That search is most of
        // the difference in output size, and since frames are driven from
        // `emit_frame` here, it can be done at this level instead: build one
        // config per candidate order and keep whichever encodes the block
        // smallest.
        let mut configs = Vec::with_capacity(candidates.len());
        for &(order, tukey_alpha) in candidates {
            let mut cfg = config::Encoder::default();
            cfg.block_size = block_size;
            cfg.subframe_coding.qlpc.lpc_order = order;
            cfg.subframe_coding.qlpc.window = config::Window::Tukey { alpha: tukey_alpha };
            // Pick the fixed-predictor order by actually counting the encoded
            // bits instead of estimating entropy from a handful of partitions.
            cfg.subframe_coding.fixed.order_sel = config::OrderSel::BitCount;
            // Frames are driven one at a time from `emit_frame`; the crate's
            // internal thread pool is for its own whole-stream encoder.
            cfg.multithread = false;
            configs.push(
                cfg.into_verified()
                    .map_err(|e| format!("FLAC encoder config rejected: {:?}", e))?,
            );
        }
        if configs.is_empty() {
            return Err("FLAC encoder needs at least one candidate".to_string());
        }

        let mut stream_info = StreamInfo::new(PLACEHOLDER_RATE, 2, OUTPUT_BITS)
            .map_err(|e| format!("FLAC stream info rejected: {:?}", e))?;
        // Equal min/max marks a fixed-block-size stream, which is what
        // `encode_fixed_size_frame` produces (frame headers carry a frame
        // number rather than a sample offset). A shorter final frame is
        // explicitly allowed by the format.
        stream_info
            .set_block_sizes(block_size, block_size)
            .map_err(|e| format!("FLAC block size rejected: {:?}", e))?;

        let file = std::fs::File::create(output_path)
            .map_err(|e| format!("cannot create {}: {}", output_path.display(), e))?;
        let mut file = std::io::BufWriter::new(file);

        // `fLaC` magic, then a last-metadata-block header announcing a 34-byte
        // STREAMINFO.
        file.write_all(&[0x66, 0x4C, 0x61, 0x43, 0x80, 0x00, 0x00, STREAMINFO_LEN])
            .map_err(|e| format!("FLAC header write error: {}", e))?;

        let framebuf = FrameBuf::with_size(2, block_size)
            .map_err(|e| format!("FLAC frame buffer rejected: {:?}", e))?;

        let mut enc = Self {
            file: Some(file),
            configs,
            stream_info,
            framebuf,
            context: Context::new(OUTPUT_BITS, 2),
            pending: Vec::new(),
            sink: ByteSink::new(),
            output_path: output_path.to_path_buf(),
            sample_rate,
            block_size,
        };

        // Placeholder payload — rewritten in `finish` once the MD5, the sample
        // count and the frame-size extremes are known.
        let info = enc.serialize_stream_info()?;
        enc.write_all(&info)?;
        Ok(enc)
    }

    fn serialize_stream_info(&mut self) -> Result<Vec<u8>, String> {
        self.sink.clear();
        self.stream_info
            .write(&mut self.sink)
            .map_err(|e| format!("FLAC stream info serialization failed: {:?}", e))?;
        let mut payload = self.sink.as_slice().to_vec();
        if payload.len() != STREAMINFO_LEN as usize {
            return Err(format!(
                "FLAC stream info is {} bytes, expected {}",
                payload.len(),
                STREAMINFO_LEN
            ));
        }
        patch_stream_info_rate(&mut payload, self.sample_rate);
        Ok(payload)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        let file = self.file.as_mut().ok_or("encoder already finished")?;
        file.write_all(bytes)
            .map_err(|e| format!("FLAC write error: {}", e))
    }

    /// A cancelled or failed encode must never leave a half-written FLAC at
    /// the destination path — it is indistinguishable from a good file until
    /// the user tries to play it.
    fn abort(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.output_path);
    }

    /// Replace a freshly encoded frame's header with one carrying the real
    /// output rate. The subframes move across untouched — this rewrites the
    /// header only, and both CRCs are computed at write time.
    fn retag_frame_rate(&self, frame: Frame, frame_number: usize) -> Result<Frame, String> {
        let (header, subframes) = frame.into_parts();
        let bits = header
            .bits_per_sample()
            .ok_or("FLAC frame header carries no sample size")?;
        let rebuilt = FrameHeader::new(
            header.block_size(),
            header.channel_assignment().clone(),
            bits,
            self.sample_rate as usize,
            FrameOffset::Frame(frame_number as u32),
        )
        .map_err(|e| {
            format!(
                "FLAC frame header rejected at {} Hz: {:?}",
                self.sample_rate, e
            )
        })?;
        Frame::new(rebuilt, subframes.into_iter()).map_err(|e| format!("FLAC frame rebuild failed: {:?}", e))
    }

    /// Encode one block of interleaved samples and append it to the file.
    fn emit_frame(&mut self, interleaved: &[i32]) -> Result<(), String> {
        self.framebuf
            .fill_interleaved(interleaved)
            .map_err(|e| format!("FLAC frame fill failed: {:?}", e))?;
        self.context
            .fill_interleaved(interleaved)
            .map_err(|e| format!("FLAC context fill failed: {:?}", e))?;

        let frame_number = self
            .context
            .current_frame_number()
            .ok_or("FLAC frame numbering desynchronized")?;
        // Per-block order search: encode with every candidate and keep the
        // one that costs the fewest bits. `count_bits` is what the frame will
        // actually occupy, so this is a measurement, not an estimate.
        // The candidates are independent: the same block fitted four ways,
        // of which only the smallest is kept. Fitting them in sequence made
        // this search about three quarters of the encode stage, and the
        // encode stage the most expensive single thing in the pipeline on
        // real material -- ahead of either convolution pass, and on a machine
        // whose GPU was idle two thirds of the batch.
        //
        // The tie-break must not move. In sequence the FIRST candidate won a
        // tie, because `bits < best_bits` is strict and FLAC_CANDIDATES is in
        // the order the compression benchmark chose. `collect` preserves that
        // order and the fold below keeps the earliest minimum, so the file
        // written here is byte-for-byte the file the serial search wrote.
        let encoded = self
            .configs
            .par_iter()
            .map(|cfg| {
                encode_fixed_size_frame(cfg, &self.framebuf, frame_number, &self.stream_info)
                    .map(|frame| (frame.count_bits(), frame))
                    .map_err(|e| format!("FLAC frame encode failed: {:?}", e))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let frame = encoded
            .into_iter()
            .reduce(|best, cand| if cand.0 < best.0 { cand } else { best })
            .expect("at least one candidate config")
            .1;

        // Must happen before the size accounting below: the true rate needs a
        // 16-bit "sample rate in daHz" field in the header that the
        // placeholder's canned 96 kHz code does not, so it changes the encoded
        // frame length.
        let frame = self.retag_frame_rate(frame, frame_number)?;

        // Keeps min/max frame size in STREAMINFO honest.
        self.stream_info.update_frame_info(&frame);

        self.sink.clear();
        frame
            .write(&mut self.sink)
            .map_err(|e| format!("FLAC frame serialization failed: {:?}", e))?;
        let bytes = self.sink.as_slice().to_vec();
        self.write_all(&bytes)
    }

    /// Feed one stereo chunk (planar slices, equal length).
    pub fn feed(&mut self, samples_l: &[f64], samples_r: &[f64]) -> Result<(), String> {
        if CONV_CANCEL.load(Ordering::Relaxed) {
            self.abort();
            return Err("Cancelled".to_string());
        }

        self.pending.reserve(samples_l.len() * 2);
        for i in 0..samples_l.len() {
            self.pending.push(to_i24(samples_l[i]));
            self.pending.push(to_i24(samples_r[i]));
        }

        // Moved out so `emit_frame` can borrow `self` mutably while the block
        // slice stays alive.
        let block_len = self.block_size * 2;
        let mut pending = std::mem::take(&mut self.pending);
        let mut consumed = 0;
        let mut result = Ok(());
        while pending.len() - consumed >= block_len {
            if let Err(e) = self.emit_frame(&pending[consumed..consumed + block_len]) {
                result = Err(e);
                break;
            }
            consumed += block_len;
        }
        pending.drain(..consumed);
        self.pending = pending;

        if result.is_err() {
            self.abort();
        }
        result
    }

    /// Flush the tail, then rewrite STREAMINFO with the real MD5, sample count
    /// and frame-size extremes.
    pub fn finish(mut self) -> Result<(), String> {
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            if let Err(e) = self.emit_frame(&pending) {
                self.abort();
                return Err(e);
            }
        }

        // `update_frame_info` has been tracking the real extremes, which drags
        // `min_block_size` down to the length of the short final frame. Left
        // that way, `min != max` declares a *variable*-block-size stream, and a
        // decoder then expects each frame header to carry a sample number where
        // ours carries a frame number — the stream desynchronizes immediately.
        // The reference encoder reports the nominal block size here for exactly
        // this reason; a shorter last frame is allowed regardless.
        self.stream_info
            .set_block_sizes(self.block_size, self.block_size)
            .map_err(|e| format!("FLAC block size rejected: {:?}", e))?;
        self.stream_info
            .set_total_samples(self.context.total_samples());
        self.stream_info.set_md5_digest(&self.context.md5_digest());

        let info = match self.serialize_stream_info() {
            Ok(info) => info,
            Err(e) => {
                self.abort();
                return Err(e);
            }
        };

        let mut file = match self.file.take() {
            Some(f) => f
                .into_inner()
                .map_err(|e| format!("FLAC flush error: {}", e))?,
            None => return Err("encoder already finished".to_string()),
        };

        let rewrite = file
            .seek(SeekFrom::Start(STREAMINFO_OFFSET))
            .and_then(|_| file.write_all(&info))
            .and_then(|_| file.flush());
        if let Err(e) = rewrite {
            drop(file);
            let _ = std::fs::remove_file(&self.output_path);
            return Err(format!("FLAC stream info rewrite failed: {}", e));
        }
        Ok(())
    }
}

/// Encode f64 PCM to FLAC
pub fn encode_flac(
    samples_l: &[f64],
    samples_r: &[f64],
    sample_rate: u32,
    output_path: &Path,
) -> Result<(), String> {
    let mut enc = StreamingFlacEncoder::new(sample_rate, output_path)?;
    let total = samples_l.len();
    let chunk_size = 8192;
    for start in (0..total).step_by(chunk_size) {
        let end = (start + chunk_size).min(total);
        enc.feed(&samples_l[start..end], &samples_r[start..end])?;
    }
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic signal already sitting on the 24-bit grid, exactly as the
    /// dither stage would hand it over.
    fn grid_signal(n: usize, phase: f64) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let x = 0.8 * ((i as f64) * 0.011 + phase).sin();
                (x * SCALE_24).round() / SCALE_24
            })
            .collect()
    }

    /// Decode a FLAC with Symphonia, with MD5 verification switched ON, and
    /// return the samples. `verify_flac` in the production path runs with
    /// `verify: false`, so a wrong STREAMINFO digest would slip past it — this
    /// is where that gets caught.
    pub(super) fn decode_verified(path: &Path) -> Result<(Vec<f64>, Vec<f64>, u32), String> {
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;

        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("flac");

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("probe: {}", e))?;
        let mut format = probed.format;
        let track = format.tracks().first().ok_or("no track")?;
        let track_id = track.id;
        let rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
        let mut decoder = symphonia::default::get_codecs()
            .make(
                &track.codec_params,
                &DecoderOptions { verify: true },
            )
            .map_err(|e| format!("codec: {}", e))?;

        const SCALE_I32_TO_F64: f64 = 1.0 / 2_147_483_648.0_f64;
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut buf: Option<SampleBuffer<i32>> = None;

        loop {
            let packet = match format.next_packet() {
                Ok(p) => p,
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break
                }
                Err(e) => return Err(format!("packet: {}", e)),
            };
            if packet.track_id() != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).map_err(|e| format!("decode: {}", e))?;
            let spec = *decoded.spec();
            let sb = buf.get_or_insert_with(|| {
                SampleBuffer::<i32>::new(decoded.capacity() as u64, spec)
            });
            sb.copy_interleaved_ref(decoded);
            for frame in sb.samples().chunks_exact(2) {
                l.push(frame[0] as f64 * SCALE_I32_TO_F64);
                r.push(frame[1] as f64 * SCALE_I32_TO_F64);
            }
        }

        // Surfaces an MD5 mismatch between STREAMINFO and the actual audio.
        let md5_ok = decoder
            .finalize()
            .verify_ok
            .ok_or("STREAMINFO carries no MD5 digest")?;
        if !md5_ok {
            return Err("STREAMINFO MD5 does not match the encoded audio".to_string());
        }
        Ok((l, r, rate))
    }

    /// Full round trip at a rate the converter actually emits. The sample
    /// count is deliberately not a multiple of the block size, and the chunks
    /// fed in are deliberately not aligned to it either, so the block
    /// buffering and the short final frame both get exercised.
    #[test]
    fn round_trip_is_lossless_at_384k() {
        let n = 4096 * 3 + 1234;
        let (want_l, want_r) = (grid_signal(n, 0.0), grid_signal(n, 1.7));

        let path = std::env::temp_dir().join("ae_encode_round_trip_384k.flac");
        let mut enc = StreamingFlacEncoder::new(384_000, &path).expect("encoder");
        for chunk in (0..n).step_by(1000) {
            let end = (chunk + 1000).min(n);
            enc.feed(&want_l[chunk..end], &want_r[chunk..end])
                .expect("feed");
        }
        enc.finish().expect("finish");

        let (got_l, got_r, rate) = decode_verified(&path).expect("decode");
        std::fs::remove_file(&path).ok();

        assert_eq!(rate, 384_000, "sample rate not preserved");
        assert_eq!(got_l.len(), n, "sample count not preserved");
        for i in 0..n {
            assert_eq!(got_l[i], want_l[i], "left channel differs at {}", i);
            assert_eq!(got_r[i], want_r[i], "right channel differs at {}", i);
        }
    }

    /// 44.1 kHz x 8 is the other design point shipped in the portable package.
    #[test]
    fn round_trip_is_lossless_at_352k8() {
        let n = 4096 + 7;
        let (want_l, want_r) = (grid_signal(n, 0.3), grid_signal(n, 2.1));

        let path = std::env::temp_dir().join("ae_encode_round_trip_352k8.flac");
        let mut enc = StreamingFlacEncoder::new(352_800, &path).expect("encoder");
        enc.feed(&want_l, &want_r).expect("feed");
        enc.finish().expect("finish");

        let (got_l, got_r, rate) = decode_verified(&path).expect("decode");
        std::fs::remove_file(&path).ok();

        assert_eq!(rate, 352_800);
        assert_eq!(got_l.len(), n);
        assert_eq!(got_l, want_l);
        assert_eq!(got_r, want_r);
    }

    /// Full-scale rails must survive as the extreme 24-bit codes rather than
    /// wrapping around, which is what a missing clamp would do.
    #[test]
    fn full_scale_rails_do_not_wrap() {
        assert_eq!(to_i24(1.0), 8_388_607);
        assert_eq!(to_i24(-1.0), -8_388_608);
        assert_eq!(to_i24(0.0), 0);
        assert_eq!(to_i24(2.0), 8_388_607, "overshoot must clamp, not wrap");
        assert_eq!(to_i24(-2.0), -8_388_608, "overshoot must clamp, not wrap");
    }

    /// The per-block order search runs the four candidates in parallel. Two
    /// encodes of the same samples must therefore produce the same bytes: if
    /// the search were racy, or if it depended on which candidate finished
    /// first rather than on which one is smallest, this is where it shows.
    ///
    /// Dither is deliberately seeded from entropy, so a whole conversion is
    /// NOT reproducible and cannot be used to check this. The encoder is fed
    /// post-dither samples here, which is exactly the boundary where
    /// reproducibility starts being required.
    #[test]
    fn parallel_candidate_search_is_byte_reproducible() {
        let n = 8192 * 3 + 517;
        let (l, r) = (grid_signal(n, 0.0), grid_signal(n, 2.3));

        let mut bytes = Vec::new();
        for run in 0..2 {
            let path = std::env::temp_dir().join(format!("ae_encode_repro_{}.flac", run));
            let mut enc = StreamingFlacEncoder::new(352_800, &path).expect("encoder");
            enc.feed(&l, &r).expect("feed");
            enc.finish().expect("finish");
            bytes.push(std::fs::read(&path).expect("read back"));
            let _ = std::fs::remove_file(&path);
        }

        assert_eq!(
            bytes[0].len(),
            bytes[1].len(),
            "two encodes of identical samples differ in length"
        );
        assert!(
            bytes[0] == bytes[1],
            "two encodes of identical samples differ in content -- the candidate              search is order-dependent or racy"
        );
    }

    /// The search keeps the FIRST candidate on a tie, because FLAC_CANDIDATES
    /// is ordered by what the compression benchmark preferred. Running the
    /// candidates in parallel must not disturb that: `collect` preserves the
    /// order and the fold takes the earliest minimum. This pins the rule so a
    /// later refactor to `min_by_key` -- which keeps the LAST minimum -- gets
    /// caught here rather than in someone's file.
    #[test]
    fn candidate_search_keeps_the_first_of_equal_sizes() {
        let pick = |cands: Vec<(usize, &'static str)>| -> &'static str {
            cands
                .into_iter()
                .reduce(|best, cand| if cand.0 < best.0 { cand } else { best })
                .expect("at least one candidate")
                .1
        };

        assert_eq!(pick(vec![(100, "a"), (100, "b")]), "a", "tie keeps the first");
        assert_eq!(pick(vec![(100, "a"), (99, "b"), (99, "c")]), "b", "first of the smallest");
        assert_eq!(pick(vec![(101, "a"), (100, "b"), (99, "c")]), "c");
        assert_eq!(pick(vec![(99, "a"), (100, "b"), (101, "c")]), "a");
    }

    /// A cancelled encode must not leave a playable-looking stub behind.
    #[test]
    fn abort_removes_the_partial_file() {
        let path = std::env::temp_dir().join("ae_encode_abort.flac");
        let mut enc = StreamingFlacEncoder::new(352_800, &path).expect("encoder");
        let sig = grid_signal(512, 0.0);
        enc.feed(&sig, &sig).expect("feed");
        assert!(path.exists(), "file should exist while encoding");
        enc.abort();
        assert!(!path.exists(), "abort must unlink the partial file");
    }
}

#[cfg(test)]
mod bench {
    use super::tests::decode_verified;
    use super::*;

    /// Manual compression benchmark against a real file — the round-trip tests
    /// prove correctness, this one watches size.
    ///
    ///   set AE_ENCODE_BENCH_SRC=<path to a .flac>
    ///   cargo test --release encode_compression_ratio -- --ignored --nocapture
    ///
    /// Reports this encoder's output size against the source, so a settings
    /// change can be judged on real material rather than on a synthetic tone.
    #[test]
    #[ignore]
    fn encode_compression_ratio() {
        let src = match std::env::var("AE_ENCODE_BENCH_SRC") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => {
                println!("AE_ENCODE_BENCH_SRC not set — skipping");
                return;
            }
        };
        let src_len = std::fs::metadata(&src).expect("source metadata").len();

        let (l, r, rate) = decode_verified(&src).expect("decode source");
        println!(
            "source: {} frames @ {} Hz, {:.1} MB",
            l.len(),
            rate,
            src_len as f64 / 1e6
        );

        let raw = l.len() as u64 * 2 * 3;
        println!(
            "raw:    {:.1} MB    source: {:.1} MB ({:.1}% of raw)\n",
            raw as f64 / 1e6,
            src_len as f64 / 1e6,
            100.0 * src_len as f64 / raw as f64
        );
        println!("{:<28}  {:>10}  {:>9}  {:>10}", "candidate orders", "size MB", "% of raw", "vs source");

        // Best candidate set found by the earlier order/window sweeps.
        const ORDERS: &[(usize, f32)] = &[(6, 0.4), (8, 0.4), (10, 0.4), (12, 0.4)];
        // 4096 is tuned for 44.1 kHz, where it spans 93 ms. At 352.8 kHz the
        // same count is 11.6 ms. The FLAC streamable subset allows up to 16384
        // above 48 kHz, so that is the ceiling worth trying — going past it
        // would risk hardware decoders refusing the file.
        const BLOCK_SIZES: &[usize] = &[4096, 8192, 16384];

        let out = std::env::temp_dir().join("ae_encode_bench_out.flac");
        let mut best: (String, u64) = (String::new(), u64::MAX);
        for &block in BLOCK_SIZES {
            let label = format!("block {} ({:.0} ms)", block, 1000.0 * block as f64 / rate as f64);
            let mut enc = StreamingFlacEncoder::with_candidates(rate, &out, block, ORDERS)
                .expect("encoder");
            for start in (0..l.len()).step_by(65536) {
                let end = (start + 65536).min(l.len());
                enc.feed(&l[start..end], &r[start..end]).expect("feed");
            }
            enc.finish().expect("finish");

            let len = std::fs::metadata(&out).expect("output metadata").len();
            println!(
                "{:<28}  {:>10.1}  {:>8.1}%  {:>+9.1}%",
                &label,
                len as f64 / 1e6,
                100.0 * len as f64 / raw as f64,
                100.0 * (len as f64 - src_len as f64) / src_len as f64
            );
            if len < best.1 {
                best = (label.clone(), len);
            }
            std::fs::remove_file(&out).ok();
        }
        println!(
            "\nbest: {:?} at {:.1} MB ({:+.1}% vs source)",
            best.0,
            best.1 as f64 / 1e6,
            100.0 * (best.1 as f64 - src_len as f64) / src_len as f64
        );
    }
}
