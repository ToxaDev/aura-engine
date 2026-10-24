// ═══════════════════════════════════════════════════════════════════
// GPU OLA Shader — Complex Multiply-Accumulate across K partitions
// Partitioned Overlap-Save (OLA) convolution pipeline
//
// For each frequency bin i:
//   accum[i] = Σ_{k=0}^{K-1} delay[(cursor+k)%K][i] × H[k][i]
//
// Uses Double-Single (DS) arithmetic for ~48-bit mantissa precision
// during accumulation. Each product f32×f32 is computed as an exact
// DS pair via FMA, then accumulated with compensated Knuth two-sum.
// Final result is collapsed to vec2<f32> for the IFFT stage.
//
// Precision: ~288 dB dynamic range (48-bit mantissa)
// vs ~144 dB with plain f32 accumulation.
// ═══════════════════════════════════════════════════════════════════

struct OlaParams {
    n: u32,           // FFT size (= 2 × block_size)
    num_blocks: u32,  // K — number of filter partitions
    cursor: u32,      // Current position in circular delay line
    _pad: u32,
}

// Double-Single representation: val ≈ hi + lo, where |lo| ≤ 0.5 ulp(hi)
struct DS {
    hi: f32,
    lo: f32,
}

@group(0) @binding(0) var<uniform> params: OlaParams;
@group(0) @binding(1) var<storage, read> h_freq: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> delay: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read_write> accum: array<vec2<f32>>;

// ── DS Arithmetic (Compensated Knuth two-sum + FMA error-free product) ──

fn two_sum(a: f32, b: f32) -> DS {
    let s = a + b;
    let b_v = s - a;
    let a_v = s - b_v;
    let e = (a - a_v) + (b - b_v);
    return DS(s, e);
}

fn add_ds(a: DS, b: DS) -> DS {
    let s = two_sum(a.hi, b.hi);
    let t = two_sum(a.lo, b.lo);
    let c = s.lo + t.hi;
    let s_hi = s.hi + c;
    let s_lo = (s.hi - s_hi) + c + t.lo;
    return DS(s_hi, s_lo);
}

// Error-free product: a×b = p + e, where p = fl(a×b), e = a×b - p
fn mul_f32_to_ds(a: f32, b: f32) -> DS {
    let p = a * b;
    let e = fma(a, b, -p);
    return DS(p, e);
}

// ─── Complex multiply-accumulate with DS precision ───
// Dispatch: ceil(N / 256) workgroups
@compute @workgroup_size(256)
fn cmul_accum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.n) { return; }
    
    // DS accumulators for real and imaginary parts
    var acc_re = DS(0.0, 0.0);
    var acc_im = DS(0.0, 0.0);
    
    let n = params.n;
    let K = params.num_blocks;
    let cursor = params.cursor;
    
    for (var k = 0u; k < K; k = k + 1u) {
        let delay_pos = ((cursor + k) % K) * n + i;
        let h_pos = k * n + i;
        
        let d = delay[delay_pos];   // Complex<f32>: (re, im)
        let h = h_freq[h_pos];      // Complex<f32>: (re, im)
        
        // Complex multiply: (d.re + d.im·i)(h.re + h.im·i)
        //   real part = d.re·h.re - d.im·h.im
        //   imag part = d.re·h.im + d.im·h.re
        
        // Each f32×f32 product computed as exact DS pair via FMA
        let p_ac = mul_f32_to_ds(d.x, h.x);  // d.re × h.re → DS
        let p_bd = mul_f32_to_ds(d.y, h.y);  // d.im × h.im → DS
        let p_ad = mul_f32_to_ds(d.x, h.y);  // d.re × h.im → DS
        let p_bc = mul_f32_to_ds(d.y, h.x);  // d.im × h.re → DS
        
        // Real: p_ac - p_bd (negate p_bd for subtraction)
        let neg_bd = DS(-p_bd.hi, -p_bd.lo);
        let re_term = add_ds(p_ac, neg_bd);
        acc_re = add_ds(acc_re, re_term);
        
        // Imag: p_ad + p_bc
        let im_term = add_ds(p_ad, p_bc);
        acc_im = add_ds(acc_im, im_term);
    }
    
    // Collapse DS → f32 for IFFT stage
    // This is the ONLY precision reduction: 48-bit → 24-bit at the very end
    accum[i] = vec2<f32>(acc_re.hi + acc_re.lo, acc_im.hi + acc_im.lo);
}
