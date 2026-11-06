//! The signed list of the `filters` release: every filter file the app may
//! download, with its size and SHA-256.
//!
//! The manifest is trusted only through its signature, made with the update
//! key over [`signed_message`]; every file is trusted only when its size and
//! SHA-256 match the manifest. Nothing in it is taken as a path: a name is
//! checked to be a plain `fir_…` file name that agrees with the cell it says
//! it belongs to.

use serde::Deserialize;

use crate::audio::converter::dsp::filter as flt;

pub const MANIFEST_NAME: &str = "filters-manifest.json";
pub const SIG_NAME: &str = "filters-manifest.json.sig";
pub const TAG: &str = "filters";

/// The manifest lists a few hundred files; a megabyte is far more than that.
pub const MANIFEST_MAX_BYTES: u64 = 1 << 20;
/// Base64 of 64 bytes, with room for a line end.
pub const SIG_MAX_BYTES: u64 = 1024;
/// The largest filter file is 1 000 000 128 bytes (1G at ×32, branch 0).
pub const FILE_MAX_BYTES: u64 = 2 << 30;

/// One downloadable file.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
    pub taps: usize,
    /// The design rate, the rate in the file name.
    pub rate: u32,
    pub minimum: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub created: String,
    /// Sorted by name.
    pub files: Vec<Entry>,
}

impl Manifest {
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.files
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.files[i])
    }

    /// Every file of one cell: the same taps, design rate and phase.
    pub fn cell(&self, taps: usize, rate: u32, minimum: bool) -> Vec<&Entry> {
        self.files
            .iter()
            .filter(|e| e.taps == taps && e.rate == rate && e.minimum == minimum)
            .collect()
    }
}

#[derive(Deserialize)]
struct RawManifest {
    format: u32,
    tag: String,
    #[serde(default)]
    created: String,
    files: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    name: String,
    size: u64,
    sha256: String,
    taps: String,
    rate: u32,
    phase: String,
    family: String,
}

/// The exact text the manifest's signature is made over.
pub fn signed_message(manifest: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(manifest).iter().map(|b| format!("{b:02x}")).collect();
    format!("aura-engine-filters-v1\n{TAG}\n{MANIFEST_NAME}\n{hex}\n")
}

/// The manifest, once its signature `sig` (base64 text) checks out with
/// `pubkey`. The signature is checked before a byte of it is parsed.
pub fn verify(manifest: &[u8], sig: &[u8], pubkey: &[u8; 32]) -> Result<Manifest, String> {
    let sig = std::str::from_utf8(sig).map_err(|_| "the filter list's signature is not text".to_string())?;
    crate::updater::verify_ed25519(pubkey, signed_message(manifest).as_bytes(), sig.trim())
        .map_err(|e| format!("the filter list's signature does not check out ({e})"))?;
    parse(manifest)
}

/// A plain file name: `fir_`, then letters, digits, `_`, `.` and `-`, no `..`.
fn plain_name(name: &str) -> bool {
    name.starts_with("fir_")
        && name.len() <= 128
        && !name.contains("..")
        && !name.ends_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
    let raw: RawManifest =
        serde_json::from_slice(bytes).map_err(|e| format!("the filter list is not readable: {e}"))?;
    if raw.format != 1 {
        return Err(format!("the filter list is format {}, this version reads format 1", raw.format));
    }
    if raw.tag != TAG {
        return Err(format!("the filter list is for '{}', not '{TAG}'", raw.tag));
    }
    let mut files = Vec::with_capacity(raw.files.len());
    for f in raw.files {
        let bad = |why: &str| format!("the filter list has a bad entry '{}': {why}", f.name);
        if !plain_name(&f.name) {
            return Err(bad("not a plain filter file name"));
        }
        if f.size == 0 || f.size > FILE_MAX_BYTES {
            return Err(bad("size out of range"));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("sha256 is not 64 hex digits"));
        }
        let taps = flt::TAP_LADDER
            .iter()
            .copied()
            .find(|t| flt::taps_label(*t) == Some(f.taps.as_str()))
            .ok_or_else(|| bad("unknown tap count"))?;
        if !flt::TARGET_RATES.contains(&f.rate) {
            return Err(bad("unknown rate"));
        }
        let minimum = match f.phase.as_str() {
            "linear" => false,
            "minimum" => true,
            _ => return Err(bad("unknown phase")),
        };
        let family = if f.rate % 44_100 == 0 { "44k" } else { "48k" };
        if f.family != family {
            return Err(bad("family does not match the rate"));
        }
        // The name has to say the same cell, so a file is never filed under a
        // setting it does not serve. `<stem>_meta.json` is the design note the
        // ×32 packs carry beside a whole filter.
        let stem = format!("fir_{}_{}_{}_phase", f.taps, f.rate, f.phase);
        let meta = format!("{stem}_meta.json");
        if !f.name.starts_with(&format!("{stem}.")) && f.name != meta {
            return Err(bad("name does not match its taps, rate and phase"));
        }
        files.push(Entry {
            name: f.name,
            size: f.size,
            sha256: f.sha256.to_ascii_lowercase(),
            taps,
            rate: f.rate,
            minimum,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    if files.windows(2).any(|w| w[0].name == w[1].name) {
        return Err("the filter list names a file twice".to_string());
    }
    Ok(Manifest { created: raw.created, files })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    pub(crate) fn entry_json(name: &str, size: u64, sha: &str, taps: &str, rate: u32, phase: &str) -> serde_json::Value {
        let family = if rate % 44_100 == 0 { "44k" } else { "48k" };
        serde_json::json!({"name": name, "size": size, "sha256": sha, "taps": taps, "rate": rate,
                           "phase": phase, "family": family})
    }

    pub(crate) fn manifest_json(files: Vec<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"format": 1, "tag": "filters", "created": "2026-10-24", "files": files}))
            .unwrap()
    }

    fn one(name: &str) -> Result<Manifest, String> {
        parse(&manifest_json(vec![entry_json(name, 10, SHA_A, "1G", 705_600, "linear")]))
    }

    #[test]
    fn a_good_list_is_read_sorted_and_by_cell() {
        let m = parse(&manifest_json(vec![
            entry_json("fir_1G_705600_linear_phase.b0.npy", 500, SHA_A, "1G", 705_600, "linear"),
            entry_json("fir_1G_705600_linear_phase.b0.ends.npy", 50, SHA_A, "1G", 705_600, "linear"),
            entry_json("fir_30M_384000_minimum_phase.npy", 7, &SHA_A.to_uppercase(), "30M", 384_000, "minimum"),
        ]))
        .unwrap();
        assert_eq!(m.files[0].name, "fir_1G_705600_linear_phase.b0.ends.npy");
        assert_eq!(m.cell(1_000_000_000, 705_600, false).len(), 2);
        assert_eq!(m.cell(1_000_000_000, 705_600, true).len(), 0);
        let e = m.get("fir_30M_384000_minimum_phase.npy").unwrap();
        assert_eq!((e.taps, e.rate, e.minimum, e.sha256.as_str()), (30_000_000, 384_000, true, SHA_A));
    }

    #[test]
    fn names_that_are_not_plain_filter_files_are_refused() {
        for name in [
            "../fir_1G_705600_linear_phase.b0.npy",
            "fir_1G_705600_linear_phase/../../x.npy",
            "fir_1G_705600_linear_phase.b0.npy\\x",
            "C:fir_1G_705600_linear_phase.npy",
            "x_1G_705600_linear_phase.npy",
            "fir_1G_705600_linear_phase..npy",
            "fir_1G_705600_linear_phase.",
            "fir_1G_705600_minimum_phase.npy",
            "fir_1G_1411200_linear_phase.npy",
            "fir_1G_705600_linear_phase_meta.npy",
            "fir_1G_705600_linear_phase_meta.json.npy",
            "fir_1G_705600_minimum_phase_meta.json",
        ] {
            assert!(one(name).is_err(), "{name} was accepted");
        }
        assert!(one("fir_1G_705600_linear_phase.b0.json").is_ok());
        assert!(one("fir_1G_705600_linear_phase_meta.json").is_ok());
    }

    #[test]
    fn bad_fields_and_duplicates_are_refused() {
        let e = |size, sha: &str, taps, rate, phase| {
            parse(&manifest_json(vec![entry_json("fir_5k_88200_linear_phase.npy", size, sha, taps, rate, phase)]))
        };
        assert!(e(0, SHA_A, "5k", 88_200, "linear").is_err());
        assert!(e(FILE_MAX_BYTES + 1, SHA_A, "5k", 88_200, "linear").is_err());
        assert!(e(1, "abc", "5k", 88_200, "linear").is_err());
        assert!(e(1, &"g".repeat(64), "5k", 88_200, "linear").is_err());
        assert!(e(1, SHA_A, "7k", 88_200, "linear").is_err());
        assert!(e(1, SHA_A, "5k", 88_201, "linear").is_err());
        assert!(e(1, SHA_A, "5k", 88_200, "mixed").is_err());
        let mut wrong_family = entry_json("fir_5k_88200_linear_phase.npy", 1, SHA_A, "5k", 88_200, "linear");
        wrong_family["family"] = "48k".into();
        assert!(parse(&manifest_json(vec![wrong_family])).is_err());
        let twice = entry_json("fir_5k_88200_linear_phase.npy", 1, SHA_A, "5k", 88_200, "linear");
        assert!(parse(&manifest_json(vec![twice.clone(), twice])).is_err());
        let other_tag = serde_json::json!({"format": 1, "tag": "v1.8.1", "files": []});
        assert!(parse(&serde_json::to_vec(&other_tag).unwrap()).is_err());
        let format2 = serde_json::json!({"format": 2, "tag": "filters", "files": []});
        assert!(parse(&serde_json::to_vec(&format2).unwrap()).is_err());
    }

    pub(crate) fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    pub(crate) fn sign(key: &ed25519_dalek::SigningKey, manifest: &[u8]) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::Signer;
        let sig = key.sign(signed_message(manifest).as_bytes());
        format!("{}\n", STANDARD.encode(sig.to_bytes())).into_bytes()
    }

    #[test]
    fn the_signed_message_follows_the_spec() {
        let msg = signed_message(b"abc");
        assert_eq!(
            msg,
            "aura-engine-filters-v1\nfilters\nfilters-manifest.json\n\
             ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"
        );
    }

    #[test]
    fn only_a_list_signed_with_the_key_is_trusted() {
        let key = test_key();
        let pk = key.verifying_key().to_bytes();
        let m = manifest_json(vec![entry_json("fir_5k_88200_linear_phase.npy", 1, SHA_A, "5k", 88_200, "linear")]);
        let sig = sign(&key, &m);
        assert_eq!(verify(&m, &sig, &pk).unwrap().files.len(), 1);

        let mut tampered = m.clone();
        let at = tampered.iter().position(|&b| b == b'1').unwrap();
        tampered[at] = b'2';
        assert!(verify(&tampered, &sig, &pk).unwrap_err().contains("signature"));

        let other = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        assert!(verify(&m, &sign(&other, &m), &pk).is_err());
        assert!(verify(&m, b"not base64 !!", &pk).is_err());
        assert!(verify(&m, &[0xff, 0xfe], &pk).is_err());
    }
}
