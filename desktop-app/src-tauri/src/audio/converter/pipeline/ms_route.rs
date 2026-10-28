//! The converter's multistage route (`UpsamplerMode::Multi`): stage 0 is the
//! blob's polyphase branch 0, h[kL], run at the SOURCE rate through the same
//! convolver the single-stage pass uses (CPU, or the GPU with branch 0's
//! cached spectrum), then the short chain of `dsp::multistage` takes it up to
//! ×L — whole-track in RAM, or streamed segment by segment on the giant route.
//!
//! Only branch 0 is read (RAM ≈ N/L): its Σh over the whole blob, the blob's
//! length and its head come with it, and today's symmetry test reads the last
//! taps straight off the file's end. The trim is today's (`MsPlan`'s `d`, the
//! single-stage trim without the convolver's pre-roll), so the output lands on
//! today's frames; the stage-0 convolver's own pre-roll is dropped before the
//! chain, together with the plan's `k0`.
//!
//! Multi is the default; Single (the hidden fallback) is untouched: nothing
//! here runs when the settings ask for Single or the gate (`chain_for`) finds
//! no chain.

use crate::audio::converter::decode::set_status;
use crate::audio::converter::dsp::ms_h1::H1Stage;
use crate::audio::converter::dsp::multistage::{multistage_chain, InterpChain, MsPlan, UpsamplerMode};
use crate::audio::converter::pipeline::segmented::{run_polyphase_pass_segmented_from, SourceFrames};
use crate::audio::converter::process::{run_polyphase_pass_from, FilterSource};
use crate::audio::converter::state::{cancel_requested, current_file, enter_file, FileConvState};
use crate::audio::converter::types::ConvertSettings;
use crate::audio::dsp_core::NpyBranch;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Stage-0 frames handed to the chain at a time on the in-RAM route (a
/// cancellation point; any piece size gives the same bits).
const CHAIN_PIECE: usize = 1 << 20;

/// Taps at each end today's linear-phase test compares.
pub(crate) const SYMMETRY_TAPS: usize = 50;

/// Today's linear-phase test: the first and the last `min(50, n/2)` taps of an
/// `n`-tap filter mirror each other. `first` = h[..c], `last` = h[n − c..].
pub(crate) fn linear_by_symmetry(first: &[f64], last: &[f64], n: usize) -> bool {
    if n <= 10 {
        return false;
    }
    let c = SYMMETRY_TAPS.min(n / 2);
    let mut sym_err = 0.0;
    for i in 0..c {
        sym_err += (first[i] - last[c - 1 - i]).abs();
    }
    sym_err < 1e-6 * c as f64
}

/// The chain a conversion plays multistage through, or `None` = single stage.
///
/// Hybrid-Phase (and its continuous-alpha blend) plays both halves
/// multistage, each through the same chain (`prepare_min` for the
/// minimum-phase half), blended at the output rate as today. A custom filter
/// stays single-stage — its design rate and wall are unknown, and the chain
/// is built for the shipped blobs' wall under the source's Nyquist.
pub(crate) fn chain_for(
    settings: &ConvertSettings,
    taps: usize,
    src_rate: u32,
    l: usize,
) -> Option<&'static [H1Stage]> {
    if settings.custom_filter_path.is_some() {
        return None;
    }
    multistage_chain(settings.upsampler, taps, src_rate, l)
}

/// The hidden single-stage fallback changed this conversion's route: Single was asked for where the default
/// (multistage) would have run. Such a render is named and tagged as single-stage; one that is single-stage
/// either way (1M, an odd rate, a custom filter) is not.
pub(crate) fn single_fallback(settings: &ConvertSettings, taps: usize, src_rate: u32, l: usize) -> bool {
    settings.upsampler == UpsamplerMode::Single
        && chain_for(&ConvertSettings { upsampler: UpsamplerMode::Multi, ..settings.clone() }, taps, src_rate, l)
            .is_some()
}

/// Stage 0 of one conversion, ready to run.
pub(crate) struct MsStage0 {
    /// h[kL]: the blob's polyphase branch 0.
    pub(crate) branch: Vec<f64>,
    pub(crate) plan: MsPlan,
    /// The blob the branch came from: the identity of its spectrum in the GPU
    /// cache (`None` for coefficients with no file behind them, as in tests).
    pub(crate) path: Option<String>,
    /// The stage-0 convolver's pre-roll, source frames.
    pub(crate) ola_latency: usize,
}

impl MsStage0 {
    /// Stage 0 for a blob read as `branch` (branch 0 of `l`), with `tail` its
    /// last taps (`SYMMETRY_TAPS.min(full_len / 2)` of them) and `tfs_ahead`
    /// the TFS look-ahead when the blob is a TFS filter. The trim `d` is
    /// today's: the look-ahead for TFS, (N − 1)/2 for a symmetric blob, 0 for
    /// any other (the converter's minimum-phase rule).
    pub(crate) fn new(
        b: NpyBranch,
        tail: &[f64],
        tfs_ahead: Option<usize>,
        l: usize,
        chain: &[H1Stage],
        path: Option<String>,
        use_gpu: bool,
    ) -> MsStage0 {
        let n = b.full_len;
        let d = match tfs_ahead {
            Some(k) => k,
            None if linear_by_symmetry(&b.head, tail, n) => (n - 1) / 2,
            None => 0,
        };
        MsStage0::with_d(b, d, l, chain, path, use_gpu)
    }

    /// Stage 0 for a blob read as `branch` with the trim `d` given (output
    /// frames): the Hybrid-Phase minimum-phase half's band-weighted delay.
    pub(crate) fn with_d(
        b: NpyBranch,
        d: usize,
        l: usize,
        chain: &[H1Stage],
        path: Option<String>,
        use_gpu: bool,
    ) -> MsStage0 {
        let plan = MsPlan::new(d, l, b.sum, chain);
        let ola_latency = if use_gpu {
            crate::audio::gpu::GpuDspProcessor::output_latency_for(b.taps.len())
        } else {
            crate::audio::dsp_core::CpuDspProcessor::output_latency_for(b.taps.len())
        };
        MsStage0 { branch: b.taps, plan, path, ola_latency }
    }

    /// Stage-0 frames the convolver produces before the plan's index 0: its
    /// pre-roll and the plan's `k0`.
    pub(crate) fn skip(&self) -> usize {
        self.ola_latency + self.plan.k0
    }

    /// Zeros after the source so that stage 0 produces every frame the chain
    /// reads for `n_out` outputs (the chain itself has no flush).
    pub(crate) fn flush(&self, total_input_samples: usize, n_out: usize) -> usize {
        (self.skip() + self.plan.stage0_frames(n_out as u64) as usize).saturating_sub(total_input_samples)
    }

    fn source(&self) -> Option<FilterSource<'_>> {
        self.path.as_deref().map(|path| FilterSource { path, ratio: self.plan.l })
    }
}

/// Multi's stage 0 for the blob at `path`, or `None` when the gate keeps the
/// file single-stage. Reads branch 0 alone (and the blob's last taps).
pub(crate) fn prepare(
    settings: &ConvertSettings,
    tfs_active: bool,
    path: &str,
    src_rate: u32,
    out_rate: u32,
    l: usize,
) -> Result<Option<MsStage0>, String> {
    // The rung decides (its nominal size, as the player's gate: a "5M" blob may be a few taps short of 5M), before
    // anything is read.
    if settings.upsampler != UpsamplerMode::Multi {
        return Ok(None);
    }
    let Some(chain) = chain_for(settings, settings.taps, src_rate, l) else {
        return Ok(None);
    };
    let b = crate::audio::dsp_core::load_npy_f64_branch(path, l, 0)?;
    let tail = npy_tail(path, SYMMETRY_TAPS.min(b.full_len / 2))?;
    let tfs_ahead = tfs_active.then(|| crate::audio::converter::dsp::lab::tfs::look_ahead(b.full_len, out_rate));
    let full_len = b.full_len;
    let ms = MsStage0::new(b, &tail, tfs_ahead, l, chain, Some(path.to_string()), settings.use_gpu);
    crate::aelog!(
        "[CONV] Multistage: {} taps → branch 0 ({} taps) at {} Hz + {}-stage chain ×{} | d = {} output frames, k0 = {}, stage-0 history {} | {} multiplies per source frame in the chain",
        full_len,
        ms.branch.len(),
        src_rate,
        chain.len(),
        l,
        ms.plan.d,
        ms.plan.k0,
        ms.plan.c,
        ms.plan.mults_per_source_frame()
    );
    Ok(Some(ms))
}

/// Multi's stage 0 for the Hybrid-Phase minimum-phase blob at `path`, or
/// `None` when the gate keeps the file single-stage (then the linear half is
/// single-stage too: the same gate). The trim is today's for that half: the
/// band-weighted (200 Hz – 6 kHz) group delay at the output rate, from
/// branch 0 (`NpyBranch::band_weighted_delay`, equal to the whole blob's).
pub(crate) fn prepare_min(
    settings: &ConvertSettings,
    path: &str,
    src_rate: u32,
    out_rate: u32,
    l: usize,
) -> Result<Option<MsStage0>, String> {
    if settings.upsampler != UpsamplerMode::Multi {
        return Ok(None);
    }
    let Some(chain) = chain_for(settings, settings.taps, src_rate, l) else {
        return Ok(None);
    };
    let b = crate::audio::dsp_core::load_npy_f64_branch(path, l, 0)?;
    let d = b.band_weighted_delay(out_rate);
    let full_len = b.full_len;
    let ms = MsStage0::with_d(b, d, l, chain, Some(path.to_string()), settings.use_gpu);
    crate::aelog!(
        "[CONV] Hybrid-Phase multistage: min-phase {} taps → branch 0 ({} taps) + {}-stage chain ×{} | band-weighted d = {} output frames, k0 = {}, stage-0 history {}",
        full_len,
        ms.branch.len(),
        chain.len(),
        l,
        ms.plan.d,
        ms.plan.k0,
        ms.plan.c
    );
    Ok(Some(ms))
}

/// The last `n` taps of a `.npy` filter: its data runs to the end of the file. A branch-0 rung keeps the whole
/// filter's beside it.
fn npy_tail(path: &str, n: usize) -> Result<Vec<f64>, String> {
    use std::io::{Read, Seek, SeekFrom};
    if crate::audio::dsp_core::is_branch0_file(path) {
        return crate::audio::dsp_core::branch0_tail(path, n);
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {}", path, e))?;
    f.seek(SeekFrom::End(-((n * 8) as i64))).map_err(|e| format!("Seek error: {}", e))?;
    let mut bytes = vec![0u8; n * 8];
    f.read_exact(&mut bytes).map_err(|e| format!("Read error: {}", e))?;
    Ok(bytes.chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap())).collect())
}

/// One channel's stage-0 frames (plan index 0 on) through the chain: output
/// frames 0 .. `n_out`, fed in pieces with a cancellation check between them.
fn chain_channel(plan: &MsPlan, z: &[f64], n_out: usize) -> Result<Vec<f64>, String> {
    let need = plan.stage0_frames(n_out as u64) as usize;
    assert!(z.len() >= need, "{} stage-0 frames, {} needed", z.len(), need);
    let mut chain = InterpChain::new(plan, 0);
    assert_eq!(chain.input_start(), 0, "a track starts at stage-0 frame 0");
    // The last piece completes at most a few outputs past n_out.
    let mut out = Vec::with_capacity(n_out + 1024 * plan.l);
    for piece in z[..need].chunks(CHAIN_PIECE) {
        if cancel_requested() {
            return Err("Cancelled".to_string());
        }
        chain.process(piece, &mut out);
    }
    out.truncate(n_out);
    Ok(out)
}

/// The whole track in RAM: stage 0 into a source-rate buffer, then both
/// channels through the chain side by side. `n_out` output frames each,
/// aligned as today's single-stage output after its trim.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_in_ram(
    ms: &MsStage0,
    audio_l: &[f64],
    audio_r: &[f64],
    total_input_samples: usize,
    n_out: usize,
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    let flush = ms.flush(total_input_samples, n_out);
    let len0 = total_input_samples + flush;
    let mut z_l = vec![0.0f64; len0];
    let mut z_r = vec![0.0f64; len0];
    run_polyphase_pass_from(
        std::slice::from_ref(&ms.branch),
        audio_l,
        audio_r,
        total_input_samples,
        flush,
        ms.plan.scale0,
        &mut z_l,
        &mut z_r,
        use_gpu,
        precision,
        status_label,
        ms.source(),
        file_state,
        pct_base,
        pct_span - 20,
    )?;
    set_status("Multistage: short interpolation chain...");
    file_state.gpu_pct.store(pct_base + pct_span - 20, Ordering::Relaxed);
    let skip = ms.skip();
    let required_mb = (n_out as f64 * 8.0 * 2.0 / 1024.0 / 1024.0) as u64;
    let (out_l, out_r) = crate::audio::memory::await_free_ram_and_allocate(required_mb, || {
        // The pool's threads check the cancel of the file this one works on.
        let file = current_file();
        rayon::join(
            || { let _f = enter_file(file.clone()); chain_channel(&ms.plan, &z_l[skip..], n_out) },
            || { let _f = enter_file(file.clone()); chain_channel(&ms.plan, &z_r[skip..], n_out) },
        )
    });
    let (out_l, out_r) = (out_l?, out_r?);
    file_state.gpu_pct.store(pct_base + pct_span, Ordering::Relaxed);
    Ok((out_l, out_r))
}

/// The streaming twin of `render_in_ram`: stage 0 segment by segment (the
/// convolver persists), each segment's frames through two persistent chains,
/// exactly `n_out` output frames emitted in order — the same bits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_segmented(
    ms: &MsStage0,
    source: SourceFrames<'_>,
    total_input_samples: usize,
    n_out: usize,
    seg_input_samples: usize,
    use_gpu: bool,
    precision: u32,
    status_label: &str,
    file_state: &Arc<FileConvState>,
    pct_base: u32,
    pct_span: u32,
    mut emit: impl FnMut(&[f64], &[f64]) -> Result<(), String>,
) -> Result<(), String> {
    let flush = ms.flush(total_input_samples, n_out);
    let need = ms.plan.stage0_frames(n_out as u64) as usize;
    let mut chain_l = InterpChain::new(&ms.plan, 0);
    let mut chain_r = InterpChain::new(&ms.plan, 0);
    assert_eq!(chain_l.input_start(), 0, "a track starts at stage-0 frame 0");
    let (mut skip, mut fed, mut emitted) = (ms.skip(), 0usize, 0usize);
    let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
    run_polyphase_pass_segmented_from(
        std::slice::from_ref(&ms.branch),
        source,
        total_input_samples,
        flush,
        ms.plan.scale0,
        seg_input_samples,
        use_gpu,
        precision,
        status_label,
        ms.source(),
        file_state,
        pct_base,
        pct_span,
        |l, r| {
            let s = skip.min(l.len());
            skip -= s;
            let t = (need - fed).min(l.len() - s);
            if t == 0 {
                return Ok(());
            }
            let (l, r) = (&l[s..s + t], &r[s..s + t]);
            fed += t;
            out_l.clear();
            out_r.clear();
            rayon::join(|| chain_l.process(l, &mut out_l), || chain_r.process(r, &mut out_r));
            let k = (n_out - emitted).min(out_l.len());
            if k > 0 {
                emit(&out_l[..k], &out_r[..k])?;
                emitted += k;
            }
            Ok(())
        },
    )?;
    if emitted != n_out {
        return Err(format!("multistage segmented pass produced {} frames, expected {}", emitted, n_out));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::process::{run_polyphase_pass, single_pass_plan};
    use crate::audio::dsp_core::{load_npy_f64, load_npy_f64_branch};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    const FS: f64 = 44_100.0;
    const SRC: u32 = 44_100;
    /// The test chains are flat to 19.5 kHz and stop the images from fs − 19.5 kHz; the test blobs stop from 19 kHz
    /// (the layout of the shipped pairs: the blob's wall inside the chain's passband).
    const PASS: f64 = 19_500.0;
    const A_H1: f64 = 222.0;

    fn bessel_i0(x: f64) -> f64 {
        let (mut sum, mut term, q, mut k) = (1.0, 1.0, x * x / 4.0, 1.0);
        while term > sum * 1e-18 {
            term *= q / (k * k);
            sum += term;
            k += 1.0;
        }
        sum
    }

    /// Kaiser-windowed sinc, odd `taps`, cutoff `fc` (cycles per sample), DC gain `gain`; a half-band (`fc` = 0.25)
    /// with exact zeros.
    fn kaiser_lp(taps: usize, fc: f64, beta: f64, gain: f64) -> Vec<f64> {
        let m = (taps - 1) / 2;
        let i0b = bessel_i0(beta);
        let mut h = vec![0.0f64; taps];
        for k in 0..=m {
            let v = if k == 0 {
                2.0 * fc
            } else if fc == 0.25 && k % 2 == 0 {
                0.0
            } else {
                let x = std::f64::consts::PI * 2.0 * fc * k as f64;
                2.0 * fc * x.sin() / x
            };
            let r = k as f64 / m as f64;
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            h[m + k] = v * w;
            h[m - k] = v * w;
        }
        let s: f64 = h.iter().sum();
        for v in h.iter_mut() {
            *v *= gain / s;
        }
        h
    }

    /// A Kaiser design at `rate`: flat to `fp`, `a_db` down from `fstop`.
    fn design(rate: f64, fp: f64, fstop: f64, a_db: f64, gain: f64, halfband: bool) -> Vec<f64> {
        let beta = 0.1102 * (a_db - 8.7);
        let dw = 2.0 * std::f64::consts::PI * (fstop - fp) / rate;
        let n = ((a_db - 7.95) / (2.285 * dw) + 1.0).ceil() as usize;
        let n = if halfband { 4 * n.max(3).div_ceil(4) + 3 } else { n | 1 };
        kaiser_lp(n, if halfband { 0.25 } else { (fp + fstop) / 2.0 / rate }, beta, gain)
    }

    fn leak(stages: Vec<(usize, Vec<f64>)>) -> &'static [H1Stage] {
        let v: Vec<H1Stage> =
            stages.into_iter().map(|(ratio, taps)| H1Stage { ratio, taps: Box::leak(taps.into_boxed_slice()) }).collect();
        Box::leak(v.into_boxed_slice())
    }

    /// The core's test chains: (name, L, stages) — a half-band alone, a half-band and a ×4 polyphase stage, three
    /// half-bands.
    fn chains() -> &'static [(&'static str, usize, &'static [H1Stage])] {
        static C: OnceLock<Vec<(&'static str, usize, &'static [H1Stage])>> = OnceLock::new();
        C.get_or_init(|| {
            let hb1 = || design(2.0 * FS, PASS, FS - PASS, A_H1, 2.0, true);
            vec![
                ("hb1 x2", 2, leak(vec![(2, hb1())])),
                ("hb1+p4 x8", 8, leak(vec![(2, hb1()), (4, design(8.0 * FS, PASS, 2.0 * FS - (FS - PASS), A_H1, 4.0, false))])),
                (
                    "hb1+hb2+hb3 x8",
                    8,
                    leak(vec![
                        (2, hb1()),
                        (2, design(4.0 * FS, PASS, 2.0 * FS - PASS, A_H1, 2.0, true)),
                        (2, design(8.0 * FS, PASS, 4.0 * FS - PASS, A_H1, 2.0, true)),
                    ]),
                ),
            ]
        })
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
            })
            .collect()
    }

    fn peak_db(a: &[f64], b: &[f64]) -> f64 {
        let e = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max);
        20.0 * e.max(1e-300).log10()
    }

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    fn settings(upsampler: UpsamplerMode) -> ConvertSettings {
        ConvertSettings {
            out_rate: 0,
            fs_multiplier: 8,
            taps: 30_000_000,
            precision: 64,
            custom_filter_path: None,
            use_gpu: false,
            use_fir_resampling: true,
            apodizing: 0,
            headroom_db: 0.0,
            adaptive_apodizer: false,
            hybrid_phase: false,
            iir_dc_blocking: false,
            lab: Default::default(),
            subsonic_hz: 0,
            rack_sig: None,
            upsampler,
            dsd: None,
        }
    }

    /// A `.npy` (v1.0, '<f8', 1-D) the loaders read, in the temp folder; removed on drop.
    struct TempNpy(PathBuf);
    impl TempNpy {
        fn new(tag: &str, h: &[f64]) -> TempNpy {
            let path = std::env::temp_dir().join(format!("ms_route_{}_{}.npy", tag, std::process::id()));
            let mut dict = format!("{{'descr': '<f8', 'fortran_order': False, 'shape': ({},), }}", h.len());
            while (10 + dict.len() + 1) % 64 != 0 {
                dict.push(' ');
            }
            dict.push('\n');
            let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
            bytes.extend_from_slice(&(dict.len() as u16).to_le_bytes());
            bytes.extend_from_slice(dict.as_bytes());
            for v in h {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            std::fs::write(&path, bytes).unwrap();
            TempNpy(path)
        }
        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }
    impl Drop for TempNpy {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Today's single-stage in-RAM route, as process.rs runs it: the plan from the file, the pass, the trim.
    fn single_route(path: &str, l: usize, tfs: bool, x: &(Vec<f64>, Vec<f64>)) -> (Vec<f64>, Vec<f64>) {
        let sp = single_pass_plan(path, l, &settings(UpsamplerMode::Single), tfs, SRC, SRC * l as u32).unwrap();
        let n_in = x.0.len();
        let n_out = n_in * l;
        let mut out_l = vec![0.0f64; n_out + sp.flush_input_samples * l];
        let mut out_r = out_l.clone();
        let fs = Arc::new(FileConvState::new());
        run_polyphase_pass(
            &sp.phases, &x.0, &x.1, n_in, sp.flush_input_samples, sp.scale, &mut out_l, &mut out_r, false, 64, "single",
            None, &fs, 0, 600,
        )
        .unwrap();
        for o in [&mut out_l, &mut out_r] {
            o.drain(..sp.group_delay_samples);
            o.truncate(n_out);
        }
        (out_l, out_r)
    }

    /// Multi's stage 0 for the blob in `path`, read as `prepare` reads it, with `chain`.
    fn stage0(path: &str, l: usize, tfs: bool, chain: &[H1Stage]) -> MsStage0 {
        let b = load_npy_f64_branch(path, l, 0).unwrap();
        let tail = npy_tail(path, SYMMETRY_TAPS.min(b.full_len / 2)).unwrap();
        let ahead = tfs.then(|| crate::audio::converter::dsp::lab::tfs::look_ahead(b.full_len, SRC * l as u32));
        MsStage0::new(b, &tail, ahead, l, chain, None, false)
    }

    fn multi_route(ms: &MsStage0, x: &(Vec<f64>, Vec<f64>)) -> (Vec<f64>, Vec<f64>) {
        let fs = Arc::new(FileConvState::new());
        render_in_ram(ms, &x.0, &x.1, x.0.len(), x.0.len() * ms.plan.l, false, 64, "multi", &fs, 0, 600).unwrap()
    }

    #[test]
    fn the_symmetry_test_is_todays() {
        let today = |h: &[f64]| -> bool {
            let n = h.len();
            if n > 10 {
                let check_count = 50.min(n / 2);
                let mut sym_err = 0.0;
                for i in 0..check_count {
                    sym_err += (h[i] - h[n - 1 - i]).abs();
                }
                sym_err < 1e-6 * check_count as f64
            } else {
                false
            }
        };
        let lin = design(8.0 * FS, 17_000.0, 19_000.0, 160.0, 1.0, false);
        let min = crate::audio::dsp_core::to_minimum_phase(&lin);
        let mut nudged = lin.clone();
        nudged[3] += 1e-3;
        for h in [&lin[..], &min[..], &nudged[..], &lin[..9], &lin[..60], &[1.0, 2.0, 1.0][..]] {
            let c = SYMMETRY_TAPS.min(h.len() / 2);
            assert_eq!(linear_by_symmetry(&h[..c], &h[h.len() - c..], h.len()), today(h), "{} taps", h.len());
        }
        assert!(today(&lin) && !today(&min) && !today(&nudged));
    }

    #[test]
    fn the_tail_and_the_branch_come_off_the_file() {
        let h = noise(70_001, 5);
        let f = TempNpy::new("tail", &h);
        assert_eq!(bits(&npy_tail(f.path(), 50).unwrap()), bits(&h[h.len() - 50..]));
        assert_eq!(bits(&load_npy_f64(f.path()).unwrap()), bits(&h));
        let (_, _, chain) = chains()[1];
        let ms = stage0(f.path(), 8, false, chain);
        assert_eq!(bits(&ms.branch), bits(&h.iter().step_by(8).copied().collect::<Vec<_>>()));
        // Noise is not symmetric: the converter's minimum-phase rule, no trim.
        assert_eq!(ms.plan.d, 0);
        assert_eq!(ms.plan.scale0.to_bits(), (8.0 / h.iter().sum::<f64>().abs().max(1e-10)).to_bits());
    }

    #[test]
    fn a_branch0_rung_converts_as_its_whole_file_and_single_stage_refuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let lin = design(8.0 * FS, 17_000.0, 19_000.0, 160.0, 1.0, false);
        let min = crate::audio::dsp_core::to_minimum_phase(&lin);
        let x = (noise(3_000, 1), noise(3_000, 2));
        let multi = settings(UpsamplerMode::Multi);
        for (name, h) in [("linear_phase", &lin), ("minimum_phase", &min)] {
            let full = TempNpy::new(&format!("b0_{name}"), h);
            let b0 = dir.path().join(format!("fir_30M_352800_{name}.b0.npy"));
            let b0 = b0.to_str().unwrap();
            crate::audio::dsp_core::save_branch0(b0, h, 8, SRC, name).unwrap();
            let same = |a: &MsStage0, b: &MsStage0| {
                assert_eq!(bits(&a.branch), bits(&b.branch), "{name}");
                assert_eq!((a.plan.d, a.plan.k0, a.plan.scale0.to_bits()), (b.plan.d, b.plan.k0, b.plan.scale0.to_bits()), "{name}");
                assert_eq!(multi_route(a, &x), multi_route(b, &x), "{name}");
            };
            let at = |p: &str| prepare(&multi, false, p, SRC, SRC * 8, 8).unwrap().expect("multistage");
            let (a, b) = (at(full.path()), at(b0));
            assert_eq!(a.plan.d, if name == "linear_phase" { (lin.len() - 1) / 2 } else { 0 }, "{name}");
            same(&a, &b);
            let at_min = |p: &str| prepare_min(&multi, p, SRC, SRC * 8, 8).unwrap().expect("multistage");
            same(&at_min(full.path()), &at_min(b0));
            let e = single_pass_plan(b0, 8, &settings(UpsamplerMode::Single), false, SRC, SRC * 8).err().expect("refused");
            assert!(e.contains("branch 0"), "{e}");
        }
    }

    #[test]
    fn the_gate_keeps_custom_filters_and_single_mode_single_stage() {
        use crate::audio::converter::dsp::multistage::multistage_chain;
        let multi = settings(UpsamplerMode::Multi);
        for &(src, l) in &[(44_100u32, 8usize), (48_000, 8), (96_000, 4), (44_100, 2), (44_100, 16)] {
            let open = multistage_chain(UpsamplerMode::Multi, 30_000_000, src, l).is_some();
            assert_eq!(chain_for(&multi, 30_000_000, src, l).is_some(), open);
            // Hybrid-Phase and its continuous-alpha blend play both halves multistage.
            let hp = ConvertSettings { hybrid_phase: true, ..multi.clone() };
            assert_eq!(chain_for(&hp, 30_000_000, src, l).is_some(), open);
            let alpha = ConvertSettings {
                lab: crate::audio::converter::types::LabFeatures { continuous_alpha: true, ..Default::default() },
                ..hp.clone()
            };
            assert_eq!(chain_for(&alpha, 30_000_000, src, l).is_some(), open);
            let custom = ConvertSettings { custom_filter_path: Some("my.npy".into()), ..hp.clone() };
            assert!(chain_for(&custom, 30_000_000, src, l).is_none());
            assert!(chain_for(&settings(UpsamplerMode::Single), 30_000_000, src, l).is_none());
            assert!(chain_for(&multi, 1_000_000, src, l).is_none());
            // The fallback is named only where it changed the route.
            let single = settings(UpsamplerMode::Single);
            assert_eq!(single_fallback(&single, 30_000_000, src, l), open);
            assert!(!single_fallback(&multi, 30_000_000, src, l));
            assert!(!single_fallback(&single, 1_000_000, src, l));
            assert!(!single_fallback(&ConvertSettings { custom_filter_path: Some("my.npy".into()), ..single }, 30_000_000, src, l));
        }
        // The minimum-phase half: the same gate, nothing read when it is shut.
        let hp = ConvertSettings { hybrid_phase: true, ..multi.clone() };
        assert!(matches!(prepare_min(&ConvertSettings { upsampler: UpsamplerMode::Single, ..hp.clone() }, "no such file.npy", 44_100, 352_800, 8), Ok(None)));
        assert!(matches!(prepare_min(&ConvertSettings { taps: 1_000_000, ..hp.clone() }, "no such file.npy", 44_100, 352_800, 8), Ok(None)));
        // Single, and a rung below the gate, never read the blob; the nominal rung opens it (the file is read).
        let missing = "no such file.npy";
        assert!(matches!(prepare(&settings(UpsamplerMode::Single), false, missing, 44_100, 352_800, 8), Ok(None)));
        let small = ConvertSettings { taps: 1_000_000, ..multi.clone() };
        assert!(matches!(prepare(&small, false, missing, 44_100, 352_800, 8), Ok(None)));
        if multistage_chain(UpsamplerMode::Multi, 30_000_000, 44_100, 8).is_some() {
            assert!(prepare(&multi, false, missing, 44_100, 352_800, 8).is_err());
        }
    }

    /// The blobs under test at ×`l`: (phase, taps, TFS). Linear and minimum: flat to 17 kHz, 255 dB down from 19 kHz.
    /// The TFS case is a linear blob long enough that the 30 ms look-ahead is shorter than its half (the trim is
    /// what TFS changes on this route).
    fn blobs(l: usize) -> Vec<(&'static str, Vec<f64>, bool)> {
        let r = l as f64 * FS;
        let lin = design(r, 17_000.0, 19_000.0, 255.0, 1.0, false);
        let min = crate::audio::dsp_core::to_minimum_phase(&lin);
        let long = design(r, 18_750.0, 19_000.0, 255.0, 1.0, false);
        assert!(crate::audio::converter::dsp::lab::tfs::look_ahead(long.len(), r as u32) < (long.len() - 1) / 2);
        vec![("linear", lin, false), ("minimum", min, false), ("tfs", long, true)]
    }

    #[test]
    fn multi_reproduces_todays_single_stage_route() {
        for (name, l, chain) in chains() {
            for (phase, h, tfs) in blobs(*l) {
                let f = TempNpy::new(&format!("e2e_{}_{}", name.replace([' ', '+'], "_"), phase), &h);
                let x = (noise(8_820, 7), noise(8_820, 8));
                let want = single_route(f.path(), *l, tfs, &x);
                let ms = stage0(f.path(), *l, tfs, chain);
                let got = multi_route(&ms, &x);
                assert_eq!(got.0.len(), want.0.len());
                let e = peak_db(&got.0, &want.0).max(peak_db(&got.1, &want.1));
                let n10 = SRC as usize / 100 * l;
                let e10 = peak_db(&got.0[..n10], &want.0[..n10]);
                println!("[ms-conv] {name} {phase}: d {} k0 {} | null {e:.1} dBFS, first 10 ms {e10:.1}", ms.plan.d, ms.plan.k0);
                let limit = if tfs { -203.0 } else { -200.0 };
                assert!(e <= limit, "{name} {phase}: null {e:.1} dBFS");
            }
        }
    }

    #[test]
    fn segmented_multi_is_the_in_ram_multi_bit_for_bit() {
        for (name, l, chain) in chains() {
            let h = design(*l as f64 * FS, 17_000.0, 19_000.0, 255.0, 1.0, false);
            let f = TempNpy::new(&format!("seg_{}", name.replace([' ', '+'], "_")), &h);
            let ms = stage0(f.path(), *l, false, chain);
            let x = (noise(120_000, 11), noise(120_000, 12));
            let n_out = x.0.len() * l;
            let want = multi_route(&ms, &x);
            for seg in [10_000usize, 32_768, 50_001, 1 << 20] {
                let (mut got_l, mut got_r) = (Vec::new(), Vec::new());
                let fs = Arc::new(FileConvState::new());
                run_segmented(&ms, SourceFrames::Ram(&x.0, &x.1), x.0.len(), n_out, seg, false, 64, "seg", &fs, 0, 600, |a, b| {
                    got_l.extend_from_slice(a);
                    got_r.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
                assert_eq!(got_l.len(), n_out, "{name} seg {seg}");
                assert!(bits(&got_l) == bits(&want.0) && bits(&got_r) == bits(&want.1), "{name} seg {seg}");
            }
        }
    }

    /// The Hybrid-Phase minimum-phase half (`prepare_min`'s stage 0: branch 0, today's band-weighted trim, k0 and
    /// history of its own): the trim is today's (the whole blob's estimate), and the giant route's segments give the
    /// in-RAM route's bits.
    #[test]
    fn the_minimum_phase_half_streams_the_in_ram_bits() {
        for (name, l, chain) in chains() {
            let h = crate::audio::dsp_core::to_minimum_phase(&design(*l as f64 * FS, 17_000.0, 19_000.0, 255.0, 1.0, false));
            let f = TempNpy::new(&format!("min_{}", name.replace([' ', '+'], "_")), &h);
            let out_rate = SRC * *l as u32;
            let b = load_npy_f64_branch(f.path(), *l, 0).unwrap();
            let d = b.band_weighted_delay(out_rate);
            assert_eq!(
                d,
                crate::audio::dsp_core::estimate_band_weighted_group_delay(&h, out_rate as f64, 200.0, 6000.0),
                "{name}: today's trim"
            );
            assert!(d > 0, "{name}: a minimum-phase half is trimmed");
            let ms = MsStage0::with_d(b, d, *l, chain, None, false);
            assert_eq!(ms.plan.d, d);
            let x = (noise(120_000, 13), noise(120_000, 14));
            let n_out = x.0.len() * l;
            let want = multi_route(&ms, &x);
            for seg in [10_000usize, 50_001] {
                let (mut got_l, mut got_r) = (Vec::new(), Vec::new());
                let fs = Arc::new(FileConvState::new());
                run_segmented(&ms, SourceFrames::Ram(&x.0, &x.1), x.0.len(), n_out, seg, false, 64, "seg", &fs, 400, 300, |a, b| {
                    got_l.extend_from_slice(a);
                    got_r.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
                assert_eq!(got_l.len(), n_out, "{name} seg {seg}");
                assert!(bits(&got_l) == bits(&want.0) && bits(&got_r) == bits(&want.1), "{name} seg {seg}");
            }
        }
    }

    /// The real 30M blobs at 44.1k ×8, Multi against today's route: linear, minimum (the converter's rule: no trim)
    /// and TFS (when its derived blob is cached beside the others). The chain is the shipped one when there is one,
    /// else the research chain from `AURA_MS_H1_DIR` (`ms_44k-hb1.npy` ×2, then `ms_44k-p4@2.npy` ×4).
    ///
    ///     AURA_FILTER_DIR=<blobs> AURA_MS_H1_DIR=<chain> cargo test --profile fast --bins real_30m_x8 -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_30m_x8_multi_against_single() {
        let Ok(dir) = std::env::var("AURA_FILTER_DIR") else {
            println!("[ms-conv] AURA_FILTER_DIR not set - skip");
            return;
        };
        let chain: &'static [H1Stage] = match crate::audio::converter::dsp::ms_h1::h1_chain(44_100, 8) {
            Some(c) => c,
            None => {
                let Ok(h1) = std::env::var("AURA_MS_H1_DIR") else {
                    println!("[ms-conv] no shipped chain and AURA_MS_H1_DIR not set - skip");
                    return;
                };
                let load = |n: &str| load_npy_f64(&format!("{}/{}", h1.trim_end_matches(['/', '\\']), n)).unwrap();
                leak(vec![(2, load("ms_44k-hb1.npy")), (4, load("ms_44k-p4@2.npy"))])
            }
        };
        let at = |n: &str| format!("{}/{}", dir.trim_end_matches(['/', '\\']), n);
        let mut cases = vec![
            ("linear", at("fir_30M_352800_linear_phase.npy"), false),
            ("minimum", at("fir_30M_352800_minimum_phase.npy"), false),
        ];
        match crate::audio::converter::dsp::lab::tfs::cached(30_000_000, 44_100, 352_800) {
            Some(p) => cases.push(("tfs", p, true)),
            None => println!("[ms-conv] no cached 30M TFS blob - TFS row skipped"),
        }
        let n = SRC as usize * 2;
        let mut x = (noise(n, 31), noise(n, 32));
        x.0[1000] = 0.9;
        x.1[n / 2] = -0.9;
        println!("| blob (30M, 44.1k x8) | d | k0 | null whole (dBFS) | first 10 ms | last 10 ms |");
        println!("|---|---|---|---|---|---|");
        for (phase, path, tfs) in cases {
            let t = std::time::Instant::now();
            let want = single_route(&path, 8, tfs, &x);
            let t_single = t.elapsed();
            let t = std::time::Instant::now();
            let ms = stage0(&path, 8, tfs, chain);
            let got = multi_route(&ms, &x);
            let t_multi = t.elapsed();
            let m = SRC as usize / 100 * 8;
            let e = peak_db(&got.0, &want.0).max(peak_db(&got.1, &want.1));
            let e_head = peak_db(&got.0[..m], &want.0[..m]).max(peak_db(&got.1[..m], &want.1[..m]));
            let k = got.0.len() - m;
            let e_tail = peak_db(&got.0[k..], &want.0[k..]).max(peak_db(&got.1[k..], &want.1[k..]));
            println!("| {phase} | {} | {} | {e:.1} | {e_head:.1} | {e_tail:.1} |", ms.plan.d, ms.plan.k0);
            println!(
                "[ms-conv] {phase}: single {:.1} s, multi {:.1} s (2 s of audio, wall clock, incl. load and setup)",
                t_single.as_secs_f64(),
                t_multi.as_secs_f64()
            );
        }
    }

    /// The GPU route: stage 0 of the real 30M linear blob at 44.1k ×8 through the GPU convolver (branch 0's own
    /// pre-roll, its spectrum cached as phase 0 of ×8) against the CPU route, the chain on the CPU in both. Same
    /// chain source as `real_30m_x8_multi_against_single`.
    ///
    ///     AURA_FILTER_DIR=<blobs> AURA_MS_H1_DIR=<chain> cargo test --profile fast --bins real_30m_x8_gpu -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_30m_x8_gpu_stage_0_matches_cpu() {
        let Ok(dir) = std::env::var("AURA_FILTER_DIR") else {
            println!("[ms-conv] AURA_FILTER_DIR not set - skip");
            return;
        };
        let chain: &'static [H1Stage] = match crate::audio::converter::dsp::ms_h1::h1_chain(44_100, 8) {
            Some(c) => c,
            None => {
                let Ok(h1) = std::env::var("AURA_MS_H1_DIR") else {
                    println!("[ms-conv] no shipped chain and AURA_MS_H1_DIR not set - skip");
                    return;
                };
                let load = |n: &str| load_npy_f64(&format!("{}/{}", h1.trim_end_matches(['/', '\\']), n)).unwrap();
                leak(vec![(2, load("ms_44k-hb1.npy")), (4, load("ms_44k-p4@2.npy"))])
            }
        };
        let path = format!("{}/fir_30M_352800_linear_phase.npy", dir.trim_end_matches(['/', '\\']));
        let n = SRC as usize * 2;
        let x = (noise(n, 41), noise(n, 42));
        let run = |gpu: bool| {
            let b = load_npy_f64_branch(&path, 8, 0).unwrap();
            let tail = npy_tail(&path, SYMMETRY_TAPS).unwrap();
            let ms = MsStage0::new(b, &tail, None, 8, chain, Some(path.clone()), gpu);
            let fs = Arc::new(FileConvState::new());
            let t = std::time::Instant::now();
            let out = render_in_ram(&ms, &x.0, &x.1, n, n * 8, gpu, 64, "gpu-eq", &fs, 0, 600).unwrap();
            (out, ms.ola_latency, t.elapsed().as_secs_f64())
        };
        let (cpu, ola_cpu, t_cpu) = run(false);
        let (gpu, ola_gpu, t_gpu) = run(true);
        let e = peak_db(&gpu.0, &cpu.0).max(peak_db(&gpu.1, &cpu.1));
        println!(
            "[ms-conv] 30M x8 stage 0 GPU vs CPU: null {e:.1} dBFS | pre-roll GPU {ola_gpu} / CPU {ola_cpu} | GPU {t_gpu:.1} s, CPU {t_cpu:.1} s (2 s of audio, incl. setup)"
        );
        assert!(e <= -200.0, "GPU stage 0 differs from the CPU one by {e:.1} dBFS");
    }
}
