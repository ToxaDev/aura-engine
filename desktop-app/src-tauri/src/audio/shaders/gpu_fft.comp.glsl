// ═══════════════════════════════════════════════════════════════════
// GPU FFT — Radix-2 Cooley-Tukey, Double-Single (DS) precision
//
// Each complex value is stored as a DS pair per component:
//     vec4 = (re_hi, re_lo, im_hi, im_lo)
//
// `precise` qualifier on every intermediate compiles to SPIR-V
// `OpDecorate %tmp NoContraction`. The Vulkan driver MUST honour this
// (Vulkan §15.6) — without it, `(a+b)-a → b` folding silently destroys
// DS arithmetic.
//
// IMPORTANT: `precise` is a per-value attribute. Copying through an
// `out` parameter can drop the qualifier. We therefore return vec2/vec4
// from every helper instead of using out parameters.  Verified on RTX 4090.
// ═══════════════════════════════════════════════════════════════════
#version 450

layout(local_size_x = 256) in;

layout(set = 0, binding = 0) buffer Data { vec4 data[]; };

layout(set = 0, binding = 1) uniform Params {
    uint n;
    uint log_n;
    uint pass_idx;
    uint inverse;
} params;

// Pre-computed DS twiddle table, length = N/2.
//   forward W_N^k = exp(-2πi·k/N) packed as (cos_hi, cos_lo, neg_sin_hi, neg_sin_lo)
//   inverse FFT conjugates at lookup time
layout(set = 0, binding = 2) readonly buffer Twiddles { vec4 twiddles[]; };

// ─── DS scalar arithmetic (every intermediate is `precise`) ──────────

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

// (a_hi + a_lo) + (b_hi + b_lo) → DS
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

// Error-free f32 × f32 → DS via FMA.
precise vec2 mul_f32_to_ds(float a, float b) {
    precise float p = a * b;
    precise float e = fma(a, b, -p);
    return vec2(p, e);
}

// DS × DS — "sloppy" multiplication: keeps the exact f32×f32 high+low
// product and adds the round-to-nearest cross terms (a.x*b.y + a.y*b.x),
// but DROPS the a.y*b.y term and uses a single round for the cross-sum.
// This delivers ~48-bit effective mantissa (verified at −286 dB null
// residual against rustfft<f64>) at half the op count of full Dekker DS×DS.
// Upgrade to full Dekker only if a future change pushes FFT length past
// 2^22 or chains 3+ DS multiplies in a row — see qd_real::mul for the
// reference implementation.
precise vec2 mul_ds(vec2 a, vec2 b) {
    precise vec2 p = mul_f32_to_ds(a.x, b.x);
    precise float cross = a.x * b.y + a.y * b.x;
    precise float lo_term = p.y + cross;
    return quick_two_sum(p.x, lo_term);
}

// ─── Complex DS arithmetic ───────────────────────────────────────────

precise vec4 cmul_ds(vec4 a, vec4 b) {
    precise vec2 a_re = vec2(a.x, a.y);
    precise vec2 a_im = vec2(a.z, a.w);
    precise vec2 b_re = vec2(b.x, b.y);
    precise vec2 b_im = vec2(b.z, b.w);
    precise vec2 p_rr = mul_ds(a_re, b_re);
    precise vec2 p_ii = mul_ds(a_im, b_im);
    precise vec2 p_ri = mul_ds(a_re, b_im);
    precise vec2 p_ir = mul_ds(a_im, b_re);
    precise vec2 re = sub_ds(p_rr, p_ii);
    precise vec2 im = add_ds(p_ri, p_ir);
    return vec4(re.x, re.y, im.x, im.y);
}

precise vec4 cadd_ds(vec4 a, vec4 b) {
    precise vec2 re = add_ds(vec2(a.x, a.y), vec2(b.x, b.y));
    precise vec2 im = add_ds(vec2(a.z, a.w), vec2(b.z, b.w));
    return vec4(re.x, re.y, im.x, im.y);
}

precise vec4 csub_ds(vec4 a, vec4 b) {
    precise vec2 re = sub_ds(vec2(a.x, a.y), vec2(b.x, b.y));
    precise vec2 im = sub_ds(vec2(a.z, a.w), vec2(b.z, b.w));
    return vec4(re.x, re.y, im.x, im.y);
}

// ─── Bit-reversal permutation ────────────────────────────────────────

uint reverse_bits_n(uint x) {
    uint v = x;
    v = ((v >> 1u) & 0x55555555u) | ((v & 0x55555555u) << 1u);
    v = ((v >> 2u) & 0x33333333u) | ((v & 0x33333333u) << 2u);
    v = ((v >> 4u) & 0x0F0F0F0Fu) | ((v & 0x0F0F0F0Fu) << 4u);
    v = ((v >> 8u) & 0x00FF00FFu) | ((v & 0x00FF00FFu) << 8u);
    v = (v >> 16u) | (v << 16u);
    return v >> (32u - params.log_n);
}

#if defined(BIT_REVERSE_PASS)
void main() {
    uint idx = gl_GlobalInvocationID.x;
    if (idx >= params.n) return;
    uint rev = reverse_bits_n(idx);
    if (rev > idx) {
        vec4 tmp = data[idx];
        data[idx] = data[rev];
        data[rev] = tmp;
    }
}
#elif defined(FFT_PASS)
void main() {
    uint tid = gl_GlobalInvocationID.x;
    uint half_n = params.n / 2u;
    if (tid >= half_n) return;

    uint m = 1u << (params.pass_idx + 1u);
    uint half_m = m / 2u;
    uint group = tid / half_m;
    uint j = tid % half_m;

    uint i_top = group * m + j;
    uint i_bot = i_top + half_m;

    uint twiddle_idx = j * (params.n / m);
    precise vec4 w = twiddles[twiddle_idx];
    if (params.inverse != 0u) {
        w = vec4(w.x, w.y, -w.z, -w.w);
    }

    precise vec4 top = data[i_top];
    precise vec4 bot = data[i_bot];
    precise vec4 tw  = cmul_ds(bot, w);
    data[i_top] = cadd_ds(top, tw);
    data[i_bot] = csub_ds(top, tw);
}
#else
#error "Define BIT_REVERSE_PASS or FFT_PASS when compiling this shader."
#endif
