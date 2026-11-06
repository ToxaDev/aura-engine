// ════════════════════════════════════════════════════════════════════
// Player streaming convolver — DS CMUL-ACCUM over the FDL partitions
// with sound, both channels in one dispatch (AURA_FIR_OPT)
//
// The same sum as gpu_poly.comp.glsl, bit for bit: per channel exactly its
// `precise` DS operations in its order. Two things differ:
//   - one dispatch per branch sums left and right, reading h once;
//   - the host sums only partitions k_first..(last with sound): before the
//     first partition with sound the sum is +0 and a zero product leaves it
//     so; after the last one each partition would add a product of exactly
//     zero (cmul_ds(0, h) = vec4(+0) for finite h), and that Kahan step
//     depends on (acc, comp) alone, so once it leaves a bin unchanged every
//     later one does — the last piece runs it at most `settle` times and
//     stops at that fixpoint.
//
// Buffer layout:
//   binding=0  uniform   → Poly2Params {n, P, cursor, k_first, k_begin,
//                          k_end, settle, zero_bits}
//   binding=1  storage   → h_freq[P * n]  — filter spectra, DS complex
//   binding=2  storage   → delay_l[P * n] — FDL ring left, DS complex
//   binding=3  storage   → delay_r[P * n] — FDL ring right
//   binding=4  storage   → accum_l[n]     — accumulator out, DS complex
//   binding=5  storage   → accum_r[n]
//   binding=6  storage   → comp_l[n]      — Kahan term between pieces
//   binding=7  storage   → comp_r[n]
//
// DS format: each complex = vec4(re_hi, re_lo, im_hi, im_lo).
// FDL slot for partition k = (cursor + P - k) % P, cursor = j % P.
// ════════════════════════════════════════════════════════════════════
#version 450

layout(local_size_x = 256) in;

layout(set = 0, binding = 0) uniform Poly2Params {
    uint n;           // FFT size (NFFT = 65536)
    uint num_blocks;  // P partitions per branch
    uint cursor;      // j % P — index of most recent FDL slot
    uint k_first;     // first partition of this block's sum (the first
                      // with sound)
    uint k_begin;     // this dispatch sums partitions k_begin..k_end
    uint k_end;
    uint settle;      // zero-product steps after the loop (last piece only)
    uint zero_bits;   // always 0: the zero product comes through here, so
                      // the compiler cannot fold `0 − comp`
} params;

layout(set = 0, binding = 1) readonly buffer HFreq  { vec4 h_freq[]; };
layout(set = 0, binding = 2) readonly buffer DelayL { vec4 delay_l[]; };
layout(set = 0, binding = 3) readonly buffer DelayR { vec4 delay_r[]; };
layout(set = 0, binding = 4) buffer AccumL { vec4 accum_l[]; };
layout(set = 0, binding = 5) buffer AccumR { vec4 accum_r[]; };
layout(set = 0, binding = 6) buffer CompL  { vec4 comp_l[]; };
layout(set = 0, binding = 7) buffer CompR  { vec4 comp_r[]; };

// ── DS scalar arithmetic (as in gpu_poly.comp.glsl) ──────────────────

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

precise vec2 mul_f32_to_ds(float a, float b) {
    precise float p = a * b;
    precise float e = fma(a, b, -p);
    return vec2(p, e);
}

precise vec2 mul_ds(vec2 a, vec2 b) {
    precise vec2 p = mul_f32_to_ds(a.x, b.x);
    precise float cross = a.x * b.y + a.y * b.x;
    precise float lo_term = p.y + cross;
    return quick_two_sum(p.x, lo_term);
}

// ── Complex DS arithmetic ────────────────────────────────────────────

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

// One Kahan step in DS, gpu_poly.comp.glsl's loop body: y = prod − comp,
// t = acc + y, comp = (t − acc) − y, acc = t.
void kahan_ds(inout vec4 acc, inout vec4 comp, vec4 prod) {
    precise vec2 y_re = sub_ds(vec2(prod.x, prod.y), vec2(comp.x, comp.y));
    precise vec2 y_im = sub_ds(vec2(prod.z, prod.w), vec2(comp.z, comp.w));
    precise vec4 y_ds = vec4(y_re.x, y_re.y, y_im.x, y_im.y);

    precise vec4 t_ds = cadd_ds(acc, y_ds);

    precise vec2 t_re  = vec2(t_ds.x, t_ds.y);
    precise vec2 t_im  = vec2(t_ds.z, t_ds.w);
    precise vec2 a_re  = vec2(acc.x, acc.y);
    precise vec2 a_im  = vec2(acc.z, acc.w);
    precise vec2 cr    = sub_ds(sub_ds(t_re, a_re), y_re);
    precise vec2 ci    = sub_ds(sub_ds(t_im, a_im), y_im);
    comp = vec4(cr.x, cr.y, ci.x, ci.y);

    acc = t_ds;
}

// ── Main ─────────────────────────────────────────────────────────────

void main() {
    uint i = gl_GlobalInvocationID.x;
    if (i >= params.n) return;

    uint n       = params.n;
    uint P       = params.num_blocks;
    uint cursor  = params.cursor;

    precise vec4 acc_l  = vec4(0.0);
    precise vec4 acc_r  = vec4(0.0);
    precise vec4 comp_kl = vec4(0.0);
    precise vec4 comp_kr = vec4(0.0);

    // A later piece of the block goes on from where the previous one left
    // the sums and their compensations.
    if (params.k_begin > params.k_first) {
        acc_l   = accum_l[i];
        acc_r   = accum_r[i];
        comp_kl = comp_l[i];
        comp_kr = comp_r[i];
    }

    for (uint k = params.k_begin; k < min(params.k_end, P); k = k + 1u) {
        uint fdl_slot  = (cursor + P - k) % P;
        uint delay_pos = fdl_slot * n + i;
        precise vec4 h      = h_freq[k * n + i];
        precise vec4 prod_l = cmul_ds(delay_l[delay_pos], h);
        precise vec4 prod_r = cmul_ds(delay_r[delay_pos], h);
        kahan_ds(acc_l, comp_kl, prod_l);
        kahan_ds(acc_r, comp_kr, prod_r);
    }

    // The silent partitions after the last one with sound: the zero-product
    // step until it changes nothing, per channel.
    precise vec4 zero = vec4(uintBitsToFloat(params.zero_bits));
    bool done_l = false;
    bool done_r = false;
    for (uint s = 0u; s < params.settle && !(done_l && done_r); s = s + 1u) {
        if (!done_l) {
            uvec4 a0 = floatBitsToUint(acc_l);
            uvec4 c0 = floatBitsToUint(comp_kl);
            kahan_ds(acc_l, comp_kl, zero);
            done_l = all(equal(a0, floatBitsToUint(acc_l))) && all(equal(c0, floatBitsToUint(comp_kl)));
        }
        if (!done_r) {
            uvec4 a0 = floatBitsToUint(acc_r);
            uvec4 c0 = floatBitsToUint(comp_kr);
            kahan_ds(acc_r, comp_kr, zero);
            done_r = all(equal(a0, floatBitsToUint(acc_r))) && all(equal(c0, floatBitsToUint(comp_kr)));
        }
    }

    accum_l[i] = acc_l;
    accum_r[i] = acc_r;
    comp_l[i]  = comp_kl;
    comp_r[i]  = comp_kr;
}
