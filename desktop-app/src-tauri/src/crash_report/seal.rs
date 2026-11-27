//! A report on its way out: deflated, then sealed to the project's public key.
//!
//! A sealed box (X25519 + XSalsa20-Poly1305, as in libsodium): a key pair made
//! for this one message, the private half thrown away. Neither the machine it
//! was sealed on nor anything between it and the reader can open it again;
//! only the private key, which is not in this program, can.
//!
//! The `.aerep` format: the magic, then the sealed bytes of a zlib stream of
//! the report's JSON. `tools/reports/decrypt.py` reads it.

use crypto_box::aead::OsRng;
use crypto_box::PublicKey;
use std::io::Write as _;

pub const MAGIC: &[u8] = b"AEREP1\n";

/// The report receiver takes no more than this.
pub const MAX_FILE: usize = 1024 * 1024;

/// The project's key for reports (its private half stays with the project).
const REPORT_PUBLIC_KEY: [u8; 32] = [
    0xf1, 0x08, 0xd8, 0xde, 0x18, 0xdd, 0x03, 0xc8, 0xf6, 0x4e, 0x94, 0x58, 0xe9, 0x26, 0x52, 0x7e,
    0x40, 0x31, 0x24, 0x1f, 0x10, 0x59, 0xf4, 0xc0, 0xb1, 0x16, 0x66, 0xce, 0xbc, 0x73, 0xc3, 0x25,
];

pub fn deflate(json: &[u8]) -> Result<Vec<u8>, String> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(json).map_err(|e| e.to_string())?;
    z.finish().map_err(|e| e.to_string())
}

fn seal_to(key: &PublicKey, json: &[u8]) -> Result<Vec<u8>, String> {
    let sealed = key.seal(&mut OsRng, &deflate(json)?).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The bytes of a `.aerep` file.
pub fn seal(json: &[u8]) -> Result<Vec<u8>, String> {
    seal_to(&PublicKey::from(REPORT_PUBLIC_KEY), json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto_box::SecretKey;
    use std::io::Read as _;

    #[test]
    fn only_the_private_key_opens_it() {
        let secret = SecretKey::generate(&mut OsRng);
        let json = br#"{"schema_version":1,"note":"secret words"}"#;
        let file = seal_to(&secret.public_key(), json).unwrap();
        assert!(file.starts_with(MAGIC));
        assert!(!file.windows(6).any(|w| w == b"secret"), "nothing readable in the file");

        let inflated = secret.unseal(&file[MAGIC.len()..]).unwrap();
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(&inflated[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, json);

        let other = SecretKey::generate(&mut OsRng);
        assert!(other.unseal(&file[MAGIC.len()..]).is_err());
    }

    /// A file for `tools/reports/decrypt.py` to prove itself on:
    /// `AURA_REPORT_SAMPLE=<file> cargo test … sample_for_the_reader -- --ignored`.
    #[test]
    #[ignore]
    fn sample_for_the_reader() {
        let json = br#"{"schema_version":1,"note":"sample","log":["line one","line two"]}"#;
        std::fs::write(std::env::var("AURA_REPORT_SAMPLE").unwrap(), seal(json).unwrap()).unwrap();
    }

    #[test]
    fn two_seals_of_one_report_differ() {
        let key = SecretKey::generate(&mut OsRng).public_key();
        assert_ne!(seal_to(&key, b"x").unwrap(), seal_to(&key, b"x").unwrap());
    }
}
