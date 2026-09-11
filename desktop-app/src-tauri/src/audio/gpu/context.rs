use pollster::block_on;
use std::sync::{Arc, Mutex, OnceLock};

pub struct GpuContext {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub align: usize,
    pub adapter_name: String,
    pub backend_name: String,
    /// True when the device was created with SPIRV_SHADER_PASSTHROUGH —
    /// the DS-precision FFT pipeline can only be used when this is true.
    /// An adapter without it is not a GPU this engine can convolve on:
    /// `probe()` reports it as unavailable and the batch runs on the CPU.
    pub spirv_passthrough: bool,
}

/// The device, or the reason there is not one.
///
/// The failure is cached along with the success on purpose. A machine with no
/// Vulkan and no DX12 adapter will not grow one part-way through a batch, and
/// re-probing per file would stand up a `wgpu::Instance` each time — instances
/// created and dropped in quick succession have already cost this project a
/// driver-handle exhaustion bug.
static GPU_CTX: OnceLock<Result<GpuContext, String>> = OnceLock::new();

/// The last error the device raised through the uncaptured-error handler.
///
/// wgpu's default handler for these is a panic, and the one that brought this
/// project here — `Not enough memory left` out of `create_buffer` — is an
/// error a convolver can survive by running on the CPU instead. Installing a
/// handler turns it back into a value the constructor can return.
///
/// One slot for the whole process rather than one per thread: wgpu reports
/// through this handler from whatever thread hit the error, and a slot that
/// could miss one would hand an invalid buffer to a convolver. Two workers
/// allocating at once can therefore make each other fall back. That costs
/// wall clock; missing an error costs correctness.
static LAST_DEVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn error_slot() -> std::sync::MutexGuard<'static, Option<String>> {
    LAST_DEVICE_ERROR.lock().unwrap_or_else(|p| p.into_inner())
}

/// Forget any error already recorded, so what `take_device_error` returns
/// afterwards belongs to the caller's own allocations.
pub fn clear_device_error() {
    *error_slot() = None;
}

/// Take the recorded error, if the device raised one since the last clear.
pub fn take_device_error() -> Option<String> {
    error_slot().take()
}

/// Held across clear → allocate → poll → take while a convolver is being
/// built.
///
/// The error slot above is one per process, so two workers constructing
/// convolvers at the same time would race for it: the second one's
/// `clear_device_error` would wipe the first one's refusal before the first
/// had read it, and that worker would go on to use buffers the device never
/// gave it — a missed error, which is the one outcome the single slot exists
/// to prevent. Construction is not where the time goes; the convolution is.
///
/// Taken AFTER the VRAM reservation, never before. A thread waiting for
/// device budget must not be holding the gate a thread that already has
/// budget needs to finish and release it.
static ALLOC_GATE: Mutex<()> = Mutex::new(());

pub fn lock_allocations() -> std::sync::MutexGuard<'static, ()> {
    ALLOC_GATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The GPU device if this machine has one the engine can use.
///
/// This used to be `get_gpu_context()`, which ended in `.expect("No GPU
/// adapter found")`. Anything without a Vulkan or DX12 adapter — a driver
/// that failed to load, a virtual machine, a remote session — took the whole
/// process down, while the README promised the opposite: that the app falls
/// back to the CPU reference path on its own. Now the absence is a value the
/// caller can act on.
pub fn try_gpu_context() -> Result<GpuContext, String> {
    GPU_CTX.get_or_init(build_context).clone()
}

fn build_context() -> Result<GpuContext, String> {
    // Drives the no-GPU path on a machine that has a GPU. Same reason as
    // `AURA_SELFTEST_NO_BOX` in startup.rs: a fallback nobody can exercise is
    // a fallback nobody has tested, and the machine this was written on has a
    // working Vulkan adapter. Not documented anywhere a user would read.
    if std::env::var_os("AURA_NO_GPU").is_some() {
        return Err("disabled by AURA_NO_GPU".to_string());
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::DX12,
        ..Default::default()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }));
    let Some(adapter) = adapter else {
        return Err(
            "no Vulkan or DX12 adapter on this machine (missing or failed GPU driver, \
             a virtual machine, or a remote session)"
                .to_string(),
        );
    };

    let adapter_name = adapter.get_info().name.clone();
    let backend_name = format!("{:?}", adapter.get_info().backend);

    let limits = adapter.limits();

    // Try to enable SPIRV_SHADER_PASSTHROUGH so we can load pre-compiled
    // DS-precision SPIR-V shaders that bypass naga (and therefore preserve
    // `precise` / NoContraction decorations). If the adapter doesn't support
    // it, we ask for an empty feature set instead — the device is still
    // usable, but the DS pipeline in setup.rs is not, and `probe()` below
    // treats that as no GPU at all.
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
    .map_err(|e| format!("adapter '{}' would not give us a device: {}", adapter_name, e))?;

    // Without this, wgpu's default handler panics on any error the caller did
    // not capture — which is how a card that simply ran out of room took the
    // whole worker thread down. Record it instead; `setup.rs` reads the slot
    // after it allocates and reports a failure the caller can fall back from.
    device.on_uncaptured_error(Box::new(|e| {
        let msg = e.to_string();
        *error_slot() = Some(msg.clone());
        crate::aelog!("[GPU/CTX] device error: {}", msg);
    }));

    let align = device.limits().min_uniform_buffer_offset_alignment as usize;

    crate::aelog!(
        "[GPU/CTX] adapter '{}' on {} — SPIR-V passthrough: {}",
        adapter_name,
        backend_name,
        if spirv_passthrough { "ENABLED (DS path available)" } else { "unavailable (WGSL f32 path only)" }
    );

    Ok(GpuContext {
        device: Arc::new(device),
        queue: Arc::new(queue),
        align,
        adapter_name,
        backend_name,
        spirv_passthrough,
    })
}

/// Can this machine actually run the GPU convolution path?
///
/// Answers the two questions `setup.rs` would otherwise discover one at a
/// time, per file, after allocating: is there a device, and does it expose
/// the SPIR-V passthrough the DS pipeline is built on. The error is a
/// sentence meant to be shown to a person, not an error code.
pub fn probe() -> Result<(), String> {
    let ctx = try_gpu_context()?;
    if !ctx.spirv_passthrough {
        return Err(format!(
            "adapter '{}' does not expose SPIRV_SHADER_PASSTHROUGH, which the \
             double-single pipeline is built on",
            ctx.adapter_name
        ));
    }
    Ok(())
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
