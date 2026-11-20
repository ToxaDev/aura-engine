//! The signature of an update: one key and one message for the app and the
//! streamer alike.
//!
//! Each release ships a `.sig` beside every installable asset. The signature is
//! Ed25519 over exactly:
//!
//! ```text
//! aura-engine-update-v1\n
//! <tag, e.g. v2.0.0>\n
//! <asset file name>\n
//! <lowercase hex SHA-256 of the asset>\n
//! ```
//!
//! The `.sig` file is base64 of the 64-byte signature (one line). The program
//! rebuilds this message from the tag it is installing, the asset name it
//! downloaded and the SHA-256 it computed itself, then verifies before
//! extracting anything. `tools/sign_update.py` writes the same message.

/// The production public key (Ed25519, 32 bytes as lowercase hex).
pub const PROD_PUBKEY_HEX: &str = "cd2a803ad18aadae9e100644d385549a3a46fbfc19df00a8fd0621b424b391a6";

/// Build the exact message that is signed/verified.
pub fn build_update_message(tag: &str, asset_name: &str, sha256_hex: &str) -> String {
    format!("aura-engine-update-v1\n{}\n{}\n{}\n", tag, asset_name, sha256_hex)
}

/// Hex-decode a 64-char public key string into 32 bytes.
pub fn decode_pubkey_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Verify an Ed25519 signature. `sig_b64` is trimmed base64.
pub fn verify_ed25519(pubkey_bytes: &[u8; 32], message: &[u8], sig_b64: &str) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let sig_raw = STANDARD.decode(sig_b64).map_err(|e| format!("base64 decode: {}", e))?;
    if sig_raw.len() != 64 {
        return Err(format!("signature is {} bytes, expected 64", sig_raw.len()));
    }
    let sig_arr: [u8; 64] = sig_raw.try_into().unwrap();

    let vk = VerifyingKey::from_bytes(pubkey_bytes).map_err(|e| format!("invalid public key: {}", e))?;
    let sig = Signature::from_bytes(&sig_arr);

    vk.verify(message, &sig).map_err(|_| "signature mismatch".to_string())
}

/// Semver comparison: returns true when `a` is strictly greater than `b`.
/// Both may be prefixed with `v`. Compares as [major, minor, patch].
pub fn semver_gt(a: &str, b: &str) -> bool {
    fn parse(s: &str) -> Option<[u64; 3]> {
        let s = s.trim_start_matches('v');
        let mut it = s.splitn(4, '.');
        let major: u64 = it.next()?.parse().ok()?;
        let minor: u64 = it.next()?.parse().ok()?;
        // Patch may have a pre-release suffix; take only the leading digits.
        let patch_raw = it.next()?;
        let patch: u64 = patch_raw.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
        Some([major, minor, patch])
    }
    match (parse(a), parse(b)) {
        (Some(av), Some(bv)) => av > bv,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_key_decodes() {
        assert!(decode_pubkey_hex(PROD_PUBKEY_HEX).is_some());
        assert!(decode_pubkey_hex("xy").is_none());
    }

    #[test]
    fn a_signature_verifies_only_for_its_own_message() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pk = key.verifying_key().to_bytes();
        let msg = build_update_message("v2.0.0", "aura-streamer-v2.0.0-windows-x64.zip", "ab");
        let sig = STANDARD.encode(key.sign(msg.as_bytes()).to_bytes());
        assert!(verify_ed25519(&pk, msg.as_bytes(), &sig).is_ok());
        let other = build_update_message("v2.0.0", "aura-engine-v2.0.0-windows-x64.zip", "ab");
        assert_eq!(verify_ed25519(&pk, other.as_bytes(), &sig).unwrap_err(), "signature mismatch");
        assert!(verify_ed25519(&pk, msg.as_bytes(), "AAAA").unwrap_err().contains("expected 64"));
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(semver_gt("v2.0.0", "1.9.9"));
        assert!(semver_gt("1.10.0", "1.9.9"));
        assert!(!semver_gt("v2.0.0", "2.0.0"));
        assert!(!semver_gt("x", "1.0.0"));
    }
}
