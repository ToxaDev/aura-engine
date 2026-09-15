use super::processor::{GpuDspProcessor, FftParams};
use std::time::Instant;

// Pre-compiled SPIR-V blobs from build.rs (glslangValidator output of GLSL
// shaders with `precise` qualifier → SPIR-V with NoContraction decorations).
const SPV_FFT_BIT_REVERSE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_fft_bit_reverse.spv"));
const SPV_FFT_PASS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_fft_pass.spv"));
const SPV_OLA: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_ola.spv"));

/// A refusal the device did not actually raise.
///
/// `AURA_GPU_OOM_AFTER=n` makes the (n+1)-th convolver built in this process,
/// and every one after it, report the error a full card would have raised.
/// Same reason as `AURA_NO_GPU` in `context.rs`: a fallback nobody can
/// exercise is a fallback nobody has tested, and the machine this was written
/// on has 24 GB of VRAM and will never take this branch on its own.
/// `AURA_GPU_OOM_AFTER=0` sends every convolver to the CPU; `=2` leaves the
/// first two on the GPU, which is what a bank filling up part-way looks like.
/// Not documented anywhere a user would read.
fn synthetic_refusal() -> Option<String> {
    static BUILT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let after: usize = std::env::var("AURA_GPU_OOM_AFTER").ok()?.parse().ok()?;
    let nth = BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if nth < after {
        return None;
    }
    Some(format!(
        "Validation Error In Device::create_buffer Not enough memory left. (synthetic, convolver #{} under AURA_GPU_OOM_AFTER={})",
        nth, after
    ))
}

impl GpuDspProcessor {
    /// Build the full GPU pipeline (buffers, pipelines, bind groups) with an
    /// EMPTY (all-zero) filter spectrum. `new_with_coefficients` uploads the
    /// real pre-computed taps immediately afterwards — the old design first
    /// generated a full placeholder Kaiser FIR (seconds of Bessel evaluation
    /// + a complete DS FFT for 10M+ taps) only to overwrite it.
    pub(crate) fn new_uninitialized(target_taps: usize, precision: u32) -> Result<Self, String> {
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

        // The device first, and only then admission. Both orders work — the
        // context allocates no per-conversion memory — but this one cannot
        // reach the VRAM budget on a machine that has no device to take a
        // budget from.
        //
        // A batch normally never gets here without a usable GPU: the manager
        // probes once and clears `use_gpu` for everybody. These two remain
        // because this constructor is also reachable from tests and from the
        // filter-cache warm-up, and an error is a per-file failure message
        // while the panic this replaced took the whole worker thread down.
        let ctx = crate::audio::gpu::context::try_gpu_context()?;
        if !ctx.spirv_passthrough {
            return Err(format!(
                "GPU adapter '{}' does not expose SPIRV_SHADER_PASSTHROUGH — \
                 the DS GPU pipeline cannot run on this device.",
                ctx.adapter_name
            ));
        }

        // Admission BEFORE any allocation: block here if another worker's
        // convolver is already using the device budget. On an idle device
        // this returns immediately whatever the demand.
        let vram = crate::audio::gpu::vram_admission::reserve(
            crate::audio::gpu::vram_admission::processor_bytes(target_taps),
        );

        // Anything the device refuses from here on is ours. The handler
        // installed in `context.rs` records it instead of panicking, and the
        // check after the last allocation below turns it into an `Err`.
        //
        // The gate makes "ours" true: the slot is one per process, so this
        // has to be the only convolver being built while it is in use. Taken
        // after the reservation above, never before — see `lock_allocations`.
        let alloc_gate = crate::audio::gpu::context::lock_allocations();
        crate::audio::gpu::context::clear_device_error();

        let device = ctx.device;
        let queue = ctx.queue;
        let align = ctx.align;

        crate::aelog!("[GPU] Device: {} (backend: {})", ctx.adapter_name, ctx.backend_name);

        // ── DS layout: every complex value is vec4<f32> = 16 bytes ──
        const DS_BYTES: usize = 16;
        let h_freq_bytes = num_blocks * n * DS_BYTES;
        let delay_bytes = h_freq_bytes;
        let complex_buf_bytes = (n * DS_BYTES) as u64;
        let twiddle_bytes = (n / 2) * DS_BYTES;

        // ── Pre-computed DS twiddle table on CPU ──
        // Layout and derivation live in filter_cache::twiddles(); the table
        // depends on nothing but N, so it is built once per FFT size and
        // reused by every subsequent processor. On the polyphase path that
        // matters: a processor is constructed per sub-filter, and the ~42 ms
        // of f64 sin/cos at N = 4M used to be paid 16 times per file.
        let twiddle_data = crate::audio::gpu::filter_cache::twiddles(n);

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
        queue.write_buffer(&twiddle_buf, 0, bytemuck::cast_slice(&twiddle_data[..]));

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

        // Every buffer this convolver needs has now been asked for. `poll`
        // flushes what the queue was still holding, so an error raised by the
        // twiddle or params upload is in the slot before we read it.
        //
        // Checking HERE, and not after the bind groups, is deliberate: a bind
        // group built over a buffer the device never gave us raises a second,
        // less informative error and buries the first.
        device.poll(wgpu::Maintain::Poll);
        let refusal = crate::audio::gpu::context::take_device_error()
            .or_else(synthetic_refusal);
        drop(alloc_gate);
        if let Some(err) = refusal {
            let want = crate::audio::gpu::vram_admission::processor_bytes(target_taps);
            let one_line = err.split_whitespace().collect::<Vec<_>>().join(" ");
            // Only a memory refusal tells us anything about this device's
            // ceiling. Every other error still fails this convolver, but it
            // must not teach the admission gate a limit that isn't there.
            if one_line.to_ascii_lowercase().contains("memory") {
                crate::audio::gpu::vram_admission::note_oom(want);
            }
            crate::aelog!(
                "[GPU] Device refused the {} MB this {}-tap convolver needs → CPU for this filter",
                want / 1_048_576,
                target_taps
            );
            return Err(format!(
                "GPU would not allocate the {} MB this {}-tap convolver needs: {}",
                want / 1_048_576,
                target_taps,
                one_line
            ));
        }

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
            _vram: vram,
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
    ///
    /// The spectrum is not cached — use `new_with_coefficients_keyed` when the
    /// coefficients have a stable provenance (a `.npy` path, optionally a
    /// polyphase sub-filter index) so repeated files in a batch can reuse it.
    pub fn new_with_coefficients(coeffs: &[f64], precision: u32) -> Result<Self, String> {
        Self::new_with_coefficients_keyed(coeffs, precision, None)
    }

    /// As `new_with_coefficients`, but `id` names where the coefficients came
    /// from so the partitioned spectrum can be cached across files.
    ///
    /// `H[ω]` is a pure function of the coefficients, so a cache hit uploads
    /// byte-for-byte what a miss would have computed — the convolution is
    /// bit-identical either way. See `filter_cache` for the sizing policy.
    pub fn new_with_coefficients_keyed(
        coeffs: &[f64],
        precision: u32,
        id: Option<&crate::audio::gpu::filter_cache::FilterId>,
    ) -> Result<Self, String> {
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

        if crate::audio::cancel_flag::check() {
            return Err("Cancelled".into());
        }

        // 4 f32 per complex value (re_hi, re_lo, im_hi, im_lo), num_blocks × n
        // complex values — must match h_freq_buf exactly.
        let expect_words = num_blocks * n * 4;
        let h_freq_data =
            match crate::audio::gpu::filter_cache::get_spectrum(id, n, expect_words) {
            Some(hit) => {
                crate::aelog!("[GPU] H_blocks reused from cache (no FFT)");
                hit
            }
            None => {
                // `coeffs` is passed straight in: compute_h_blocks_cpu_ds_f64
                // bounds-checks `offset + i < h_time.len()` against a
                // zero-initialised block, so a short slice pads itself. The
                // explicit `h_padded` copy this used to build was a redundant
                // num_blocks × b_size × 8-byte allocation per construction.
                let t_fft = Instant::now();
                let computed =
                    Self::compute_h_blocks_cpu_ds_f64(coeffs, b_size, n, num_blocks)
                        .ok_or_else(|| "Cancelled".to_string())?;
                crate::aelog!(
                    "[GPU] Custom H_blocks FFT'd (f64→DS) in {:.2}s",
                    t_fft.elapsed().as_secs_f64()
                );
                debug_assert_eq!(computed.len(), expect_words);
                crate::audio::gpu::filter_cache::put_spectrum(id, n, computed)
            }
        };

        // The spectrum is the largest single upload of the build; on a device
        // that was already close to full this is where it shows. Same gate as
        // the allocations, and for the same reason.
        let alloc_gate = crate::audio::gpu::context::lock_allocations();
        crate::audio::gpu::context::clear_device_error();
        proc.queue
            .write_buffer(&proc.h_freq_buf, 0, bytemuck::cast_slice(&h_freq_data[..]));
        proc.device.poll(wgpu::Maintain::Poll);
        let refusal = crate::audio::gpu::context::take_device_error();
        drop(alloc_gate);
        if let Some(err) = refusal {
            let one_line = err.split_whitespace().collect::<Vec<_>>().join(" ");
            if one_line.to_ascii_lowercase().contains("memory") {
                crate::audio::gpu::vram_admission::note_oom(
                    crate::audio::gpu::vram_admission::processor_bytes(target_taps),
                );
            }
            return Err(format!(
                "GPU would not take the filter spectrum for {} taps: {}",
                target_taps, one_line
            ));
        }
        crate::aelog!(
            "[GPU] Custom DS filter uploaded ({} bytes)",
            h_freq_data.len() * 4
        );
        crate::aelog!("[GPU] ═══════════════════════════════════════════");

        Ok(proc)
    }
}
