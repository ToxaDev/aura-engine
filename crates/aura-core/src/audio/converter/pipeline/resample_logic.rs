pub fn calculate_snap(out_rate: u32, src_rate: u32, use_fir: bool) -> u32 {
    let mut actual_out = out_rate;
    if use_fir && src_rate > 0 && actual_out > src_rate {
        if actual_out % src_rate != 0 {
            // Snap DOWN to a power-of-2 multiple
            let mut pow2_ratio = 1;
            while pow2_ratio * 2 * src_rate <= actual_out {
                pow2_ratio *= 2;
            }
            let snapped = if pow2_ratio >= 2 {
                src_rate * pow2_ratio
            } else {
                actual_out
            };

            if snapped != actual_out {
                crate::aelog!(
                    "[CONV] FIR Resampling: {}Hz is not integer mult of {}Hz, snapping DOWN to {}Hz (x{})",
                    actual_out, src_rate, snapped, snapped / src_rate
                );
                actual_out = snapped;
            }
        }
    }
    actual_out
}
