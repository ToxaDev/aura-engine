//! Puts the web remote into the exe: the desktop app's pages (`desktop-app/src`)
//! and the remote's own files (`web/remote`). The table it writes is included by
//! `assets.rs`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `prefix` + the path of `file` under `root`, with forward slashes.
fn web_path(root: &Path, file: &Path, prefix: &str) -> String {
    let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
    format!("{prefix}{rel}")
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.parent().unwrap().parent().unwrap();
    let app = repo.join("desktop-app").join("src");
    let remote = repo.join("web").join("remote");
    let over = remote.join("override");
    println!("cargo:rerun-if-changed={}", app.display());
    println!("cargo:rerun-if-changed={}", remote.display());

    // The first of a path wins: the remote's overrides stand before the app's
    // file of that name.
    let mut table = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    let mut add = |root: &Path, prefix: &str, skip: Option<&Path>| {
        let mut files = Vec::new();
        walk(root, &mut files);
        files.sort();
        for f in files {
            if skip.is_some_and(|s| f.starts_with(s)) {
                continue;
            }
            let path = web_path(root, &f, prefix);
            writeln!(table, "    ({path:?}, include_bytes!({:?})),", f.display().to_string()).unwrap();
        }
    };
    add(&over, "", None);
    add(&remote, "remote/", Some(&over));
    add(&app, "", None);
    table.push_str("];\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("web_assets.rs");
    std::fs::write(out, table).unwrap();
}
