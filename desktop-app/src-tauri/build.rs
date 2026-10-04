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
    // The exe's icon resource and the window's icon come from icons/: a new
    // icon must rebuild them (tauri-build watches only tauri.conf.json).
    println!("cargo:rerun-if-changed=icons");
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
    check_ui_scripts(&out_dir);

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

/// Every script of the interface has to parse. The pages are compiled into
/// the binary, and one module that does not parse takes the whole page down
/// while the window still opens and looks fine — the first build of the
/// player went out like that, a window that could not add a file or list a
/// device, because a `'\n'` in its main module had become a real line break.
///
/// With Node on PATH each module is checked as the ES module the WebView
/// loads (copied to `.mjs` — a `.js` file is checked as CommonJS, and a stray
/// line break inside a string then passes), and each inline `<script>` of the
/// pages and components as a classic script. A failure stops the build.
/// Without Node the check is skipped with a warning.
fn check_ui_scripts(out_dir: &Path) {
    let ui = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("src");
    let scratch = out_dir.join("ui-check");
    let _ = fs::create_dir_all(&scratch);
    // A directory is scanned whole, so an added file reruns the check too.
    println!("cargo:rerun-if-changed={}", ui.display());

    // (what to call it in a message, the file node should check)
    let mut jobs: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(ui.join("js")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "js").unwrap_or(false) {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let copy = scratch.join(name.replace(".js", ".mjs"));
                if fs::copy(&path, &copy).is_ok() {
                    jobs.push((format!("js/{name}"), copy));
                }
            }
        }
    }
    for (dir, prefix) in [(ui.clone(), ""), (ui.join("components"), "components/")] {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().map(|e| e == "html").unwrap_or(false) {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let html = fs::read_to_string(&path).unwrap_or_default();
            let mut rest = html.as_str();
            let mut n = 0;
            while let Some(open) = rest.find("<script") {
                let after = &rest[open..];
                let Some(gt) = after.find('>') else { break };
                let tag = &after[..gt];
                let body_start = &after[gt + 1..];
                let Some(close) = body_start.find("</script>") else { break };
                let body = &body_start[..close];
                // External and module scripts are checked as files above.
                if !tag.contains("src=") && !tag.contains("module") && !body.trim().is_empty() {
                    n += 1;
                    let copy = scratch.join(format!("{}{name}.inline{n}.js", prefix.replace('/', "_")));
                    if fs::write(&copy, body).is_ok() {
                        jobs.push((format!("{prefix}{name} inline script {n}"), copy));
                    }
                }
                rest = &body_start[close..];
            }
        }
    }

    let mut failures = Vec::new();
    for (label, file) in &jobs {
        match Command::new("node").arg("--check").arg(file).output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => failures.push(format!(
                "{label}:\n{}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(_) => {
                println!("cargo:warning=node not found: interface scripts were not syntax-checked");
                return;
            }
        }
    }
    if !failures.is_empty() {
        panic!(
            "Interface script(s) do not parse — the window would open and do nothing:\n\n{}",
            failures.join("\n\n")
        );
    }
}
