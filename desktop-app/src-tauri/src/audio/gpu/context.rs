use pollster::block_on;
use std::sync::{Arc, OnceLock};

pub struct GpuContext {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub align: usize,
    pub adapter_name: String,
    pub backend_name: String,
    /// True when the device was created with SPIRV_SHADER_PASSTHROUGH —
    /// the DS-precision FFT pipeline can only be used when this is true.
    /// On adapters that don't expose the feature, we silently fall back to
    /// the WGSL f32 path (same behaviour as before).
    pub spirv_passthrough: bool,
}

static GPU_CTX: OnceLock<GpuContext> = OnceLock::new();

pub fn get_gpu_context() -> GpuContext {
    GPU_CTX
        .get_or_init(|| {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN | wgpu::Backends::DX12,
                ..Default::default()
            });
            let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            }))
            .expect("No GPU adapter found");

            let adapter_name = adapter.get_info().name.clone();
            let backend_name = format!("{:?}", adapter.get_info().backend);

            let limits = adapter.limits();

            // Try to enable SPIRV_SHADER_PASSTHROUGH so we can load
            // pre-compiled DS-precision SPIR-V shaders that bypass naga
            // (and therefore preserve `precise` / NoContraction decorations).
            // If the adapter doesn't support it, we ask for an empty
            // feature set instead — the runtime then falls back to the
            // WGSL f32 path automatically.
            let supported = adapter.features();
            let want = wgpu::Features::SPIRV_SHADER_PASSTHROUGH;
            let spirv_passthrough = supported.contains(want);
            let required_features = if spirv_passthrough { want } else { wgpu::Features::empty() };

            let (device, queue) = block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("AuraEngine Global GPU"),
                    required_features,
                    required_limits: limits,
                },
                None,
            ))
            .expect("Failed to create GPU device");

            let align = device.limits().min_uniform_buffer_offset_alignment as usize;

            crate::aelog!(
                "[GPU/CTX] adapter '{}' on {} — SPIR-V passthrough: {}",
                adapter_name,
                backend_name,
                if spirv_passthrough { "ENABLED (DS path available)" } else { "unavailable (WGSL f32 path only)" }
            );

            GpuContext {
                device: Arc::new(device),
                queue: Arc::new(queue),
                align,
                adapter_name,
                backend_name,
                spirv_passthrough,
            }
        })
        .clone()
}

/// Estimate the maximum number of GPU workers that can run simultaneously
/// without exhausting GPU memory for filters of `target_taps`.
///
/// Per worker we allocate ~3.5 GB on a 30M-tap filter (h_freq + 2× delay,
/// each ≈ num_partitions × N × 16 bytes for DS layout). A second worker
/// doubles that. If `2 × per_worker > 0.7 × max_buffer_size`, we cap at 1.
///
/// We use `adapter.limits().max_storage_buffer_binding_size` as a proxy
/// for VRAM headroom — wgpu doesn't expose physical VRAM, but this limit
/// is what the driver actually allows for a single storage binding (4 GB
/// on desktop NVIDIA/AMD, much less on integrated GPUs).
pub fn recommended_gpu_workers(target_taps: usize) -> usize {
    let ctx = get_gpu_context();
    let max_storage = ctx.device.limits().max_storage_buffer_binding_size as u64;

    // Use the canonical helper so this estimate stays in sync with the
    // actual allocator in setup.rs.
    let b_size = crate::audio::gpu::GpuDspProcessor::block_size(target_taps);
    let n = b_size * 2;
    let num_blocks = (target_taps + b_size - 1) / b_size;

    // Per-worker GPU memory: h_freq + delay_l + delay_r (each num_blocks × N × 16 bytes
    // in DS layout) + smaller scratch buffers. The big three dominate.
    let h_or_delay_bytes = (num_blocks * n * 16) as u64;
    let per_worker_bytes = h_or_delay_bytes * 3 + (n as u64 * 16 * 4); // h + 2×delay + work/accum

    // Use 70% of max binding as a safe ceiling for combined per-worker
    // demand (leaves headroom for staging buffers, command buffers, OS
    // overhead, and the Tauri webview's own GPU budget).
    let safe_budget = (max_storage as f64 * 0.7) as u64;

    let workers = if per_worker_bytes > safe_budget {
        crate::aelog!(
            "[GPU/CTX] taps={} needs ~{} MB VRAM/worker but max storage \
             binding is ~{} MB (×0.7 = {} MB safe) → forcing 1 GPU worker",
            target_taps,
            per_worker_bytes / 1_048_576,
            max_storage / 1_048_576,
            safe_budget / 1_048_576,
        );
        1
    } else if per_worker_bytes * 2 > safe_budget {
        crate::aelog!(
            "[GPU/CTX] taps={} ~{} MB/worker; cannot fit 2× workers in {} MB \
             safe budget → using 1 GPU worker",
            target_taps,
            per_worker_bytes / 1_048_576,
            safe_budget / 1_048_576,
        );
        1
    } else {
        2
    };

    workers
}

impl Clone for GpuContext {
    fn clone(&self) -> Self {
        GpuContext {
            device: Arc::clone(&self.device),
            queue: Arc::clone(&self.queue),
            align: self.align,
            adapter_name: self.adapter_name.clone(),
            backend_name: self.backend_name.clone(),
            spirv_passthrough: self.spirv_passthrough,
        }
    }
}
