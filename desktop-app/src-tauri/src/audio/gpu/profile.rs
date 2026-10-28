//! Where the time of one GPU OLA block goes, stage by stage.
//!
//! Each stage of `process_ola_block` is submitted on its own and waited for,
//! so a stage's wall clock is its GPU time plus one submit and one wait. That
//! overhead is measured on an empty submit and subtracted. The block as the
//! converter runs it — one submit for everything — is timed beside the stages,
//! and so is the sample loop of `process_audio` that feeds it.
//!
//! Needs the GPU and a minute or so, so it is ignored:
//!
//!     cargo test --profile fast --bins -- --ignored ola_block_profile --nocapture
//!
//! `AURA_PROFILE_TAPS` (comma-separated) replaces the default shapes.

#![cfg(test)]

use super::processor::GpuDspProcessor;
use crate::audio::processor::DspProcessor;
use std::time::Instant;

/// Sub-filter lengths the converter actually builds at 352.8 kHz: 1M, 10M and
/// 30M ×8 (44.1 kHz sources), 30M ×2 (176.4 kHz) and the whole 30M filter.
const DEFAULT_TAPS: [usize; 5] = [125_000, 1_250_000, 3_750_000, 15_000_000, 30_000_000];

const ROUNDS: usize = 6;

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// Submit what `f` encodes and wait for the device to finish it.
fn run(p: &GpuDspProcessor, f: impl FnOnce(&mut wgpu::CommandEncoder)) -> f64 {
    let t = Instant::now();
    let mut enc = p
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    f(&mut enc);
    p.queue.submit(Some(enc.finish()));
    p.device.poll(wgpu::Maintain::Wait);
    ms_since(t)
}

fn encode_bit_reverse(p: &GpuDspProcessor, enc: &mut wgpu::CommandEncoder, bg: &wgpu::BindGroup, inverse: bool) {
    let base = if inverse { (1 + p.log2_n as usize) * p.align } else { 0 };
    let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });
    cp.set_pipeline(&p.bit_reverse_pipeline);
    cp.set_bind_group(0, bg, &[base as u32]);
    cp.dispatch_workgroups((p.n as u32 + 255) / 256, 1, 1);
}

fn encode_passes(p: &GpuDspProcessor, enc: &mut wgpu::CommandEncoder, bg: &wgpu::BindGroup, inverse: bool) {
    let base = if inverse { (1 + p.log2_n as usize) * p.align } else { 0 };
    for pass in 0..p.log2_n as usize {
        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        cp.set_pipeline(&p.fft_pass_pipeline);
        cp.set_bind_group(0, bg, &[(base + (1 + pass) * p.align) as u32]);
        cp.dispatch_workgroups((p.n as u32 / 2 + 255) / 256, 1, 1);
    }
}

const STAGES: [&str; 10] = [
    "pack (CPU)",
    "upload",
    "fwd FFT bit-reverse",
    "fwd FFT passes",
    "split",
    "CMAC + join",
    "inv FFT bit-reverse",
    "inv FFT passes",
    "readback",
    "unpack (CPU)",
];

/// One block, stage by stage, as `process_ola_block` does it.
fn staged_block(p: &mut GpuDspProcessor, overhead: f64) -> [f64; 10] {
    let mut t = [0.0f64; 10];
    let b = p.b_size;

    let t0 = Instant::now();
    GpuDspProcessor::pack_block(&mut p.upload, &p.in_buf_l, &p.in_buf_r);
    t[0] = ms_since(t0);

    // The new half up, and the window put together on the card.
    let t1 = Instant::now();
    p.queue.write_buffer(&p.work_buf, (b * 16) as u64, bytemuck::cast_slice(&p.upload));
    p.write_ola_params();
    let mut enc = p
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    p.encode_window(&mut enc);
    p.queue.submit(Some(enc.finish()));
    p.device.poll(wgpu::Maintain::Wait);
    t[1] = ms_since(t1);

    t[2] = run(p, |e| encode_bit_reverse(p, e, &p.fft_bg_work, false)) - overhead;
    t[3] = run(p, |e| encode_passes(p, e, &p.fft_bg_work, false)) - overhead;
    t[4] = run(p, |e| p.encode_split(e)) - overhead;
    t[5] = run(p, |e| p.encode_cmac(e)) - overhead;
    t[6] = run(p, |e| encode_bit_reverse(p, e, &p.fft_bg_accum, true)) - overhead;
    t[7] = run(p, |e| encode_passes(p, e, &p.fft_bg_accum, true)) - overhead;

    let half = (b * 16) as u64;
    let t8 = Instant::now();
    let mut enc = p
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_buffer_to_buffer(&p.accum_buf, half, &p.staging_buf, 0, half);
    p.queue.submit(Some(enc.finish()));
    let slice = p.staging_buf.slice(0..half);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    p.device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    t[8] = ms_since(t8);

    let t9 = Instant::now();
    {
        let mapped = slice.get_mapped_range();
        let data: &[f32] = bytemuck::cast_slice(&mapped);
        let scale = 0.5 / p.n as f64;
        GpuDspProcessor::unpack_block(&mut p.out_buf_l, &mut p.out_buf_r, data, scale);
    }
    p.staging_buf.unmap();
    t[9] = ms_since(t9);
    t
}

fn profile_one(taps: usize) {
    // A decaying noise-like filter: the values do not change the timing, the
    // shape (N, K) does.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };
    let coeffs: Vec<f64> = (0..taps)
        .map(|i| rnd() * (-(i as f64) / taps as f64 * 8.0).exp() * 1e-3)
        .collect();
    // What this process holds on the card, by DXGI, around the build — with
    // whatever the previous shape left behind released first (wgpu frees a
    // dropped buffer when the device is next maintained).
    let held = || {
        let ctx = crate::audio::gpu::context::try_gpu_context().ok()?;
        ctx.device.poll(wgpu::Maintain::Wait);
        crate::audio::gpu::dxgi_memory::query(ctx.vendor_id, ctx.device_id)
            .ok()
            .map(|m| m.usage)
    };
    let before = held();
    let t_build = Instant::now();
    let mut p = match GpuDspProcessor::new_with_coefficients(&coeffs, 64) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[PROFILE] taps={} — no GPU processor: {}", taps, e);
            return;
        }
    };
    let build_ms = ms_since(t_build);
    p.device.poll(wgpu::Maintain::Wait);
    let dxgi_mb = match (before, held()) {
        (Some(a), Some(b)) => format!("{:.0} MB", (b as f64 - a as f64) / 1_048_576.0),
        _ => "n/a".to_string(),
    };
    drop(coeffs);
    let (b, n, k) = (p.b_size, p.n, p.num_blocks);

    let input_l: Vec<f64> = (0..b).map(|_| rnd() * 0.5).collect();
    let input_r: Vec<f64> = (0..b).map(|_| rnd() * 0.5).collect();
    p.in_buf_l.copy_from_slice(&input_l);
    p.in_buf_r.copy_from_slice(&input_r);

    // Warm-up: pipelines compiled, buffers resident.
    for _ in 0..2 {
        p.process_ola_block();
    }

    let overhead = {
        let mut best = f64::MAX;
        for _ in 0..8 {
            best = best.min(run(&p, |_| {}));
        }
        best
    };

    // The block as the converter runs it: one submit.
    let mut whole = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        p.process_ola_block();
        whole.push(ms_since(t));
    }

    // The same block through process_audio: the sample loop on top.
    let chunk = 32_768usize;
    let mut out_l = vec![0.0f64; chunk];
    let mut out_r = vec![0.0f64; chunk];
    let mut fed = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let mut pos = 0;
        while pos < b {
            let c = chunk.min(b - pos);
            p.process_audio(
                &input_l[pos..pos + c],
                &input_r[pos..pos + c],
                &mut out_l[..c],
                &mut out_r[..c],
                c,
            );
            pos += c;
        }
        fed.push(ms_since(t));
    }

    let mut stages = [0.0f64; 10];
    for _ in 0..ROUNDS {
        let t = staged_block(&mut p, overhead);
        for i in 0..10 {
            stages[i] += t[i] / ROUNDS as f64;
        }
    }

    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let whole_ms = median(&mut whole);
    let fed_ms = median(&mut fed);
    let sum: f64 = stages.iter().map(|v| v.max(0.0)).sum();
    let mb = crate::audio::gpu::vram_admission::processor_bytes(taps) as f64 / 1_048_576.0;

    eprintln!(
        "[PROFILE] taps={} b={} N={} K={} | VRAM {:.0} MB by formula, {} by DXGI | build {:.0} ms | block {:.2} ms \
         (one submit), {:.2} ms through process_audio | submit+wait overhead {:.3} ms",
        taps, b, n, k, mb, dxgi_mb, build_ms, whole_ms, fed_ms, overhead
    );
    for (i, name) in STAGES.iter().enumerate() {
        eprintln!(
            "[PROFILE]   {:<22} {:>8.2} ms {:>5.1}%",
            name,
            stages[i],
            100.0 * stages[i].max(0.0) / sum
        );
    }
    let fft = stages[2] + stages[3] + stages[6] + stages[7];
    let transfer = stages[1] + stages[8];
    let cpu = stages[0] + stages[9];
    let mac = stages[4] + stages[5];
    eprintln!(
        "[PROFILE]   = FFT {:.1}% | split + CMAC {:.1}% | upload+readback {:.1}% | CPU pack/unpack {:.1}% \
         (staged sum {:.2} ms; one submit {:.2} ms)",
        100.0 * fft / sum,
        100.0 * mac / sum,
        100.0 * transfer / sum,
        100.0 * cpu / sum,
        sum,
        whole_ms
    );
    // One channel's forward transform, pass by pass: where inside the FFT
    // the time goes (the bit reversal and the strided middle passes).
    let mut per_pass = Vec::with_capacity(p.log2_n as usize + 1);
    per_pass.push(run(&p, |e| encode_bit_reverse(&p, e, &p.fft_bg_work, false)) - overhead);
    for pass in 0..p.log2_n as usize {
        per_pass.push(
            run(&p, |e| {
                let mut cp = e.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                cp.set_pipeline(&p.fft_pass_pipeline);
                cp.set_bind_group(0, &p.fft_bg_work, &[((1 + pass) * p.align) as u32]);
                cp.dispatch_workgroups((p.n as u32 / 2 + 255) / 256, 1, 1);
            }) - overhead,
        );
    }
    eprintln!(
        "[PROFILE]   one FFT, bit-reverse then passes (ms): {}",
        per_pass.iter().map(|v| format!("{:.2}", v)).collect::<Vec<_>>().join(" ")
    );
    // Effective bandwidth of the FFT passes: each pass reads and writes N
    // complex DS values (16 B); one transform each way carries both channels.
    let pass_bytes = 2.0 * (n as f64) * 16.0 * p.log2_n as f64;
    eprintln!(
        "[PROFILE]   FFT passes {:.0} GB/s, CMAC reads {:.0} GB/s",
        pass_bytes * 2.0 / ((stages[3] + stages[7]) * 1e-3) / 1e9,
        (3.0 * k as f64 * p.stride as f64 * 16.0) / (stages[5] * 1e-3) / 1e9
    );
}

/// Ways of getting one block of samples onto the card, timed against each
/// other at the 4M-point size: `write_buffer` (a fresh staging buffer and a
/// copy per call) against a buffer mapped for writing that stays allocated,
/// with the f64 → DS packing done into a vector first or straight into the
/// mapped memory, on one thread or on all of them.
#[test]
#[ignore]
fn upload_paths_profile() {
    use rayon::prelude::*;
    let ctx = match crate::audio::gpu::context::try_gpu_context() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[UPLOAD] no GPU: {}", e);
            return;
        }
    };
    let (device, queue) = (&ctx.device, &ctx.queue);
    let b = 2_097_152usize;
    let l: Vec<f64> = (0..b).map(|i| ((i as f64) * 0.001).sin() * 0.5).collect();
    let r: Vec<f64> = (0..b).map(|i| ((i as f64) * 0.0013).cos() * 0.5).collect();
    let pack_into = |dst: &mut [f32], l: &[f64], r: &[f64]| {
        for i in 0..l.len() {
            let (lh, rh) = (l[i] as f32, r[i] as f32);
            dst[i * 4] = lh;
            dst[i * 4 + 1] = (l[i] - lh as f64) as f32;
            dst[i * 4 + 2] = rh;
            dst[i * 4 + 3] = (r[i] - rh as f64) as f32;
        }
    };
    let pack_par = |dst: &mut [f32], l: &[f64], r: &[f64]| {
        const C: usize = 16_384;
        dst.par_chunks_mut(C * 4).enumerate().for_each(|(c, d)| {
            let s = c * C;
            let e = (s + C).min(l.len());
            pack_into(d, &l[s..e], &r[s..e]);
        });
    };
    let bytes = (b * 16) as u64;
    let dst = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dst"),
        size: bytes * 2,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let up = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("upload"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mut v = vec![0.0f32; b * 4];
    let wait = || {
        device.poll(wgpu::Maintain::Wait);
    };
    let map_write = |f: &dyn Fn(&mut [f32])| {
        let slice = up.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Write, move |res| {
            let _ = tx.send(res);
        });
        wait();
        rx.recv().unwrap().unwrap();
        {
            let mut m = slice.get_mapped_range_mut();
            f(bytemuck::cast_slice_mut(&mut m[..]));
        }
        up.unmap();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_buffer_to_buffer(&up, 0, &dst, bytes, bytes);
        queue.submit(Some(enc.finish()));
        wait();
    };

    let mut rows: Vec<(&str, Vec<f64>)> = Vec::new();
    for round in 0..7 {
        let mut t = Vec::new();
        let s = Instant::now();
        pack_into(&mut v, &l, &r);
        t.push(ms_since(s));
        let s = Instant::now();
        pack_par(&mut v, &l, &r);
        t.push(ms_since(s));
        let s = Instant::now();
        queue.write_buffer(&dst, 0, bytemuck::cast_slice(&v));
        queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
        wait();
        t.push(ms_since(s));
        let s = Instant::now();
        queue.write_buffer(&dst, 0, bytemuck::cast_slice(&v));
        queue.write_buffer(&dst, bytes, bytemuck::cast_slice(&v));
        queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
        wait();
        t.push(ms_since(s));
        let s = Instant::now();
        map_write(&|m| m.copy_from_slice(&v));
        t.push(ms_since(s));
        let s = Instant::now();
        map_write(&|m| pack_into(m, &l, &r));
        t.push(ms_since(s));
        let s = Instant::now();
        map_write(&|m| pack_par(m, &l, &r));
        t.push(ms_since(s));
        if round == 0 {
            continue; // warm-up
        }
        if rows.is_empty() {
            for name in [
                "pack 2M frames, one thread",
                "pack 2M frames, rayon",
                "write_buffer 32 MB",
                "write_buffer 64 MB",
                "mapped 32 MB: memcpy + copy",
                "mapped 32 MB: pack in place",
                "mapped 32 MB: pack in place, rayon",
            ] {
                rows.push((name, Vec::new()));
            }
        }
        for (i, ms) in t.into_iter().enumerate() {
            rows[i].1.push(ms);
        }
    }
    for (name, mut v) in rows {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!("[UPLOAD] {:<36} median {:>7.2} ms  min {:>7.2} ms", name, v[v.len() / 2], v[0]);
    }
}

#[test]
#[ignore]
fn ola_block_profile() {
    let shapes: Vec<usize> = std::env::var("AURA_PROFILE_TAPS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| DEFAULT_TAPS.to_vec());
    for taps in shapes {
        profile_one(taps);
    }
}
