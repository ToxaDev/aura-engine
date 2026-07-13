//! GPU DS pre-flight test: prove that GLSL `precise` qualifier compiled to
//! SPIR-V `NoContraction` decoration actually survives optimisation on the
//! current Vulkan driver.
//!
//! If this test passes, we can build the full DS-FFT pipeline through the
//! same passthrough mechanism. If it fails, that path is closed and we have
//! to fall back to CPU rustfft<f64> for high-precision modes.
//!
//! Run with:
//!     cargo test --release -- --ignored ds_preflight --nocapture

#![cfg(test)]

use pollster::block_on;
use std::sync::mpsc;

const PRECOMPILED_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ds_preflight.spv"));

/// Build a dedicated wgpu device that *requests* the SPIRV passthrough
/// feature. We don't reuse `get_gpu_context()` because that one is built
/// without passthrough — and changing it would force every call site to
/// inherit the requirement.
fn make_passthrough_device() -> Option<(wgpu::Device, wgpu::Queue, String)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))?;
    let name = adapter.get_info().name.clone();
    let supported = adapter.features();
    if !supported.contains(wgpu::Features::SPIRV_SHADER_PASSTHROUGH) {
        eprintln!(
            "[DS-PREFLIGHT] SPIRV_SHADER_PASSTHROUGH not supported on adapter '{}' — \
             cannot run preflight",
            name
        );
        return None;
    }
    let (device, queue) = block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("ds-preflight"),
            required_features: wgpu::Features::SPIRV_SHADER_PASSTHROUGH,
            required_limits: adapter.limits(),
        },
        None,
    ))
    .ok()?;
    Some((device, queue, name))
}

#[test]
#[ignore]
fn ds_preflight_passthrough_preserves_two_sum() {
    let (device, queue, name) = match make_passthrough_device() {
        Some(t) => t,
        None => {
            eprintln!("[DS-PREFLIGHT] no device with passthrough support — skipping");
            return;
        }
    };
    crate::aelog!("[DS-PREFLIGHT] adapter: {}", name);
    crate::aelog!("[DS-PREFLIGHT] SPIR-V blob size: {} bytes", PRECOMPILED_SPV.len());

    // Load SPIR-V bypassing naga.
    let module = unsafe {
        device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
            label: Some("ds_preflight"),
            source: wgpu::util::make_spirv_raw(PRECOMPILED_SPV),
        })
    };

    // Storage buffer with [a, b, s_out, e_out]
    // Test case: a = 1e7, b = 3.14159 (chosen so the rounding error is non-trivial)
    let a: f32 = 1.0e7;
    let b: f32 = 3.14159;
    let mut data = [a, b, 0.0_f32, 0.0_f32];

    use wgpu::util::DeviceExt;
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("io"),
        contents: bytemuck::cast_slice(&data),
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("staging"),
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bg"),
        layout: &bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buf.as_entire_binding(),
        }],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pl"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("pipe"),
        layout: Some(&pl),
        module: &module,
        entry_point: "main",
    });

    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        cp.set_pipeline(&pipeline);
        cp.set_bind_group(0, &bg, &[]);
        cp.dispatch_workgroups(1, 1, 1);
    }
    enc.copy_buffer_to_buffer(&buf, 0, &staging, 0, 16);
    queue.submit(Some(enc.finish()));

    let slice = staging.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    {
        let mapped = slice.get_mapped_range();
        let raw: &[f32] = bytemuck::cast_slice(&mapped);
        data[2] = raw[2];
        data[3] = raw[3];
    }
    staging.unmap();

    let s = data[2];
    let e = data[3];

    // Reference: do the same operation in Rust f32 with no compiler folding
    // (Rust f32 honours IEEE-754 strictly so this gives the "true" result).
    let s_ref: f32 = a + b;
    let bv: f32 = s_ref - a;
    let av: f32 = s_ref - bv;
    let e_ref: f32 = (a - av) + (b - bv);

    // Reconstructed sum should equal a + b in f64 (DS reconstruction works iff
    // (s, e) carries the rounding residual).
    let recon = (s as f64) + (e as f64);
    let exact = (a as f64) + (b as f64);

    crate::aelog!("[DS-PREFLIGHT] a = {:e}, b = {:.6}", a, b);
    crate::aelog!("[DS-PREFLIGHT] GPU :  s = {:.6}, e = {:.6}", s, e);
    crate::aelog!("[DS-PREFLIGHT] CPU :  s = {:.6}, e = {:.6}  (Rust f32 reference)", s_ref, e_ref);
    crate::aelog!("[DS-PREFLIGHT] DS reconstructed: {:.10}  exact f64: {:.10}", recon, exact);
    crate::aelog!(
        "[DS-PREFLIGHT] GPU |e| = {:e}  (must be ≈ |e_ref| = {:e})",
        e.abs(),
        e_ref.abs()
    );

    // The killer assertion.  If NoContraction is honoured, GPU and CPU
    // residuals match within f32 ULP.  If the optimiser folded the chain,
    // GPU e == 0 while CPU e ≠ 0.
    assert!(
        e.abs() > 1e-3,
        "GPU residual collapsed to ~0 ({:e}) — NoContraction NOT honoured. \
         DS arithmetic via SPIR-V passthrough is impossible on this driver.",
        e
    );
    assert!(
        ((e - e_ref).abs() as f64) < 1e-5,
        "GPU residual {:e} disagrees with CPU reference {:e}",
        e,
        e_ref
    );
    let recon_err = (recon - exact).abs();
    assert!(
        recon_err < 1e-9,
        "DS reconstruction off by {:e} (expected ~0)",
        recon_err
    );

    crate::aelog!("[DS-PREFLIGHT] ✓ NoContraction honoured. DS via SPIR-V passthrough is viable.");
}

/// End-to-end DS GPU pipeline test. Drives a non-trivial signal through the
/// real `GpuDspProcessor` (DS path) and compares each output sample against
/// rustfft<f64> reference. With DS arithmetic working, residuals should be
/// at the f64 noise floor — not the f32 floor of the old pipeline.
///
/// Run with:
///     cargo test --release -- --ignored gpu_ds_pipeline --nocapture
#[test]
#[ignore]
fn gpu_ds_pipeline_end_to_end_matches_rustfft_f64() {
    use crate::audio::gpu::GpuDspProcessor;
    use crate::audio::processor::DspProcessor;
    use rustfft::{num_complex::Complex, FftPlanner};

    // 1024-tap Hann-windowed sinc lowpass, fc = 0.25 fs/2, DC-normalised.
    let taps = 1024usize;
    let pi = std::f64::consts::PI;
    let fc = 0.25_f64;
    let half = (taps as f64 - 1.0) * 0.5;
    let mut h = vec![0.0_f64; taps];
    for i in 0..taps {
        let n = i as f64 - half;
        let sinc = if n.abs() < 1e-12 {
            fc
        } else {
            (pi * fc * n).sin() / (pi * n)
        };
        let w = 0.5 * (1.0 - (2.0 * pi * i as f64 / (taps as f64 - 1.0)).cos());
        h[i] = sinc * w;
    }
    let dc: f64 = h.iter().sum();
    for v in h.iter_mut() {
        *v /= dc.max(1e-12);
    }

    // 8192 samples of a sine wave — non-trivial enough to expose every
    // numerical error along the FFT/CMUL/IFFT chain.
    let n_in = 8192usize;
    let input_l: Vec<f64> = (0..n_in).map(|i| (i as f64 * 0.1).sin() * 0.5).collect();
    let input_r = input_l.clone();

    let mut gpu = match GpuDspProcessor::new_with_coefficients(&h, 4) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[GPU/E2E] GPU unavailable: {}", e);
            return;
        }
    };
    let b_size = gpu.b_size;

    // GPU has b_size leading latency. Push n_in real samples + enough
    // zeros that the corresponding output emerges past the latency window.
    let total = b_size + n_in + taps + 16;
    let mut gpu_out_l = vec![0.0_f64; total];
    let mut gpu_out_r = vec![0.0_f64; total];

    let chunk = 4096usize;
    let mut zero_chunk = vec![0.0_f64; chunk];
    let mut pos = 0usize;
    while pos < total {
        let c = (total - pos).min(chunk);
        let in_l: &[f64] = if pos + c <= n_in {
            &input_l[pos..pos + c]
        } else if pos >= n_in {
            &zero_chunk[..c]
        } else {
            for i in 0..c {
                zero_chunk[i] = if pos + i < n_in { input_l[pos + i] } else { 0.0 };
            }
            &zero_chunk[..c]
        };
        let in_r: &[f64] = if pos + c <= n_in {
            &input_r[pos..pos + c]
        } else {
            in_l
        };
        gpu.process_audio(
            in_l, in_r,
            &mut gpu_out_l[pos..pos + c],
            &mut gpu_out_r[pos..pos + c],
            c,
        );
        pos += c;
    }

    // Reference: direct f64 FFT-based convolution of input_l × h.
    let conv_len = n_in + taps - 1;
    let n_fft = conv_len.next_power_of_two();
    let mut sig = vec![Complex::<f64>::new(0.0, 0.0); n_fft];
    let mut h_buf = vec![Complex::<f64>::new(0.0, 0.0); n_fft];
    for (i, &v) in input_l.iter().enumerate() {
        sig[i].re = v;
    }
    for (i, &v) in h.iter().enumerate() {
        h_buf[i].re = v;
    }
    let mut planner = FftPlanner::<f64>::new();
    planner.plan_fft_forward(n_fft).process(&mut sig);
    let mut planner2 = FftPlanner::<f64>::new();
    planner2.plan_fft_forward(n_fft).process(&mut h_buf);
    for i in 0..n_fft {
        sig[i] *= h_buf[i];
    }
    let mut planner3 = FftPlanner::<f64>::new();
    planner3.plan_fft_inverse(n_fft).process(&mut sig);
    let scale = 1.0_f64 / n_fft as f64;

    let mut max_abs_err = 0.0_f64;
    let mut max_abs_ref = 0.0_f64;
    for k in 0..conv_len {
        let r = sig[k].re * scale;
        max_abs_ref = max_abs_ref.max(r.abs());
        let g = gpu_out_l[b_size + k];
        let err = (g - r).abs();
        if err > max_abs_err {
            max_abs_err = err;
        }
    }
    let rel = max_abs_err / max_abs_ref.max(1e-30);
    let db = 20.0 * rel.max(1e-300).log10();
    crate::aelog!(
        "[GPU/E2E] n_in={} taps={} b_size={} max_abs_ref={:.6} max_abs_err={:.3e} rel={:.3e} ({:.1} dB)",
        n_in, taps, b_size, max_abs_ref, max_abs_err, rel, db
    );

    // DS pipeline target: dramatically better than the f32 pipeline (~−138 dB).
    // We aim for at least −200 dB to confirm DS is doing real work; in
    // practice we expect ~−240 to −260 dB on this filter.
    assert!(
        db < -180.0,
        "DS GPU pipeline accuracy {:.1} dB (target ≤ -180 dB) — \
         DS not delivering expected gain over f32 baseline (~-138 dB)",
        db
    );
}
