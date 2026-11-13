use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // The exe's icon resource and the window's icon come from icons/: a new
    // icon must rebuild them (tauri-build watches only tauri.conf.json).
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build();

    // Which commit a crash report came from (best effort: not re-read while
    // this script has no other reason to run; a clean release build always does).
    if let Some(commit) = std::process::Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
    {
        println!("cargo:rustc-env=AURA_BUILD_COMMIT={}", commit.trim());
    }

    let out_dir: PathBuf = env::var("OUT_DIR")
        .expect("OUT_DIR is always set by cargo")
        .into();
    check_ui_scripts(&out_dir);
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
