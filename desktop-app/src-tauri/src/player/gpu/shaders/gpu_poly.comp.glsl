// ════════════════════════════════════════════════════════════════════
// Player streaming convolver — DS CMUL-ACCUM over P FDL partitions
//
// Implements one branch of the partitioned overlap-save convolution for
// the player's real-time GPU path.  Dispatched once per (branch, channel)
// pair per OLA block.
//
// Buffer layout:
//   binding=0  uniform   → PolyParams {n, P, cursor, k0}
//   binding=1  storage   → h_freq[P * n]  — filter spectra, DS complex
//   binding=2  storage   → delay[P * n]   — FDL ring, DS complex
//   binding=3  storage   → accum[n]       — accumulator out, DS complex
//
// DS format: each complex = vec4(re_hi, re_lo, im_hi, im_lo).
// `precise` qualifier → SPIR-V NoContraction → driver cannot fold or fuse
// the fast-two-sum error terms. Required for DS arithmetic correctness.
//
// Index convention: partition k=0 aligns with the most recent FDL slot.
// FDL slot for partition k = (cursor + P - k) % P, where cursor = j % P
// and j is the current block index (j starts at 0, advances per block).
// This is the inverse of the engine's convention (engine uses K=2 and
// has a different ring ordering that happens to work for K=2 only).
// ════════════════════════════════════════════════════════════════════
#version 450

layout(local_size_x = 256) in;

layout(set = 0, binding = 0) uniform PolyParams {
    uint n;           // FFT size (NFFT = 65536)
    uint num_blocks;  // P partitions per branch
    uint cursor;      // j % P — index of most recent FDL slot
    uint k0;          // first partition summed: 0 (whole block), or 1 for a
                      // live stream's tail (its head is on the CPU)
} params;

layout(set = 0, binding = 1) readonly buffer HFreq { vec4 h_freq[]; };
layout(set = 0, binding = 2) readonly buffer Delay  { vec4 delay[]; };
layout(set = 0, binding = 3) buffer Accum { vec4 accum[]; };

// ── DS scalar arithmetic ─────────────────────────────────────────────
// Every intermediate is `precise` to preserve the error terms that make
// DS arithmetic work.  See gpu_ola.comp.glsl for detailed comments.

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

// ── Main ─────────────────────────────────────────────────────────────

void main() {
    uint i = gl_GlobalInvocationID.x;
    if (i >= params.n) return;

    precise vec4 acc = vec4(0.0);
    uint n       = params.n;
    uint P       = params.num_blocks;
    uint cursor  = params.cursor;

    // Kahan compensation in DS.  The DS representation already gives
    // ~48-bit precision per value; the Kahan compensation term tracks
    // the rounding error across the P-term sum for an additional few
    // bits of headroom when P is large (115 for 30M taps).
    precise vec4 comp = vec4(0.0);

    for (uint k = params.k0; k < P; k = k + 1u) {
        // Partition k=0: most recent FDL slot (cursor).
        // Partition k=P-1: oldest FDL slot ((cursor+1)%P).
        // Formula: (cursor + P - k) % P  avoids signed overflow in uint.
        uint fdl_slot = (cursor + P - k) % P;
        uint delay_pos = fdl_slot * n + i;
        uint h_pos     = k * n + i;

        precise vec4 d    = delay[delay_pos];
        precise vec4 h    = h_freq[h_pos];
        precise vec4 prod = cmul_ds(d, h);

        // Kahan step in DS: y = prod - comp (DS sub, component-wise).
        precise vec2 y_re = sub_ds(vec2(prod.x, prod.y), vec2(comp.x, comp.y));
        precise vec2 y_im = sub_ds(vec2(prod.z, prod.w), vec2(comp.z, comp.w));
        precise vec4 y_ds = vec4(y_re.x, y_re.y, y_im.x, y_im.y);

        precise vec4 t_ds = cadd_ds(acc, y_ds);

        // comp = (t - acc) - y  in DS
        precise vec2 t_re  = vec2(t_ds.x, t_ds.y);
        precise vec2 t_im  = vec2(t_ds.z, t_ds.w);
        precise vec2 a_re  = vec2(acc.x, acc.y);
        precise vec2 a_im  = vec2(acc.z, acc.w);
        precise vec2 cr    = sub_ds(sub_ds(t_re, a_re), y_re);
        precise vec2 ci    = sub_ds(sub_ds(t_im, a_im), y_im);
        comp = vec4(cr.x, cr.y, ci.x, ci.y);

        acc = t_ds;
    }

    accum[i] = acc;
}
