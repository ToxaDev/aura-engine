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
    bg_l: wgpu::BindGroup,
    bg_r: wgpu::BindGroup,
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

        let l = bank.l;
        let sub_taps = (bank.full_len + l - 1) / l;
        let p = partitions(sub_taps);

        // Device limits check (K10): per-branch binding and buffer sizes.
        if !ctx.fits(bank.full_len, l) {
            crate::aelog!(
                "[player/gpu] device limits check failed for {} taps L={} — CPU path",
                bank.full_len, l
            );
            return None;
        }

        // VRAM check: dxgi free × 0.75 ≥ demand. A background stream takes
        // at most a quarter, so playback's next stream still fits after it.
        let need = super::vram_demand(bank.full_len, l);
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
        let bk = BankKey { path: bank.path.clone(), l, full_len: bank.full_len };
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

        // params_buf: uniform {n: u32, P: u32, cursor: u32, _pad: u32}
        let params_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player/gpu poly_params"),
            size: 16,
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

            let bg_l = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("player/gpu bg_l"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params_buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(16),
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
                            size: wgpu::BufferSize::new(16),
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
                ],
            });
            PerBranch { accum_l, accum_r, staging_l, staging_r, bg_l, bg_r }
        }).collect();

        let mut planner = RealFftPlanner::<f64>::new();
        let fft  = planner.plan_fft_forward(NFFT);
        let ifft = planner.plan_fft_inverse(NFFT);

        let total  = input.total(l);
        let c      = start + bank.delay as u64;
        let t      = (c / l as u64) as i64;
        let j0     = t / BLOCK as i64;

        // A live stream plays head and tail; the ring holds every block
        // before j0 after the prime.
        let ht = matches!(input, Input::Live(_)).then(|| HeadTail::new(&bank, j0 - 1));
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
            // Fault injection for window checks; see the field. Off unless set.
            fail_after: std::env::var("AURA_GPU_FAIL_AFTER").ok().and_then(|s| s.parse().ok()),
            injected_fault: false,
            stream_error: false,
            last_block_wall_ns: 0,
            bank,
        };
        s.prime(j0);
        // Drain all pending uploads (h_freq + FDL prime writes) before returning
        // to the controller thread.  The first render-thread block is then fully
        // steady-state with no staging copies in flight.
        s.ctx.device.poll(wgpu::Maintain::Wait);
        Some(s)
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
    /// (228 calls for P=115), we assemble the full P-slot ring image in RAM
    /// and issue exactly 2 write_buffer calls (one per channel).  For 30M the
    /// FDL is 2 × 115 × 65536 × 16 B ≈ 2 × 115 MB; the write_buffer overhead
    /// is large relative to the data for small writes, so batching it saves
    /// measurable time.
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
        let p_usize = p as usize;

        // Parallel encoding of all P − 1 history blocks.
        // The remaining slot (j0 itself, slot j0 % P) is written by the first
        // compute_block call; we leave it zero here (FdlRing::new zeroed it).
        let encoded: Vec<(usize, Vec<u8>, Vec<u8>)> = (first..j0)
            .collect::<Vec<i64>>()
            .into_par_iter()
            .map(|j| {
                let sl = spectrum_of(input, fft, 0, j);
                let sr = spectrum_of(input, fft, 1, j);
                let slot = j.rem_euclid(p) as usize;
                let mut enc_l = Vec::with_capacity(enc_bytes);
                let mut enc_r = Vec::with_capacity(enc_bytes);
                encode_spectrum_ds(&sl, &mut enc_l);
                encode_spectrum_ds(&sr, &mut enc_r);
                (slot, enc_l, enc_r)
            })
            .collect();

        // Assemble ring images (zero-initialised; unwanted slot stays zero).
        let mut ring_l = vec![0u8; p_usize * slot_bytes];
        let mut ring_r = vec![0u8; p_usize * slot_bytes];
        for (slot, enc_l, enc_r) in &encoded {
            let off = slot * slot_bytes;
            ring_l[off..off + enc_bytes].copy_from_slice(enc_l);
            ring_r[off..off + enc_bytes].copy_from_slice(enc_r);
        }

        // Two writes: one per channel, covering the whole ring.
        self.ctx.queue.write_buffer(&self.fdl.buf_l, 0, &ring_l);
        self.ctx.queue.write_buffer(&self.fdl.buf_r, 0, &ring_r);
        self.ctx.queue.submit(std::iter::empty());
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
        let sl = spectrum_of(&self.input, &self.fft, 0, j);
        let sr = spectrum_of(&self.input, &self.fft, 1, j);
        encode_spectrum_ds(&sl, &mut self.enc_buf_l);
        encode_spectrum_ds(&sr, &mut self.enc_buf_r);
        self.fdl.write_slot(&self.ctx.queue, j, &self.enc_buf_l, &self.enc_buf_r);

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
    }
    /// Multiply-accumulate block `j` on the card over partitions `k0..`
    /// (the FDL slot of block j - k for partition k), read the accumulators
    /// back and transform them into `time_*`. False on a GPU error (the
    /// stream is failed; nothing was written).
    fn cmac(&mut self, j: i64, k0: usize) -> bool {
        let p  = self.fdl.partitions;
        let cursor = j.rem_euclid(p as i64) as u32;

        // 2. Update params uniform: {n, P, cursor, k0}.
        let params: [u32; 4] = [GPU_BINS as u32, p as u32, cursor, k0 as u32];
        self.ctx.queue.write_buffer(
            &self.params_buf, 0,
            bytemuck::bytes_of(&params),
        );

        // 3. Encode dispatches (one per branch × 2 channels) + readback copies.
        let groups = ((GPU_BINS + 255) / 256) as u32;
        let mut encoder = self.ctx.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("player/gpu poly_block"),
            });

        for pb in &self.per_branch {
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("player/gpu cmul_l"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.ctx.ola_pipeline);
                pass.set_bind_group(0, &pb.bg_l, &[]);
                pass.dispatch_workgroups(groups, 1, 1);
            }
            encoder.copy_buffer_to_buffer(
                &pb.accum_l, 0, &pb.staging_l, 0, (NBIN * 16) as u64,
            );
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("player/gpu cmul_r"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.ctx.ola_pipeline);
                pass.set_bind_group(0, &pb.bg_r, &[]);
                pass.dispatch_workgroups(groups, 1, 1);
            }
            encoder.copy_buffer_to_buffer(
                &pb.accum_r, 0, &pb.staging_r, 0, (NBIN * 16) as u64,
            );
        }

        // 4. Submit (before map_async per spec).
        self.ctx.queue.submit(std::iter::once(encoder.finish()));

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
                let sl = spectrum_of(&self.input, &self.fft, 0, b);
                let sr = spectrum_of(&self.input, &self.fft, 1, b);
                encode_spectrum_ds(&sl, &mut self.enc_buf_l);
                encode_spectrum_ds(&sr, &mut self.enc_buf_r);
                self.fdl.write_slot(&self.ctx.queue, b, &self.enc_buf_l, &self.enc_buf_r);
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

/// Block j's input spectrum, channel `ch`: the CPU stream's half spectrum.
fn spectrum_of(input: &Input, fft: &R2c, ch: usize, j: i64) -> Vec<C> {
    input.spectrum(fft, ch, j)
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
