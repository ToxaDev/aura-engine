//! `GpuPolyCtx` — the player's own wgpu device + OLA pipeline.
//!
//! The player creates its own wgpu Instance → adapter → device + queue,
//! independent from the engine's converter.  This means:
//! - `poll(Maintain::Wait)` waits only for the player's own work, not for
//!   the converter's OLA submissions.
//! - The player's `on_uncaptured_error` handler writes only to the player's
//!   own flag; the converter's `LAST_DEVICE_ERROR` is not disturbed.
//! - DXGI memory queries use the player's own adapter vendor/device ids.
//!
//! Created lazily on first call; the result (success or permanent failure)
//! is cached for the process lifetime.

use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pollster::block_on;

use super::bank_cache::{BankKey, GpuBankCache};
use super::{GPU_BINS, partitions};

// Pre-compiled SPIR-V for the player CMUL-ACCUM kernel.
const SPV_POLY_OLA: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gpu_poly_ola.spv"));

/// Discrete GPU only.  Set to `true` to allow integrated GPUs (off by default
/// — integrated adapters cannot run the full DS path at real-time speed).
const ALLOW_INTEGRATED: bool = false;

/// GPU context for the player's streaming convolver.
///
/// Created at most once per process via `try_build`.  Shared across all
/// `GpuPolyStream` instances for the lifetime of the player.
pub struct GpuPolyCtx {
    pub device:  Arc<wgpu::Device>,
    pub queue:   Arc<wgpu::Queue>,
    /// The OLA CMUL-ACCUM compute pipeline (gpu_poly_ola.spv).
    pub ola_pipeline: Arc<wgpu::ComputePipeline>,
    /// Bind group layout for the OLA kernel.
    pub ola_bind_group_layout: Arc<wgpu::BindGroupLayout>,
    pub bank_cache: Mutex<GpuBankCache>,
    /// Set by the player's own `on_uncaptured_error` handler, or when
    /// `watchdog_timeout_count` reaches the threshold.  Checked at the start of
    /// every `GpuPolyStream::try_new()` and `read()` call.
    pub player_device_error: Arc<AtomicBool>,
    /// Number of watchdog timeouts seen across all streams in this process.
    /// A single timeout fails only that stream; ≥ 2 disables the GPU process-wide.
    pub watchdog_timeout_count: AtomicUsize,
    /// PCI vendor/device ids for DXGI memory queries.
    pub vendor_id: u32,
    pub device_id: u32,
    // Kept for future device-type-specific scheduling or diagnostics.
    #[allow(dead_code)]
    pub device_type: wgpu::DeviceType,
}

static PLAYER_GPU_CTX: OnceLock<Option<Arc<GpuPolyCtx>>> = OnceLock::new();

impl GpuPolyCtx {
    /// Returns the shared player GPU context, or `None` if unavailable.
    /// Result is cached for the process lifetime; a failure is logged once.
    pub fn try_build() -> Option<Arc<GpuPolyCtx>> {
        PLAYER_GPU_CTX
            .get_or_init(|| Self::build_inner().map(Arc::new))
            .clone()
    }

    fn build_inner() -> Option<GpuPolyCtx> {
        if std::env::var_os("AURA_NO_GPU").is_some() {
            crate::aelog!("[player/gpu] disabled by AURA_NO_GPU");
            return None;
        }

        // Player-owned Vulkan instance — SPIRV_SHADER_PASSTHROUGH is only
        // available on the Vulkan backend in wgpu 0.19.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;

        let info = adapter.get_info();

        // Discrete GPU only (D4/D9).
        if !ALLOW_INTEGRATED && info.device_type != wgpu::DeviceType::DiscreteGpu {
            crate::aelog!(
                "[player/gpu] adapter '{}' is {:?}, not discrete — CPU path",
                info.name, info.device_type
            );
            return None;
        }

        // Require SPIRV_SHADER_PASSTHROUGH for the DS-precision OLA kernel.
        let want = wgpu::Features::SPIRV_SHADER_PASSTHROUGH;
        if !adapter.features().contains(want) {
            crate::aelog!(
                "[player/gpu] adapter '{}' lacks SPIRV_SHADER_PASSTHROUGH — DS path unavailable",
                info.name
            );
            return None;
        }

        // required_limits = adapter.limits() so h_freq buffers (up to ~480 MB)
        // exceed wgpu's default max_buffer_size (256 MB).
        let (device, queue) = match block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("AuraEngine Player GPU"),
                required_features: want,
                required_limits: adapter.limits(),
            },
            None,
        )) {
            Ok(pair) => pair,
            Err(e) => {
                crate::aelog!("[player/gpu] device request failed: {}", e);
                return None;
            }
        };

        // Install the player's own uncaptured-error handler.  This is the ONLY
        // call to on_uncaptured_error on this device; the engine's device is
        // not touched.
        //
        // The handler fires on wgpu's internal error thread.  `aelog!` acquires
        // OUT_LOCK and calls println!, which works from any thread but is not
        // captured by the per-test stdout hook.  We also write to stderr so the
        // message is always visible in test output.
        let player_device_error = Arc::new(AtomicBool::new(false));
        {
            let flag = Arc::clone(&player_device_error);
            device.on_uncaptured_error(Box::new(move |e| {
                // stderr is not suppressed by the test harness.
                eprintln!("[player/gpu] UNCAPTURED GPU ERROR (real device loss): {}", e);
                crate::aelog!("[player/gpu] uncaptured device error (real device loss): {}", e);
                flag.store(true, Ordering::Release);
            }));
        }

        // OLA bind group layout.
        let ola_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("player/gpu ola bgl"),
            entries: &[
                // binding=0: uniform PolyParams (16 bytes)
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
                // binding=1: h_freq (RO storage)
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
                // binding=2: delay/FDL (RO storage)
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
                // binding=3: accum (RW storage)
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

        // Build OLA compute pipeline from pre-compiled SPIR-V.
        let ola_shader = unsafe {
            device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                label: Some("player/gpu gpu_poly_ola"),
                source: wgpu::util::make_spirv_raw(SPV_POLY_OLA),
            })
        };

        let ola_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("player/gpu ola layout"),
            bind_group_layouts: &[&ola_bgl],
            push_constant_ranges: &[],
        });

        let ola_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("player/gpu ola pipeline"),
            layout: Some(&ola_pipeline_layout),
            module: &ola_shader,
            entry_point: "main",
        });

        crate::aelog!(
            "[player/gpu] context ready on '{}' ({:?}, vendor={:#x} device={:#x})",
            info.name, info.device_type, info.vendor, info.device
        );

        Some(GpuPolyCtx {
            device: Arc::new(device),
            queue:  Arc::new(queue),
            ola_pipeline: Arc::new(ola_pipeline),
            ola_bind_group_layout: Arc::new(ola_bgl),
            bank_cache: Mutex::new(GpuBankCache::new()),
            player_device_error,
            watchdog_timeout_count: AtomicUsize::new(0),
            vendor_id: info.vendor,
            device_id: info.device,
            device_type: info.device_type,
        })
    }

    // ── VRAM and limits helpers ──────────────────────────────────────────────

    /// Returns true when the device limits allow a stream with `taps` taps
    /// at polyphase factor `l`.
    ///
    /// Checks:
    /// - h_freq total buffer ≤ max_buffer_size (up to ~480 MB for 30M/FS8)
    /// - per-branch h_freq binding ≤ max_storage_buffer_binding_size
    ///   (~60 MB at L=8, ~120 MB at L=4 for 30M)
    /// - FDL buffer per channel ≤ max_buffer_size (same size as per-branch h_freq)
    /// - FDL binding ≤ max_storage_buffer_binding_size (entire FDL buffer)
    pub fn fits(&self, taps: usize, l: usize) -> bool {
        let sub_taps = (taps + l - 1) / l;
        let p = partitions(sub_taps);
        let h_branch_bytes = (p * GPU_BINS * 16) as u64;
        let h_total_bytes  = (l as u64) * h_branch_bytes;
        let fdl_bytes      = h_branch_bytes; // P × GPU_BINS × 16 per channel

        let lim = self.device.limits();
        h_total_bytes  <= lim.max_buffer_size
            && h_branch_bytes <= lim.max_storage_buffer_binding_size as u64
            && fdl_bytes      <= lim.max_buffer_size
            && fdl_bytes      <= lim.max_storage_buffer_binding_size as u64
    }

    /// Query free VRAM via DXGI using the player's own adapter ids.
    /// Returns 0 if the query fails.
    pub fn free_vram_bytes(&self) -> u64 {
        crate::audio::gpu::dxgi_memory::query(self.vendor_id, self.device_id)
            .map(|m| m.free())
            .unwrap_or(0)
    }

    // ── Bank cache management ────────────────────────────────────────────────

    /// Drop all cached filter banks, freeing their GPU buffers.
    /// Called on player stop and when a conversion starts.
    pub fn clear_bank_cache(&self) {
        self.bank_cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Keep only the filter banks whose keys appear in `keep`; evict the rest.
    /// Selective eviction for track changes where some filter banks are reused.
    #[allow(dead_code)]
    pub fn retain_bank_keys(&self, keep: &[BankKey]) {
        self.bank_cache.lock().unwrap_or_else(|p| p.into_inner()).retain_keys(keep);
    }
}
