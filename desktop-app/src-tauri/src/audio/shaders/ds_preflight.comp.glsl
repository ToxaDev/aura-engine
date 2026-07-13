// ─────────────────────────────────────────────────────────────────────
// DS Pre-Flight Compute Shader
//
// Purpose: prove that GLSL's `precise` qualifier survives compilation
// to SPIR-V (as `NoContraction` decoration) and that the Vulkan driver
// honours it.  If this works, double-single arithmetic on f32 GPUs is
// viable and we can build the full DS-FFT pipeline on top.
//
// Test: compute the error term of a known double-rounding case
//       (1.0 + 1e-7 in f32).  Without NoContraction the optimiser
//       would fold (a+b)-a → b → e=0.  With NoContraction we get
//       e ≈ 1e-7 (the actual rounding residual is 0 because 1e-7 fits
//       below f32 precision relative to 1.0 — but the chain still
//       evaluates correctly because (s - a) actually computes).
//
//       More robust test: 1e7 + 3.14159
//         s  = round(1e7 + 3.14159) = 10000003.0
//         e  = (3.14159) - (s - 1e7)
//            = 3.14159 - 3.0
//            = 0.14159
//       Without NoContraction → optimiser folds, e=0.  With it → e≈0.14159.
// ─────────────────────────────────────────────────────────────────────
#version 450
#extension GL_KHR_shader_subgroup_basic : enable

layout(local_size_x = 1) in;

layout(set = 0, binding = 0) buffer InOut {
    // [a, b, s_out, e_out]
    float data[];
};

void main() {
    if (gl_GlobalInvocationID.x != 0u) return;

    float a = data[0];
    float b = data[1];

    // Knuth two-sum.  `precise` on every intermediate forces glslang to
    // emit `OpDecorate %tmp NoContraction`, which the Vulkan driver then
    // promises not to fold or re-associate.
    precise float s  = a + b;
    precise float bv = s - a;
    precise float av = s - bv;
    precise float e  = (a - av) + (b - bv);

    data[2] = s;
    data[3] = e;
}
