use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The id of Aura Player ASIO's sources, shared with the driver's build.
#[allow(dead_code)]
#[path = "../../desktop-app/asio/source_id.rs"]
mod asio_source_id;

/// The source stamps of the pre-committed SPIR-V blobs, shared with the
/// engine's test of them.
#[path = "src/shader_stamp.rs"]
mod shader_stamp;

/// Locate a `glslangValidator` (or `glslang`) binary. Order:
///   1. $GLSLANG_VALIDATOR explicit override
///   2. $VULKAN_SDK/Bin (or bin)/glslangValidator
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
        // `Bin` in the Windows SDK, `bin` in the Linux and macOS ones.
        for dir in &["Bin", "bin"] {
            for name in &["glslangValidator.exe", "glslangValidator"] {
                let p = PathBuf::from(&sdk).join(dir).join(name);
                if p.exists() {
                    return Some(p);
                }
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
    // The app knows a registered Aura Player ASIO built from the same
    // sources as its own, whatever its bytes (player/input/driver.rs).
    let asio = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("..")
        .join("desktop-app")
        .join("asio");
    if let Ok((id, watch)) = asio_source_id::compute(&asio) {
        for p in watch {
            println!("cargo:rerun-if-changed={}", p.display());
        }
        println!("cargo:rustc-env=AURA_ASIO_SOURCE_ID={id}");
    }

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

    // ── Audio engine shaders ──
    let audio_shader_dir = Path::new("src/audio/shaders");

    // (glsl_source, spv_output, [defines...])
    let audio_jobs: &[(&str, &str, &[&str])] = &[
        ("ds_preflight.comp.glsl", "ds_preflight.spv", &[]),
        // gpu_fft is built twice — once as the bit-reversal kernel, once as
        // the radix-2 butterfly kernel. Each gets its own SPIR-V module
        // because glslang emits a single entry point per compilation.
        ("gpu_fft.comp.glsl", "gpu_fft_bit_reverse.spv", &["BIT_REVERSE_PASS"]),
        ("gpu_fft.comp.glsl", "gpu_fft_pass.spv",       &["FFT_PASS"]),
        // gpu_ola likewise: the split of the two channels out of one
        // spectrum, and the multiply-accumulate that joins them again.
        ("gpu_ola.comp.glsl", "gpu_ola_split.spv",      &["SPLIT_PASS"]),
        ("gpu_ola.comp.glsl", "gpu_ola.spv",            &["CMAC_PASS"]),
    ];

    // ── Player streaming convolver shader ──
    // One GLSL source compiled to one SPIR-V blob.  The player's CMUL-ACCUM
    // kernel uses the same DS arithmetic helpers as the audio engine but a
    // different FDL ring indexing convention (partition 0 = most recent input,
    // slot = (cursor + P − k) % P) and is dispatched once per (branch, channel)
    // pair per OLA block.
    let player_shader_dir = Path::new("src/player/gpu/shaders");

    let player_jobs: &[(&str, &str, &[&str])] = &[
        ("gpu_poly.comp.glsl", "gpu_poly_ola.spv", &[]),
        // Both channels in one dispatch, only the partitions with sound
        // (AURA_FIR_OPT); the one above stays for AURA_FIR_OPT=0.
        ("gpu_poly2.comp.glsl", "gpu_poly2_ola.spv", &[]),
    ];

    // Combine both shader batches into one list for unified glslang detection.
    // Each entry is (source_dir, glsl_name, spv_name, defines).
    let all_batches: &[(&Path, &[(&str, &str, &[&str])])] = &[
        (audio_shader_dir, audio_jobs),
        (player_shader_dir, player_jobs),
    ];

    let audio_precompiled_dir = audio_shader_dir.join("precompiled");
    let _player_precompiled_dir = player_shader_dir.join("precompiled");
    let glslang = find_glslang();

    // Trigger a rebuild whenever any GLSL source or pre-committed .spv changes.
    for (sdir, batch) in all_batches {
        let precompiled = sdir.join("precompiled");
        for (glsl_name, spv_name, _) in *batch {
            let glsl_path = sdir.join(glsl_name);
            if glsl_path.exists() {
                println!("cargo:rerun-if-changed={}", glsl_path.display());
            }
            let spv_path = precompiled.join(spv_name);
            if spv_path.exists() {
                println!("cargo:rerun-if-changed={}", spv_path.display());
            }
            let stamp = shader_stamp::stamp_path(&spv_path);
            if stamp.exists() {
                println!("cargo:rerun-if-changed={}", stamp.display());
            }
        }
    }
    println!("cargo:rerun-if-env-changed=GLSLANG_VALIDATOR");
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    println!("cargo:rerun-if-env-changed=AURA_REFRESH_PRECOMPILED");

    // Mirror freshly-built .spv into the respective precompiled/ directory ONLY
    // when explicitly requested.
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
            for (sdir, batch) in all_batches {
                let precompiled_dir = sdir.join("precompiled");
                for (glsl_name, spv_name, defines) in *batch {
                    let glsl_path = sdir.join(glsl_name);
                    if !glsl_path.exists() {
                        continue;
                    }
                    let out_spv = out_dir.join(spv_name);
                    compile_shader(&glslang, &glsl_path, &out_spv, defines);

                    let repo_spv = precompiled_dir.join(spv_name);
                    if !refresh_precompiled && shader_stamp::check(sdir, &repo_spv).is_err() {
                        println!(
                            "cargo:warning={} is stale: a build without glslang would ship it.                              Rebuild with AURA_REFRESH_PRECOMPILED=1 and commit it with its .src stamp",
                            repo_spv.display()
                        );
                    }
                    if refresh_precompiled {
                        let _ = fs::create_dir_all(&precompiled_dir);
                        let differs = match fs::read(&repo_spv) {
                            Ok(existing) => existing != fs::read(&out_spv).unwrap_or_default(),
                            Err(_) => true,
                        };
                        let glsl = fs::read(&glsl_path).unwrap_or_default();
                        let stamp = shader_stamp::line(glsl_name, defines, &glsl);
                        let _ = fs::write(shader_stamp::stamp_path(&repo_spv), stamp);
                        if differs {
                            if let Err(e) = fs::copy(&out_spv, &repo_spv) {
                                println!(
                                    "cargo:warning=AURA_REFRESH_PRECOMPILED set but \
                                     cannot write {}: {} (read-only checkout?)",
                                    repo_spv.display(), e
                                );
                            } else {
                                println!(
                                    "cargo:warning=mirrored {} -> {} (commit it)",
                                    spv_name,
                                    repo_spv.display()
                                );
                            }
                        }
                    }
                    eprintln!(
                        "[build.rs] compiled {} -> {}",
                        glsl_name,
                        out_spv.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                    );
                }
            }
        }
        None => {
            // Fallback path: copy pre-committed .spv blobs from src/ into OUT_DIR.
            // End users and CI don't need glslangValidator installed unless they
            // want to MODIFY the shaders.
            println!(
                "cargo:warning=glslangValidator not found — \
                 using pre-committed SPIR-V blobs from {}",
                audio_precompiled_dir.display()
            );
            for (sdir, batch) in all_batches {
                let precompiled_dir = sdir.join("precompiled");
                for (glsl_name, spv_name, _) in *batch {
                    let src = precompiled_dir.join(spv_name);
                    let dst = out_dir.join(spv_name);
                    let glsl_path = sdir.join(glsl_name);
                    if !src.exists() {
                        panic!(
                            "Neither glslangValidator nor pre-committed {} found. \
                             Install Vulkan SDK / glslang, or restore {} from git.",
                            spv_name, src.display()
                        );
                    }
                    // A blob compiled from an older shader would ship the old
                    // kernel (mtimes say nothing in a fresh checkout).
                    if let Err(e) = shader_stamp::check(sdir, &src) {
                        panic!(
                            "{e}; glslangValidator is not installed to recompile {}.                              Install Vulkan SDK / glslang (or set GLSLANG_VALIDATOR), rebuild with                              AURA_REFRESH_PRECOMPILED=1 and commit the blob with its .src stamp.",
                            glsl_path.display()
                        );
                    }
                    let src_bytes = fs::read(&src).unwrap_or_else(|e| {
                        panic!("Failed to read {}: {}", src.display(), e)
                    });
                    let needs_write = match fs::read(&dst) {
                        Ok(existing) => existing != src_bytes,
                        Err(_) => true,
                    };
                    if needs_write {
                        fs::write(&dst, &src_bytes).unwrap_or_else(|e| {
                            panic!("Failed to write {} -> {}: {}", src.display(), dst.display(), e)
                        });
                    }
                    eprintln!("[build.rs] used pre-committed {}", spv_name);
                }
            }
        }
    }
}
