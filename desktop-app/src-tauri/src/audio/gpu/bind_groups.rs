use super::processor::{GpuDspProcessor, FftParams};

impl GpuDspProcessor {
    pub(crate) fn write_fft_params(buf: &mut [u8], entry: usize, align: usize, params: FftParams) {
        let offset = entry * align;
        let bytes = bytemuck::bytes_of(&params);
        buf[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Bind group for the DS FFT shader.
    /// Layout (matches gpu_fft.comp.glsl):
    ///   binding=0  storage buffer  → data array (vec4<f32>, DS)
    ///   binding=1  uniform buffer  → FftParams (16 bytes, dynamic offset)
    ///   binding=2  storage buffer  → twiddle table (vec4<f32>, DS, read-only)
    pub(crate) fn create_fft_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        data_buf: &wgpu::Buffer,
        params_buf: &wgpu::Buffer,
        twiddle_buf: &wgpu::Buffer,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: data_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: params_buf,
                        offset: 0,
                        size: wgpu::BufferSize::new(16),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: twiddle_buf.as_entire_binding(),
                },
            ],
        })
    }

    /// Bind group for the DS OLA cmul-accum shader.
    /// Layout (matches gpu_ola.comp.glsl):
    ///   binding=0  uniform        → OlaParams (16 bytes)
    ///   binding=1  storage RO     → h_freq (vec4<f32>, DS)
    ///   binding=2  storage RO     → delay  (vec4<f32>, DS)
    ///   binding=3  storage RW     → accum  (vec4<f32>, DS)
    pub(crate) fn create_ola_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        params_buf: &wgpu::Buffer,
        h_freq_buf: &wgpu::Buffer,
        delay_buf: &wgpu::Buffer,
        accum_buf: &wgpu::Buffer,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: h_freq_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: delay_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: accum_buf.as_entire_binding(),
                },
            ],
        })
    }
}
