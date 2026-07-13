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
/// Maximum OLA block size on the GPU. Caps memory at ~32 MB per buffer
/// per channel (in DS layout: N × 2 × 16 bytes = 64 MB scratch per ch).
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
//     mantissa (~−260 dB null residual vs rustfft<f64>).

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
    pub(crate) _pad: u32,
}

#[allow(dead_code)]
pub struct GpuDspProcessor {
    pub(crate) b_size: usize,     // OLA block size = N/2
    pub(crate) n: usize,          // FFT size = 2 × b_size
    pub(crate) log2_n: u32,
    pub(crate) num_blocks: usize, // K = ceil(taps / b_size)
    pub(crate) taps: usize,
    pub(crate) precision: u32,

    pub(crate) device: Arc<wgpu::Device>,
    pub(crate) queue: Arc<wgpu::Queue>,

    pub(crate) bit_reverse_pipeline: wgpu::ComputePipeline,
    pub(crate) fft_pass_pipeline: wgpu::ComputePipeline,
    pub(crate) cmul_accum_pipeline: wgpu::ComputePipeline,

    pub(crate) fft_params_buf: wgpu::Buffer,
    pub(crate) ola_params_buf: wgpu::Buffer,
    pub(crate) align: usize,

    // Every complex slot is a DS pair = vec4<f32> = 16 bytes.
    pub(crate) work_l_buf: wgpu::Buffer,
    pub(crate) work_r_buf: wgpu::Buffer,
    pub(crate) accum_l_buf: wgpu::Buffer,
    pub(crate) accum_r_buf: wgpu::Buffer,
    pub(crate) h_freq_buf: wgpu::Buffer,
    pub(crate) delay_l_buf: wgpu::Buffer,
    pub(crate) delay_r_buf: wgpu::Buffer,
    pub(crate) twiddle_buf: wgpu::Buffer,
    pub(crate) staging_buf: wgpu::Buffer,

    pub(crate) fft_bg_work_l: wgpu::BindGroup,
    pub(crate) fft_bg_work_r: wgpu::BindGroup,
    pub(crate) fft_bg_accum_l: wgpu::BindGroup,
    pub(crate) fft_bg_accum_r: wgpu::BindGroup,
    pub(crate) ola_bg_l: wgpu::BindGroup,
    pub(crate) ola_bg_r: wgpu::BindGroup,

    // I/O kept in f64 throughout; converted to DS pair only at GPU upload.
    pub(crate) save_buf_l: Vec<f64>,
    pub(crate) save_buf_r: Vec<f64>,
    pub(crate) in_buf_l: Vec<f64>,
    pub(crate) in_buf_r: Vec<f64>,
    pub(crate) out_buf_l: Vec<f64>,
    pub(crate) out_buf_r: Vec<f64>,
    pub(crate) io_pos: usize,
    pub(crate) cursor: usize,

    // Scratch buffer for DS-encoded uploads: 4 × f32 per complex value.
    pub(crate) complex_l: Vec<f32>,
    pub(crate) complex_r: Vec<f32>,

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

        for i in 0..chunk {
            // f64 input flows straight into the f64 ring buffer; the f64→DS
            // pair conversion is deferred until process_ola_block() actually
            // uploads a full block to the GPU.
            self.in_buf_l[self.io_pos] = in_l[i];
            self.in_buf_r[self.io_pos] = in_r[i];

            let mut val_l = self.out_buf_l[self.io_pos];
            let mut val_r = self.out_buf_r[self.io_pos];

            if val_l.is_nan() || val_l.is_infinite() {
                self.nan_count += 1;
                val_l = 0.0;
            }
            if val_r.is_nan() || val_r.is_infinite() {
                self.nan_count += 1;
                val_r = 0.0;
            }
            let abs_l = val_l.abs();
            let abs_r = val_r.abs();
            if abs_l > self.max_abs_val {
                self.max_abs_val = abs_l;
            }
            if abs_r > self.max_abs_val {
                self.max_abs_val = abs_r;
            }
            if abs_l > 1.0 || abs_r > 1.0 {
                self.clip_count += 1;
            }

            out_l[i] = val_l;
            out_r[i] = val_r;

            self.io_pos += 1;
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
