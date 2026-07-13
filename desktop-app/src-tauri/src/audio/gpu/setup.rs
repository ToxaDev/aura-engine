use super::processor::{GpuDspProcessor, FftParams};
use std::time::Instant;
use crate::audio::gpu::context::get_gpu_context;

// Pre-compiled SPIR-V blobs from build.rs (glslangValidator output of GLSL
// shaders with `precise` qualifier → SPIR-V with NoContraction decorations).
const SPV_FFT_BIT_REVERSE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_fft_bit_reverse.spv"));
const SPV_FFT_PASS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_fft_pass.spv"));
const SPV_OLA: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_ola.spv"));

impl GpuDspProcessor {
    /// Build the full GPU pipeline (buffers, pipelines, bind groups) with an
    /// EMPTY (all-zero) filter spectrum. `new_with_coefficients` uploads the
    /// real pre-computed taps immediately afterwards — the old design first
    /// generated a full placeholder Kaiser FIR (seconds of Bessel evaluation
    /// + a complete DS FFT for 10M+ taps) only to overwrite it.
    fn new_uninitialized(target_taps: usize, precision: u32) -> Result<Self, String> {
        crate::aelog!("[GPU] ═══════════════════════════════════════════");
        crate::aelog!("[GPU] Initializing GPU FFT OLA Processor (DS-precision via SPIR-V passthrough)");
        crate::aelog!("[GPU] FIR Taps: {}", target_taps);

        let b_size: usize = Self::block_size(target_taps);
        let n = b_size * 2;
        // n is always a power of two (b_size is clamped to a power-of-two
        // range and then doubled), so use integer trailing-zeros instead of
        // f64::log2 + truncation.  The float path could in principle return
        // 20.999... → as u32 = 20, off-by-one, breaking every dispatch.
        debug_assert!(n.is_power_of_two(), "FFT size {} must be power of two", n);
        let log2_n: u32 = n.trailing_zeros();
        let num_blocks = (target_taps + b_size - 1) / b_size;
        let total_taps = num_blocks * b_size;

        crate::aelog!("[GPU] Algorithm: Partitioned FFT Overlap-Save (DS arithmetic)");
        crate::aelog!("[GPU] Block size: {}  FFT size: {}  log2: {}", b_size, n, log2_n);
        crate::aelog!("[GPU] Partitions: {} (total taps aligned: {})", num_blocks, total_taps);

        let ctx = get_gpu_context();
        let device = ctx.device;
        let queue = ctx.queue;
        let align = ctx.align;
        if !ctx.spirv_passthrough {
            // Return a clean error instead of panicking: a panic here unwinds
            // the whole conversion worker thread, while an Err surfaces as a
            // normal per-file failure message in the UI.
            return Err(format!(
                "GPU adapter '{}' does not expose SPIRV_SHADER_PASSTHROUGH — \
                 the DS GPU pipeline cannot run on this device. Use the CPU path \
                 (uncheck GPU in settings).",
                ctx.adapter_name
            ));
        }

        crate::aelog!("[GPU] Device: {} (backend: {})", ctx.adapter_name, ctx.backend_name);

        // ── DS layout: every complex value is vec4<f32> = 16 bytes ──
        const DS_BYTES: usize = 16;
        let h_freq_bytes = num_blocks * n * DS_BYTES;
        let delay_bytes = h_freq_bytes;
        let complex_buf_bytes = (n * DS_BYTES) as u64;
        let twiddle_bytes = (n / 2) * DS_BYTES;

        // ── Pre-computed DS twiddle table on CPU ──
        // Layout: (cos_hi, cos_lo, neg_sin_hi, neg_sin_lo) for k in 0..N/2.
        // Forward W_N^k = exp(-2πi·k/N).  Shader conjugates for IFFT.
        let twiddle_data: Vec<f32> = {
            let mut v = Vec::with_capacity((n / 2) * 4);
            let two_pi = 2.0 * std::f64::consts::PI;
            for k in 0..(n / 2) {
                let angle = two_pi * (k as f64) / (n as f64);
                let cos_v = angle.cos();
                let sin_v = -angle.sin();
                let cos_hi = cos_v as f32;
                let cos_lo = (cos_v - cos_hi as f64) as f32;
                let sin_hi = sin_v as f32;
                let sin_lo = (sin_v - sin_hi as f64) as f32;
                v.push(cos_hi);
                v.push(cos_lo);
                v.push(sin_hi);
                v.push(sin_lo);
            }
            v
        };

        let h_mb = h_freq_bytes as f64 / 1_048_576.0;
        let d_mb = delay_bytes as f64 / 1_048_576.0;
        let t_mb = twiddle_bytes as f64 / 1_048_576.0;
        crate::aelog!(
            "[GPU] Memory: h_freq={:.1}MB delay×2={:.1}MB twiddle={:.1}MB total={:.1}MB",
            h_mb, d_mb * 2.0, t_mb, h_mb + d_mb * 2.0 + t_mb
        );

        // ── Buffers ──
        let work_l_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("work_l"),
            size: complex_buf_bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let work_r_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("work_r"),
            size: complex_buf_bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let accum_l_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("accum_l"),
            size: complex_buf_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let accum_r_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("accum_r"),
            size: complex_buf_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        // Zero-initialized; new_with_coefficients uploads the real spectrum.
        let h_freq_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("h_freq"),
            size: h_freq_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let delay_l_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("delay_l"),
            size: delay_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let delay_r_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("delay_r"),
            size: delay_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let twiddle_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("twiddles_ds"),
            size: twiddle_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&twiddle_buf, 0, bytemuck::cast_slice(&twiddle_data));

        let staging_bytes = (b_size * DS_BYTES * 2) as u64;
        let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: staging_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ── FFT params buffer (one entry per pass + 2 bit-reverse entries) ──
        let num_fft_entries = 2 + 2 * log2_n as usize;
        let fft_params_size = num_fft_entries * align;
        let mut fft_params_data = vec![0u8; fft_params_size];

        Self::write_fft_params(&mut fft_params_data, 0, align,
            FftParams { n: n as u32, log_n: log2_n, pass_idx: 0, inverse: 0 });
        for p in 0..log2_n {
            Self::write_fft_params(&mut fft_params_data, 1 + p as usize, align,
                FftParams { n: n as u32, log_n: log2_n, pass_idx: p, inverse: 0 });
        }
        Self::write_fft_params(&mut fft_params_data, 1 + log2_n as usize, align,
            FftParams { n: n as u32, log_n: log2_n, pass_idx: 0, inverse: 1 });
        for p in 0..log2_n {
            Self::write_fft_params(&mut fft_params_data, 2 + log2_n as usize + p as usize, align,
                FftParams { n: n as u32, log_n: log2_n, pass_idx: p, inverse: 1 });
        }

        let fft_params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fft_params"),
            size: fft_params_size as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&fft_params_buf, 0, &fft_params_data);

        let ola_params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ola_params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ── SPIR-V passthrough shader modules ──
        // We MUST go through create_shader_module_spirv (not create_shader_module
        // with a SpirV ShaderSource), because the latter routes through naga,
        // which strips NoContraction decorations.
        let bit_reverse_module = unsafe {
            device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                label: Some("gpu_fft.bit_reverse.spv"),
                source: wgpu::util::make_spirv_raw(SPV_FFT_BIT_REVERSE),
            })
        };
        let fft_pass_module = unsafe {
            device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                label: Some("gpu_fft.pass.spv"),
                source: wgpu::util::make_spirv_raw(SPV_FFT_PASS),
            })
        };
        let ola_module = unsafe {
            device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                label: Some("gpu_ola.spv"),
                source: wgpu::util::make_spirv_raw(SPV_OLA),
            })
        };

        // ── FFT bind-group layout (data + dynamic-offset uniform + twiddle) ──
        let fft_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fft_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let fft_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fft_pl"),
            bind_group_layouts: &[&fft_bgl],
            push_constant_ranges: &[],
        });
        let bit_reverse_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("bit_reverse"),
            layout: Some(&fft_pipeline_layout),
            module: &bit_reverse_module,
            entry_point: "main",
        });
        let fft_pass_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("fft_pass"),
            layout: Some(&fft_pipeline_layout),
            module: &fft_pass_module,
            entry_point: "main",
        });

        // ── OLA bind-group layout (params + h_freq + delay + accum) ──
        let ola_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ola_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let ola_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ola_pl"),
            bind_group_layouts: &[&ola_bgl],
            push_constant_ranges: &[],
        });
        let cmul_accum_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("cmul_accum"),
            layout: Some(&ola_pipeline_layout),
            module: &ola_module,
            entry_point: "main",
        });

        let fft_bg_work_l = Self::create_fft_bind_group(
            &device, &fft_bgl, &work_l_buf, &fft_params_buf, &twiddle_buf, "work_l");
        let fft_bg_work_r = Self::create_fft_bind_group(
            &device, &fft_bgl, &work_r_buf, &fft_params_buf, &twiddle_buf, "work_r");
        let fft_bg_accum_l = Self::create_fft_bind_group(
            &device, &fft_bgl, &accum_l_buf, &fft_params_buf, &twiddle_buf, "accum_l");
        let fft_bg_accum_r = Self::create_fft_bind_group(
            &device, &fft_bgl, &accum_r_buf, &fft_params_buf, &twiddle_buf, "accum_r");

        let ola_bg_l = Self::create_ola_bind_group(
            &device, &ola_bgl, &ola_params_buf, &h_freq_buf, &delay_l_buf, &accum_l_buf, "L");
        let ola_bg_r = Self::create_ola_bind_group(
            &device, &ola_bgl, &ola_params_buf, &h_freq_buf, &delay_r_buf, &accum_r_buf, "R");

        crate::aelog!("[GPU] Pipelines: bit_reverse, fft_pass, cmul_accum (all DS via SPIR-V passthrough)");
        crate::aelog!("[GPU] Precision: Double-Single, ~48-bit mantissa, ~−260 dB null residual");
        crate::aelog!("[GPU] ═══════════════════════════════════════════");

        Ok(Self {
            b_size, n, log2_n, num_blocks,
            taps: target_taps, precision,
            device, queue,
            bit_reverse_pipeline, fft_pass_pipeline, cmul_accum_pipeline,
            fft_params_buf, ola_params_buf, align,
            work_l_buf, work_r_buf, accum_l_buf, accum_r_buf,
            h_freq_buf, delay_l_buf, delay_r_buf, twiddle_buf, staging_buf,
            fft_bg_work_l, fft_bg_work_r, fft_bg_accum_l, fft_bg_accum_r,
            ola_bg_l, ola_bg_r,
            save_buf_l: vec![0.0; b_size],
            save_buf_r: vec![0.0; b_size],
            in_buf_l: vec![0.0; b_size],
            in_buf_r: vec![0.0; b_size],
            out_buf_l: vec![0.0; b_size],
            out_buf_r: vec![0.0; b_size],
            io_pos: 0,
            cursor: 0,
            // 4 × f32 per complex value (re_hi, re_lo, im_hi, im_lo)
            complex_l: vec![0.0; n * 4],
            complex_r: vec![0.0; n * 4],
            clip_count: 0, nan_count: 0, max_abs_val: 0.0,
            call_count: 0, block_count: 0, total_gpu_time_us: 0,
        })
    }

    /// Create GPU processor from pre-computed f64 coefficients (.npy from
    /// fir-optimizer). The user's 128-bit-generated taps reach the GPU as DS
    /// pairs without any f64→f32 round-trip on the way in.
    pub fn new_with_coefficients(coeffs: &[f64], precision: u32) -> Result<Self, String> {
        let target_taps = coeffs.len();
        crate::aelog!("[GPU] ═══════════════════════════════════════════");
        crate::aelog!("[GPU] Loading CUSTOM filter (DS path): {} taps", target_taps);

        if crate::audio::cancel_flag::check() {
            return Err("Cancelled".into());
        }

        // Build the pipeline with a zeroed spectrum; the real DS spectrum is
        // uploaded below (no placeholder FIR generation / double FFT).
        let proc = Self::new_uninitialized(target_taps, precision)?;

        let b_size = proc.b_size;
        let n = proc.n;
        let num_blocks = proc.num_blocks;
        let total_taps = num_blocks * b_size;

        if crate::audio::cancel_flag::check() {
            return Err("Cancelled".into());
        }

        let mut h_padded = vec![0.0f64; total_taps];
        for (i, &v) in coeffs.iter().enumerate() {
            h_padded[i] = v;
        }

        let t_fft = Instant::now();
        let h_freq_data = Self::compute_h_blocks_cpu_ds_f64(&h_padded, b_size, n, num_blocks)
            .ok_or_else(|| "Cancelled".to_string())?;
        crate::aelog!(
            "[GPU] Custom H_blocks FFT'd (f64→DS) in {:.2}s",
            t_fft.elapsed().as_secs_f64()
        );

        proc.queue
            .write_buffer(&proc.h_freq_buf, 0, bytemuck::cast_slice(&h_freq_data));
        crate::aelog!(
            "[GPU] Custom DS filter uploaded ({} bytes)",
            h_freq_data.len() * 4
        );
        crate::aelog!("[GPU] ═══════════════════════════════════════════");

        Ok(proc)
    }
}
