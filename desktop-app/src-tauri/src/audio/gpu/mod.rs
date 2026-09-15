pub mod context;
pub mod processor;
pub mod setup;
pub mod bind_groups;
pub mod wola;
pub mod fft_math;
pub mod filter_cache;
pub mod vram_admission;

#[cfg(test)]
pub mod ds_preflight;

pub use processor::*;

/// Build the convolver for one filter, preferring the GPU and falling back to
/// the CPU reference path when the device will not give us the memory.
///
/// Centralised because the policy has to be identical everywhere. Each of the
/// four construction sites used to propagate the failure with `?`, so a card
/// that merely ran out of room failed the conversion — and before the device
/// error was caught at all, took the process down. Falling back costs time and
/// nothing else: the CPU path is the f64 reference the GPU path is verified
/// against.
///
/// Cancellation is not a fallback. It propagates.
pub fn build_convolver(
    coeffs: &[f64],
    precision: u32,
    id: Option<&filter_cache::FilterId>,
    use_gpu: bool,
) -> Result<Box<dyn crate::audio::processor::DspProcessor + Send>, String> {
    let cpu = || -> Box<dyn crate::audio::processor::DspProcessor + Send> {
        Box::new(crate::audio::dsp_core::CpuDspProcessor::new_with_coefficients(coeffs))
    };

    if !use_gpu {
        return Ok(cpu());
    }

    // Asking a device that has already refused this much to refuse it again,
    // once per file for the rest of the batch, buys nothing but the wait.
    let want = vram_admission::processor_bytes(coeffs.len());
    if vram_admission::known_to_fail(want) {
        crate::aelog!(
            "[GPU] {} MB convolver is at or above the ceiling this device already refused → CPU convolver for this filter",
            want / 1_048_576
        );
        return Ok(cpu());
    }

    match GpuDspProcessor::new_with_coefficients_keyed(coeffs, precision, id) {
        Ok(p) => Ok(Box::new(p)),
        Err(e) if e == "Cancelled" => Err(e),
        Err(e) => {
            crate::aelog!("[GPU] {} → CPU convolver for this filter", e);
            Ok(cpu())
        }
    }
}
