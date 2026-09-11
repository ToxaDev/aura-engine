use super::processor::{GpuDspProcessor, OlaParams};
use std::time::Instant;

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

    /// f64 sample → DS pair (real component; imag = 0).  `dst` must hold 4 f32s.
    #[inline]
    fn pack_real_ds(v: f64, dst: &mut [f32]) {
        let re_hi = v as f32;
        let re_lo = (v - re_hi as f64) as f32;
        dst[0] = re_hi;
        dst[1] = re_lo;
        dst[2] = 0.0;
        dst[3] = 0.0;
    }

    /// DS pair → f64 (only the real component is meaningful after IFFT of a
    /// real-input convolution; imag is at the noise floor).
    #[inline]
    fn unpack_real_ds(re_hi: f32, re_lo: f32) -> f64 {
        (re_hi as f64) + (re_lo as f64)
    }

    pub(crate) fn process_ola_block(&mut self) {
        let t_block = Instant::now();

        // ── Build DS-encoded input: [save_buf | in_buf] ──
        // Each complex slot = 4 × f32 (re_hi, re_lo, im_hi=0, im_lo=0).
        for i in 0..self.b_size {
            let base = i * 4;
            Self::pack_real_ds(self.save_buf_l[i], &mut self.complex_l[base..base + 4]);
            Self::pack_real_ds(self.save_buf_r[i], &mut self.complex_r[base..base + 4]);
        }
        for i in 0..self.b_size {
            let base = (i + self.b_size) * 4;
            Self::pack_real_ds(self.in_buf_l[i], &mut self.complex_l[base..base + 4]);
            Self::pack_real_ds(self.in_buf_r[i], &mut self.complex_r[base..base + 4]);
        }
        // Save current input for next block's overlap
        self.save_buf_l.copy_from_slice(&self.in_buf_l);
        self.save_buf_r.copy_from_slice(&self.in_buf_r);

        // Upload DS blobs
        self.queue
            .write_buffer(&self.work_l_buf, 0, bytemuck::cast_slice(&self.complex_l));
        self.queue
            .write_buffer(&self.work_r_buf, 0, bytemuck::cast_slice(&self.complex_r));

        let ola_params = OlaParams {
            n: self.n as u32,
            num_blocks: self.num_blocks as u32,
            cursor: self.cursor as u32,
            _pad: 0,
        };
        self.queue
            .write_buffer(&self.ola_params_buf, 0, bytemuck::bytes_of(&ola_params));

        // ── Encode the full DS pipeline in one command encoder ──
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let n32 = self.n as u32;
        let wg_n = (n32 + 255) / 256;

        // Forward FFT (DS)
        Self::encode_fft(&mut encoder, &self.fft_bg_work_l,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, false);
        Self::encode_fft(&mut encoder, &self.fft_bg_work_r,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, false);

        // Copy work → delay_line[cursor]  (DS = 16 bytes per complex)
        let delay_offset = (self.cursor * self.n * 16) as u64;
        let n_bytes_ds = (self.n * 16) as u64;
        encoder.copy_buffer_to_buffer(
            &self.work_l_buf, 0, &self.delay_l_buf, delay_offset, n_bytes_ds);
        encoder.copy_buffer_to_buffer(
            &self.work_r_buf, 0, &self.delay_r_buf, delay_offset, n_bytes_ds);

        // Complex DS multiply-accumulate
        for bg in [&self.ola_bg_l, &self.ola_bg_r] {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            cp.set_pipeline(&self.cmul_accum_pipeline);
            cp.set_bind_group(0, bg, &[]);
            cp.dispatch_workgroups(wg_n, 1, 1);
        }

        // Inverse FFT (DS)
        Self::encode_fft(&mut encoder, &self.fft_bg_accum_l,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, true);
        Self::encode_fft(&mut encoder, &self.fft_bg_accum_r,
            &self.bit_reverse_pipeline, &self.fft_pass_pipeline,
            n32, self.log2_n, self.align, true);

        // Copy second half (valid output) of accum to staging
        let half_offset = (self.b_size * 16) as u64;
        let output_bytes = (self.b_size * 16) as u64;
        encoder.copy_buffer_to_buffer(
            &self.accum_l_buf, half_offset, &self.staging_buf, 0, output_bytes);
        encoder.copy_buffer_to_buffer(
            &self.accum_r_buf, half_offset, &self.staging_buf, output_bytes, output_bytes);

        self.queue.submit(Some(encoder.finish()));

        // ── Read back DS pairs and reconstruct f64 ──
        let slice = self.staging_buf.slice(0..(output_bytes * 2));
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
            let scale = 1.0_f64 / self.n as f64;
            let half_words = self.b_size * 4; // offset to R-channel block

            for i in 0..self.b_size {
                let l_re_hi = data[i * 4];
                let l_re_lo = data[i * 4 + 1];
                let r_re_hi = data[half_words + i * 4];
                let r_re_lo = data[half_words + i * 4 + 1];
                self.out_buf_l[i] = Self::unpack_real_ds(l_re_hi, l_re_lo) * scale;
                self.out_buf_r[i] = Self::unpack_real_ds(r_re_hi, r_re_lo) * scale;
            }
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
                "[GPU/DS] OLA block #{}: {:.2}ms (avg {:.2}ms) | {} blocks × {} = {:.1}MB",
                self.block_count,
                block_us as f64 / 1000.0,
                avg_ms,
                self.num_blocks, self.n,
                (self.num_blocks * self.n * 16 * 2) as f64 / 1_048_576.0
            );
        }
    }
}
