// gpu/processor.rs — DS-precision GPU FFT pipeline (SPIR-V passthrough)
//
// SINGLE SOURCE OF TRUTH for the OLA block size.  Every place in the
// codebase that needs to know `b_size` (setup.rs allocator, process.rs
// trim arithmetic, apodize.rs flush sizing, hybrid_mixer.rs latency
// compensation, manager.rs VRAM accounting) must call
// `GpuDspProcessor::block_size(taps)` — never duplicate the formula.
// If the constants ever drift between call sites, OLA latency trims
// silently desync and you get sample shifts in the output.

/// Minimum OLA block size on the GPU. Chosen so even short filters get
/// large enough FFTs to amortise upload overhead.
pub const GPU_MIN_BLOCK_SIZE: usize = 262_144;
/// Maximum OLA block size on the GPU: an FFT of N = 4 194 304 points, 64 MB
/// per complex DS buffer.
pub const GPU_MAX_BLOCK_SIZE: usize = 2_097_152;
//
// Architecture:
//   * GLSL source files in src/audio/shaders/gpu_*.comp.glsl use the
//     `precise` qualifier on every intermediate. glslangValidator (called
//     from build.rs) compiles them to SPIR-V with `OpDecorate NoContraction`
//     decorations on each precise op. Vulkan drivers MUST honour those
//     decorations, which is what allows DS arithmetic to survive optimisation.
//   * wgpu loads the .spv blobs via Device::create_shader_module_spirv,
//     bypassing naga (which strips NoContraction).
//   * Every complex value is stored as vec4<f32> = (re_hi, re_lo, im_hi, im_lo).
//     CPU side keeps audio in f64; the f64↔DS pair conversion happens once at
//     upload and once at readback. End-to-end effective precision: ~48-bit
//     mantissa (~−280 dBFS against the f64 CPU convolver).
//   * Both channels share one complex FFT each way: the window goes up as
//     L + i·R, the split kernel recovers the two half spectra (bins 0 … N/2;
//     a real signal's spectrum is Hermitian, the other half is its mirror),
//     the filter spectrum, the delay lines and the multiply-accumulate are
//     all half spectra, and the accumulated pair is joined again so one
//     inverse FFT brings L back in re and R in im. See gpu_ola.comp.glsl.

use std::sync::Arc;
use std::time::Instant;
use crate::audio::processor::DspProcessor;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct FftParams {
    pub(crate) n: u32,
    pub(crate) log_n: u32,
    pub(crate) pass_idx: u32,
    pub(crate) inverse: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct OlaParams {
    pub(crate) n: u32,
    pub(crate) num_blocks: u32,
    pub(crate) cursor: u32,
    pub(crate) stride: u32,
}

#[allow(dead_code)]
pub struct GpuDspProcessor {
    pub(crate) b_size: usize,     // OLA block size = N/2
    pub(crate) n: usize,          // FFT size = 2 × b_size
    pub(crate) log2_n: u32,
    pub(crate) num_blocks: usize, // K = ceil(taps / b_size)
    /// Complex slots per partition of the filter spectrum and the delay
    /// lines: the N/2 + 1 bins of a half spectrum, rounded up.
    pub(crate) stride: usize,
    pub(crate) taps: usize,
    pub(crate) precision: u32,

    pub(crate) device: Arc<wgpu::Device>,
    pub(crate) queue: Arc<wgpu::Queue>,

    /// Device-memory admission ticket for this processor's buffers. Held for
    /// the processor's whole life and released on drop, so a second worker
    /// cannot start allocating until this one is gone. See
    /// `gpu::vram_admission` for why the budget is enforced here rather than
    /// guessed once per batch.
    pub(crate) _vram: crate::audio::gpu::vram_admission::VramReservation,

    pub(crate) bit_reverse_pipeline: wgpu::ComputePipeline,
    pub(crate) fft_pass_pipeline: wgpu::ComputePipeline,
    pub(crate) split_pipeline: wgpu::ComputePipeline,
    pub(crate) cmac_pipeline: wgpu::ComputePipeline,

    pub(crate) fft_params_buf: wgpu::Buffer,
    pub(crate) ola_params_buf: wgpu::Buffer,
    pub(crate) align: usize,

    // Every complex slot is a DS pair = vec4<f32> = 16 bytes.
    /// The window as L + i·R (N slots); the forward FFT runs in place.
    pub(crate) work_buf: wgpu::Buffer,
    /// The previous block's samples (b_size slots) — the first half of the
    /// next window, kept on the card so only the new half goes up.
    pub(crate) prev_buf: wgpu::Buffer,
    /// The joined spectrum W, then (inverse FFT in place) 2N·(L + i·R).
    pub(crate) accum_buf: wgpu::Buffer,
    /// K partitions × `stride` slots each, half spectra.
    pub(crate) h_freq_buf: wgpu::Buffer,
    pub(crate) delay_l_buf: wgpu::Buffer,
    pub(crate) delay_r_buf: wgpu::Buffer,
    pub(crate) twiddle_buf: wgpu::Buffer,
    pub(crate) staging_buf: wgpu::Buffer,

    pub(crate) fft_bg_work: wgpu::BindGroup,
    pub(crate) fft_bg_accum: wgpu::BindGroup,
    pub(crate) split_bg: wgpu::BindGroup,
    pub(crate) cmac_bg: wgpu::BindGroup,

    // I/O kept in f64 throughout; converted to DS pair only at GPU upload.
    pub(crate) in_buf_l: Vec<f64>,
    pub(crate) in_buf_r: Vec<f64>,
    pub(crate) out_buf_l: Vec<f64>,
    pub(crate) out_buf_r: Vec<f64>,
    pub(crate) io_pos: usize,
    pub(crate) cursor: usize,

    /// The new half of the window on its way up, DS-encoded:
    /// (L_hi, L_lo, R_hi, R_lo) per sample.
    pub(crate) upload: Vec<f32>,

    pub clip_count: u64,
    pub nan_count: u64,
    pub max_abs_val: f64,
    pub(crate) call_count: u64,
    pub(crate) block_count: u64,
    pub(crate) total_gpu_time_us: u64,
}

impl GpuDspProcessor {
    /// Canonical OLA block size for `target_taps`. ALL call sites that
    /// need to size buffers, compute trim offsets, or estimate VRAM usage
    /// must use this — otherwise constants drift between modules and OLA
    /// latency trims desync silently.
    #[inline]
    pub fn block_size(target_taps: usize) -> usize {
        target_taps
            .next_power_of_two()
            .clamp(GPU_MIN_BLOCK_SIZE, GPU_MAX_BLOCK_SIZE)
    }

    /// Slots per partition of a half spectrum of an `n`-point FFT: its
    /// n/2 + 1 bins, rounded up to 16 so every partition starts on a
    /// 256-byte boundary. The slots past the last bin stay zero.
    #[inline]
    pub(crate) fn half_stride(n: usize) -> usize {
        (n / 2 + 1).next_multiple_of(16)
    }

    /// Total algorithmic output latency of the GPU convolver in samples.
    /// Exactly 1 × block_size: `process_ola_block` computes the just-filled
    /// block synchronously at the block boundary, so — unlike the CPU
    /// convolver — there is no extra deferred-read block.
    #[inline]
    pub fn output_latency_for(target_taps: usize) -> usize {
        Self::block_size(target_taps)
    }
}

impl DspProcessor for GpuDspProcessor {
    fn process_audio(
        &mut self,
        in_l: &[f64],
        in_r: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        chunk: usize,
    ) {
        if chunk == 0 {
            return;
        }
        let t0 = Instant::now();

        // Runs up to the next block boundary at a time. f64 input flows
        // straight into the f64 ring buffer (the DS pair conversion waits
        // until process_ola_block() uploads a full block); the output was
        // made finite, and counted for the log, when its block was read back.
        let mut i = 0;
        while i < chunk {
            let take = (chunk - i).min(self.b_size - self.io_pos);
            let (a, z) = (self.io_pos, self.io_pos + take);
            self.in_buf_l[a..z].copy_from_slice(&in_l[i..i + take]);
            self.in_buf_r[a..z].copy_from_slice(&in_r[i..i + take]);
            out_l[i..i + take].copy_from_slice(&self.out_buf_l[a..z]);
            out_r[i..i + take].copy_from_slice(&self.out_buf_r[a..z]);
            i += take;
            self.io_pos = z;
            if self.io_pos >= self.b_size {
                self.process_ola_block();
                self.io_pos = 0;
            }
        }

        let elapsed_us = t0.elapsed().as_micros() as u64;
        self.call_count += 1;

        if self.call_count <= 5 || self.call_count % 5000 == 0 {
            crate::aelog!(
                "[GPU/DS] #{} frames={} time={:.2}ms blocks_done={} clips={} nan={}",
                self.call_count,
                chunk,
                elapsed_us as f64 / 1000.0,
                self.block_count,
                self.clip_count,
                self.nan_count
            );
        }
    }

    fn block_size(&self) -> usize {
        self.b_size
    }

    fn output_latency(&self) -> usize {
        self.b_size
    }
}
