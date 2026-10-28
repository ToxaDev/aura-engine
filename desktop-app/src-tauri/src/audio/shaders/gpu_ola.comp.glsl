// ═══════════════════════════════════════════════════════════════════
// GPU OLA — the two kernels between the forward and the inverse FFT,
// Double-Single (DS) precision, on half spectra.
//
// Both channels go through ONE complex FFT: the window is packed as
// z = L + i·R. The spectrum of a real signal is Hermitian,
// X[N−k] = conj X[k], so the two come back out of Z = FFT(z) exactly:
//
//     2·X_L[k] = Z[k] + conj Z[N−k]
//     2·X_R[k] = −i · (Z[k] − conj Z[N−k])            k = 0 … N/2
//
// and only bins 0 … N/2 are kept — the rest is the mirror. After the
// multiply-accumulate the two half spectra go back into one,
// W = A_L + i·A_R, its upper half filled from the same symmetry, and one
// inverse FFT gives the left channel in re and the right in im.
//
//   SPLIT_PASS  Z → 2·X_L, 2·X_R, written straight into the newest slot
//               of the two delay lines.
//   CMAC_PASS   Σ_k delay[(cursor + k) % K] · H[k] for both channels (H is
//               read once for the two), then W.
//
// The factor 2 is never divided out: the inverse comes out as
// 2N·(L + i·R) and the readback scales by 1/(2N), a power of two — exact.
//
// Every complex value is vec4 = (re_hi, re_lo, im_hi, im_lo). Helpers
// return precise values rather than writing `out` parameters, so the
// qualifier survives the call (see gpu_fft.comp.glsl); glslang turns it
// into NoContraction on every add, subtract, multiply and negate.
// ═══════════════════════════════════════════════════════════════════
#version 450

layout(local_size_x = 256) in;

layout(set = 0, binding = 0) uniform Params {
    uint n;           // FFT size N
    uint num_blocks;  // K partitions
    uint cursor;      // delay-line slot of the newest block
    uint stride;      // complex slots per partition: N/2 + 1, rounded up
} params;

// ── DS arithmetic (mirrors gpu_fft.comp.glsl exactly) ────────────────

precise vec2 two_sum(float a, float b) {
    precise float s  = a + b;
    precise float bv = s - a;
    precise float av = s - bv;
    precise float e  = (a - av) + (b - bv);
    return vec2(s, e);
}

precise vec2 quick_two_sum(float a, float b) {
    precise float s = a + b;
    precise float e = b - (s - a);
    return vec2(s, e);
}

precise vec2 add_ds(vec2 a, vec2 b) {
    precise vec2 s   = two_sum(a.x, b.x);
    precise vec2 t   = two_sum(a.y, b.y);
    precise float mid = s.y + t.x;
    precise vec2 v   = quick_two_sum(s.x, mid);
    precise float lo = v.y + t.y;
    return quick_two_sum(v.x, lo);
}

precise vec2 sub_ds(vec2 a, vec2 b) {
    return add_ds(a, vec2(-b.x, -b.y));
}

#if defined(SPLIT_PASS)

layout(set = 0, binding = 1) readonly buffer Spec { vec4 z[]; };
layout(set = 0, binding = 2) writeonly buffer DelayL { vec4 delay_l[]; };
layout(set = 0, binding = 3) writeonly buffer DelayR { vec4 delay_r[]; };

void main() {
    uint k = gl_GlobalInvocationID.x;
    uint half_n = params.n / 2u;
    if (k > half_n) return;

    // Z[N−k]; bin 0 pairs with itself.
    precise vec4 a = z[k];
    precise vec4 b = z[(params.n - k) & (params.n - 1u)];

    // 2·X_L = a + conj b        = (a.re + b.re, a.im − b.im)
    // 2·X_R = −i · (a − conj b) = (a.im + b.im, b.re − a.re)
    precise vec2 l_re = add_ds(a.xy, b.xy);
    precise vec2 l_im = sub_ds(a.zw, b.zw);
    precise vec2 r_re = add_ds(a.zw, b.zw);
    precise vec2 r_im = sub_ds(b.xy, a.xy);

    uint slot = params.cursor * params.stride + k;
    delay_l[slot] = vec4(l_re, l_im);
    delay_r[slot] = vec4(r_re, r_im);
}

#elif defined(CMAC_PASS)

layout(set = 0, binding = 1) readonly buffer HFreq { vec4 h_freq[]; };
layout(set = 0, binding = 2) readonly buffer DelayL { vec4 delay_l[]; };
layout(set = 0, binding = 3) readonly buffer DelayR { vec4 delay_r[]; };
layout(set = 0, binding = 4) writeonly buffer Spec { vec4 w[]; };

precise vec2 mul_f32_to_ds(float a, float b) {
    precise float p = a * b;
    precise float e = fma(a, b, -p);
    return vec2(p, e);
}

// "Sloppy" DS × DS, as in gpu_fft.comp.glsl: exact hi×hi plus the
// rounded cross terms, lo×lo dropped.
precise vec2 mul_ds(vec2 a, vec2 b) {
    precise vec2 p = mul_f32_to_ds(a.x, b.x);
    precise float cross = a.x * b.y + a.y * b.x;
    precise float lo_term = p.y + cross;
    return quick_two_sum(p.x, lo_term);
}

precise vec4 cmul_ds(vec4 a, vec4 b) {
    precise vec2 p_rr = mul_ds(a.xy, b.xy);
    precise vec2 p_ii = mul_ds(a.zw, b.zw);
    precise vec2 p_ri = mul_ds(a.xy, b.zw);
    precise vec2 p_ir = mul_ds(a.zw, b.xy);
    precise vec2 re = sub_ds(p_rr, p_ii);
    precise vec2 im = add_ds(p_ri, p_ir);
    return vec4(re, im);
}

precise vec4 cadd_ds(vec4 a, vec4 b) {
    precise vec2 re = add_ds(a.xy, b.xy);
    precise vec2 im = add_ds(a.zw, b.zw);
    return vec4(re, im);
}

void main() {
    uint i = gl_GlobalInvocationID.x;
    uint half_n = params.n / 2u;
    if (i > half_n) return;

    precise vec4 acc_l = vec4(0.0);
    precise vec4 acc_r = vec4(0.0);
    uint K = params.num_blocks;
    for (uint k = 0u; k < K; k = k + 1u) {
        uint d = ((params.cursor + k) % K) * params.stride + i;
        precise vec4 h = h_freq[k * params.stride + i];
        precise vec4 dl = delay_l[d];
        precise vec4 dr = delay_r[d];
        acc_l = cadd_ds(acc_l, cmul_ds(dl, h));
        acc_r = cadd_ds(acc_r, cmul_ds(dr, h));
    }

    // Bins 0 and N/2 of a real signal's spectrum are real; what sits in
    // their imaginary parts is rounding. The full-spectrum path dropped it
    // the same way — it never reaches the real part of the inverse.
    if (i == 0u || i == half_n) {
        acc_l = vec4(acc_l.xy, 0.0, 0.0);
        acc_r = vec4(acc_r.xy, 0.0, 0.0);
    }

    // W[i]   = A_L + i·A_R            = (A_L.re − A_R.im, A_L.im + A_R.re)
    // W[N−i] = conj A_L + i·conj A_R  = (A_L.re + A_R.im, A_R.re − A_L.im)
    precise vec2 lo_re = sub_ds(acc_l.xy, acc_r.zw);
    precise vec2 lo_im = add_ds(acc_l.zw, acc_r.xy);
    w[i] = vec4(lo_re, lo_im);
    if (i != 0u && i != half_n) {
        precise vec2 hi_re = add_ds(acc_l.xy, acc_r.zw);
        precise vec2 hi_im = sub_ds(acc_r.xy, acc_l.zw);
        w[params.n - i] = vec4(hi_re, hi_im);
    }
}

#else
#error "Define SPLIT_PASS or CMAC_PASS when compiling this shader."
#endif
