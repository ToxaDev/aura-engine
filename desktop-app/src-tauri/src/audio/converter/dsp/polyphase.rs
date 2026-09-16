/// h_k[m] = h[m*L + k] for k in 0..L
pub fn polyphase_decompose(coeffs: &[f64], l: usize) -> Vec<Vec<f64>> {
    let mut phases = Vec::with_capacity(l);
    for k in 0..l {
        let sub_len = (coeffs.len() + l - 1 - k) / l;
        let sub: Vec<f64> = (0..sub_len)
            .map(|m| {
                let idx = m * l + k;
                if idx < coeffs.len() {
                    coeffs[idx]
                } else {
                    0.0
                }
            })
            .collect();
        phases.push(sub);
    }
    phases
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decomposition is a strict reordering of the original coefficients:
    /// every coefficient must appear in exactly one sub-filter at the correct
    /// position. Reconstructing h from its phases must yield h itself
    /// (bit-exact).
    #[test]
    fn round_trip_reconstruction_is_bit_exact() {
        for &l in &[2usize, 4, 8, 16] {
            let coeffs: Vec<f64> = (0..1000).map(|i| (i as f64) * 0.13 - 0.07).collect();
            let phases = polyphase_decompose(&coeffs, l);
            assert_eq!(phases.len(), l);
            let mut reconstructed = vec![0.0; coeffs.len()];
            for (k, sub) in phases.iter().enumerate() {
                for (m, &v) in sub.iter().enumerate() {
                    let idx = m * l + k;
                    if idx < reconstructed.len() {
                        reconstructed[idx] = v;
                    }
                }
            }
            for i in 0..coeffs.len() {
                assert_eq!(
                    coeffs[i], reconstructed[i],
                    "L={} mismatch at i={}", l, i
                );
            }
        }
    }

    #[test]
    fn sum_of_phase_lengths_equals_or_exceeds_input() {
        for &l in &[3usize, 5, 7, 8] {
            let coeffs = vec![0.0_f64; 1234];
            let phases = polyphase_decompose(&coeffs, l);
            let total: usize = phases.iter().map(|p| p.len()).sum();
            assert!(total >= coeffs.len());
            // Each phase is len = ceil((n - k) / l), sum is bounded by n + l.
            assert!(total <= coeffs.len() + l);
        }
    }
}
