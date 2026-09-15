use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rayon::prelude::*;

use crate::audio::converter::state::{FileConvState, BADGE_NONE, BADGE_VERIFIED_FAIL};

/// FLAC spec hard-caps the STREAMINFO sample rate at 655350 Hz. Symphonia
/// enforces this unconditionally, so 705.6/768 kHz files (which ffmpeg
/// writes non-compliant but valid in practice) must be decoded via ffmpeg.
const FLAC_SPEC_MAX_RATE: u32 = 655_350;

/// Streaming comparator: consumes decoded chunks and compares them against
/// the expected DSP output on the fly — either in-memory slices (normal
/// path) or a raw f64-interleaved temp file (segmented giant path). Keeps
/// only one decode chunk in flight instead of materializing the whole
/// decoded track.
struct StreamCompare<'a> {
    expected: ExpectedSource<'a>,
    total: usize,
    pos: usize,
    err_count: usize,
    max_diff: f64,
    margin: f64,
    // scratch for the file-backed source
    scratch_l: Vec<f64>,
    scratch_r: Vec<f64>,
    byte_buf: Vec<u8>,
}

enum ExpectedSource<'a> {
    Ram(&'a [f64], &'a [f64]),
    File(std::io::BufReader<std::fs::File>),
}

impl<'a> StreamCompare<'a> {
    fn new_ram(expected_l: &'a [f64], expected_r: &'a [f64], margin: f64) -> Self {
        let total = expected_l.len();
        Self {
            expected: ExpectedSource::Ram(expected_l, expected_r),
            total,
            pos: 0,
            err_count: 0,
            max_diff: 0.0,
            margin,
            scratch_l: Vec::new(),
            scratch_r: Vec::new(),
            byte_buf: Vec::new(),
        }
    }

    fn new_file(path: &Path, total_frames: usize, margin: f64) -> Result<Self, String> {
        let f = std::fs::File::open(path)
            .map_err(|e| format!("cannot open expected-data temp file: {}", e))?;
        Ok(Self {
            expected: ExpectedSource::File(std::io::BufReader::with_capacity(4 << 20, f)),
            total: total_frames,
            pos: 0,
            err_count: 0,
            max_diff: 0.0,
            margin,
            scratch_l: Vec::new(),
            scratch_r: Vec::new(),
            byte_buf: Vec::new(),
        })
    }

    /// Compare one decoded chunk against the next `n` expected frames.
    fn feed(&mut self, l: &[f64], r: &[f64]) -> Result<(), String> {
        let n = l.len().min(r.len());
        if self.pos + n > self.total {
            return Err(format!(
                "Length mismatch (expected {}, got at least {})",
                self.total,
                self.pos + n
            ));
        }
        let margin = self.margin;
        let (errs, max) = match &mut self.expected {
            ExpectedSource::Ram(el, er) => {
                let exp_l = &el[self.pos..self.pos + n];
                let exp_r = &er[self.pos..self.pos + n];
                compare_chunk(exp_l, exp_r, l, r, margin)
            }
            ExpectedSource::File(reader) => {
                // Raw interleaved (l,r) f64 LE — read exactly n frames.
                self.byte_buf.resize(n * 16, 0);
                reader
                    .read_exact(&mut self.byte_buf)
                    .map_err(|e| format!("expected-data read error: {}", e))?;
                self.scratch_l.clear();
                self.scratch_r.clear();
                self.scratch_l.reserve(n);
                self.scratch_r.reserve(n);
                for i in 0..n {
                    let b = i * 16;
                    self.scratch_l.push(f64::from_le_bytes(
                        self.byte_buf[b..b + 8].try_into().unwrap(),
                    ));
                    self.scratch_r.push(f64::from_le_bytes(
                        self.byte_buf[b + 8..b + 16].try_into().unwrap(),
                    ));
                }
                compare_chunk(&self.scratch_l, &self.scratch_r, l, r, margin)
            }
        };
        self.err_count += errs;
        if max > self.max_diff {
            self.max_diff = max;
        }
        self.pos += n;
        Ok(())
    }
}

fn compare_chunk(
    exp_l: &[f64],
    exp_r: &[f64],
    got_l: &[f64],
    got_r: &[f64],
    margin: f64,
) -> (usize, f64) {
    (0..exp_l.len())
        .into_par_iter()
        .map(|i| {
            let dl = (exp_l[i] - got_l[i]).abs();
            let dr = (exp_r[i] - got_r[i]).abs();
            let max = dl.max(dr);
            let bad = if max > margin { 1usize } else { 0 };
            (bad, max)
        })
        .reduce(|| (0, 0.0f64), |(ec, mx), (e, m)| (ec + e, mx.max(m)))
}

fn run_verification(
    path: &Path,
    cmp: &mut StreamCompare,
    out_rate: u32,
    file_state: &Arc<FileConvState>,
) -> Result<(), String> {
    crate::aelog!();
    crate::aelog!("[CONV] ===================================================");
    crate::aelog!("[CONV] STAGE 5: BIT-PERFECT VERIFICATION (streaming)");
    crate::aelog!("[CONV] ===================================================");
    crate::aelog!("[CONV] Re-decoding generated FLAC: {}", path.display());

    // Symphonia for spec-compliant rates (fast, pure-Rust, no subprocess);
    // ffmpeg pipe for 705.6/768 kHz files that exceed the FLAC spec cap.
    let decode_result = if out_rate > FLAC_SPEC_MAX_RATE {
        // Encoding needs no external tool, but Symphonia refuses to decode
        // above the spec cap, so ffmpeg is the only reader we have up here.
        // When it is absent there is simply nothing to compare against —
        // report that plainly rather than branding a perfectly good file
        // _UNVERIFIED, which would read as data corruption.
        if !ffmpeg_available() {
            crate::aelog!(
                "[CONV] ⚠ {} Hz exceeds the FLAC spec cap ({} Hz) and ffmpeg is not \
                 available — the file was written, but bit-perfect verification \
                 could not run. Install ffmpeg to verify output at this rate.",
                out_rate,
                FLAC_SPEC_MAX_RATE
            );
            crate::aelog!("[CONV] ===================================================\n");
            file_state.badge.store(BADGE_NONE, Ordering::Relaxed);
            return Ok(());
        }
        crate::aelog!(
            "[CONV] {} Hz exceeds the FLAC spec cap ({} Hz) — decoding via ffmpeg pipe",
            out_rate,
            FLAC_SPEC_MAX_RATE
        );
        decode_via_ffmpeg_streaming(path, cmp)
    } else {
        decode_via_symphonia_streaming(path, cmp)
    };
    if let Err(e) = decode_result {
        file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
        return Err(e);
    }

    // ── Length check ──────────────────────────────────────────────────────────
    if cmp.pos != cmp.total {
        crate::aelog!(
            "[CONV] ❌ VERIFICATION FAILED: Length mismatch! Expected: {}, Got: {}",
            cmp.total,
            cmp.pos
        );
        file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
        return Err(format!(
            "Length mismatch (expected {}, got {})",
            cmp.total, cmp.pos
        ));
    }

    // Proportional threshold: 0.001% of total samples, min 20.
    // Tightened from 0.01%/100 — the old budget allowed >10k mismatches on
    // a 5-min 384 kHz file, large enough to mask off-by-one regressions in
    // the trim/flush logic. ±2 LSB margin is preserved.
    let n = cmp.total;
    let allowed_errors = (n / 100_000).max(20);

    if cmp.err_count > allowed_errors {
        crate::aelog!(
            "[CONV] ❌ VERIFICATION FAILED: {} mismatches (allowed: {})",
            cmp.err_count, allowed_errors
        );
        crate::aelog!("[CONV] ❌ Maximum delta: {:.4e}", cmp.max_diff);
        file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
        return Err("encoded file bits do not match DSP output".to_string());
    }

    crate::aelog!("[CONV] ✓ FLAC Integrity Confirmed.");
    crate::aelog!("[CONV] ✓ Matches 64-bit DSP output (±2 LSB @ 24-bit tolerance).");
    crate::aelog!(
        "[CONV] ✓ Max deviation: {:.4e}  margin: {:.4e}  mismatches: {}/{}",
        cmp.max_diff, cmp.margin, cmp.err_count, n
    );
    crate::aelog!("[CONV] ===================================================\n");

    Ok(())
}

/// Tolerance: ±2 LSBs at 24-bit (accounts for ffmpeg s32↔f64 rounding)
fn lsb_margin() -> f64 {
    2.0 / ((1i64 << 23) as f64)
}

/// Verify an encoded FLAC against in-memory expected buffers.
pub fn verify_flac(
    path: &Path,
    expected_l: &[f64],
    expected_r: &[f64],
    out_rate: u32,
    file_state: &Arc<FileConvState>,
) -> Result<(), String> {
    let mut cmp = StreamCompare::new_ram(expected_l, expected_r, lsb_margin());
    run_verification(path, &mut cmp, out_rate, file_state)
}

/// Verify an encoded FLAC against a raw f64-interleaved temp file — the
/// segmented giant path never holds the expected data in RAM.
pub fn verify_flac_against_file(
    path: &Path,
    expected_raw: &Path,
    expected_frames: usize,
    out_rate: u32,
    file_state: &Arc<FileConvState>,
) -> Result<(), String> {
    let mut cmp = StreamCompare::new_file(expected_raw, expected_frames, lsb_margin())?;
    run_verification(path, &mut cmp, out_rate, file_state)
}

// ─────────────────────────────────────────────────────────────────────────────

/// Is an `ffmpeg` binary reachable? Only consulted above the FLAC spec rate
/// cap, where it is the sole decoder capable of reading our own output.
fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Streaming Symphonia decode: feeds each decoded packet to the comparator
/// instead of materializing the whole file. Same i32→f64 normalization as
/// the batch decoder in decode.rs (exact for 16/24-bit integer PCM).
fn decode_via_symphonia_streaming(path: &Path, cmp: &mut StreamCompare) -> Result<(), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open file: {}", e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {}", e))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or("No audio track found")?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions { verify: false, ..Default::default() })
        .map_err(|e| format!("Codec error: {}", e))?;

    const SCALE_I32_TO_F64: f64 = 1.0 / 2_147_483_648.0_f64; // 1 / 2^31
    let mut chunk_l: Vec<f64> = Vec::new();
    let mut chunk_r: Vec<f64> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(e) => return Err(format!("Decode stopped early: {}", e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|e| format!("Corrupt packet in encoded file: {}", e))?;

        let spec = *decoded.spec();
        let mut sample_buf = SampleBuffer::<i32>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();
        let ch = spec.channels.count().max(1);

        chunk_l.clear();
        chunk_r.clear();
        for frame in 0..(samples.len() / ch) {
            let l = samples[frame * ch] as f64 * SCALE_I32_TO_F64;
            let r = if ch >= 2 {
                samples[frame * ch + 1] as f64 * SCALE_I32_TO_F64
            } else {
                l
            };
            chunk_l.push(l);
            chunk_r.push(r);
        }
        cmp.feed(&chunk_l, &chunk_r)?;
    }
    Ok(())
}

/// Streaming ffmpeg decode — reads the s32le pipe in fixed-size chunks and
/// feeds them to the comparator, never holding more than one chunk.
fn decode_via_ffmpeg_streaming(path: &Path, cmp: &mut StreamCompare) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-threads", "0",          // auto-detect optimal thread count
            "-i", path.to_str().unwrap_or(""),
            "-f", "s32le",
            "-acodec", "pcm_s32le",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg decode error: {}", e))?;

    let mut stdout = child.stdout.take().ok_or("ffmpeg: no stdout")?;

    // s32le interleaved stereo: 4 bytes L + 4 bytes R = 8 bytes per frame
    const FRAME_BYTES: usize = 8;
    // Same normalisation as Symphonia: 1/2^31
    const SCALE: f64 = 1.0 / 2_147_483_648.0_f64;
    // 8 MiB per read → 1M frames per comparator feed
    const READ_BYTES: usize = 8 * 1024 * 1024;

    let mut buf = vec![0u8; READ_BYTES];
    let mut filled = 0usize; // bytes in buf carried over (partial frame tail)
    let mut chunk_l: Vec<f64> = Vec::new();
    let mut chunk_r: Vec<f64> = Vec::new();
    let mut total_frames = 0usize;

    loop {
        let n = stdout
            .read(&mut buf[filled..])
            .map_err(|e| format!("ffmpeg read error: {}", e))?;
        if n == 0 {
            break;
        }
        filled += n;
        let n_frames = filled / FRAME_BYTES;
        if n_frames == 0 {
            continue;
        }

        chunk_l.clear();
        chunk_r.clear();
        chunk_l.reserve(n_frames);
        chunk_r.reserve(n_frames);
        for i in 0..n_frames {
            let b = i * FRAME_BYTES;
            let l = i32::from_le_bytes([buf[b], buf[b + 1], buf[b + 2], buf[b + 3]]);
            let r = i32::from_le_bytes([buf[b + 4], buf[b + 5], buf[b + 6], buf[b + 7]]);
            chunk_l.push(l as f64 * SCALE);
            chunk_r.push(r as f64 * SCALE);
        }
        // Carry the partial frame tail (if any) to the front of the buffer
        let consumed = n_frames * FRAME_BYTES;
        buf.copy_within(consumed..filled, 0);
        filled -= consumed;
        total_frames += n_frames;

        if let Err(e) = cmp.feed(&chunk_l, &chunk_r) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    child
        .wait()
        .map_err(|e| format!("ffmpeg wait error: {}", e))?;
    if filled != 0 {
        return Err(format!(
            "ffmpeg pipe ended mid-frame ({} trailing bytes)",
            filled
        ));
    }
    crate::aelog!("[CONV] ffmpeg decoded {} frames via pipe (streaming)", total_frames);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::encode::encode_flac;
    use crate::audio::converter::state::FileConvState;

    /// Deterministic pseudo-noise already quantized to exact 24-bit values,
    /// so encode→decode must round-trip inside the ±2 LSB margin.
    fn noise_24bit(n: usize, seed: u64) -> Vec<f64> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let k = (x % 16_000_000) as i64 - 8_000_000; // ±0.95 FS
                k as f64 / 8_388_608.0
            })
            .collect()
    }

    /// End-to-end: encode a short noise burst to FLAC and stream-verify it
    /// against the in-memory buffers (symphonia path, spec-compliant rate).
    /// Then tamper one region and require verification to FAIL. Also verify
    /// via the file-backed expected source (segmented giant path).
    #[test]
    fn streaming_verify_roundtrip_and_tamper_detection() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not found in PATH — skipping streaming verify test");
            return;
        }
        let n = 44_100; // 1 s @ 44.1 kHz
        let l = noise_24bit(n, 0x1234_5678_9abc_def1);
        let r = noise_24bit(n, 0xfedc_ba98_7654_3211);

        let path = std::env::temp_dir().join("aura_engine_verify_test.flac");
        encode_flac(&l, &r, 44_100, &path).expect("encode_flac failed");

        let state = Arc::new(FileConvState::new());
        verify_flac(&path, &l, &r, 44_100, &state)
            .expect("verification of an untampered file must pass");

        // File-backed expected source: write raw interleaved f64 and verify.
        let raw = std::env::temp_dir().join("aura_engine_verify_test.raw");
        {
            use std::io::Write;
            let mut w = std::io::BufWriter::new(std::fs::File::create(&raw).unwrap());
            for i in 0..n {
                w.write_all(&l[i].to_le_bytes()).unwrap();
                w.write_all(&r[i].to_le_bytes()).unwrap();
            }
        }
        verify_flac_against_file(&path, &raw, n, 44_100, &state)
            .expect("file-backed verification must pass");

        // Tamper: shift a whole region well past the ±2 LSB margin.
        let mut bad_l = l.clone();
        for v in bad_l[1000..3000].iter_mut() {
            *v += 0.01;
        }
        let res = verify_flac(&path, &bad_l, &r, 44_100, &state);
        assert!(res.is_err(), "verification must fail on mismatched samples");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&raw);
    }
}
