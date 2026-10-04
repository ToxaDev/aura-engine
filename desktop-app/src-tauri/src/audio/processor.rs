pub trait DspProcessor: Send {
    fn process_audio(
        &mut self,
        in_l: &[f64],
        in_r: &[f64],
        out_l: &mut [f64],
        out_r: &mut [f64],
        num_frames: usize,
    );

    /// OLA/OLS block size of this convolver, in samples.
    /// (Not yet consumed by the pipeline — output_latency() is the value the
    /// trim/flush arithmetic needs — but kept per docs/10 Fix #4 so future
    /// call sites never re-derive the formula.)
    #[allow(dead_code)]
    fn block_size(&self) -> usize;

    /// Total algorithmic output latency in samples: how many output samples
    /// of pre-roll this convolver emits before the first real sample appears.
    /// Excludes the filter's own group delay — callers add that separately.
    ///
    /// CPU (CpuDspProcessor): 2 × block_size — one block inherent to
    /// overlap-save plus one block from the deferred out_buf read pattern
    /// (proven by `convolver_latency_is_two_blocks_and_unity_gain`).
    /// GPU (GpuDspProcessor): 1 × block_size — process_ola_block computes the
    /// just-filled block synchronously at the block boundary.
    fn output_latency(&self) -> usize;
}
