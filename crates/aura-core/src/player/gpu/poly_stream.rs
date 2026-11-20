//! `GpuPolyStream` — partitioned overlap-save convolver using the GPU for the
//! CMUL-ACCUM inner loop.
//!
//! The FFT and IFFT run on the CPU with realfft f64 (identical to PolyStream:
//! half spectra, bins 0 ..= NFFT/2).
//! Only the multiply-accumulate over the P partitions of the frequency-domain
//! delay line is offloaded to the GPU via the DS CMUL kernel.
//!
//! Every stream owns its own `FdlRing` (K9 — the shared-FDL path has been
//! removed; HP/αHP pairs each prime their ring independently).
//!
//! Stage contract: implements `Stage` from `stages.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use std::time::Instant;
use rustfft::num_complex::Complex;
use realfft::RealFftPlanner;
use rayon::prelude::*;

use crate::audio::dsp_core::{c2r, C2r, R2c};
use crate::player::convolver::{Bank, HeadTail, Input, BLOCK, HEAD_CHUNK, NBIN};
#[cfg(test)]
use crate::player::convolver::SourceBuf;
use crate::player::stages::Stage;

use super::{GPU_BINS, NFFT, partitions};
use super::ctx::GpuPolyCtx;
use super::fdl::{FdlRing, encode_spectrum_ds, decode_spectrum_ds};
use super::filter_buf::GpuFilterBank;
use super::bank_cache::BankKey;

type C = Complex<f64>;

/// Per-branch GPU resources.
struct PerBranch {
    accum_l:   wgpu::Buffer,
    accum_r:   wgpu::Buffer,
    staging_l: wgpu::Buffer,
    staging_r: wgpu::Buffer,
    /// The Kahan term a block's piece leaves for the next (the shader's
    /// binding 4); never read back.
    #[allow(dead_code)]
    comp_l:    wgpu::Buffer,
    #[allow(dead_code)]
    comp_r:    wgpu::Buffer,
    bg_l: wgpu::BindGroup,
    bg_r: wgpu::BindGroup,
    /// Both channels for gpu_poly2 (AURA_FIR_OPT).
    bg2:  wgpu::BindGroup,
}

thread_local! {
    /// Set by a background job (the analyzer's O pass) on its own thread
    /// around its GPU work: its streams take less VRAM (`try_new`) and their
    /// watchdog timeouts fail only themselves, never the process-wide count
    /// playback depends on.
    static BACKGROUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the calling thread's GPU work as background (see `BACKGROUND`).
pub fn set_background(on: bool) {
    BACKGROUND.with(|b| b.set(on));
}

fn is_background() -> bool {
    BACKGROUND.with(|b| b.get())
}

/// A block is summed in pieces of at most this many partition dispatches
/// (partitions × branches × 2 channels), each piece its own submit. One long
/// dispatch keeps the card from the desktop: when the card has dropped its
/// clocks between our bursts, a whole 1G ×16 block took 80–180 ms in one go
/// and every window lost frames; in pieces the desktop gets in between them.
const PIECE_DISPATCHES: usize = 32;

/// With AURA_FIR_OPT, one dispatch per branch sums both channels, and a
/// piece is about this many partitions over all branches.
const PIECE_PARTS_OPT: usize = 96;

/// Diagnostics, read once: `AURA_GPU_SLICE=n` — n partitions per submit
/// instead of `PIECE_DISPATCHES`' share; `AURA_GPU_GAP_US=n` — wait for each
/// submit but the last, then sleep n µs; `AURA_GPU_TIMING=1` — log each
/// block's time on the card.
struct SubmitKnobs {
    slice: usize,
    gap_us: u64,
    timing: bool,
}

fn knobs() -> &'static SubmitKnobs {
    static K: std::sync::OnceLock<SubmitKnobs> = std::sync::OnceLock::new();
    K.get_or_init(|| {
        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let k = SubmitKnobs {
            slice: num("AURA_GPU_SLICE").unwrap_or(0) as usize,
            gap_us: num("AURA_GPU_GAP_US").unwrap_or(0),
            timing: std::env::var_os("AURA_GPU_TIMING").is_some(),
        };
        if k.slice > 0 || k.gap_us > 0 || k.timing {
            crate::aelog!(
                "[player/gpu] submit knobs: {} partitions per submit, gap {} µs, timing {}",
                k.slice, k.gap_us, k.timing
            );
        }
        k
    })
}

/// The history blocks a prime encodes between two looks at the music on
/// the air (`prime`).
const PRIME_PIECE: usize = 64;

/// The history goes to the card in pieces of about this many bytes (whole
/// slots).
const PRIME_PIECE_BYTES: usize = 32 << 20;

/// The history's slots (sorted, encoded L/R) as ring images: each run of
/// neighbouring slots in pieces of at most `per` slots, handed to `put` as
/// (first slot, image L, image R); a slot's bins past the encoded ones stay
/// zero.
fn history_pieces(
    encoded: &[(usize, Vec<u8>, Vec<u8>)],
    slot_bytes: usize,
    per: usize,
    mut put: impl FnMut(usize, &[u8], &[u8]),
) {
    let longest = encoded.chunk_by(|a, b| b.0 == a.0 + 1).map(|r| r.len()).max().unwrap_or(0);
    let mut img_l = vec![0u8; per.min(longest) * slot_bytes];
    let mut img_r = vec![0u8; per.min(longest) * slot_bytes];
    for run in encoded.chunk_by(|a, b| b.0 == a.0 + 1) {
        for part in run.chunks(per) {
            for (m, (_, enc_l, enc_r)) in part.iter().enumerate() {
                let off = m * slot_bytes;
                img_l[off..off + enc_l.len()].copy_from_slice(enc_l);
                img_r[off..off + enc_r.len()].copy_from_slice(enc_r);
            }
            let n = part.len() * slot_bytes;
            put(part[0].0, &img_l[..n], &img_r[..n]);
        }
    }
}


/// GPU-accelerated polyphase streaming convolver.
pub struct GpuPolyStream {
    ctx:        Arc<GpuPolyCtx>,
    bank:       Arc<Bank>,
    /// Never read, and must stay: it ties the filter bank — its `h_freq`
    /// buffer is what the bind groups `bg_l`/`bg_r` point at — to this
    /// stream's lifetime. Without it the buffer would live only as long as
    /// the bank cache chooses to keep it.
    #[allow(dead_code)]
    filter:     Arc<GpuFilterBank>,
    fdl:        Arc<FdlRing>,
    per_branch: Vec<PerBranch>,
    params_buf: wgpu::Buffer,
    input:      Input,
    fft:        R2c,
    ifft:       C2r,
    next_block: i64,
    pend_l:     Vec<f64>,
    pend_r:     Vec<f64>,
    pend_pos:   usize,
    pend_start: u64,
    pos:        u64,
    total:      u64,
    /// A live stream's head and tail (convolver.rs): the card sums the
    /// tail, the head runs on the CPU. None for a file.
    ht:         Option<HeadTail>,
    /// Reused scratch: decoded half spectra per (branch, channel), and
    /// their inverse transforms.
    scratch_l:  Vec<Vec<C>>,
    scratch_r:  Vec<Vec<C>>,
    time_l:     Vec<Vec<f64>>,
    time_r:     Vec<Vec<f64>>,
    enc_buf_l:  Vec<u8>,
    enc_buf_r:  Vec<u8>,
    /// Number of blocks computed so far (for fault injection).
    blocks_computed: u64,
    /// Partitions per submit; 0 = `PIECE_DISPATCHES`' share.
    piece: usize,
    /// Fault injection, kept in release builds on purpose: the checks run in
    /// the real window set `AURA_GPU_FAIL_AFTER=<n>` to make every GPU
    /// stream fail after n blocks and watch the fallback to the CPU (no gap,
    /// blinking taps chip). Unset — the default — it is `None` and costs
    /// nothing. Unit tests use `set_fail_after`; the e2e suite (e2e.rs) sets
    /// the variable itself. Listed with the other switches in
    /// docs/04-developer-guide.md.
    fail_after: Option<u64>,
    /// Set when `fail_after` is reached. Fails this stream only: the device
    /// flag is process-wide and never cleared, so setting it would disable the
    /// GPU for every later stream (and every test running beside this one).
    injected_fault: bool,
    /// Set when a watchdog timeout fires for this specific stream.  Fails this
    /// stream without touching the process-wide flag (unless the session
    /// threshold is reached — see compute_block).
    stream_error: bool,
    /// Wall time of the most recently completed compute_block(), nanoseconds.
    // Written in every non-test build; read only by the test/feature getter below.
    #[allow(dead_code)]
    last_block_wall_ns: u64,
    /// The last `cmac`: card time (ns, first submit to readback mapped),
    /// submits, and partitions summed of all — for the `[firperf]` lines.
    card_last: (u64, usize, (usize, usize)),
    perf: Option<crate::player::firperf::StreamPerf>,
    /// Per FDL slot: both channels' block there is silent (its spectrum all
    /// zeros). The new ring is all silence; set where a slot is written.
    silent: Vec<bool>,
    /// AURA_FIR_OPT: sum on gpu_poly2 only the partitions with sound;
    /// off — gpu_poly, every partition, one channel per dispatch.
    opt: bool,
}

impl GpuPolyStream {
    /// Check VRAM budget and device limits, then build, or return `None` if
    /// the GPU is unavailable, OOM, or the allocation would exceed device limits.
    ///
    /// Every stream allocates its own `FdlRing` — `fdl_in` parameter removed
    /// (K9).  HP/αHP pairs each own their ring and prime it independently.
    #[cfg(test)]
    pub fn try_new(
        bank:  Arc<Bank>,
        src:   Arc<SourceBuf>,
        start: u64,
        ctx:   Arc<GpuPolyCtx>,
    ) -> Option<GpuPolyStream> {
        GpuPolyStream::try_with_input(bank, Input::File(src), start, ctx)
    }

    /// `try_new` over any input: a file, or a live stream (`radio`).
    pub fn try_with_input(
        bank:  Arc<Bank>,
        input: Input,
        start: u64,
        ctx:   Arc<GpuPolyCtx>,
    ) -> Option<GpuPolyStream> {
        if ctx.player_device_error.load(Ordering::Acquire) {
            crate::aelog!("[player/gpu] device error flag already set — skipping GPU stream");
            return None;
        }

        // The taps the bank holds: a multistage stage 0 is branch 0 alone
        // (N/L taps at l = 1), never the whole filter.
        let l = bank.l;
        let sub_taps = (bank.taps + l - 1) / l;
        let p = partitions(sub_taps);

        // Device limits check (K10): per-branch binding and buffer sizes.
        if !ctx.fits(bank.taps, l) {
            crate::aelog!(
                "[player/gpu] device limits check failed for {} taps L={} — CPU path",
                bank.taps, l
            );
            return None;
        }

        // VRAM check: dxgi free × 0.75 ≥ demand. A background stream takes
        // at most a quarter, so playback's next stream still fits after it.
        let need = super::vram_demand(bank.taps, l);
        let free_bytes = ctx.free_vram_bytes();
        let share = if is_background() { 0.25 } else { 0.75 };
        if (free_bytes as f64 * share) < need as f64 {
            crate::aelog!(
                "[player/gpu] VRAM check failed: need {} MB, free {} MB (×0.75 = {} MB)",
                need >> 20,
                free_bytes >> 20,
                ((free_bytes as f64 * 0.75) as u64) >> 20
            );
            return None;
        }

        // Get or build the GpuFilterBank.  Encoding runs outside the mutex (C14)
        // to avoid holding the lock for 200-400 ms.
        let bk = BankKey { path: bank.path.clone(), l, taps: bank.taps };
        let filter = {
            // Brief lock: check the cache.
            let cached = ctx.bank_cache.lock().unwrap_or_else(|p| p.into_inner()).get(&bk);
            match cached {
                Some(f) => f,
                None => {
                    // Encode + upload outside the mutex (rayon-parallel inside from_bank).
                    let f = GpuFilterBank::from_bank(&ctx.device, &ctx.queue, &bank);
                    // Brief lock: insert (another task may have beaten us).
                    let mut cache = ctx.bank_cache.lock().unwrap_or_else(|p| p.into_inner());
                    match cache.get(&bk) {
                        Some(existing) => existing,
                        None => {
                            cache.insert(bk, Arc::clone(&f));
                            f
                        }
                    }
                }
            }
        };

        // Every stream owns its own FdlRing (K9 — no shared FDL).
        let fdl = FdlRing::new(&ctx.device, p);

        // params_buf: uniform {n, P, cursor, k0, k_begin, k_end, _pad, _pad: u32}
        // (gpu_poly2: {n, P, cursor, k_first, k_begin, k_end, settle, zero_bits})
        let params_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player/gpu poly_params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let acc_size   = (GPU_BINS * 16) as u64;
        let stage_size = acc_size;
        let bgl = Arc::clone(&ctx.ola_bind_group_layout);

        let per_branch: Vec<PerBranch> = (0..l).map(|branch| {
            let h_off = (branch * p * GPU_BINS * 16) as u64;
            let h_sz  = (p * GPU_BINS * 16) as u64;

            let accum_l   = mk_buf(&ctx.device, acc_size,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, "accum_l");
            let accum_r   = mk_buf(&ctx.device, acc_size,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, "accum_r");
            let staging_l = mk_buf(&ctx.device, stage_size,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, "stage_l");
            let staging_r = mk_buf(&ctx.device, stage_size,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, "stage_r");
            let comp_l    = mk_buf(&ctx.device, acc_size, wgpu::BufferUsages::STORAGE, "comp_l");
            let comp_r    = mk_buf(&ctx.device, acc_size, wgpu::BufferUsages::STORAGE, "comp_r");

            let bg_l = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("player/gpu bg_l"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params_buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(32),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &filter.h_freq,
                            offset: h_off,
                            size: wgpu::BufferSize::new(h_sz),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: fdl.buf_l.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: accum_l.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: comp_l.as_entire_binding(),
                    },
                ],
            });
            let bg_r = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("player/gpu bg_r"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params_buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(32),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &filter.h_freq,
                            offset: h_off,
                            size: wgpu::BufferSize::new(h_sz),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: fdl.buf_r.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: accum_r.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: comp_r.as_entire_binding(),
                    },
                ],
            });
            fn buf(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
                wgpu::BindGroupEntry { binding, resource: buffer.as_entire_binding() }
            }
            let bg2 = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("player/gpu bg2"),
                layout: &ctx.ola2_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params_buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(32),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &filter.h_freq,
                            offset: h_off,
                            size: wgpu::BufferSize::new(h_sz),
                        }),
                    },
                    buf(2, &fdl.buf_l),
                    buf(3, &fdl.buf_r),
                    buf(4, &accum_l),
                    buf(5, &accum_r),
                    buf(6, &comp_l),
                    buf(7, &comp_r),
                ],
            });
            PerBranch { accum_l, accum_r, staging_l, staging_r, comp_l, comp_r, bg_l, bg_r, bg2 }
        }).collect();

        let mut planner = RealFftPlanner::<f64>::new();
        let fft  = planner.plan_fft_forward(NFFT);
        let ifft = planner.plan_fft_inverse(NFFT);

        let total  = bank.stream_total(&input);
        let c      = start + bank.delay as u64;
        let t      = (c / l as u64) as i64;
        let j0     = t / BLOCK as i64;

        // A live stream plays head and tail; the ring holds every block
        // before j0 after the prime.
        let ht = matches!(input, Input::Live(_)).then(|| HeadTail::new(&bank, j0 - 1));
        let mut perf = crate::player::firperf::StreamPerf::new("gpu", &bank, &input, j0);
        perf.set_vram(filter.h_freq.size() + fdl.buf_l.size() + fdl.buf_r.size());
        let mut s = GpuPolyStream {
            ctx,
            filter,
            fdl,
            per_branch,
            params_buf,
            input,
            ht,
            fft,
            ifft,
            next_block: j0,
            pend_l: Vec::with_capacity(BLOCK * l),
            pend_r: Vec::with_capacity(BLOCK * l),
            pend_pos: 0,
            pend_start: 0,
            pos: start,
            total,
            scratch_l: (0..l).map(|_| vec![C::new(0.0, 0.0); NBIN]).collect(),
            scratch_r: (0..l).map(|_| vec![C::new(0.0, 0.0); NBIN]).collect(),
            time_l: (0..l).map(|_| vec![0.0; NFFT]).collect(),
            time_r: (0..l).map(|_| vec![0.0; NFFT]).collect(),
            enc_buf_l: Vec::with_capacity(NBIN * 16),
            enc_buf_r: Vec::with_capacity(NBIN * 16),
            blocks_computed: 0,
            piece: knobs().slice,
            // Fault injection for window checks; see the field. Off unless set.
            fail_after: std::env::var("AURA_GPU_FAIL_AFTER").ok().and_then(|s| s.parse().ok()),
            injected_fault: false,
            stream_error: false,
            last_block_wall_ns: 0,
            card_last: (0, 0, (0, 0)),
            perf: Some(perf),
            silent: vec![true; p],
            opt: crate::audio::dsp_core::fir_opt(),
            bank,
        };
        let tp = Instant::now();
        s.prime(j0);
        // Drain all pending uploads (h_freq + FDL prime writes) before returning
        // to the controller thread.  The first render-thread block is then fully
        // steady-state with no staging copies in flight.
        let busy = crate::watchdog::GPU.enter(&"player prime");
        s.ctx.device.poll(wgpu::Maintain::Wait);
        drop(busy);
        if let Some(p) = s.perf.as_mut() {
            p.primed(tp.elapsed());
        }
        Some(s)
    }

    /// Partitions per submit, for the test that cuts a block differently.
    #[cfg(test)]
    pub fn set_piece(&mut self, n: usize) {
        self.piece = n;
    }

    /// AURA_FIR_OPT for this stream alone, for the tests that hold the two
    /// kernels against each other.
    #[cfg(any(test, feature = "gpu-player-tests"))]
    pub fn set_opt(&mut self, on: bool) {
        self.opt = on;
    }

    /// The last block's partitions summed on the card, of all (the
    /// `[firperf]` hook).
    #[cfg(any(test, feature = "gpu-player-tests"))]
    pub fn last_parts(&self) -> (usize, usize) {
        self.card_last.2
    }

    /// Returns true iff this stream (or the process-wide device) has an error.
    ///
    /// Three sources:
    /// - `injected_fault`: test-injected per-stream fault via `set_fail_after`.
    /// - `stream_error`: this stream timed out its watchdog (single timeout →
    ///   fails only this stream; ≥ 2 sessions → process-wide).
    /// - `player_device_error`: real device loss set by the uncaptured-error
    ///   handler or after the session watchdog threshold is reached.
    pub fn has_error(&self) -> bool {
        self.injected_fault
            || self.stream_error
            || self.ctx.player_device_error.load(Ordering::Acquire)
    }

    /// Fault injection for tests, without touching the process environment
    /// that streams built by parallel tests read.
    #[cfg(any(test, feature = "gpu-player-tests"))]
    pub fn set_fail_after(&mut self, n: u64) {
        self.fail_after = Some(n);
    }

    /// Wall time of the most recently completed compute_block(), nanoseconds.
    /// Called from the GPU throughput benchmark in tests.rs.
    #[cfg(any(test, feature = "gpu-player-tests"))]
    pub fn last_block_wall_ns(&self) -> u64 {
        self.last_block_wall_ns
    }

    /// Compute exactly one block and return its wall time.
    /// Used by the calibration policy for a pre-trial timing run.
    /// Advances the stream's internal state normally.
    #[allow(dead_code)]
    pub fn time_one_block(&mut self) -> std::time::Duration {
        let t0 = Instant::now();
        self.compute_block();
        t0.elapsed()
    }

    /// Prime the FDL: compute and upload the (P − 1) input spectra that
    /// precede block j0.  Runs the FFTs in parallel via rayon (C4).
    ///
    /// Optimization: instead of 2 × (P − 1) individual queue.write_buffer calls
    /// (228 calls for P=115), neighbouring slots are assembled in RAM and go
    /// up in pieces of `PRIME_PIECE_BYTES`, one write per channel each
    /// (`history_pieces`).
    fn prime(&mut self, j0: i64) {
        let p = self.fdl.partitions as i64;
        let first = j0 - (p - 1);
        // The history ends where block j0's window starts its second half.
        self.input.ensure(j0 * BLOCK as i64);
        let input = &self.input;
        let fft   = &self.fft;
        // A slot holds NBIN bins at a stride of GPU_BINS; the bins between
        // stay zero.
        let enc_bytes  = NBIN * 16;
        let slot_bytes = GPU_BINS * 16;
        // Built behind the music (the caller's mark: the next track's
        // prewarm), the history gives the cores back between its pieces while
        // the music on the air runs short; a start's or a seek's own never
        // waits.
        let way = crate::player::foreground::GiveWay::current();
        static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let blocks: Vec<i64> = (first..j0).collect();

        // Parallel encoding of all P − 1 history blocks, those with sound
        // kept. The remaining slot (j0 itself, slot j0 % P) is written by the
        // first compute_block call; we leave it zero here (FdlRing::new
        // zeroed it).
        let mut encoded: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
        for piece in blocks.chunks(PRIME_PIECE) {
            if let Some(w) = &way {
                w.pause(&NEVER);
            }
            encoded.par_extend(piece.par_iter().filter_map(|&j| {
                let (sl, _) = spectrum_of(input, fft, 0, j);
                let (sr, _) = spectrum_of(input, fft, 1, j);
                let slot = j.rem_euclid(p) as usize;
                let mut enc_l = Vec::with_capacity(enc_bytes);
                let mut enc_r = Vec::with_capacity(enc_bytes);
                encode_spectrum_ds(&sl, &mut enc_l);
                encode_spectrum_ds(&sr, &mut enc_r);
                let silent = |e: &[u8]| e.iter().all(|&b| b == 0);
                (!(silent(&enc_l) && silent(&enc_r))).then_some((slot, enc_l, enc_r))
            }));
        }

        // Only the slots with sound go to the card, each run of neighbours in
        // one write per channel: the new ring is zero already. A stream of
        // a 1G filter wrote its whole ring, 2 × 1 GB from RAM, most of it the
        // silence before a track's start, on the queue the stream on the air
        // uses — and that one ran dry.
        if encoded.is_empty() {
            return;
        }
        for e in &encoded {
            self.silent[e.0] = false;
        }
        encoded.sort_unstable_by_key(|e| e.0);
        // In pieces, each sent and waited for: the next track's history in
        // one write (2 × 139 MB for 1G ×16) took up to 290 ms in the driver
        // while the old track played, and the windows lost a frame that long.
        let (ctx, fdl) = (&self.ctx, &self.fdl);
        let per = (PRIME_PIECE_BYTES / slot_bytes).max(1);
        history_pieces(&encoded, slot_bytes, per, |slot, img_l, img_r| {
            let at = (slot * slot_bytes) as u64;
            ctx.queue.write_buffer(&fdl.buf_l, at, img_l);
            ctx.queue.write_buffer(&fdl.buf_r, at, img_r);
            let idx = ctx.queue.submit(std::iter::empty());
            let _busy = crate::watchdog::GPU.enter(&"player history");
            ctx.device.poll(wgpu::Maintain::WaitForSubmissionIndex(idx));
        });
    }

    /// Block j's input spectra into its FDL slot, and whether both are
    /// silent.
    fn write_block(&mut self, j: i64) {
        let (sl, ql) = spectrum_of(&self.input, &self.fft, 0, j);
        let (sr, qr) = spectrum_of(&self.input, &self.fft, 1, j);
        encode_spectrum_ds(&sl, &mut self.enc_buf_l);
        encode_spectrum_ds(&sr, &mut self.enc_buf_r);
        self.fdl.write_slot(&self.ctx.queue, j, &self.enc_buf_l, &self.enc_buf_r);
        self.silent[j.rem_euclid(self.fdl.partitions as i64) as usize] = ql && qr;
    }

    fn compute_block(&mut self) {
        let t0 = Instant::now();

        // Fault injection: AURA_GPU_FAIL_AFTER=N sends this stream down the
        // error path after N computed blocks.
        self.blocks_computed += 1;
        if self.fail_after.is_some_and(|n| self.blocks_computed >= n) {
            self.injected_fault = true;
        }

        if self.has_error() {
            // GPU error path: emit zeros and record timing.
            let j = self.next_block;
            self.zeros(t0, j, BLOCK);
            self.next_block = j + 1;
            return;
        }

        let j  = self.next_block;
        let l  = self.bank.l;

        // 1. Compute and upload input spectra for this block (its window
        //    reaches source frame (j + 1)·B).
        self.input.ensure((j + 1) * BLOCK as i64);
        self.write_block(j);

        if !self.cmac(j, 0) {
            self.zeros(t0, j, BLOCK);
            self.next_block = j + 1;
            return;
        }

        // 10. Scatter to pend_l/pend_r.
        let inv_n = 1.0 / NFFT as f64;
        let scale = self.bank.scale;
        let d     = self.bank.delay as i64;
        let c0    = j * (BLOCK * l) as i64;
        let m0    = c0 - d;

        self.pend_l.clear();
        self.pend_r.clear();
        for t in 0..BLOCK {
            for p in 0..l {
                let vl = self.time_l[p][t + BLOCK] * inv_n;
                let vr = self.time_r[p][t + BLOCK] * inv_n;
                self.pend_l.push(vl * scale);
                self.pend_r.push(vr * scale);
            }
        }

        let skip = if (self.pos as i64) > m0 {
            (self.pos as i64 - m0) as usize
        } else {
            0
        };
        self.pend_pos   = skip.min(self.pend_l.len());
        self.pend_start = (m0 + self.pend_pos as i64).max(0) as u64;
        self.next_block = j + 1;
        self.last_block_wall_ns = t0.elapsed().as_nanos() as u64;
        let (card_ns, submits, parts) = self.card_last;
        if let Some(p) = self.perf.as_mut() {
            p.block(BLOCK, self.last_block_wall_ns, Some(card_ns), parts, submits);
        }
    }
    /// Multiply-accumulate block `j` on the card over partitions `k0..`
    /// (the FDL slot of block j - k for partition k), read the accumulators
    /// back and transform them into `time_*`. False on a GPU error (the
    /// stream is failed; nothing was written).
    fn cmac(&mut self, j: i64, k0: usize) -> bool {
        let p  = self.fdl.partitions;
        let cursor = j.rem_euclid(p as i64) as u32;

        // 2–4. Encode and submit the block in pieces of partitions: each
        //    piece writes its params {n, P, cursor, k0, k_begin, k_end}, then
        //    one dispatch per branch × channel; the last piece adds the
        //    readback copies. The card is ours from the first submit until
        //    the readbacks are mapped (the load badge, perf.rs).
        //    AURA_FIR_OPT: only partitions ka..kz (the first and the last
        //    with sound) and one dispatch per branch for both channels; the
        //    silent ones after kz are the last piece's `settle` steps
        //    (gpu_poly2.comp.glsl), the ones before ka add nothing.
        let groups = ((GPU_BINS + 255) / 256) as u32;
        let knobs = knobs();
        let first = k0.min(p);
        let (ka, kz) = if self.opt { self.sound_range(j, first) } else { (first, p) };
        let settle = if kz > ka { p - kz } else { 0 };
        let piece = match self.piece {
            0 if self.opt => (PIECE_PARTS_OPT / self.per_branch.len()).max(1),
            0 => (PIECE_DISPATCHES / (2 * self.per_branch.len()).max(1)).max(1),
            n => n,
        };
        let n_submits = (kz - ka).div_ceil(piece).max(1);
        let busy = crate::perf::gpu_span();
        let t_card = Instant::now();
        for n in 0..n_submits {
            let kb = ka + n * piece;
            let last = n + 1 == n_submits;
            let ke = if last { kz } else { kb.saturating_add(piece) };
            let params: [u32; 8] = if self.opt {
                let settle = if last { settle } else { 0 };
                [GPU_BINS as u32, p as u32, cursor, ka as u32, kb as u32, ke as u32, settle as u32, 0]
            } else {
                [GPU_BINS as u32, p as u32, cursor, k0 as u32, kb as u32, ke as u32, 0, 0]
            };
            self.ctx.queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));
            let mut encoder = self.ctx.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("player/gpu poly_block"),
                });
            for pb in &self.per_branch {
                if self.opt {
                    {
                        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("player/gpu cmul_lr"),
                            timestamp_writes: None,
                        });
                        pass.set_pipeline(&self.ctx.ola2_pipeline);
                        pass.set_bind_group(0, &pb.bg2, &[]);
                        pass.dispatch_workgroups(groups, 1, 1);
                    }
                    if last {
                        encoder.copy_buffer_to_buffer(&pb.accum_l, 0, &pb.staging_l, 0, (NBIN * 16) as u64);
                        encoder.copy_buffer_to_buffer(&pb.accum_r, 0, &pb.staging_r, 0, (NBIN * 16) as u64);
                    }
                    continue;
                }
                for (bg, accum, staging, label) in [
                    (&pb.bg_l, &pb.accum_l, &pb.staging_l, "player/gpu cmul_l"),
                    (&pb.bg_r, &pb.accum_r, &pb.staging_r, "player/gpu cmul_r"),
                ] {
                    {
                        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some(label),
                            timestamp_writes: None,
                        });
                        pass.set_pipeline(&self.ctx.ola_pipeline);
                        pass.set_bind_group(0, bg, &[]);
                        pass.dispatch_workgroups(groups, 1, 1);
                    }
                    if last {
                        encoder.copy_buffer_to_buffer(accum, 0, staging, 0, (NBIN * 16) as u64);
                    }
                }
            }
            let idx = self.ctx.queue.submit(std::iter::once(encoder.finish()));
            if knobs.gap_us > 0 && !last {
                self.ctx.device.poll(wgpu::Maintain::WaitForSubmissionIndex(idx));
                std::thread::sleep(std::time::Duration::from_micros(knobs.gap_us));
            }
        }

        // 5. map_async all staging buffers with result channels (C5).
        let slice_size = (NBIN * 16) as u64;
        let map_done  = Arc::new(AtomicUsize::new(0));
        let map_error = Arc::new(AtomicBool::new(false));
        let expected  = self.per_branch.len() * 2;

        for pb in &self.per_branch {
            let cnt = Arc::clone(&map_done);
            let err = Arc::clone(&map_error);
            pb.staging_l.slice(..slice_size).map_async(wgpu::MapMode::Read, move |r| {
                if r.is_err() { err.store(true, Ordering::Release); }
                cnt.fetch_add(1, Ordering::Release);
            });
            let cnt = Arc::clone(&map_done);
            let err = Arc::clone(&map_error);
            pb.staging_r.slice(..slice_size).map_async(wgpu::MapMode::Read, move |r| {
                if r.is_err() { err.store(true, Ordering::Release); }
                cnt.fetch_add(1, Ordering::Release);
            });
        }

        // 6. Watchdog poll loop.  Deadline = 1800 ms (measured: steady blocks
        //    5–50 ms on RTX 4090; under parallel test load ≤ 300 ms; Windows TDR
        //    fires at 2000 ms so our deadline is safely below it).  No yield_now
        //    (render thread runs at HIGHEST priority — K1).
        //
        //    Semantics (K14 fix):
        //    - A single timeout → this stream only (stream_error); bumps the
        //      session counter.  The controller sees is_gpu_failed() and swaps
        //      to the CPU path for this track.
        //    - ≥ 2 timeouts in this process session → real device trouble;
        //      player_device_error is set process-wide so no further GPU streams
        //      are attempted.
        let deadline = Instant::now() + std::time::Duration::from_millis(1800);
        loop {
            self.ctx.device.poll(wgpu::Maintain::Poll);
            if map_done.load(Ordering::Acquire) == expected {
                break;
            }
            if self.ctx.player_device_error.load(Ordering::Acquire) {
                break;
            }
            if Instant::now() >= deadline && is_background() {
                // A background stream's timeout fails that stream only; it
                // never counts toward disabling the GPU for playback.
                crate::aelog!("[player/gpu] background stream watchdog timeout for block {}", j);
                self.stream_error = true;
                break;
            }
            if Instant::now() >= deadline {
                let prev = self.ctx.watchdog_timeout_count
                    .fetch_add(1, Ordering::AcqRel);
                let session_count = prev + 1;
                crate::aelog!(
                    "[player/gpu] poll watchdog timeout for block {} \
                     (session watchdog count = {})",
                    j, session_count
                );
                self.stream_error = true;
                if session_count >= 2 {
                    crate::aelog!(
                        "[player/gpu] {} watchdog timeouts in this session — \
                         disabling GPU process-wide",
                        session_count
                    );
                    self.ctx.player_device_error.store(true, Ordering::Release);
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        drop(busy);
        self.card_last = (t_card.elapsed().as_nanos() as u64, n_submits, (kz - ka, p - first));
        if knobs.timing {
            crate::aelog!(
                "[player/gpu] block {} card {:.1} ms in {} submits",
                j, t_card.elapsed().as_secs_f64() * 1e3, n_submits
            );
        }

        // 7. Check map result; on error take the error path (C6).
        //    A map_async failure (map_error) indicates a real device error —
        //    set the process-wide flag.  has_error() may also be true because
        //    the watchdog already set stream_error or injected_fault: those do
        //    NOT escalate to the process-wide flag here.
        let is_map_err = map_error.load(Ordering::Acquire);
        if is_map_err {
            // Real device error: escalate process-wide.
            self.ctx.player_device_error.store(true, Ordering::Release);
        }
        if self.has_error() || is_map_err {
            for pb in &self.per_branch {
                // Unmap any buffers that did get mapped.
                pb.staging_l.unmap();
                pb.staging_r.unmap();
            }
            return false;
        }

        // 8. Read back, decode DS → Complex<f64>.
        for (branch, pb) in self.per_branch.iter().enumerate() {
            {
                let mapped = pb.staging_l.slice(..slice_size).get_mapped_range();
                let decoded = decode_spectrum_ds(&mapped);
                self.scratch_l[branch].copy_from_slice(&decoded);
            }
            pb.staging_l.unmap();
            {
                let mapped = pb.staging_r.slice(..slice_size).get_mapped_range();
                let decoded = decode_spectrum_ds(&mapped);
                self.scratch_r[branch].copy_from_slice(&decoded);
            }
            pb.staging_r.unmap();
        }

        // 9. Parallel CPU IFFTs — all branches left, then all branches right (C2).
        let ifft = &self.ifft;
        self.scratch_l.par_iter_mut().zip(self.time_l.par_iter_mut()).for_each(|(s, y)| c2r(ifft, s, y));
        self.scratch_r.par_iter_mut().zip(self.time_r.par_iter_mut()).for_each(|(s, y)| c2r(ifft, s, y));

        true
    }

    /// Block j's partitions from `first` to sum: from the first whose FDL
    /// slot (block j − k) has sound to the last such + 1; (P, P) when none
    /// has.
    fn sound_range(&self, j: i64, first: usize) -> (usize, usize) {
        let p = self.fdl.partitions;
        let sound = |k: usize| !self.silent[(j - k as i64).rem_euclid(p as i64) as usize];
        match (first..p).find(|&k| sound(k)) {
            None => (p, p),
            Some(ka) => (ka, (ka..p).rev().find(|&k| sound(k)).map_or(ka, |k| k) + 1),
        }
    }

    /// The error path: `frames` branch frames of silence from block `j`'s
    /// piece starting where `pend` would start (`frames` = a block, or a live
    /// stream's piece at `chunk`).
    fn zeros_at(&mut self, t0: Instant, c0: i64, frames: usize) {
        let l = self.bank.l;
        self.pend_l.clear();
        self.pend_r.clear();
        self.pend_l.resize(frames * l, 0.0);
        self.pend_r.resize(frames * l, 0.0);
        self.pend_pos   = 0;
        self.pend_start = (c0 - self.bank.delay as i64).max(0) as u64;
        self.last_block_wall_ns = t0.elapsed().as_nanos() as u64;
    }

    fn zeros(&mut self, t0: Instant, j: i64, frames: usize) {
        let c0 = j * (BLOCK * self.bank.l) as i64;
        self.zeros_at(t0, c0, frames);
    }

    /// A live stream's next piece of block `next_block` (convolver.rs, head
    /// and tail): at the block's first piece the earlier blocks go into the
    /// ring and the card sums the tail (partitions 1..); every piece the
    /// head's levels run on the CPU and are added to it.
    fn compute_chunk(&mut self) {
        let t0 = Instant::now();
        let mut ht = self.ht.take().expect("a head-and-tail stream");
        let j = self.next_block;
        let c = ht.chunk;
        let l = self.bank.l;
        let m = j * (BLOCK / HEAD_CHUNK) as i64 + c as i64;
        let c0 = (j * BLOCK as i64 + c as i64 * HEAD_CHUNK as i64) * l as i64;
        let advance = |ht: &mut HeadTail, next_block: &mut i64| {
            ht.chunk = c + 1;
            if ht.chunk == BLOCK / HEAD_CHUNK {
                ht.chunk = 0;
                *next_block = j + 1;
            }
        };
        if c == 0 {
            self.blocks_computed += 1;
            if self.fail_after.is_some_and(|n| self.blocks_computed >= n) {
                self.injected_fault = true;
            }
        }
        let mut ok = !self.has_error();
        if ok && c == 0 {
            while ht.filled < j - 1 {
                let b = ht.filled + 1;
                self.input.ensure((b + 1) * BLOCK as i64);
                self.write_block(b);
                ht.filled = b;
            }
            ok = self.cmac(j, 1);
        }
        if !ok {
            self.zeros_at(t0, c0, HEAD_CHUNK);
            advance(&mut ht, &mut self.next_block);
            self.ht = Some(ht);
            return;
        }
        self.input.ensure((m + 1) * HEAD_CHUNK as i64);
        let (tl, tr) = (&self.time_l, &self.time_r);
        ht.piece(
            &self.input, m, c, l, self.bank.scale,
            |idx, tb| if idx % 2 == 0 { tl[idx / 2][BLOCK + tb] } else { tr[idx / 2][BLOCK + tb] },
            &mut self.pend_l, &mut self.pend_r,
        );
        let m0 = c0 - self.bank.delay as i64;
        let skip = if (self.pos as i64) > m0 { (self.pos as i64 - m0) as usize } else { 0 };
        self.pend_pos   = skip.min(self.pend_l.len());
        self.pend_start = (m0 + self.pend_pos as i64).max(0) as u64;
        advance(&mut ht, &mut self.next_block);
        self.ht = Some(ht);
        self.last_block_wall_ns = t0.elapsed().as_nanos() as u64;
        // The card sums a block's tail at its first piece.
        let (card_ns, submits, parts) = self.card_last;
        if let Some(p) = self.perf.as_mut() {
            if c == 0 {
                p.block(HEAD_CHUNK, self.last_block_wall_ns, Some(card_ns), parts, submits);
            } else {
                p.block(HEAD_CHUNK, self.last_block_wall_ns, None, (0, 0), 0);
            }
        }
    }
}

fn mk_buf(
    device: &wgpu::Device,
    size: u64,
    usage: wgpu::BufferUsages,
    label: &str,
) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: false,
    })
}

/// Block j's input spectrum, channel `ch`: the CPU stream's half spectrum,
/// a silent block's as its exact zeros (`Input::spectrum` leaves it empty);
/// and whether it was silent.
fn spectrum_of(input: &Input, fft: &R2c, ch: usize, j: i64) -> (Vec<C>, bool) {
    let s = input.spectrum(fft, ch, j);
    if s.is_empty() {
        (vec![C::new(0.0, 0.0); NBIN], true)
    } else {
        (s, false)
    }
}

// ── Stage impl ───────────────────────────────────────────────────────────────

impl Stage for GpuPolyStream {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if self.pos >= self.total {
                for i in done..n {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                break;
            }
            if self.pend_pos >= self.pend_l.len() {
                if self.ht.is_some() {
                    self.compute_chunk();
                } else {
                    self.compute_block();
                }
                continue;
            }
            debug_assert_eq!(self.pend_start, self.pos,
                "GPU stream position mismatch: pend_start={} pos={}", self.pend_start, self.pos);
            let avail = self.pend_l.len() - self.pend_pos;
            let left_in_track = (self.total - self.pos) as usize;
            let take = avail.min(n - done).min(left_in_track);
            out_l[done..done + take]
                .copy_from_slice(&self.pend_l[self.pend_pos..self.pend_pos + take]);
            out_r[done..done + take]
                .copy_from_slice(&self.pend_r[self.pend_pos..self.pend_pos + take]);
            self.pend_pos   += take;
            self.pend_start += take as u64;
            self.pos        += take as u64;
            done            += take;
            inside          += take;
        }
        inside
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn total(&self) -> u64 {
        self.total
    }

    fn is_gpu_failed(&self) -> bool {
        self.has_error()
    }
}

#[cfg(test)]
mod tests {
    use super::history_pieces;

    /// The pieces put the same bytes in the ring as one write per run.
    #[test]
    fn history_pieces_make_the_one_write_image() {
        let (slot_bytes, enc) = (8usize, 5usize);
        let slots = [0usize, 1, 2, 3, 4, 7, 9, 10, 11];
        let encoded: Vec<(usize, Vec<u8>, Vec<u8>)> = slots
            .iter()
            .map(|&s| (s, vec![s as u8 + 1; enc], vec![s as u8 + 101; enc]))
            .collect();
        let ring = |per: usize| {
            let mut l = vec![0xAAu8; 12 * slot_bytes];
            let mut r = l.clone();
            history_pieces(&encoded, slot_bytes, per, |slot, il, ir| {
                assert!(il.len() <= per * slot_bytes);
                l[slot * slot_bytes..][..il.len()].copy_from_slice(il);
                r[slot * slot_bytes..][..ir.len()].copy_from_slice(ir);
            });
            (l, r)
        };
        let one = ring(usize::MAX);
        for &s in &slots {
            let at = s * slot_bytes;
            assert_eq!(one.0[at..at + enc], [s as u8 + 1; 5]);
            assert_eq!(one.0[at + enc..at + slot_bytes], [0; 3]);
            assert_eq!(one.1[at..at + enc], [s as u8 + 101; 5]);
        }
        assert_eq!(one.0[5 * slot_bytes], 0xAA);
        for per in [1, 2, 3, 4] {
            assert_eq!(ring(per), one, "per {per}");
        }
    }
}
