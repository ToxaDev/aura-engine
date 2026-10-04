use super::processor::{GpuDspProcessor, OlaParams};
use rayon::prelude::*;
use std::time::Instant;

/// Samples per rayon task when a block is packed for the card or unpacked
/// from it.
const PAR_FRAMES: usize = 16_384;

/// What a block of output held besides its samples, for the convolver's
/// log: values that were not finite (written as zeros), the largest
/// magnitude, and frames with a channel past full scale.
#[derive(Clone, Copy, Default)]
pub(crate) struct BlockStats {
    pub(crate) nan: u64,
    pub(crate) clips: u64,
    pub(crate) max_abs: f64,
}

impl BlockStats {
    fn merge(a: Self, b: Self) -> Self {
        Self {
            nan: a.nan + b.nan,
            clips: a.clips + b.clips,
            max_abs: a.max_abs.max(b.max_abs),
        }
    }
}

impl GpuDspProcessor {
    pub(crate) fn encode_fft(
        encoder: &mut wgpu::CommandEncoder,
        bind_group: &wgpu::BindGroup,
        bit_reverse_pipeline: &wgpu::ComputePipeline,
        fft_pass_pipeline: &wgpu::ComputePipeline,
        n: u32,
        log2_n: u32,
        align: usize,
        inverse: bool,
    ) {
        let base = if inverse {
            (1 + log2_n as usize) * align
        } else {
            0
        };
        let wg_n = (n + 255) / 256;
        let wg_half = (n / 2 + 255) / 256;

        // Bit-reverse permutation
        {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            cp.set_pipeline(bit_reverse_pipeline);
            cp.set_bind_group(0, bind_group, &[base as u32]);
            cp.dispatch_workgroups(wg_n, 1, 1);
        }

        // log2(N) DS butterfly passes
        for pass in 0..log2_n as usize {
            let offset = base + (1 + pass) * align;
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            cp.set_pipeline(fft_pass_pipeline);
            cp.set_bind_group(0, bind_group, &[offset as u32]);
            cp.dispatch_workgroups(wg_half, 1, 1);
        }
    }

    /// One sample of each channel as the DS pair of L + i·R:
    /// (L_hi, L_lo, R_hi, R_lo). `dst` must hold 4 f32s.
    #[inline]
    fn pack_pair(l: f64, r: f64, dst: &mut [f32]) {
        let l_hi = l as f32;
        let r_hi = r as f32;
        dst[0] = l_hi;
        dst[1] = (l - l_hi as f64) as f32;
        dst[2] = r_hi;
        dst[3] = (r - r_hi as f64) as f32;
    }

    /// The new block of both channels into `upload`, one complex value per
    /// sample. The window's other half — the previous block — is already on
    /// the card.
    pub(crate) fn pack_block(upload: &mut [f32], in_l: &[f64], in_r: &[f64]) {
        upload
            .par_chunks_mut(PAR_FRAMES * 4)
            .enumerate()
            .for_each(|(c, dst)| {
                for (j, d) in dst.chunks_exact_mut(4).enumerate() {
                    let f = c * PAR_FRAMES + j;
                    Self::pack_pair(in_l[f], in_r[f], d);
                }
            });
    }

    /// The overlap-save window [previous | new] in `work`: the new block was
    /// written to its second half; the previous one comes from `prev`, which
    /// then takes the new block for the next window. In this order — the
    /// copies of one encoder run in sequence.
    pub(crate) fn encode_window(&self, encoder: &mut wgpu::CommandEncoder) {
        let half = (self.b_size * 16) as u64;
        encoder.copy_buffer_to_buffer(&self.prev_buf, 0, &self.work_buf, 0, half);
        encoder.copy_buffer_to_buffer(&self.work_buf, half, &self.prev_buf, 0, half);
    }

    /// The valid half of the inverse — 2N·(L + i·R) as DS pairs — into the
    /// two f64 output blocks. `scale` is 1/(2N), a power of two. A value
    /// that is not finite is written as zero and counted.
    pub(crate) fn unpack_block(
        out_l: &mut [f64],
        out_r: &mut [f64],
        data: &[f32],
        scale: f64,
    ) -> BlockStats {
        out_l
            .par_chunks_mut(PAR_FRAMES)
            .zip(out_r.par_chunks_mut(PAR_FRAMES))
            .enumerate()
            .map(|(c, (ol, or))| {
                let mut s = BlockStats::default();
                let base = c * PAR_FRAMES * 4;
                for j in 0..ol.len() {
                    let w = &data[base + j * 4..base + j * 4 + 4];
                    let mut l = (w[0] as f64 + w[1] as f64) * scale;
                    let mut r = (w[2] as f64 + w[3] as f64) * scale;
                    if !l.is_finite() {
                        s.nan += 1;
                        l = 0.0;
                    }
                    if !r.is_finite() {
                        s.nan += 1;
                        r = 0.0;
                    }
                    let (al, ar) = (l.abs(), r.abs());
                    s.max_abs = s.max_abs.max(al).max(ar);
                    if al > 1.0 || ar > 1.0 {
                        s.clips += 1;
                    }
                    ol[j] = l;
                    or[j] = r;
                }
                s
            })
            .reduce(BlockStats::default, BlockStats::merge)
    }

    /// The split: both channels' half spectra out of the window's spectrum,
    /// into the newest slot of the delay lines.
    pub(crate) fn encode_split(&self, encoder: &mut wgpu::CommandEncoder) {
        let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        cp.set_pipeline(&self.split_pipeline);
        cp.set_bind_group(0, &self.split_bg, &[]);
        cp.dispatch_workgroups((self.n as u32 / 2 + 1 + 255) / 256, 1, 1);
    }

    /// The multiply-accumulate over the K partitions, both channels, and
    /// the join into the spectrum the inverse FFT takes.
    pub(crate) fn encode_cmac(&self, encoder: &mut wgpu::CommandEncoder) {
        let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        cp.set_pipeline(&self.cmac_pipeline);
        cp.set_bind_group(0, &self.cmac_bg, &[]);
        cp.dispatch_workgroups((self.n as u32 / 2 + 1 + 255) / 256, 1, 1);
    }

    /// Everything the card does for one block, after the new half is up and
    /// before the readback is mapped.
    pub(crate) fn encode_block(&self, encoder: &mut wgpu::CommandEncoder) {
        let n32 = self.n as u32;
        self.encode_window(encoder);
        Self::encode_fft(encoder, &self.fft_bg_work,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, false);
        self.encode_split(encoder);
        self.encode_cmac(encoder);
        Self::encode_fft(encoder, &self.fft_bg_accum,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, true);
        // The second half of the inverse is the block's output.
        let half = (self.b_size * 16) as u64;
        encoder.copy_buffer_to_buffer(&self.accum_buf, half, &self.staging_buf, 0, half);
    }

    pub(crate) fn write_ola_params(&self) {
        let ola_params = OlaParams {
            n: self.n as u32,
            num_blocks: self.num_blocks as u32,
            cursor: self.cursor as u32,
            stride: self.stride as u32,
        };
        self.queue
            .write_buffer(&self.ola_params_buf, 0, bytemuck::bytes_of(&ola_params));
    }

    pub(crate) fn process_ola_block(&mut self) {
        let t_block = Instant::now();

        // ── The new block up as L + i·R, into the window's second half ──
        // (A queue write lands before the commands of the same submit.)
        Self::pack_block(&mut self.upload, &self.in_buf_l, &self.in_buf_r);
        self.queue.write_buffer(
            &self.work_buf,
            (self.b_size * 16) as u64,
            bytemuck::cast_slice(&self.upload),
        );
        self.write_ola_params();

        // ── The whole DS pipeline in one command encoder ──
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.encode_block(&mut encoder);
        self.queue.submit(Some(encoder.finish()));

        // ── Read back the DS pairs and reconstruct f64 ──
        let output_bytes = (self.b_size * 16) as u64;
        let slice = self.staging_buf.slice(0..output_bytes);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);

        // Graceful handling of GPU readback failure (DeviceLost, mapped
        // buffer error, second worker exhausted VRAM, etc.). Convert to a
        // silent zero-filled output block + bumped nan_count rather than
        // panicking the whole worker thread mid-file.
        let map_result = match rx.recv() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("[GPU/DS] readback channel disconnected; writing zeros");
                self.out_buf_l.fill(0.0);
                self.out_buf_r.fill(0.0);
                self.nan_count += self.b_size as u64;
                return;
            }
        };
        if let Err(e) = map_result {
            eprintln!(
                "[GPU/DS] map_async failed ({:?}); writing zeros for this OLA block",
                e
            );
            self.out_buf_l.fill(0.0);
            self.out_buf_r.fill(0.0);
            self.nan_count += self.b_size as u64;
            return;
        }
        {
            let mapped = slice.get_mapped_range();
            let data: &[f32] = bytemuck::cast_slice(&mapped);
            let scale = 0.5 / self.n as f64;
            let s = Self::unpack_block(&mut self.out_buf_l, &mut self.out_buf_r, data, scale);
            self.nan_count += s.nan;
            self.clip_count += s.clips;
            self.max_abs_val = self.max_abs_val.max(s.max_abs);
        }
        self.staging_buf.unmap();

        // Advance circular delay line cursor
        self.cursor = if self.cursor == 0 {
            self.num_blocks - 1
        } else {
            self.cursor - 1
        };

        let block_us = t_block.elapsed().as_micros() as u64;
        self.block_count += 1;
        self.total_gpu_time_us += block_us;

        if self.block_count <= 3 || self.block_count % 500 == 0 {
            let avg_ms = self.total_gpu_time_us as f64 / self.block_count as f64 / 1000.0;
            crate::aelog!(
                "[GPU/DS] OLA block #{}: {:.2}ms (avg {:.2}ms) | {} blocks × {} half-spectrum slots = {:.1}MB of delay lines",
                self.block_count,
                block_us as f64 / 1000.0,
                avg_ms,
                self.num_blocks, self.stride,
                (self.num_blocks * self.stride * 16 * 2) as f64 / 1_048_576.0
            );
        }
    }
}
