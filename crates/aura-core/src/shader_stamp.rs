//! The source stamp of a pre-committed SPIR-V blob (`shaders/precompiled/`):
//! `<blob>.src` names the GLSL it was compiled from, its defines and a hash
//! of that source. A build without glslangValidator (CI) embeds the blob, so
//! a blob left behind by a shader change shipped the old kernel: 1.9.6 and
//! 1.9.7 summed every partition of a 1G block in each of its pieces, ~100x
//! the card's work. build.rs refuses a stale blob there; the test below
//! catches one on a machine that has glslang.
//! Shared with build.rs (`#[path]`), so it uses std only.

use std::fs;
use std::path::{Path, PathBuf};

/// FNV-1a 64 of the source without CRs (a Windows checkout and a Linux one
/// give the same stamp), then of each define.
pub fn hash(glsl: &[u8], defines: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    glsl.iter().copied().filter(|&b| b != b'\r').for_each(&mut eat);
    for d in defines {
        eat(0);
        d.bytes().for_each(&mut eat);
    }
    h
}

/// `<blob>.src` beside the blob.
pub fn stamp_path(spv: &Path) -> PathBuf {
    let mut s = spv.as_os_str().to_owned();
    s.push(".src");
    PathBuf::from(s)
}

/// The stamp's line: `<glsl name> <defines, comma-separated or -> <hash>`.
pub fn line(glsl_name: &str, defines: &[&str], glsl: &[u8]) -> String {
    let d = if defines.is_empty() { "-".to_string() } else { defines.join(",") };
    format!("{glsl_name} {d} {:016x}\n", hash(glsl, defines))
}

/// Ok when the stamp of `spv` (in `shader_dir/precompiled`) matches the
/// GLSL in `shader_dir` as it is now; else why not.
pub fn check(shader_dir: &Path, spv: &Path) -> Result<(), String> {
    let stamp = stamp_path(spv);
    let text = fs::read_to_string(&stamp)
        .map_err(|e| format!("{}: no source stamp ({e})", stamp.display()))?;
    let mut f = text.split_whitespace();
    let (Some(name), Some(defs), Some(h)) = (f.next(), f.next(), f.next()) else {
        return Err(format!("{}: unreadable stamp", stamp.display()));
    };
    let defines: Vec<&str> = if defs == "-" { Vec::new() } else { defs.split(',').collect() };
    let glsl = fs::read(shader_dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
    // Compared by its fields, so a checkout's line ending does not matter.
    if u64::from_str_radix(h, 16).ok() == Some(hash(&glsl, &defines)) {
        Ok(())
    } else {
        Err(format!("{} was compiled from an older {name}", spv.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_crs_and_tells_defines_apart() {
        assert_eq!(hash(b"a\r\nb", &[]), hash(b"a\nb", &[]));
        assert_ne!(hash(b"x", &["A"]), hash(b"x", &["B"]));
        assert_ne!(hash(b"x", &[]), hash(b"y", &[]));
    }

    /// Every pre-committed blob has a stamp, and it matches its GLSL: a
    /// shader edit without `AURA_REFRESH_PRECOMPILED=1` fails here.
    #[test]
    fn precompiled_blobs_match_their_shaders() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut seen = 0;
        for dir in ["src/audio/shaders", "src/player/gpu/shaders"] {
            let sdir = root.join(dir);
            for e in fs::read_dir(sdir.join("precompiled")).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "spv") {
                    check(&sdir, &p).unwrap();
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 7);
    }
}
