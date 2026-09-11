// ═══════════════════════════════════════════════════════════════════
// GPU OLA — Complex DS multiply-accumulate across K filter partitions
//
// For each frequency bin i:
//   accum[i] = Σ_{k=0..K-1} delay[(cursor+k)%K][i] · H[k][i]
//
// All values stored as Complex DS (vec4 = re_hi, re_lo, im_hi, im_lo).
// Helpers return precise vec2/vec4 (not via out parameters) so the
// `precise` qualifier is preserved across function boundaries — this
// is what the Vulkan compiler turns into NoContraction decorations.
// ═══════════════════════════════════════════════════════════════════
#version 450

layout(local_size_x = 256) in;

layout(set = 0, binding = 0) uniform Params {
    uint n;
    uint num_blocks;
    uint cursor;
    uint _pad;
} params;

layout(set = 0, binding = 1) readonly buffer HFreq { vec4 h_freq[]; };
layout(set = 0, binding = 2) readonly buffer Delay  { vec4 delay[]; };
layout(set = 0, binding = 3) buffer Accum { vec4 accum[]; };

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

void main() {
    uint i = gl_GlobalInvocationID.x;
    if (i >= params.n) return;

    precise vec4 acc = vec4(0.0);
    uint n = params.n;
    uint K = params.num_blocks;
    uint cursor = params.cursor;

    for (uint k = 0u; k < K; k = k + 1u) {
        uint delay_pos = ((cursor + k) % K) * n + i;
        uint h_pos = k * n + i;
        precise vec4 d = delay[delay_pos];
        precise vec4 h = h_freq[h_pos];
        precise vec4 prod = cmul_ds(d, h);
        acc = cadd_ds(acc, prod);
    }

    accum[i] = acc;
}
