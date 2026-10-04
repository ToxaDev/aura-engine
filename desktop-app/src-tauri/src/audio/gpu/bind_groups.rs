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

    /// Bind group for one of the two kernels of gpu_ola.comp.glsl: the
    /// OlaParams uniform at binding 0, then `buffers` at 1, 2, … in order.
    ///   split: spectrum Z (RO), delay_l, delay_r
    ///   cmac:  h_freq (RO), delay_l (RO), delay_r (RO), joined spectrum W
    pub(crate) fn create_ola_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        params_buf: &wgpu::Buffer,
        buffers: &[&wgpu::Buffer],
        label: &str,
    ) -> wgpu::BindGroup {
        let mut entries = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: params_buf.as_entire_binding(),
        }];
        for (i, b) in buffers.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry {
                binding: 1 + i as u32,
                resource: b.as_entire_binding(),
            });
        }
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &entries,
        })
    }

    /// Layout matching `create_ola_bind_group`: the uniform, then one
    /// storage buffer per entry of `read_only`.
    pub(crate) fn create_ola_bind_group_layout(
        device: &wgpu::Device,
        read_only: &[bool],
        label: &str,
    ) -> wgpu::BindGroupLayout {
        let mut entries = vec![wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }];
        for (i, &ro) in read_only.iter().enumerate() {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 1 + i as u32,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: ro },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &entries,
        })
    }
}
