// SPDX-License-Identifier: GPL-3.0-only
//! Bakes the driver's source id (`../source_id.rs`) into the DLL, behind
//! its marker: Aura Player reads it from a registered DLL to know its own
//! driver across rebuilds.

#[path = "../source_id.rs"]
mod source_id;

use std::path::PathBuf;

fn main() {
    let driver = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let asio = driver.parent().expect("the asio workspace");
    let (id, watch) = source_id::compute(asio).expect("the driver's sources");
    for p in watch {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rustc-env=AURA_ASIO_SOURCE_MARKER={}{id}", source_id::MARKER);
}
