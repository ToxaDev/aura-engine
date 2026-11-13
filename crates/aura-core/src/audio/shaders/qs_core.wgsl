// ═══════════════════════════════════════════════════════════════════
// AuraEngine — 128-bit Quad-Single (QS) Precision GPU Convolution
// Uses FOUR f32 cores per accumulator to emulate ~96-bit mantissa
// Parallelized: 256 threads per output sample via shared memory reduction
// ═══════════════════════════════════════════════════════════════════

struct Params {
    cursor: u32,
    taps: u32,
    buffer_size: u32,
    chunk: u32,
}

struct DS { hi: f32, lo: f32 }
struct QS { a: f32, b: f32, c: f32, d: f32 }

// ── Compensated Arithmetic ──

fn two_sum(a: f32, b: f32) -> DS {
    let s = a + b;
    let b_v = s - a;
    let a_v = s - b_v;
    let e = (a - a_v) + (b - b_v);
    return DS(s, e);
}

fn mul_f32_to_ds(a: f32, b: f32) -> DS {
    let p = a * b;
    let e = fma(a, b, -p);
    return DS(p, e);
}

fn add_ds_to_qs(sum: QS, term: DS) -> QS {
    let s0 = two_sum(sum.a, term.hi);
    let s1 = two_sum(sum.b, s0.lo);
    let s2 = two_sum(sum.c, s1.lo);
    let s3 = two_sum(sum.d, s2.lo);

    let t0 = two_sum(s0.hi, term.lo);
    let t1 = two_sum(s1.hi, t0.lo);
    let t2 = two_sum(s2.hi, t1.lo);
    let t3 = two_sum(s3.hi, t2.lo);

    let u0 = two_sum(t0.hi, t1.hi);
    let u1 = two_sum(u0.lo, t2.hi);
    let u2 = two_sum(u1.lo, t3.hi);

    return QS(u0.hi, u1.hi, u2.hi, u2.lo);
}

// Add two QS values: treat b as two DS terms and accumulate sequentially
fn add_qs(a: QS, b: QS) -> QS {
    var result = add_ds_to_qs(a, DS(b.a, b.b));
    result = add_ds_to_qs(result, DS(b.c, b.d));
    return result;
}

// ── Bindings ──

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> fir_buf: array<f32>;
@group(0) @binding(2) var<storage, read> hist_l: array<f32>;
@group(0) @binding(3) var<storage, read> hist_r: array<f32>;
@group(0) @binding(4) var<storage, read_write> out_l: array<f32>;
@group(0) @binding(5) var<storage, read_write> out_r: array<f32>;

// ── Shared Memory for Workgroup Reduction ──
// QS = 4 × f32 = 16 bytes. 256 × 16 × 2 channels = 8 KB (within 16 KB limit)

var<workgroup> shared_l: array<QS, 256>;
var<workgroup> shared_r: array<QS, 256>;

// ── Main Compute Kernel ──
// Dispatch: dispatch_workgroups(chunk, 1, 1)
// Each workgroup = 1 output sample, 256 threads split the taps

@compute @workgroup_size(256)
fn main(
    @builtin(workgroup_id) wgid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>
) {
    let sample_idx = wgid.x;
    let thread_id = lid.x;
    if (sample_idx >= params.chunk) { return; }

    // ── Distribute taps across 256 threads ──
    let total_taps = params.taps;
    let taps_per_thread = (total_taps + 255u) / 256u;
    let tap_start = thread_id * taps_per_thread;
    let tap_end = min(tap_start + taps_per_thread, total_taps);

    // ── Each thread computes its partition with QS precision ──
    var acc_l = QS(0.0, 0.0, 0.0, 0.0);
    var acc_r = QS(0.0, 0.0, 0.0, 0.0);

    if (tap_start < total_taps) {
        let cur_idx = params.cursor + sample_idx;
        var hist_idx = (cur_idx + params.buffer_size - tap_start) % params.buffer_size;

        for (var j: u32 = tap_start; j < tap_end; j = j + 1u) {
            let coef = fir_buf[j];

            let prod_l = mul_f32_to_ds(hist_l[hist_idx], coef);
            acc_l = add_ds_to_qs(acc_l, prod_l);

            let prod_r = mul_f32_to_ds(hist_r[hist_idx], coef);
            acc_r = add_ds_to_qs(acc_r, prod_r);

            if (hist_idx == 0u) {
                hist_idx = params.buffer_size - 1u;
            } else {
                hist_idx = hist_idx - 1u;
            }
        }
    }

    // ── Store partial result to shared memory ──
    shared_l[thread_id] = acc_l;
    shared_r[thread_id] = acc_r;
    workgroupBarrier();

    // ── Parallel reduction with QS precision (8 steps for 256 threads) ──
    for (var stride: u32 = 128u; stride > 0u; stride = stride / 2u) {
        if (thread_id < stride) {
            shared_l[thread_id] = add_qs(shared_l[thread_id], shared_l[thread_id + stride]);
            shared_r[thread_id] = add_qs(shared_r[thread_id], shared_r[thread_id + stride]);
        }
        workgroupBarrier();
    }

    // ── Thread 0 writes final collapsed result ──
    if (thread_id == 0u) {
        let r_l = shared_l[0u];
        let r_r = shared_r[0u];
        out_l[sample_idx] = r_l.a + r_l.b + r_l.c + r_l.d;
        out_r[sample_idx] = r_r.a + r_r.b + r_r.c + r_r.d;
    }
}
