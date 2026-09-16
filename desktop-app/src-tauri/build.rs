use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a `glslangValidator` (or `glslang`) binary. Order:
///   1. $GLSLANG_VALIDATOR explicit override
///   2. $VULKAN_SDK/Bin/glslangValidator
///   3. Known local install path (~/.local/glslang/bin/)
///   4. PATH lookup via `where` / `which`
fn find_glslang() -> Option<PathBuf> {
    if let Ok(p) = env::var("GLSLANG_VALIDATOR") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(sdk) = env::var("VULKAN_SDK") {
        for name in &["glslangValidator.exe", "glslangValidator"] {
            let p = PathBuf::from(&sdk).join("Bin").join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    if let Ok(home) = env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        for name in &["glslangValidator.exe", "glslangValidator"] {
            let p = PathBuf::from(&home)
                .join(".local")
                .join("glslang")
                .join("bin")
                .join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    // PATH lookup
    let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
    for name in &["glslangValidator", "glslang"] {
        if let Ok(out) = Command::new(lookup_cmd).arg(name).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                let line = s.lines().next().unwrap_or("").trim();
                if !line.is_empty() {
                    let p = PathBuf::from(line);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

fn compile_shader(glslang: &Path, src: &Path, dst: &Path, defines: &[&str]) {
    // -V → SPIR-V Vulkan output, --target-env vulkan1.2 → modern capabilities.
    // No optimisation flag — DS arithmetic depends on `precise`/NoContraction
    // surviving, and spirv-opt occasionally rewrites operation trees in ways
    // that break the chain. Unoptimised SPIR-V is exactly what we need.
    let mut cmd = Command::new(glslang);
    cmd.arg("-V").arg("--target-env").arg("vulkan1.2");
    // glslang requires -D and the macro name to be glued together (no space),
    // otherwise the bare `-D` switches the input language to HLSL.
    for d in defines {
        cmd.arg(format!("-D{}", d));
    }
    cmd.arg("-o").arg(dst).arg(src);
    let status = cmd
        .status()
        .unwrap_or_else(|e| {
            panic!("Failed to invoke glslangValidator at {}: {}", glslang.display(), e)
        });
    if !status.success() {
        panic!(
            "glslangValidator failed for {} (exit {:?})",
            src.display(),
            status.code()
        );
    }
}

fn main() {
    tauri_build::build();

    // ── Compile GLSL shaders → SPIR-V ──
    //
    // We use GLSL (not WGSL) for the DS-precision GPU path because GLSL has
    // the `precise` qualifier, which compiles to a SPIR-V `NoContraction`
    // decoration. Vulkan drivers must honour NoContraction by leaving the
    // operation tree intact — exactly the property double-single arithmetic
    // needs to survive optimisation. WGSL has no equivalent qualifier.
    //
    // Output .spv blobs go to OUT_DIR and are loaded at runtime via the
    // wgpu SPIR-V passthrough API (Device::create_shader_module_spirv),
    // bypassing naga.

    let out_dir: PathBuf = env::var("OUT_DIR")
        .expect("OUT_DIR is always set by cargo")
        .into();
    let shader_dir = Path::new("src/audio/shaders");

    // (glsl_source, spv_output, [defines...])
    let jobs: &[(&str, &str, &[&str])] = &[
        ("ds_preflight.comp.glsl", "ds_preflight.spv", &[]),
        // gpu_fft is built twice — once as the bit-reversal kernel, once as
        // the radix-2 butterfly kernel. Each gets its own SPIR-V module
        // because glslang emits a single entry point per compilation.
        ("gpu_fft.comp.glsl", "gpu_fft_bit_reverse.spv", &["BIT_REVERSE_PASS"]),
        ("gpu_fft.comp.glsl", "gpu_fft_pass.spv",       &["FFT_PASS"]),
        ("gpu_ola.comp.glsl", "gpu_ola.spv",            &[]),
    ];

    let precompiled_dir = shader_dir.join("precompiled");
    let glslang = find_glslang();

    // Trigger a rebuild whenever:
    //   * a GLSL source changes (dev editing a shader)
    //   * a checked-in .spv changes (git pull bringing fresh blobs)
    //   * the toolchain location changes (someone installed Vulkan SDK)
    //   * the opt-in mirror flag is toggled
    for (glsl_name, spv_name, _) in jobs {
        let glsl_path = shader_dir.join(glsl_name);
        if glsl_path.exists() {
            println!("cargo:rerun-if-changed={}", glsl_path.display());
        }
        let spv_path = precompiled_dir.join(spv_name);
        if spv_path.exists() {
            println!("cargo:rerun-if-changed={}", spv_path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=GLSLANG_VALIDATOR");
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    println!("cargo:rerun-if-env-changed=AURA_REFRESH_PRECOMPILED");

    // Mirror freshly-built .spv into src/audio/shaders/precompiled/ ONLY when
    // explicitly requested. Otherwise every contributor with a slightly
    // different glslang version (or just a different optimisation level)
    // would silently dirty git status on every build.
    let refresh_precompiled = env::var("AURA_REFRESH_PRECOMPILED")
        .map(|v| !v.is_empty() && v != "0" && v.to_ascii_lowercase() != "false")
        .unwrap_or(false);

    match glslang {
        Some(glslang) => {
            // NOTE on `cargo:warning=...` output:
            // Cargo prefixes every line we emit through this channel with
            // the literal word `warning:`, which makes routine status
            // messages look like compiler warnings to the casual reader.
            // We therefore use it ONLY for situations the user actually
            // needs to be told about (missing toolchain, write failures
            // when refreshing pre-compiled blobs). Successful normal-path
            // compilation is silent.
            for (glsl_name, spv_name, defines) in jobs {
                let glsl_path = shader_dir.join(glsl_name);
                if !glsl_path.exists() {
                    continue;
                }
                let out_spv = out_dir.join(spv_name);
                compile_shader(&glslang, &glsl_path, &out_spv, defines);

                // Optional mirror into the repo, gated by AURA_REFRESH_PRECOMPILED=1.
                // Default = OFF, so cargo build never dirties git status, never
                // fails on read-only checkouts (vendoring, CI), and contributors
                // with slightly different glslang versions don't fight over .spv
                // bytes. When you DO want to refresh the committed blobs, run
                // `AURA_REFRESH_PRECOMPILED=1 cargo build`.
                if refresh_precompiled {
                    let _ = fs::create_dir_all(&precompiled_dir);
                    let repo_spv = precompiled_dir.join(spv_name);
                    let differs = match fs::read(&repo_spv) {
                        Ok(existing) => existing != fs::read(&out_spv).unwrap_or_default(),
                        Err(_) => true,
                    };
                    if differs {
                        if let Err(e) = fs::copy(&out_spv, &repo_spv) {
                            println!(
                                "cargo:warning=AURA_REFRESH_PRECOMPILED set but \
                                 cannot write {}: {} (read-only checkout?)",
                                repo_spv.display(), e
                            );
                        } else {
                            println!(
                                "cargo:warning=mirrored {} → {} (commit it)",
                                spv_name,
                                repo_spv.display()
                            );
                        }
                    }
                }
                // No per-shader status line. If the user needs to see what
                // got built, `cargo build -vv` exposes build-script stderr
                // (eprintln! is silent on a normal build).
                eprintln!(
                    "[build.rs] compiled {} -> {}",
                    glsl_name,
                    out_spv.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
            }
        }
        None => {
            // Fallback path: copy pre-committed .spv blobs from src/ into OUT_DIR.
            // End users and CI don't need glslangValidator installed unless they
            // want to MODIFY the shaders.
            //
            // Safety check: if a .glsl source is NEWER than its corresponding
            // .spv blob, the developer almost certainly edited the shader
            // without rebuilding it. Refuse to silently use the stale blob.
            println!(
                "cargo:warning=glslangValidator not found — \
                 using pre-committed SPIR-V blobs from {}",
                precompiled_dir.display()
            );
            for (glsl_name, spv_name, _) in jobs {
                let src = precompiled_dir.join(spv_name);
                let dst = out_dir.join(spv_name);
                let glsl_path = shader_dir.join(glsl_name);
                if !src.exists() {
                    panic!(
                        "Neither glslangValidator nor pre-committed {} found. \
                         Install Vulkan SDK / glslang, or restore {} from git.",
                        spv_name, src.display()
                    );
                }
                // Stale-blob detection
                if let (Ok(g), Ok(s)) = (fs::metadata(&glsl_path), fs::metadata(&src)) {
                    if let (Ok(g_mtime), Ok(s_mtime)) = (g.modified(), s.modified()) {
                        if g_mtime > s_mtime {
                            panic!(
                                "{} is newer than the pre-committed {}, but \
                                 glslangValidator is not installed to recompile it. \
                                 Install Vulkan SDK / glslang, set GLSLANG_VALIDATOR \
                                 to its path, and re-run `cargo build`.",
                                glsl_path.display(), src.display()
                            );
                        }
                    }
                }
                // Byte-compare before writing: fs::copy always bumps the
                // destination mtime, and the OUT_DIR .spv blobs are tracked
                // by include_bytes! fingerprints — an unconditional copy on
                // every build-script rerun forced a full ~13s relink of the
                // binary even when nothing changed. (Same guard pattern as
                // the AURA_REFRESH_PRECOMPILED mirror above.)
                let src_bytes = fs::read(&src).unwrap_or_else(|e| {
                    panic!("Failed to read {}: {}", src.display(), e)
                });
                let needs_write = match fs::read(&dst) {
                    Ok(existing) => existing != src_bytes,
                    Err(_) => true,
                };
                if needs_write {
                    fs::write(&dst, &src_bytes).unwrap_or_else(|e| {
                        panic!("Failed to write {} → {}: {}", src.display(), dst.display(), e)
                    });
                }
                // Per-shader status as build-script stderr (visible only with
                // `cargo build -vv`). The single "using pre-committed blobs"
                // notice above already tells the user we're in fallback mode.
                eprintln!("[build.rs] used pre-committed {}", spv_name);
            }
        }
    }
}
