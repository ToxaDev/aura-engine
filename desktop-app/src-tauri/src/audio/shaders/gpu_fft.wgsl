// ═══════════════════════════════════════════════════════════════════
// GPU FFT Shader — Radix-2 Cooley-Tukey (Decimation-in-Time)
// Complex values stored as vec2<f32> (x=real, y=imag)
//
// Entry points:
//   bit_reverse — Bit-reversal permutation (1 dispatch before FFT passes)
//   fft_pass    — One butterfly pass (called log2(N) times)
//
// For IFFT: set inverse=1 in params. Caller must scale output by 1/N.
// ═══════════════════════════════════════════════════════════════════

struct FftParams {
    n: u32,          // FFT size (must be power of 2)
    log_n: u32,      // log2(N) 
    pass_idx: u32,   // Current pass index (0 to log_n-1) — used by fft_pass
    inverse: u32,    // 0=forward FFT, 1=inverse FFT
}

@group(0) @binding(0) var<storage, read_write> data: array<vec2<f32>>;
@group(0) @binding(1) var<uniform> params: FftParams;

const PI: f32 = 3.14159265358979323846;

// Fast bit-reversal using parallel bit-swap technique
fn reverse_bits(x: u32) -> u32 {
    var v = x;
    v = ((v >> 1u) & 0x55555555u) | ((v & 0x55555555u) << 1u);
    v = ((v >> 2u) & 0x33333333u) | ((v & 0x33333333u) << 2u);
    v = ((v >> 4u) & 0x0F0F0F0Fu) | ((v & 0x0F0F0F0Fu) << 4u);
    v = ((v >> 8u) & 0x00FF00FFu) | ((v & 0x00FF00FFu) << 8u);
    v = (v >> 16u) | (v << 16u);
    return v >> (32u - params.log_n);
}

// ─── Bit-reversal permutation ───
// Dispatch: ceil(N / 256) workgroups
@compute @workgroup_size(256)
fn bit_reverse(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.n) { return; }
    
    let rev = reverse_bits(idx);
    // Only swap if rev > idx to avoid double-swapping
    if (rev > idx) {
        let temp = data[idx];
        data[idx] = data[rev];
        data[rev] = temp;
    }
}

// ─── One butterfly pass of Radix-2 FFT ───
// Dispatch: ceil(N/2 / 256) workgroups
// Each thread handles one butterfly operation
@compute @workgroup_size(256)
fn fft_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tid = gid.x;
    let half_n = params.n / 2u;
    if (tid >= half_n) { return; }
    
    // Butterfly group parameters
    let m = 1u << (params.pass_idx + 1u);   // group size (doubles each pass)
    let half_m = m / 2u;                     // half group size
    
    let group = tid / half_m;                // which group this thread is in
    let j = tid % half_m;                    // position within the group
    
    let i_top = group * m + j;               // top butterfly index
    let i_bot = i_top + half_m;              // bottom butterfly index
    
    // Twiddle factor: W_m^j
    // Forward: exp(-2πi·j/m), Inverse: exp(+2πi·j/m)
    let sign = select(-1.0, 1.0, params.inverse != 0u);
    let angle = sign * 2.0 * PI * f32(j) / f32(m);
    let w = vec2<f32>(cos(angle), sin(angle));
    
    let top = data[i_top];
    let bot = data[i_bot];
    
    // Complex multiply: tw = bot × w
    let tw = vec2<f32>(
        bot.x * w.x - bot.y * w.y,
        bot.x * w.y + bot.y * w.x
    );
    
    // Butterfly: top' = top + tw, bot' = top - tw
    data[i_top] = top + tw;
    data[i_bot] = top - tw;
}
