// SPDX-License-Identifier: GPL-3.0-only
//! The driver's source id: one SHA-256 over the sources of this workspace
//! (every `.rs`, `.toml` and `.lock` outside `target`, `tests` and
//! `examples`), line ends read as LF — the same on any checkout of the same
//! sources, whatever compiler or linker builds them.
//!
//! The driver's build script bakes it into the DLL behind `MARKER`; the
//! app's build script bakes the same id into the app. A registered DLL that
//! carries the app's id is the app's own driver, even when its bytes are
//! not the bundled DLL's (a rebuild stamps another time into the file, a
//! newer compiler makes other code of the same sources).
//!
//! Shared by both build scripts (`#[path]`), so it has no dependencies.

use std::io;
use std::path::{Path, PathBuf};

/// What a DLL's id follows in the file: `<MARKER><64 hex digits>`.
pub const MARKER: &str = "AuraPlayerASIO-source:";

/// The id of the sources under `asio` (this workspace's root), and what a
/// build script watches for it: each source and each folder under the root.
pub fn compute(asio: &Path) -> io::Result<(String, Vec<PathBuf>)> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(asio, "", &mut files, &mut dirs)?;
    files.sort();
    let mut all = Vec::new();
    for (name, _) in &files {
        let text = normalized(&std::fs::read(asio.join(name))?);
        all.extend_from_slice(name.as_bytes());
        all.push(0);
        all.extend_from_slice(&(text.len() as u64).to_le_bytes());
        all.extend_from_slice(&text);
    }
    let id = sha256(&all).iter().map(|b| format!("{b:02x}")).collect();
    Ok((id, files.into_iter().map(|(_, p)| p).chain(dirs).collect()))
}

/// `(path from the root with '/', full path)` of each source under `dir`.
fn walk(dir: &Path, rel: &str, files: &mut Vec<(String, PathBuf)>, dirs: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if entry.file_type()?.is_dir() {
            if !name.starts_with('.') && !["target", "tests", "examples"].contains(&name.as_str()) {
                dirs.push(path.clone());
                walk(&path, &rel, files, dirs)?;
            }
        } else if [".rs", ".toml", ".lock"].iter().any(|e| name.ends_with(e)) {
            files.push((rel, path));
        }
    }
    Ok(())
}

/// The text with each CRLF read as LF (a Windows checkout's line ends).
fn normalized(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if !(b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
            out.push(b);
        }
    }
    out
}

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in m.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}
