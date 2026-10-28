//! Segmented (bounded-RAM) conversion pipeline for very long files.
//!
//! The normal polyphase path materializes the whole output-rate track in
//! RAM: at ×8 with Hybrid-Phase that is TWO full f64 stereo copies plus the
//! blend envelope — ~40 GB for a 38-minute 192 kHz album file. This module
//! converts such "giants" with a constant working set instead:
//!
//!   * the polyphase convolver bank persists across the whole file (the
//!     convolvers are stateful), input is fed segment by segment and each
//!     interleaved output segment is flushed to a raw temp file next to the
//!     output — RAM holds one segment (~130 MB), not the track;
//!   * Hybrid-Phase blending streams the linear and min-phase temp files in
//!     lockstep: switch boundaries come from the analysis-rate envelope
//!     (evaluated on the fly — never materialized at output rate), each
//!     boundary is zero-crossing-snapped inside a small look-around margin,
//!     and blended data overwrites the linear temp file in place;
//!   * true peak is measured by a streaming scanner during the blend, the
//!     gain + dither + FLAC encode then stream the blended file once more,
//!     and verification compares the decoded FLAC against the post-dither
//!     temp file — nothing track-sized ever lives in RAM.
//!
//! Sample-exactness: convolver outputs depend only on the input SAMPLE
//! sequence (not on chunk sizes), the interleave/scale math is identical to
//! the in-RAM pass, and the blend uses the same boundary scan, snap and
//! crossfade code paths (shared with hybrid_phase.rs). The equivalence test
//! at the bottom locks the two paths together bit for bit (pre-dither).

use crate::audio::converter::decode::set_status;
use crate::audio::converter::dsp::true_peak::{target_lin_for, TruePeakScan};
use crate::audio::converter::dsp::dither::DitherState;
use crate::audio::converter::dsd_out::{self, DsdExport};
use crate::audio::converter::encode::StreamingFlacEncoder;
use crate::audio::converter::process::{FilterSource, StridedOut};
use crate::audio::converter::state::FileConvState;
use crate::audio::converter::types::ConvertSettings;
use crate::audio::converter::utils::verify::verify_flac_against_file;
use crate::audio::hybrid_phase::{
    catmull_env_at, smooth_analysis_envelope, snap_boundary, switch_fade, switch_params,
    SWITCH_THRESHOLD,
};
use crate::audio::processor::DspProcessor;
use std::collections::VecDeque;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Input-rate samples per convolution segment when RAM is plentiful. One
/// segment's output at ×8 is seg×8×16 B ≈ 134 MB — the RAM unit of the
/// giant path. Low on RAM the segment shrinks (`SegPlan`), down to
/// `SEG_INPUT_MIN`; every size is a power of two, so a multiple of the
/// 32768 processing chunk.
const SEG_INPUT_SAMPLES: usize = 1 << 20;
const SEG_INPUT_MIN: usize = 1 << 16;

/// Output frames per emission chunk in the streaming stages.
const EMIT_CHUNK: usize = 1 << 20;

/// Frames converted to or from bytes at a time by the temp-file plumbing:
/// the byte buffer stays a few MB however long the slice it is handed.
const IO_FRAMES: usize = 1 << 16;

/// Conservative VRAM budget for keeping ALL polyphase sub-filter
/// processors resident at once (GPU segmented mode). Above this the giant
/// path falls back to the CPU convolver bank, which has no such limit.
const GPU_BANK_VRAM_BUDGET: u64 = 11 * 1024 * 1024 * 1024;

// ───────────────────────── giant-plan predicates ─────────────────────────

/// Peak RAM (MB) the IN-RAM integrated path would need for a file:
/// full-length output-rate stereo f64 buffers (×2 + envelope for hybrid)
/// plus the source-rate input.
pub fn in_ram_peak_mb(n_in: u64, ratio: u64, hybrid: bool) -> u64 {
    const MB: u64 = 1 << 20;
    let out = n_in * ratio * 16 / MB;
    let base = if hybrid { out * 2 + out / 4 } else { out };
    base + n_in * 16 / MB
}

/// Files whose in-RAM peak exceeds this go through the segmented path even
/// when the RAM is free: a quarter of the installed RAM, at most 12 GB.
/// Until 1.8.3 it never went under 4 GB, so on an 8 GB machine no file was
/// ever segmented — and the in-RAM peak was then refused.
pub fn giant_threshold_mb(total_mb: u64) -> u64 {
    (total_mb / 4).min(12_288)
}

/// How one file goes through the polyphase route: in RAM, or segmented and
/// with what segment. Decided once per file (the manager does it before
/// admission and hands it down in `PreparedAudio::seg_plan`), so the RAM
/// estimate, the disk check and the pipeline all go by the same choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegPlan {
    pub segmented: bool,
    pub seg_input_samples: usize,
}

/// `plan_route_with` on the RAM there is now.
pub fn plan_route(in_ram_mb: u64, ratio: u64) -> SegPlan {
    let (total_mb, avail_mb) = crate::audio::memory::ram_now_mb();
    plan_route_with(in_ram_mb, ratio, total_mb, avail_mb)
}

/// The route for a file whose in-RAM estimate is `in_ram_mb`: segmented when
/// that is over the giant threshold or does not fit the RAM available now
/// (less the safety margin) — slower, through temp files, but its working
/// set no longer grows with the track. The segment takes at most a quarter
/// of the room left, between `SEG_INPUT_MIN` and `SEG_INPUT_SAMPLES`.
pub fn plan_route_with(in_ram_mb: u64, ratio: u64, total_mb: u64, avail_mb: u64) -> SegPlan {
    let room_mb = avail_mb.saturating_sub(crate::audio::memory::safety_margin_mb(total_mb));
    let segmented = in_ram_mb > giant_threshold_mb(total_mb) || in_ram_mb > room_mb;
    let per_sample = ratio.max(1) * 16;
    let fit = ((room_mb / 4) << 20) / per_sample;
    let mut seg = SEG_INPUT_SAMPLES;
    while seg > SEG_INPUT_MIN && seg as u64 > fit {
        seg >>= 1;
    }
    SegPlan { segmented, seg_input_samples: seg }
}

/// RAM (MB) one segment of the segmented route holds: the source frames it
/// reads, its output at the output rate and the multistage chain's input.
pub fn segment_mb(seg_input_samples: usize, ratio: u64) -> u64 {
    let seg = seg_input_samples as u64;
    (seg * ratio.max(1) * 16 + seg * 32) >> 20
}

/// RAM (MB) of the streaming stages after the passes (blend, output stages,
/// encode, verify): a few `EMIT_CHUNK` buffers and the temp readers.
pub const STREAM_STAGES_MB: u64 = 160;

/// Bytes a segmented file keeps on disk next to its output at the peak: two
/// f64 stereo renders at the output rate (the linear and minimum-phase passes
/// being blended; the render and its XTC copy; the render and the expected
/// output the verify reads), the FLAC being written, taken at 24 bits
/// uncompressed as its ceiling, and the source itself, kept on disk while
/// the passes read it.
pub fn temp_peak_bytes(n_in: u64, ratio: u64) -> u64 {
    let n_out = n_in * ratio;
    n_out * 16 * 2 + n_out * 6 + n_in * 16
}

// ───────────────────────── raw temp-file plumbing ────────────────────────

/// Temp file that never outlives the conversion (removed on drop).
struct TempRaw {
    path: PathBuf,
}
impl TempRaw {
    fn new(dir: &Path, stem: &str, tag: &str) -> Self {
        let path = dir.join(format!(
            "{}.{}.{}.ae_tmp",
            stem,
            std::process::id(),
            tag
        ));
        Self { path }
    }
}
impl Drop for TempRaw {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sequential writer of interleaved (l, r) f64 LE frames.
struct FrameWriter {
    w: BufWriter<std::fs::File>,
    frames: u64,
    byte_buf: Vec<u8>,
}
impl FrameWriter {
    fn create(path: &Path) -> Result<Self, String> {
        let f = std::fs::File::create(path).map_err(|e| {
            format!(
                "cannot create temp file {} (disk space?): {}",
                path.display(),
                e
            )
        })?;
        Ok(Self {
            w: BufWriter::with_capacity(8 << 20, f),
            frames: 0,
            byte_buf: Vec::new(),
        })
    }
    /// Open an EXISTING file for sequential in-place overwrite from frame 0.
    fn overwrite(path: &Path) -> Result<Self, String> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| format!("cannot reopen temp file {}: {}", path.display(), e))?;
        Ok(Self {
            w: BufWriter::with_capacity(8 << 20, f),
            frames: 0,
            byte_buf: Vec::new(),
        })
    }
    fn write_frames(&mut self, l: &[f64], r: &[f64]) -> Result<(), String> {
        for (l, r) in l.chunks(IO_FRAMES).zip(r.chunks(IO_FRAMES)) {
            self.byte_buf.clear();
            self.byte_buf.reserve(l.len() * 16);
            for i in 0..l.len() {
                self.byte_buf.extend_from_slice(&l[i].to_le_bytes());
                self.byte_buf.extend_from_slice(&r[i].to_le_bytes());
            }
            self.w
                .write_all(&self.byte_buf)
                .map_err(|e| format!("temp file write error (disk full?): {}", e))?;
        }
        self.frames += l.len() as u64;
        Ok(())
    }
    fn finish(mut self) -> Result<u64, String> {
        self.w
            .flush()
            .map_err(|e| format!("temp file flush error: {}", e))?;
        Ok(self.frames)
    }
}

/// Sequential reader of interleaved (l, r) f64 LE frames.
struct FrameReader {
    r: BufReader<std::fs::File>,
    byte_buf: Vec<u8>,
}
impl FrameReader {
    fn open(path: &Path) -> Result<Self, String> {
        let f = std::fs::File::open(path)
            .map_err(|e| format!("cannot open temp file {}: {}", path.display(), e))?;
        Ok(Self {
            r: BufReader::with_capacity(8 << 20, f),
            byte_buf: Vec::new(),
        })
    }
    /// Read exactly `n` frames into the provided vectors (cleared first).
    fn read_frames(
        &mut self,
        n: usize,
        l: &mut Vec<f64>,
        r: &mut Vec<f64>,
    ) -> Result<(), String> {
        l.clear();
        r.clear();
        l.reserve(n);
        r.reserve(n);
        let mut left = n;
        while left > 0 {
            let k = left.min(IO_FRAMES);
            self.byte_buf.resize(k * 16, 0);
            self.r
                .read_exact(&mut self.byte_buf)
                .map_err(|e| format!("temp file read error: {}", e))?;
            for i in 0..k {
                let b = i * 16;
                l.push(f64::from_le_bytes(self.byte_buf[b..b + 8].try_into().unwrap()));
                r.push(f64::from_le_bytes(
                    self.byte_buf[b + 8..b + 16].try_into().unwrap(),
                ));
            }
            left -= k;
        }
        Ok(())
    }
}

/// Where a segmented pass reads the source from: the decoded track in RAM,
/// or its copy on disk (`SpilledSource`).
#[derive(Clone, Copy)]
pub(crate) enum SourceFrames<'a> {
    // Only the tests read from RAM: the converter always spills the source first.
    #[cfg_attr(not(test), allow(dead_code))]
    Ram(&'a [f64], &'a [f64]),
    Spilled(&'a SpilledSource),
}

/// A segmented file's source written to a raw temp file — the f64 frames as
/// they are, so the passes read the same bits — so the track does not stay
/// in RAM through the passes: an hour at 44.1 kHz is 2.5 GB there. Removed
/// on drop.
pub(crate) struct SpilledSource {
    tmp: TempRaw,
    frames: usize,
}

impl SpilledSource {
    /// Write the source next to the output and free it.
    pub(crate) fn spill(dir: &Path, stem: &str, l: Vec<f64>, r: Vec<f64>) -> Result<Self, String> {
        let tmp = TempRaw::new(dir, stem, "src");
        let mut writer = FrameWriter::create(&tmp.path)?;
        writer.write_frames(&l, &r)?;
        let frames = writer.finish()? as usize;
        Ok(Self { tmp, frames })
    }
}

/// Reads a `SourceFrames` front to back, a segment at a time.
enum SourceCursor<'a> {
    Ram(&'a [f64], &'a [f64]),
    File { reader: FrameReader, frames: usize, at: usize },
}

impl<'a> SourceCursor<'a> {
    fn open(source: SourceFrames<'a>) -> Result<Self, String> {
        Ok(match source {
            SourceFrames::Ram(l, r) => SourceCursor::Ram(l, r),
            SourceFrames::Spilled(s) => SourceCursor::File {
                reader: FrameReader::open(&s.tmp.path)?,
                frames: s.frames,
                at: 0,
            },
        })
    }

    /// Frames `pos..pos + n` into `l`/`r` (cleared first), zeros from `end`
    /// on — the zero flush the in-RAM pass feeds past the real input. The
    /// file is read in order: `pos` follows on from the previous call.
    fn read(&mut self, pos: usize, n: usize, end: usize, l: &mut Vec<f64>, r: &mut Vec<f64>) -> Result<(), String> {
        let real = end.saturating_sub(pos).min(n);
        match self {
            SourceCursor::Ram(sl, sr) => {
                l.clear();
                r.clear();
                if real > 0 {
                    l.extend_from_slice(&sl[pos..pos + real]);
                    r.extend_from_slice(&sr[pos..pos + real]);
                }
            }
            SourceCursor::File { .. } if real == 0 => {
                l.clear();
                r.clear();
            }
            SourceCursor::File { reader, frames, at } => {
                if *at != pos || pos + real > *frames {
                    return Err(format!(
                        "spilled source read at frame {} of {} (expected {})",
                        pos, frames, at
                    ));
                }
                reader.read_frames(real, l, r)?;
                *at += real;
            }
        }
        l.resize(n, 0.0);
        r.resize(n, 0.0);
        Ok(())
    }
}

/// Skip the leading algorithmic latency + group delay, pass through exactly
/// `take` frames, swallow the flush tail — the streaming twin of the
/// in-RAM path's `drain(..trim)` + `truncate(n_out)`.
struct TrimSink<F: FnMut(&[f64], &[f64]) -> Result<(), String>> {
    skip: usize,
    take: usize,
    inner: F,
}
impl<F: FnMut(&[f64], &[f64]) -> Result<(), String>> TrimSink<F> {
    fn feed(&mut self, l: &[f64], r: &[f64]) -> Result<(), String> {
        let mut l = l;
        let mut r = r;
        if self.skip > 0 {
            let s = self.skip.min(l.len());
            l = &l[s..];
            r = &r[s..];
            self.skip -= s;
        }
        if l.is_empty() || self.take == 0 {
            return Ok(());
        }
        let t = self.take.min(l.len());
        (self.inner)(&l[..t], &r[..t])?;
        self.take -= t;
        Ok(())
    }
}

// ───────────────────────── segmented polyphase pass ──────────────────────

/// Device memory of the whole bank: `l` convolvers of `sub_taps` each, by
/// the same count the admission gate reserves.
fn gpu_bank_vram_bytes(sub_taps: usize, l: usize) -> u64 {
    crate::audio::gpu::vram_admission::processor_bytes(sub_taps) * l as u64
}

/// Run all L polyphase sub-filters over the input plus flush, emitting the
/// interleaved, gain-scaled output SEGMENT BY SEGMENT instead of
/// materializing it. The convolver bank persists across segments (the
/// processors are stateful), so the produced sample stream is identical to
/// the in-RAM pass regardless of segmentation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_polyphase_pass_segmented(
    phases: &[Vec<f64>],
    frames: SourceFrames<'_>,
    total_input_samples: usize,
    flush_input_samples: usize,
    scale: f64,
    seg_input_samples: usize,
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    // `.npy` the sub-filters came from — cache identity for the GPU filter
    // spectra, `None` when the coefficients have no stable provenance.
    filter_path: Option<&str>,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
    emit: impl FnMut(&[f64], &[f64]) -> Result<(), String>,
) -> Result<(), String> {
    run_polyphase_pass_segmented_from(
        phases,
        frames,
        total_input_samples,
        flush_input_samples,
        scale,
        seg_input_samples,
        use_gpu,
        precision,
        status_label,
        filter_path.map(|path| FilterSource { path, ratio: phases.len() }),
        file_state,
        pct_base,
        pct_span,
        emit,
    )
}

/// `run_polyphase_pass_segmented` with the sub-filters' provenance spelled
/// out (see `process::FilterSource`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_polyphase_pass_segmented_from(
    phases: &[Vec<f64>],
    frames: SourceFrames<'_>,
    total_input_samples: usize,
    flush_input_samples: usize,
    scale: f64,
    seg_input_samples: usize,
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    source: Option<FilterSource>,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
    mut emit: impl FnMut(&[f64], &[f64]) -> Result<(), String>,
) -> Result<(), String> {
    use std::sync::atomic::AtomicU64;

    let l = phases.len();
    let chunk: usize = 32768;
    let total_in = total_input_samples + flush_input_samples;

    // GPU bank keeps ALL phase processors resident — check VRAM first.
    let gpu_bank = use_gpu && gpu_bank_vram_bytes(phases[0].len(), l) <= GPU_BANK_VRAM_BUDGET;
    if use_gpu && !gpu_bank {
        crate::aelog!(
            "[CONV] Segmented pass: {}×{}-tap GPU bank needs ~{} MB VRAM (> {} MB budget) → CPU convolver bank",
            l,
            phases[0].len(),
            gpu_bank_vram_bytes(phases[0].len(), l) / 1_048_576,
            GPU_BANK_VRAM_BUDGET / 1_048_576
        );
    }
    // The whole bank's device memory, reserved before its first sub-filter
    // is built: phase by phase, a second long file could get onto a card the
    // first one's bank fills (see "Banks" in `gpu::vram_admission`).
    // Declared before `bank`, so it outlives the convolvers drawing on it.
    let _bank_vram = gpu_bank.then(|| {
        let bytes = gpu_bank_vram_bytes(phases[0].len(), l);
        crate::audio::gpu::vram_admission::reserve_bank(bytes, || {
            set_status(&format!(
                "{} — waiting for the GPU: another long file holds its memory...",
                status_label
            ));
        })
    });
    set_status(&format!(
        "{} — segmented ({} phases, {})...",
        status_label,
        l,
        if gpu_bank { "GPU bank" } else { "CPU bank" }
    ));

    let mut bank: Vec<Box<dyn DspProcessor + Send>> = Vec::with_capacity(l);
    for (phase, ph) in phases.iter().enumerate() {
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }
        let id = source
            .map(|s| crate::audio::gpu::filter_cache::FilterId::polyphase(s.path, phase, s.ratio));
        // Per phase, not per bank. The sub-filters are independent, so a
        // device that fills up part-way through keeps the phases it already
        // holds on the GPU and builds the rest on the CPU. The two paths are
        // verified equivalent, so which side of that line a phase landed on
        // does not change the samples that come out.
        bank.push(crate::audio::gpu::build_convolver(
            ph,
            precision,
            id.as_ref(),
            gpu_bank,
        )?);
    }

    let seg_max = seg_input_samples.max(1).min(total_in.max(1));
    let mut seg_out_l = vec![0.0f64; seg_max * l];
    let mut seg_out_r = vec![0.0f64; seg_max * l];
    let mut cursor = SourceCursor::open(frames)?;
    let (mut seg_in_l, mut seg_in_r) = (Vec::with_capacity(seg_max), Vec::with_capacity(seg_max));

    let chunks_per_all = ((total_in + chunk - 1) / chunk * l).max(1) as u64;
    let chunks_done = AtomicU64::new(0);
    let bump = |done: u64| {
        let pct = pct_base + ((done as f64 / chunks_per_all as f64) * pct_span as f64) as u32;
        file_state
            .gpu_pct
            .store(pct.min(pct_base + pct_span), Ordering::Relaxed);
    };

    let mut pos = 0usize;
    while pos < total_in {
        if file_state.stop_requested() {
            return Err("Cancelled".to_string());
        }
        let seg_len = seg_max.min(total_in - pos);
        let out_len = seg_len * l;
        cursor.read(pos, seg_len, total_input_samples, &mut seg_in_l, &mut seg_in_r)?;
        let (seg_in_l, seg_in_r) = (&seg_in_l, &seg_in_r);

        // Exclusive &mut borrows of the segment buffers for the whole
        // segment; the raw views exist so rayon phases can share them.
        let shared_l = StridedOut {
            ptr: seg_out_l.as_mut_ptr(),
            len: out_len,
        };
        let shared_r = StridedOut {
            ptr: seg_out_r.as_mut_ptr(),
            len: out_len,
        };

        let run_phase = |phase: usize, dsp: &mut Box<dyn DspProcessor + Send>| -> Result<(), String> {
            let mut in_l = vec![0.0f64; chunk];
            let mut in_r = vec![0.0f64; chunk];
            let mut out_l = vec![0.0f64; chunk];
            let mut out_r = vec![0.0f64; chunk];
            let mut off = 0usize;
            while off < seg_len {
                if file_state.stop_requested() {
                    return Err("Cancelled".to_string());
                }
                let c = chunk.min(seg_len - off);
                // Zero flush beyond the real input (the cursor's) — same
                // samples the in-RAM pass feeds in its separate flush loop.
                in_l[..c].copy_from_slice(&seg_in_l[off..off + c]);
                in_r[..c].copy_from_slice(&seg_in_r[off..off + c]);
                dsp.process_audio(
                    &in_l[..c],
                    &in_r[..c],
                    &mut out_l[..c],
                    &mut out_r[..c],
                    c,
                );
                for i in 0..c {
                    let idx = (off + i) * l + phase;
                    // SAFETY: this thread is the only writer of indices
                    // ≡ phase (mod L); off+i < seg_len so idx < out_len.
                    unsafe {
                        shared_l.write(idx, out_l[i] * scale);
                        shared_r.write(idx, out_r[i] * scale);
                    }
                }
                off += c;
                bump(chunks_done.fetch_add(1, Ordering::Relaxed) + 1);
            }
            Ok(())
        };

        if gpu_bank {
            for (phase, dsp) in bank.iter_mut().enumerate() {
                run_phase(phase, dsp)?;
            }
        } else {
            use rayon::prelude::*;
            bank.par_iter_mut()
                .enumerate()
                .map(|(phase, dsp)| run_phase(phase, dsp))
                .collect::<Result<Vec<()>, String>>()?;
        }

        emit(&seg_out_l[..out_len], &seg_out_r[..out_len])?;
        pos += seg_len;
    }
    Ok(())
}

// ───────────────────────── streaming hybrid blend ────────────────────────

/// Scan the analysis-rate envelope (evaluated on the fly at output rate)
/// for switch boundaries — Step 1 of compute_switch_plan, identical
/// retriggerable-hold semantics.
fn scan_boundaries(
    analysis: &[f64],
    frames_to_output: f64,
    total: usize,
    cooldown: usize,
) -> (Vec<usize>, bool) {
    let thr = SWITCH_THRESHOLD;
    let env = |i: usize| catmull_env_at(analysis, frames_to_output, i);
    let mut boundaries = Vec::new();
    let mut min_active = env(0) >= thr;
    let use_min_0 = min_active;
    let mut min_end = 0usize;
    for i in 1..total {
        let m = env(i) >= thr;
        if m {
            if !min_active {
                min_active = true;
                boundaries.push(i);
            }
            min_end = i + cooldown;
        } else if min_active && i >= min_end {
            min_active = false;
            boundaries.push(i);
        }
    }
    (boundaries, use_min_0)
}

/// Streaming twin of `blend_outputs_stereo`: consumes linear and min-phase
/// streams in lockstep, snaps the precomputed envelope boundaries to
/// zero-crossings of the MID difference inside a small backlog margin, and
/// emits the hard-switched output. Same snap, same switch fade
/// (`switch_fade`), same stereo-linked plan as the batch code.
pub(crate) struct StreamingBlender {
    total: usize,
    sw: usize,
    hf: usize,
    pending: VecDeque<usize>,
    xfades: VecDeque<(usize, usize, bool)>, // (fade_start, fade_end, from_min)
    plan_pos: usize,
    plan_min: bool,
    cur_min: bool,
    n_boundaries: usize,
    emitted: usize,
    base: usize,
    lin_l: Vec<f64>,
    lin_r: Vec<f64>,
    min_l: Vec<f64>,
    min_r: Vec<f64>,
    out_l: Vec<f64>,
    out_r: Vec<f64>,
}

impl StreamingBlender {
    pub(crate) fn new(
        total: usize,
        boundaries: Vec<usize>,
        use_min_0: bool,
        output_sr: f64,
    ) -> Self {
        let (sw, switch_fade_len, _cooldown) = switch_params(output_sr);
        let n_boundaries = boundaries.len();
        Self {
            total,
            sw,
            hf: switch_fade_len / 2,
            pending: boundaries.into(),
            xfades: VecDeque::new(),
            plan_pos: 0,
            plan_min: use_min_0,
            cur_min: use_min_0,
            n_boundaries,
            emitted: 0,
            base: 0,
            lin_l: Vec::new(),
            lin_r: Vec::new(),
            min_l: Vec::new(),
            min_r: Vec::new(),
            out_l: Vec::new(),
            out_r: Vec::new(),
        }
    }

    fn filled(&self) -> usize {
        self.base + self.lin_l.len()
    }

    fn snap_ready(&mut self, at_end: bool) {
        loop {
            let b = match self.pending.front() {
                Some(&b) => b,
                None => break,
            };
            if !(at_end || b + self.sw < self.filled()) {
                break;
            }
            self.pending.pop_front();
            let sb = {
                let (lin_l, lin_r, min_l, min_r, base) = (
                    &self.lin_l,
                    &self.lin_r,
                    &self.min_l,
                    &self.min_r,
                    self.base,
                );
                let diff = move |j: usize| {
                    let k = j - base;
                    0.5 * ((lin_l[k] - min_l[k]) + (lin_r[k] - min_r[k]))
                };
                snap_boundary(b, self.total, self.sw, &diff)
            };
            let fade_start = sb.saturating_sub(self.hf).max(self.plan_pos);
            let fade_end = (sb + self.hf).min(self.total).max(fade_start);
            if fade_end > fade_start {
                self.xfades.push_back((fade_start, fade_end, self.plan_min));
            }
            self.plan_pos = fade_end;
            self.plan_min = !self.plan_min;
        }
    }

    fn emit_upto(
        &mut self,
        frontier: usize,
        emit: &mut impl FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<(), String> {
        while self.emitted < frontier {
            let chunk_end = frontier.min(self.emitted + EMIT_CHUNK);
            let n = chunk_end - self.emitted;
            self.out_l.clear();
            self.out_r.clear();
            self.out_l.reserve(n);
            self.out_r.reserve(n);

            let mut i = self.emitted;
            while i < chunk_end {
                let ev = self.xfades.front().copied();
                match ev {
                    Some((fs, _fe, _fm)) if i < fs => {
                        // flat region before the next fade
                        let e = fs.min(chunk_end);
                        let (src_l, src_r) = if self.cur_min {
                            (&self.min_l, &self.min_r)
                        } else {
                            (&self.lin_l, &self.lin_r)
                        };
                        self.out_l
                            .extend_from_slice(&src_l[i - self.base..e - self.base]);
                        self.out_r
                            .extend_from_slice(&src_r[i - self.base..e - self.base]);
                        i = e;
                    }
                    Some((fs, fe, from_min)) => {
                        // inside a switch fade
                        let e = fe.min(chunk_end);
                        let fade_len = fe - fs;
                        for j in i..e {
                            let k = j - fs;
                            let t = k as f64 / fade_len.max(1) as f64;
                            let blend = switch_fade(t);
                            let bi = j - self.base;
                            let (from_l, from_r, to_l, to_r) = if from_min {
                                (self.min_l[bi], self.min_r[bi], self.lin_l[bi], self.lin_r[bi])
                            } else {
                                (self.lin_l[bi], self.lin_r[bi], self.min_l[bi], self.min_r[bi])
                            };
                            self.out_l.push(from_l * (1.0 - blend) + to_l * blend);
                            self.out_r.push(from_r * (1.0 - blend) + to_r * blend);
                        }
                        i = e;
                        if e == fe {
                            self.xfades.pop_front();
                            self.cur_min = !self.cur_min;
                        }
                    }
                    None => {
                        let (src_l, src_r) = if self.cur_min {
                            (&self.min_l, &self.min_r)
                        } else {
                            (&self.lin_l, &self.lin_r)
                        };
                        self.out_l
                            .extend_from_slice(&src_l[i - self.base..chunk_end - self.base]);
                        self.out_r
                            .extend_from_slice(&src_r[i - self.base..chunk_end - self.base]);
                        i = chunk_end;
                    }
                }
            }

            emit(&self.out_l, &self.out_r)?;
            self.emitted = chunk_end;
        }

        // Drop consumed backlog; future snap windows start ≥ emitted+hf.
        if self.emitted > self.base {
            let drop_n = self.emitted - self.base;
            self.lin_l.drain(..drop_n);
            self.lin_r.drain(..drop_n);
            self.min_l.drain(..drop_n);
            self.min_r.drain(..drop_n);
            self.base = self.emitted;
        }
        Ok(())
    }

    pub(crate) fn push(
        &mut self,
        lin_l: &[f64],
        lin_r: &[f64],
        min_l: &[f64],
        min_r: &[f64],
        emit: &mut impl FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<(), String> {
        self.lin_l.extend_from_slice(lin_l);
        self.lin_r.extend_from_slice(lin_r);
        self.min_l.extend_from_slice(min_l);
        self.min_r.extend_from_slice(min_r);
        self.snap_ready(false);
        let frontier = match self.pending.front() {
            // Hold back far enough that the pending boundary's snap window
            // and fade can still be planned before those samples are emitted.
            Some(&b) => self.filled().min(b.saturating_sub(self.sw + self.hf)),
            None => self.filled(),
        };
        self.emit_upto(frontier, emit)
    }

    pub(crate) fn finish(
        mut self,
        emit: &mut impl FnMut(&[f64], &[f64]) -> Result<(), String>,
    ) -> Result<(), String> {
        debug_assert_eq!(self.filled(), self.total);
        self.snap_ready(true);
        let total = self.total;
        self.emit_upto(total, emit)?;
        crate::aelog!(
            "[HybridPhase] Stereo-linked blend (streaming): {} boundaries, {} crossfades — L/R switch at identical samples (mid-difference snap)",
            self.n_boundaries,
            self.n_boundaries, // one fade per boundary (edge-clipped fades excluded)
        );
        Ok(())
    }
}

// ───────────────────────── giant orchestrator ────────────────────────────

pub struct GiantHybridParams<'a> {
    pub min_phases: &'a [Vec<f64>],
    pub min_scale: f64,
    pub min_flush_input: usize,
    pub min_trim: usize,
    /// `.npy` the min-phase sub-filters came from — cache identity for the
    /// GPU filter spectra (see gpu::filter_cache).
    pub min_filter_path: Option<&'a str>,
}

/// A giant file between its render and its encode: the finished output-rate
/// render in a temp file next to the output, and its true peak if a pass
/// over it has already measured one.
///
/// The route runs in three steps so that the caller can do between them what
/// it does on the in-RAM route — decide the name and the tags from the stages
/// that actually ran. `render_giant` convolves and blends;
/// `run_giant_output_stages` streams XTC, the ISP output limiter and the
/// subsonic guard over the render; `finish_giant` sets the ceiling, dithers,
/// encodes and verifies.
pub struct GiantRender {
    tmp: TempRaw,
    stem: String,
    output_dir: PathBuf,
    n_out: usize,
    out_rate: u32,
    true_peak: Option<f64>,
}

/// The stages the in-RAM route runs on the finished render, before the
/// ceiling is measured, and in that order: XTC, the ISP output limiter, the
/// subsonic guard. Until 1.3.4 the segmented route ran none of them.
pub struct GiantOutputStages<'a> {
    pub xtc: Option<&'a crate::audio::converter::dsp::lab::xtc::XtcPlan>,
    /// The ceiling the limiter trims to, when ISP is on.
    pub limiter_target: Option<f64>,
    /// 0 when the subsonic filter is off.
    pub subsonic_hz: u32,
}

/// What each output stage did, for the caller's log lines, tags and rack row.
#[derive(Default)]
pub struct GiantOutputReport {
    /// (peak, RMS) before and after XTC.
    pub xtc: Option<((f64, f64), (f64, f64))>,
    /// As `limit_output` returns it: None when nothing was over.
    pub limiter: Option<crate::audio::converter::dsp::lab::isp::OutputLimitReport>,
    pub subsonic: Option<crate::audio::converter::apodize::SubsonicReport>,
}

/// A giant file's source before its passes: the HPSS envelope first when the
/// file blends (it reads the whole track; its own RAM is one block of
/// frames), then the source to disk and out of RAM. The passes read it back
/// segment by segment — the same frames, so the same output bits.
#[allow(clippy::too_many_arguments)]
fn spill_source(
    src_path: &Path,
    audio_l: Vec<f64>,
    audio_r: Vec<f64>,
    source_rate: u32,
    hybrid: bool,
    file_state: &Arc<FileConvState>,
    output_dir: &Path,
    stem: &str,
) -> Result<SpilledSource, String> {
    if hybrid {
        set_status("Hybrid-Phase: generating HPSS envelope (Rust)...");
        crate::audio::hpss_native::generate_and_save(
            src_path,
            &audio_l,
            &audio_r,
            source_rate,
            &file_state.cancelled,
        )
        .map_err(|e| format!("HPSS Native failed: {}", e))?;
    }
    set_status("Segmented: moving the source to disk...");
    let spilled = SpilledSource::spill(output_dir, stem, audio_l, audio_r)?;
    crate::audio::memory::log_process_memory("segmented: source on disk");
    Ok(spilled)
}

/// Convolve (and blend) one giant file into a temp render. Everything the
/// caller computed for the in-RAM path (decomposed phases, scale, flush and
/// trim arithmetic) is reused verbatim — this only changes WHERE the
/// intermediate data lives.
#[allow(clippy::too_many_arguments)]
pub fn render_giant(
    src_path: &Path,
    audio_l: Vec<f64>,
    audio_r: Vec<f64>,
    total_input_samples: usize,
    source_rate: u32,
    out_rate: u32,
    n_out: usize,
    phases: &[Vec<f64>],
    filter_path: Option<&str>,
    scale: f64,
    flush_input_samples: usize,
    linear_trim: usize,
    hybrid: Option<GiantHybridParams>,
    seg_input_samples: usize,
    settings: &ConvertSettings,
    file_state: &Arc<FileConvState>,
    output_dir: &Path,
) -> Result<GiantRender, String> {
    let stem = src_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    crate::aelog!(
        "[CONV] GIANT file: {} input samples → {} output samples — segmented pipeline (segment {} frames, ~{} MB, temps in {})",
        total_input_samples,
        n_out,
        seg_input_samples,
        segment_mb(seg_input_samples, phases.len() as u64),
        output_dir.display()
    );
    let src = spill_source(src_path, audio_l, audio_r, source_rate, hybrid.is_some(), file_state, output_dir, &stem)?;

    // ── Pass 1: linear-phase polyphase → temp A ──
    let tmp_a = TempRaw::new(output_dir, &stem, "lin");
    {
        let mut writer = FrameWriter::create(&tmp_a.path)?;
        let mut sink = TrimSink {
            skip: linear_trim,
            take: n_out,
            inner: |l: &[f64], r: &[f64]| writer.write_frames(l, r),
        };
        run_polyphase_pass_segmented(
            phases,
            SourceFrames::Spilled(&src),
            total_input_samples,
            flush_input_samples,
            scale,
            seg_input_samples,
            settings.use_gpu,
            settings.precision,
            "Polyphase (giant)",
            filter_path,
            file_state,
            0,
            if hybrid.is_some() { 400 } else { 600 },
            |l, r| sink.feed(l, r),
        )?;
        drop(sink);
        let frames = writer.finish()?;
        if frames != n_out as u64 {
            return Err(format!(
                "segmented linear pass produced {} frames, expected {}",
                frames, n_out
            ));
        }
    }

    let mut true_peak: Option<f64> = None;

    if let Some(h) = &hybrid {
        // ── Pass 2: minimum-phase polyphase → temp B ──
        let tmp_b = TempRaw::new(output_dir, &stem, "min");
        {
            let mut writer = FrameWriter::create(&tmp_b.path)?;
            let mut sink = TrimSink {
                skip: h.min_trim,
                take: n_out,
                inner: |l: &[f64], r: &[f64]| writer.write_frames(l, r),
            };
            run_polyphase_pass_segmented(
                h.min_phases,
                SourceFrames::Spilled(&src),
                total_input_samples,
                h.min_flush_input,
                h.min_scale,
                seg_input_samples,
                settings.use_gpu,
                settings.precision,
                "Hybrid-Phase: polyphase min (giant)",
                h.min_filter_path,
                file_state,
                400,
                300,
                |l, r| sink.feed(l, r),
            )?;
            drop(sink);
            let frames = writer.finish()?;
            if frames != n_out as u64 {
                return Err(format!(
                    "segmented min pass produced {} frames, expected {}",
                    frames, n_out
                ));
            }
        }

        drop(src);
        true_peak = Some(blend_giant_hybrid(
            src_path,
            &tmp_a,
            &tmp_b,
            out_rate,
            n_out,
            settings,
            file_state,
        )?);
        // tmp_b dropped here → min temp deleted before the final stage
    }
    // Non-hybrid: nothing has read the render yet. The peak is measured by
    // whichever pass reads it next — an output stage, or finish_giant.

    Ok(GiantRender {
        tmp: tmp_a,
        stem,
        output_dir: output_dir.to_path_buf(),
        n_out,
        out_rate,
        true_peak,
    })
}

/// The Hybrid-Phase blend of a giant file's two renders: the linear half in
/// `tmp_a`, the minimum-phase half in `tmp_b` (`n_out` frames each, on the
/// same output frames) — the HPSS envelope at the source rate, then the
/// streamed zero-crossing switch (or the continuous-alpha blend) written back
/// into `tmp_a`. Returns the blend's true peak. Shared by the single-stage
/// and the multistage routes. The envelope itself was made before the
/// passes (`spill_source`), while the source was still in RAM.
fn blend_giant_hybrid(
    src_path: &Path,
    tmp_a: &TempRaw,
    tmp_b: &TempRaw,
    out_rate: u32,
    n_out: usize,
    settings: &ConvertSettings,
    file_state: &Arc<FileConvState>,
) -> Result<f64, String> {
    let true_peak: f64;
    // ── HPSS envelope (made before the passes) + boundary scan ──
    file_state.gpu_pct.store(710, Ordering::Relaxed);
    let (analysis, env_sr) = crate::audio::hybrid_phase::load_analysis_envelope(src_path)
        .ok_or_else(|| "Hybrid-Phase: failed to load HPSS envelope".to_string())?;
    let frames_to_output = out_rate as f64 / env_sr;

    // Boundary scan is only needed for the binary switch path.
    let (boundaries, use_min_0) = if !settings.lab.continuous_alpha {
        set_status("Hybrid-Phase: scanning switch boundaries...");
        let (_, _, cooldown) = switch_params(out_rate as f64);
        scan_boundaries(&analysis, frames_to_output, n_out, cooldown)
    } else {
        (vec![], false)
    };
    file_state.gpu_pct.store(730, Ordering::Relaxed);

    // ── Streaming blend: A + B → A (in place), true peak on the fly ──
    if settings.lab.continuous_alpha {
        crate::aelog!("[ALPHA] Continuous hybrid blend enabled (segmented path)");
        // Smooth the analysis-rate envelope once up front (instant attack,
        // ~30 ms release), then interpolate per output sample.
        let smoothed = smooth_analysis_envelope(&analysis, env_sr);
        let mut reader_a = FrameReader::open(&tmp_a.path)?;
        let mut reader_b = FrameReader::open(&tmp_b.path)?;
        let mut writer_a = FrameWriter::overwrite(&tmp_a.path)?;
        let mut peak_scan = TruePeakScan::new(n_out);
        let mut la = Vec::new();
        let mut ra = Vec::new();
        let mut lb = Vec::new();
        let mut rb = Vec::new();
        let mut done = 0usize;
        while done < n_out {
            if file_state.stop_requested() {
                return Err("Cancelled".to_string());
            }
            let n = EMIT_CHUNK.min(n_out - done);
            reader_a.read_frames(n, &mut la, &mut ra)?;
            reader_b.read_frames(n, &mut lb, &mut rb)?;
            for i in 0..n {
                let alpha =
                    catmull_env_at(&smoothed, frames_to_output, done + i) as f64;
                la[i] = alpha * lb[i] + (1.0 - alpha) * la[i];
                ra[i] = alpha * rb[i] + (1.0 - alpha) * ra[i];
            }
            peak_scan.push(&la, &ra);
            writer_a.write_frames(&la, &ra)?;
            done += n;
            let pct = 730 + ((done as f64 / n_out as f64) * 60.0) as u32;
            file_state.gpu_pct.store(pct.min(790), Ordering::Relaxed);
        }
        let frames = writer_a.finish()?;
        if frames != n_out as u64 {
            return Err(format!(
                "streaming continuous blend produced {} frames, expected {}",
                frames, n_out
            ));
        }
        true_peak = peak_scan.finish();
    } else {
        set_status("Hybrid-Phase: streaming zero-crossing switch...");
        let mut reader_a = FrameReader::open(&tmp_a.path)?;
        let mut reader_b = FrameReader::open(&tmp_b.path)?;
        let mut writer_a = FrameWriter::overwrite(&tmp_a.path)?;
        let mut peak_scan = TruePeakScan::new(n_out);
        let mut blender =
            StreamingBlender::new(n_out, boundaries, use_min_0, out_rate as f64);

        let mut la = Vec::new();
        let mut ra = Vec::new();
        let mut lb = Vec::new();
        let mut rb = Vec::new();
        let mut done = 0usize;
        {
            let mut emit = |l: &[f64], r: &[f64]| -> Result<(), String> {
                peak_scan.push(l, r);
                writer_a.write_frames(l, r)
            };
            while done < n_out {
                if file_state.stop_requested() {
                    return Err("Cancelled".to_string());
                }
                let n = EMIT_CHUNK.min(n_out - done);
                reader_a.read_frames(n, &mut la, &mut ra)?;
                reader_b.read_frames(n, &mut lb, &mut rb)?;
                blender.push(&la, &ra, &lb, &rb, &mut emit)?;
                done += n;
                let pct = 730 + ((done as f64 / n_out as f64) * 60.0) as u32;
                file_state.gpu_pct.store(pct.min(790), Ordering::Relaxed);
            }
            blender.finish(&mut emit)?;
        }
        let frames = writer_a.finish()?;
        if frames != n_out as u64 {
            return Err(format!(
                "streaming blend produced {} frames, expected {}",
                frames, n_out
            ));
        }
        true_peak = peak_scan.finish();
    }
    Ok(true_peak)
}

/// `render_giant` for the multistage route: stage 0 segment by segment
/// through the short chain into the temp render (`ms_route::run_segmented`,
/// bit for bit the in-RAM route's output). Hybrid-Phase: the minimum-phase
/// half (`ms_min`) the same way into a second temp, then today's blend
/// (`blend_giant_hybrid`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_giant_multi(
    src_path: &Path,
    ms: &crate::audio::converter::pipeline::ms_route::MsStage0,
    ms_min: Option<&crate::audio::converter::pipeline::ms_route::MsStage0>,
    audio_l: Vec<f64>,
    audio_r: Vec<f64>,
    total_input_samples: usize,
    source_rate: u32,
    out_rate: u32,
    n_out: usize,
    seg_input_samples: usize,
    settings: &ConvertSettings,
    file_state: &Arc<FileConvState>,
    output_dir: &Path,
) -> Result<GiantRender, String> {
    let stem = src_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    crate::aelog!(
        "[CONV] GIANT file: {} input samples → {} output samples — segmented multistage pipeline (segment {} frames, ~{} MB, temps in {})",
        total_input_samples,
        n_out,
        seg_input_samples,
        segment_mb(seg_input_samples, (n_out / total_input_samples.max(1)) as u64),
        output_dir.display()
    );
    let src = spill_source(src_path, audio_l, audio_r, source_rate, ms_min.is_some(), file_state, output_dir, &stem)?;
    let tmp_a = TempRaw::new(output_dir, &stem, "lin");
    let mut writer = FrameWriter::create(&tmp_a.path)?;
    crate::audio::converter::pipeline::ms_route::run_segmented(
        ms,
        SourceFrames::Spilled(&src),
        total_input_samples,
        n_out,
        seg_input_samples,
        settings.use_gpu,
        settings.precision,
        "Multistage stage 0 (giant)",
        file_state,
        0,
        if ms_min.is_some() { 400 } else { 600 },
        |l, r| writer.write_frames(l, r),
    )?;
    let frames = writer.finish()?;
    if frames != n_out as u64 {
        return Err(format!(
            "segmented multistage pass produced {} frames, expected {}",
            frames, n_out
        ));
    }
    let mut true_peak: Option<f64> = None;
    if let Some(ms_min) = ms_min {
        let tmp_b = TempRaw::new(output_dir, &stem, "min");
        let mut writer = FrameWriter::create(&tmp_b.path)?;
        crate::audio::converter::pipeline::ms_route::run_segmented(
            ms_min,
            SourceFrames::Spilled(&src),
            total_input_samples,
            n_out,
            seg_input_samples,
            settings.use_gpu,
            settings.precision,
            "Hybrid-Phase: multistage min (giant)",
            file_state,
            400,
            300,
            |l, r| writer.write_frames(l, r),
        )?;
        let frames = writer.finish()?;
        if frames != n_out as u64 {
            return Err(format!(
                "segmented multistage min pass produced {} frames, expected {}",
                frames, n_out
            ));
        }
        drop(src);
        true_peak = Some(blend_giant_hybrid(
            src_path,
            &tmp_a,
            &tmp_b,
            out_rate,
            n_out,
            settings,
            file_state,
        )?);
        // tmp_b dropped here → min temp deleted before the final stage
    }
    Ok(GiantRender {
        tmp: tmp_a,
        stem,
        output_dir: output_dir.to_path_buf(),
        n_out,
        out_rate,
        true_peak,
    })
}

/// Stream the output stages over the render, in the in-RAM route's order —
/// XTC, then the ISP output limiter, then the subsonic guard — with the same
/// arithmetic (`XtcStream`, `OverScan`/`LimiterPlan`, `SubsonicGuardStream`,
/// each held bit for bit against its whole-buffer twin by a test).
///
/// Two passes at most. The limiter has to see every over in the file before
/// it may decide whether the overs are sparse, so the first pass runs XTC
/// (into a new temp) and finds the overs; the second applies the dips and the
/// subsonic guard and measures the true peak on what will be written.
pub fn run_giant_output_stages(
    render: &mut GiantRender,
    stages: &GiantOutputStages,
    file_state: &Arc<FileConvState>,
) -> Result<GiantOutputReport, String> {
    use crate::audio::converter::apodize::SubsonicGuardStream;
    use crate::audio::converter::dsp::lab::isp::{plan_output_limit, OverScan};

    let n_out = render.n_out;
    let out_rate = render.out_rate;
    let cancel = &file_state.cancelled;
    let mut report = GiantOutputReport::default();
    let check_cancel = || -> Result<(), String> {
        if file_state.stop_requested() {
            Err("Cancelled".to_string())
        } else {
            Ok(())
        }
    };

    // ── Pass 1: XTC (into a new temp) and the over scan ──
    let mut clusters = None;
    if stages.xtc.is_some() || stages.limiter_target.is_some() {
        set_status(if stages.xtc.is_some() {
            "XTC: crosstalk cancellation (streaming)..."
        } else {
            "ISP: scanning the render for overs..."
        });
        let mut scan = stages.limiter_target.map(|t| OverScan::new(t, out_rate));
        let mut reader = FrameReader::open(&render.tmp.path)?;
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut done = 0usize;
        if let Some(plan) = stages.xtc {
            let tmp_x = TempRaw::new(&render.output_dir, &render.stem, "xtc");
            let mut writer = FrameWriter::create(&tmp_x.path)?;
            // Measured here too: if nothing after XTC touches the render,
            // this is the peak the ceiling is set from.
            let mut peak = TruePeakScan::new(n_out);
            let mut stream = plan.stream();
            {
                let mut emit = |a: &[f64], b: &[f64]| -> Result<(), String> {
                    if let Some(s) = scan.as_mut() {
                        s.push(a, b);
                    }
                    peak.push(a, b);
                    writer.write_frames(a, b)
                };
                while done < n_out {
                    check_cancel()?;
                    let n = EMIT_CHUNK.min(n_out - done);
                    reader.read_frames(n, &mut l, &mut r)?;
                    stream.push(&l, &r, &mut emit)?;
                    done += n;
                }
                report.xtc = Some(stream.finish(&mut emit)?);
            }
            let frames = writer.finish()?;
            if frames != n_out as u64 {
                return Err(format!("streaming XTC produced {} frames, expected {}", frames, n_out));
            }
            drop(reader);
            render.tmp = tmp_x; // the pre-XTC render is deleted here
            render.true_peak = Some(peak.finish());
        } else {
            while done < n_out {
                check_cancel()?;
                let n = EMIT_CHUNK.min(n_out - done);
                reader.read_frames(n, &mut l, &mut r)?;
                if let Some(s) = scan.as_mut() {
                    s.push(&l, &r);
                }
                done += n;
            }
        }
        if let Some(s) = scan {
            let (c, scanned) = s.finish();
            if scanned != n_out {
                return Err(format!("ISP over scan saw {} frames, expected {}", scanned, n_out));
            }
            clusters = Some(c);
        }
    }

    // ── The limiter's decision, exactly as limit_output makes it ──
    let limiter = match (clusters, stages.limiter_target) {
        (Some(c), Some(target)) => plan_output_limit(c, n_out, target, out_rate),
        _ => None,
    };
    let dips = limiter.as_ref().map(|p| p.applies()).unwrap_or(false);

    // ── Pass 2: dips + subsonic guard, in place, peak on what is written ──
    if dips || stages.subsonic_hz != 0 {
        set_status("Output stages: limiter and subsonic guard (streaming)...");
        let mut reader = FrameReader::open(&render.tmp.path)?;
        // In place is safe: the guard hands samples back h_len/2 behind the
        // reader and the dips none ahead of it, so no write lands on a frame
        // still to be read.
        let mut writer = FrameWriter::overwrite(&render.tmp.path)?;
        let mut peak = TruePeakScan::new(n_out);
        let mut guard = if stages.subsonic_hz != 0 {
            Some(SubsonicGuardStream::new(out_rate, stages.subsonic_hz)?)
        } else {
            None
        };
        let (mut l, mut r, mut scratch) = (Vec::new(), Vec::new(), Vec::new());
        let mut done = 0usize;
        {
            let mut emit = |a: &[f64], b: &[f64]| -> Result<(), String> {
                peak.push(a, b);
                writer.write_frames(a, b)
            };
            while done < n_out {
                check_cancel()?;
                let n = EMIT_CHUNK.min(n_out - done);
                reader.read_frames(n, &mut l, &mut r)?;
                if dips {
                    limiter.as_ref().unwrap().apply(done, &mut l, &mut r, &mut scratch);
                }
                match guard.as_mut() {
                    Some(g) => g.push(&l, &r, &cancel, &mut emit)?,
                    None => emit(&l, &r)?,
                }
                done += n;
            }
            if let Some(g) = guard.take() {
                report.subsonic = Some(g.finish(&cancel, &mut emit)?);
            }
        }
        let frames = writer.finish()?;
        if frames != n_out as u64 {
            return Err(format!("output stages wrote {} frames, expected {}", frames, n_out));
        }
        render.true_peak = Some(peak.finish());
    }
    report.limiter = limiter.map(|p| p.report);
    file_state.gpu_pct.store(795, Ordering::Relaxed);
    Ok(report)
}

/// Ceiling, dither, FLAC, verification — the end of the segmented route.
///
/// `album_gain` is the album level the render still takes (1.0 for a file on
/// its own); the output stages have held it at the ceiling divided by that,
/// and it goes into the dither pass with the ceiling's own gain. Returns the
/// output path and the gain the ceiling took on top of the album level.
///
/// With `dsd` (the export and its ID3 tag) the render goes through the
/// modulator into a DSF file instead: no PCM dither, read back and checked.
#[allow(clippy::too_many_arguments)]
pub fn finish_giant(
    render: GiantRender,
    true_peak_target_dbtp: f64,
    album_gain: f64,
    album_status: &str,
    file_state: &Arc<FileConvState>,
    output_name: &str,
    flac_tags: &[(String, String)],
    dsd: Option<(DsdExport, &[u8])>,
) -> Result<(String, f64), String> {
    let GiantRender { tmp: tmp_a, stem, output_dir, n_out, out_rate, true_peak } = render;
    let output_dir = output_dir.as_path();
    let mut true_peak = match true_peak {
        Some(p) => p,
        None => {
            // Nothing has read the render since it was written.
            set_status("Measuring true peak (streaming)...");
            let mut reader = FrameReader::open(&tmp_a.path)?;
            let mut scan = TruePeakScan::new(n_out);
            let mut l = Vec::new();
            let mut r = Vec::new();
            let mut done = 0usize;
            while done < n_out {
                if file_state.stop_requested() {
                    return Err("Cancelled".to_string());
                }
                let n = EMIT_CHUNK.min(n_out - done);
                reader.read_frames(n, &mut l, &mut r)?;
                scan.push(&l, &r);
                done += n;
            }
            scan.finish()
        }
    };
    if !true_peak.is_finite() {
        true_peak = 0.0;
    }

    // ── Gain decision — same policy as apply_true_peak_normalization ──
    let reduction = if album_gain != 1.0 {
        crate::audio::converter::dsp::true_peak::ceiling_after_album_level(
            true_peak * album_gain,
            true_peak_target_dbtp,
            album_gain,
        )
    } else {
        let target_lin = target_lin_for(true_peak_target_dbtp);
        let true_peak_db = 20.0 * (true_peak + 1e-300).log10();
        crate::aelog!(
            "[CONV] Output True peak: {:.2} dBTP ({:.6})  target: {:.2} dBTP ({:.6})",
            true_peak_db,
            true_peak,
            true_peak_target_dbtp,
            target_lin
        );
        if true_peak > target_lin {
            let g = target_lin / true_peak;
            crate::aelog!(
                "[CONV] Peak exceeds {:.2} dBTP — normalizing: gain {:.6} ({:.2} dB)",
                true_peak_target_dbtp,
                g,
                20.0 * g.log10()
            );
            g
        } else {
            1.0
        }
    };

    if let Some((export, id3)) = dsd {
        set_status(&format!("Encoding DSF (DSD{}, {}, streaming)...", export.multiple, export.kind.label()));
        file_state.gpu_pct.store(800, Ordering::Relaxed);
        let output_path = output_dir.join(output_name);
        let gain = album_gain * reduction;
        let mut reader = FrameReader::open(&tmp_a.path)?;
        let encoded = dsd_out::encode_dsf(
            &output_path,
            out_rate,
            export,
            n_out,
            id3,
            &file_state.cancelled,
            |_, n, l, r| {
                reader.read_frames(n, l, r)?;
                l.iter_mut().chain(r.iter_mut()).for_each(|x| *x *= gain);
                Ok(())
            },
            |k| {
                let pct = 800 + ((k as f64 / n_out.max(1) as f64) * 140.0) as u32;
                file_state.gpu_pct.store(pct.min(940), Ordering::Relaxed);
            },
        );
        drop(tmp_a);
        set_status("Verifying DSF output...");
        file_state.gpu_pct.store(950, Ordering::Relaxed);
        let checked = encoded.and_then(|done| dsd_out::check_or_rename(&output_path, &done).map(|_| done));
        file_state.gpu_pct.store(1000, Ordering::Relaxed);
        return match checked {
            Ok(done) => {
                let size_mb = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                set_status(&format!(
                    "\u{2713} Done: {} ({:.1} MB{}{}{})",
                    output_name,
                    size_mb,
                    album_status,
                    crate::audio::converter::dsp::lab::ceiling_note(reduction, true_peak_target_dbtp),
                    dsd_out::overload_note(done.overloads)
                ));
                Ok((output_path.to_string_lossy().to_string(), reduction))
            }
            Err(e) => {
                set_status(&e);
                Err(e)
            }
        };
    }

    // ── Final stage: gain → dither → FLAC (streaming) + expected temp D ──
    set_status("Encoding FLAC (streaming)...");
    file_state.gpu_pct.store(800, Ordering::Relaxed);
    let output_path = output_dir.join(output_name);
    let tmp_d = TempRaw::new(output_dir, &stem, "exp");
    {
        let mut reader = FrameReader::open(&tmp_a.path)?;
        let mut expected = FrameWriter::create(&tmp_d.path)?;
        let mut encoder = StreamingFlacEncoder::new_tagged(out_rate, &output_path, flac_tags)?;
        let mut dither = DitherState::new(out_rate);
        let mut l = Vec::new();
        let mut r = Vec::new();
        let mut done = 0usize;
        while done < n_out {
            let n = EMIT_CHUNK.min(n_out - done);
            reader.read_frames(n, &mut l, &mut r)?;
            // Gain is applied inside dither.process below — see DitherState.
            dither.process(&mut l, &mut r, album_gain * reduction);
            expected.write_frames(&l, &r)?;
            encoder.feed(&l, &r)?;
            done += n;
            let pct = 800 + ((done as f64 / n_out as f64) * 140.0) as u32;
            file_state.gpu_pct.store(pct.min(940), Ordering::Relaxed);
        }
        expected.finish()?;
        encoder.finish()?;
    }
    drop(tmp_a); // blended temp no longer needed

    // ── Verification against the post-dither expected stream ──
    set_status("Verifying bit-perfect output (streaming)...");
    file_state.gpu_pct.store(950, Ordering::Relaxed);
    match verify_flac_against_file(&output_path, &tmp_d.path, n_out, out_rate, file_state) {
        Ok(_) => {
            let file_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
            let size_mb = file_size as f64 / 1_048_576.0;
            set_status(&format!(
                "\u{2713} Done: {} ({:.1} MB{}{})",
                output_name,
                size_mb,
                album_status,
                crate::audio::converter::dsp::lab::ceiling_note(
                    reduction,
                    true_peak_target_dbtp
                )
            ));
            file_state.gpu_pct.store(1000, Ordering::Relaxed);
            Ok((output_path.to_string_lossy().to_string(), reduction))
        }
        Err(err_msg) => {
            let failed_name = output_name.replace(".flac", "_UNVERIFIED.flac");
            let failed_path = output_dir.join(&failed_name);
            let _ = std::fs::rename(&output_path, &failed_path);
            set_status(&format!("Verification failed: {}", err_msg));
            file_state.gpu_pct.store(1000, Ordering::Relaxed);
            Err(format!("Verification failed: {}", err_msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::dsp::polyphase::polyphase_decompose;
    use crate::audio::converter::dsp::true_peak::measure_true_peak;
    use crate::audio::hybrid_phase::{blend_outputs_stereo, BlendEnvelope};

    fn xorshift_noise(n: usize, seed: u64, amp: f64) -> Vec<f64> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x % 2_000_001) as f64 / 1_000_000.0 - 1.0) * amp
            })
            .collect()
    }

    /// Kaiser-windowed sinc lowpass (linear-phase), sum-normalized to DC=1.
    fn kaiser_lowpass(taps: usize, fc_norm: f64) -> Vec<f64> {
        let half = taps / 2;
        let beta = 8.0;
        let i0 = |x: f64| {
            let mut sum = 1.0;
            let mut term = 1.0;
            let x2 = (x / 2.0).powi(2);
            for k in 1..50 {
                term *= x2 / (k as f64).powi(2);
                sum += term;
                if term < 1e-15 * sum {
                    break;
                }
            }
            sum
        };
        let i0b = i0(beta);
        let mut h = Vec::with_capacity(taps);
        let mut s = 0.0;
        for i in 0..taps {
            let m = i as f64 - half as f64;
            let sinc = if m.abs() < 1e-12 {
                fc_norm
            } else {
                (std::f64::consts::PI * fc_norm * m).sin() / (std::f64::consts::PI * m)
            };
            let arg = (2.0 * i as f64) / (taps as f64 - 1.0) - 1.0;
            let v = sinc * i0(beta * (1.0 - arg * arg).max(0.0).sqrt()) / i0b;
            h.push(v);
            s += v;
        }
        for v in h.iter_mut() {
            *v /= s;
        }
        h
    }

    /// EQUIVALENCE GUARD: the segmented pass (persistent convolver bank,
    /// odd-sized segments, TrimSink) must produce BIT-IDENTICAL output to
    /// the in-RAM pass + drain/truncate for the same filter and input.
    #[test]
    fn segmented_pass_matches_in_ram_pass_bit_exact() {
        let l = 4usize;
        let taps = 4001usize;
        let h = kaiser_lowpass(taps, 1.0 / l as f64);
        let dc_gain: f64 = h.iter().sum();
        let scale = l as f64 / dc_gain.abs();
        let phases = polyphase_decompose(&h, l);
        let sub_taps = phases[0].len();

        let ola = crate::audio::dsp_core::CpuDspProcessor::output_latency_for(sub_taps);
        let sub_delay = (sub_taps - 1) / 2;
        let flush = ola + sub_delay + 1;
        let trim = ola * l + (taps - 1) / 2;

        let n_in = 150_000usize;
        let in_l = xorshift_noise(n_in, 0xA5A5_1234_5678_0001, 0.7);
        let in_r = xorshift_noise(n_in, 0x5A5A_8765_4321_0002, 0.7);
        let n_out = n_in * l;
        let file_state = Arc::new(FileConvState::new());

        // Reference: in-RAM pass + drain/truncate (the production path)
        let n_out_flush = (n_in + flush) * l;
        let mut ref_l = vec![0.0f64; n_out_flush];
        let mut ref_r = vec![0.0f64; n_out_flush];
        crate::audio::converter::process::run_polyphase_pass(
            &phases, &in_l, &in_r, n_in, flush, scale, &mut ref_l, &mut ref_r, false, 64,
            "ref", None, &file_state, 0, 600,
        )
        .expect("in-RAM pass failed");
        ref_l.drain(..trim);
        ref_l.truncate(n_out);
        ref_r.drain(..trim);
        ref_r.truncate(n_out);

        // Segmented: an odd segment size (NOT a multiple of the 32768
        // chunk), the smallest segment a low-RAM plan takes and the next one
        // up — from RAM and from the source spilled to disk.
        let dir = std::env::temp_dir();
        let spilled = SpilledSource::spill(&dir, "seg-equivalence", in_l.clone(), in_r.clone())
            .expect("spill failed");
        for (seg, from_disk) in [(10_000, false), (10_000, true), (SEG_INPUT_MIN, true), (SEG_INPUT_MIN * 2, false)] {
            let source = if from_disk { SourceFrames::Spilled(&spilled) } else { SourceFrames::Ram(&in_l, &in_r) };
            let mut seg_l: Vec<f64> = Vec::with_capacity(n_out);
            let mut seg_r: Vec<f64> = Vec::with_capacity(n_out);
            let mut sink = TrimSink {
                skip: trim,
                take: n_out,
                inner: |l: &[f64], r: &[f64]| {
                    seg_l.extend_from_slice(l);
                    seg_r.extend_from_slice(r);
                    Ok(())
                },
            };
            run_polyphase_pass_segmented(
                &phases, source, n_in, flush, scale, seg, false, 64, "seg",
                None, &file_state, 0, 600,
                |l, r| sink.feed(l, r),
            )
            .expect("segmented pass failed");
            drop(sink);

            assert_eq!(seg_l.len(), n_out);
            assert_eq!(seg_r.len(), n_out);
            for i in 0..n_out {
                assert!(
                    seg_l[i] == ref_l[i] && seg_r[i] == ref_r[i],
                    "segment {} (disk {}): output diverges at sample {}: L {} vs {}, R {} vs {}",
                    seg, from_disk, i, seg_l[i], ref_l[i], seg_r[i], ref_r[i]
                );
            }
        }
    }

    /// The route on the reported laptop (7.5 GB seen, ~2.3 GB free, FS×8):
    /// a 3-minute track no longer fits in RAM, so it goes segmented, with
    /// a segment its free RAM holds (smaller as it runs out); a big machine keeps it in RAM with the
    /// full segment.
    #[test]
    fn low_ram_takes_the_segmented_route_with_a_smaller_segment() {
        let three_min = in_ram_peak_mb(7_938_000, 8, false) * 6 / 5 + 256;
        let laptop = plan_route_with(three_min, 8, 7680, 2300);
        assert!(laptop.segmented);
        assert_eq!(laptop.seg_input_samples % 32768, 0);
        // ~1 GB free: an eighth of the full segment, its buffers ~20 MB;
        // nothing free past the margin: the smallest one.
        let tight = plan_route_with(three_min, 8, 7680, 1024);
        assert!(tight.segmented);
        assert_eq!(tight.seg_input_samples, SEG_INPUT_SAMPLES / 8);
        assert!(segment_mb(tight.seg_input_samples, 8) <= 20);
        let none = plan_route_with(three_min, 8, 7680, 600);
        assert_eq!(none, SegPlan { segmented: true, seg_input_samples: SEG_INPUT_MIN });
        let big = plan_route_with(three_min, 8, 65536, 40_000);
        assert_eq!(big, SegPlan { segmented: false, seg_input_samples: SEG_INPUT_SAMPLES });
        // An 8 GB machine has no 4 GB floor any more: a quarter of its RAM.
        assert_eq!(giant_threshold_mb(7680), 1920);
        assert_eq!(giant_threshold_mb(128 * 1024), 12_288);
    }

    /// EQUIVALENCE GUARD: the output stages the segmented route streams over
    /// its temp render — XTC, the ISP output limiter, the subsonic guard, in
    /// place and across several emit chunks — must leave exactly what the
    /// in-RAM route leaves in its buffer, and read the same true peak. Run
    /// with the limiter applying its dips, with it falling back, and with
    /// each stage alone.
    #[test]
    fn giant_output_stages_match_in_ram_chain_bit_exact() {
        use crate::audio::converter::apodize::apply_subsonic_guard;
        use crate::audio::converter::dsp::lab::{isp, xtc};
        use crate::audio::converter::dsp::true_peak::target_lin_for;
        use crate::audio::converter::types::XtcGeometry as Ui;
        use std::sync::atomic::AtomicBool;

        let rate = 88_200u32;
        let n = 2 * EMIT_CHUNK + 123_457;
        let target = target_lin_for(-0.5);
        let ui = Ui {
            speaker_span_mm: 2000.0,
            left_distance_mm: 2600.0,
            right_distance_mm: 2650.0,
            head_width_mm: 180.0,
        };
        let plan = xtc::plan(rate, &ui).expect("a usable triangle");
        let dir = std::env::temp_dir().join(format!("ae_giant_stages_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(FileConvState::new());

        // Programme at about −8 dBFS with sparse overs (the limiter dips) and
        // the same programme pushed over everywhere (it falls back).
        let base_l: Vec<f64> = xorshift_noise(n, 0x5eed_0001, 0.35)
            .iter()
            .enumerate()
            .map(|(i, v)| v + 0.2 * (i as f64 * 0.003).sin())
            .collect();
        let base_r = xorshift_noise(n, 0x5eed_0002, 0.35);
        let mut sparse_l = base_l.clone();
        let mut sparse_r = base_r.clone();
        for c in (40_000..n - 100).step_by(310_001) {
            for k in 0..60 {
                let w = (std::f64::consts::PI * k as f64 / 60.0).sin();
                sparse_l[c + k] += 0.9 * w;
                sparse_r[c + k] -= 0.7 * w;
            }
        }
        let loud_l: Vec<f64> = base_l.iter().map(|v| v * 3.5).collect();
        let loud_r: Vec<f64> = base_r.iter().map(|v| v * 3.5).collect();

        let cases: [(&str, &[f64], &[f64], bool, bool, u32); 5] = [
            ("all three, dips", &sparse_l, &sparse_r, true, true, 20),
            ("all three, fallback", &loud_l, &loud_r, true, true, 15),
            ("xtc only", &sparse_l, &sparse_r, true, false, 0),
            ("limiter only", &sparse_l, &sparse_r, false, true, 0),
            ("guard only", &sparse_l, &sparse_r, false, false, 10),
        ];
        for (name, l, r, with_xtc, with_isp, sub) in cases {
            // In RAM, as process.rs runs them.
            let (mut ml, mut mr) = (l.to_vec(), r.to_vec());
            if with_xtc {
                xtc::apply_xtc(&mut ml, &mut mr, &plan.h_direct, &plan.h_cross);
            }
            let want_lim = if with_isp { isp::limit_output(&mut ml, &mut mr, target, rate) } else { None };
            if sub != 0 {
                apply_subsonic_guard(&mut ml, &mut mr, rate, sub, &AtomicBool::new(false)).unwrap();
            }
            let want_peak = measure_true_peak(&ml, &mr);

            // Streamed, through a temp render.
            // "src", never a tag the stages use for their own temps ("xtc").
            let tmp = TempRaw::new(&dir, "stages", "src");
            let mut w = FrameWriter::create(&tmp.path).unwrap();
            w.write_frames(l, r).unwrap();
            w.finish().unwrap();
            let mut render = GiantRender {
                tmp,
                stem: "stages".to_string(),
                output_dir: dir.clone(),
                n_out: n,
                out_rate: rate,
                true_peak: None,
            };
            let stages = GiantOutputStages {
                xtc: if with_xtc { Some(&plan) } else { None },
                limiter_target: if with_isp { Some(target) } else { None },
                subsonic_hz: sub,
            };
            let rep = run_giant_output_stages(&mut render, &stages, &state).unwrap();
            let mut rd = FrameReader::open(&render.tmp.path).unwrap();
            let (mut gl, mut gr) = (Vec::new(), Vec::new());
            rd.read_frames(n, &mut gl, &mut gr).unwrap();
            assert!(gl == ml && gr == mr, "{}: streamed output differs from the in-RAM chain", name);
            assert_eq!(render.true_peak.map(f64::to_bits), Some(want_peak.to_bits()), "{}: true peak", name);
            match (&want_lim, &rep.limiter) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    assert_eq!(a.fell_back, b.fell_back, "{}", name);
                    assert_eq!(a.clusters, b.clusters, "{}", name);
                    assert_eq!(a.gr_fraction.to_bits(), b.gr_fraction.to_bits(), "{}", name);
                }
                _ => panic!("{}: limiter report differs", name),
            }
            if name == "all three, dips" {
                assert_eq!(want_lim.as_ref().and_then(|r| r.fell_back), None, "the dips case must dip");
            }
            if name == "all three, fallback" {
                assert!(want_lim.as_ref().map(|r| r.fell_back.is_some()).unwrap_or(false), "the loud case must fall back");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// EQUIVALENCE GUARD: the streaming blender (precomputed boundaries,
    /// margin-based snap, incremental crossfades) must reproduce
    /// blend_outputs_stereo BIT-EXACTLY on the same signals and envelope.
    #[test]
    fn streaming_blender_matches_batch_blend_bit_exact() {
        let out_sr = 352_800.0f64;
        let env_sr = 86.13f64;
        let n = 400_000usize;
        let frames_to_output = out_sr / env_sr;

        // Analysis-rate envelope with several bursts above the 0.3 threshold
        let n_frames = (n as f64 / frames_to_output).ceil() as usize + 4;
        let mut analysis = vec![0.0f64; n_frames];
        for (i, v) in analysis.iter_mut().enumerate() {
            let phase = i % 23;
            *v = match phase {
                3..=5 => 0.9,
                6..=8 => 0.45,
                _ => 0.05,
            };
        }

        // Distinct lin/min signals with plenty of zero crossings in the diff
        let lin_l = xorshift_noise(n, 0x1111_2222_3333_0001, 0.5);
        let lin_r = xorshift_noise(n, 0x4444_5555_6666_0002, 0.5);
        let min_l: Vec<f64> = lin_l
            .iter()
            .enumerate()
            .map(|(i, v)| v * 0.8 + 0.1 * ((i as f64) * 0.01).sin())
            .collect();
        let min_r: Vec<f64> = lin_r
            .iter()
            .enumerate()
            .map(|(i, v)| v * 0.8 + 0.1 * ((i as f64) * 0.013).cos())
            .collect();

        // Batch reference — materialized envelope through the same spline
        let envelope: Vec<f32> = (0..n)
            .map(|i| catmull_env_at(&analysis, frames_to_output, i))
            .collect();
        let env = BlendEnvelope {
            envelope,
            analysis_envelope: vec![],
            analysis_sr: env_sr,
        };
        let (ref_l, ref_r) =
            blend_outputs_stereo(&lin_l, &min_l, &lin_r, &min_r, &env, 0, out_sr);

        // Streaming path: boundary scan + incremental blender, odd segments
        let (_, _, cooldown) = switch_params(out_sr);
        let (boundaries, use_min_0) = scan_boundaries(&analysis, frames_to_output, n, cooldown);
        let mut blender = StreamingBlender::new(n, boundaries, use_min_0, out_sr);
        let mut got_l: Vec<f64> = Vec::with_capacity(n);
        let mut got_r: Vec<f64> = Vec::with_capacity(n);
        {
            let mut emit = |l: &[f64], r: &[f64]| -> Result<(), String> {
                got_l.extend_from_slice(l);
                got_r.extend_from_slice(r);
                Ok(())
            };
            let seg = 37_123usize;
            let mut pos = 0usize;
            while pos < n {
                let e = (pos + seg).min(n);
                blender
                    .push(
                        &lin_l[pos..e],
                        &lin_r[pos..e],
                        &min_l[pos..e],
                        &min_r[pos..e],
                        &mut emit,
                    )
                    .expect("blender push failed");
                pos = e;
            }
            blender.finish(&mut emit).expect("blender finish failed");
        }

        assert_eq!(got_l.len(), ref_l.len());
        for i in 0..n {
            assert!(
                got_l[i] == ref_l[i] && got_r[i] == ref_r[i],
                "streaming blend diverges at sample {}: L {} vs {}, R {} vs {}",
                i, got_l[i], ref_l[i], got_r[i], ref_r[i]
            );
        }
    }

    /// EQUIVALENCE GUARD: the streaming true-peak scanner must return the
    /// exact value of the batch measurement, including reflective edges.
    #[test]
    fn streaming_true_peak_matches_batch() {
        let n = 100_000usize;
        let l = xorshift_noise(n, 0xDEAD_BEEF_0000_0001, 1.02);
        let r = xorshift_noise(n, 0xBEEF_DEAD_0000_0002, 1.02);
        let reference = measure_true_peak(&l, &r);

        let mut scan = TruePeakScan::new(n);
        let mut pos = 0usize;
        let seg = 7_919usize; // prime-sized chunks
        while pos < n {
            let e = (pos + seg).min(n);
            scan.push(&l[pos..e], &r[pos..e]);
            pos = e;
        }
        let got = scan.finish();
        assert!(
            got == reference,
            "streaming true peak {} != batch {}",
            got, reference
        );
    }
}
