//! Station icons: fetched the first time a row shows one, kept in the app's
//! data folder (`radio\icons`) and served to the page by the `aura`
//! protocol (`/radio/icon?k=`) from there only — the protocol's handler
//! never waits for the network. Raster images only (PNG, JPEG, GIF, ICO,
//! WebP), told by their first bytes, whatever the server calls them; an icon
//! that cannot be had is remembered as such for a week, and the row shows
//! the station's initials meanwhile.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};

const MAX_BYTES: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(8);
/// An icon that could not be had is not asked for again for this long.
const NONE_FOR: Duration = Duration::from_secs(7 * 24 * 3600);
/// Icons fetched at once.
const AT_ONCE: usize = 4;
/// The kinds kept: extension, content type.
const KINDS: &[(&str, &str)] =
    &[("png", "image/png"), ("jpg", "image/jpeg"), ("gif", "image/gif"), ("ico", "image/x-icon"), ("webp", "image/webp")];

/// A raster image's kind by its first bytes: (extension, content type).
pub(super) fn sniff(b: &[u8]) -> Option<(&'static str, &'static str)> {
    let ext = if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "png"
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        "gif"
    } else if b.len() >= 6 && b.starts_with(&[0, 0, 1, 0]) && b[4..6] != [0, 0] {
        // An icon directory with at least one image in it.
        "ico"
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "webp"
    } else {
        return None;
    };
    KINDS.iter().find(|(e, _)| *e == ext).copied()
}

/// The key an icon's address is kept under: its file name in the cache.
pub(super) fn key_of(url: &str) -> String {
    Sha256::digest(url.as_bytes()).iter().take(16).map(|b| format!("{:02x}", b)).collect()
}

fn is_key(k: &str) -> bool {
    k.len() == 32 && k.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn dir() -> Option<PathBuf> {
    super::rb::dir().map(|d| d.join("icons"))
}

fn kept_in(d: &Path, key: &str) -> Option<PathBuf> {
    KINDS.iter().map(|(ext, _)| d.join(format!("{}.{}", key, ext))).find(|p| p.is_file())
}

pub(super) fn read_in(d: &Path, key: &str) -> Option<(Vec<u8>, &'static str)> {
    if !is_key(key) {
        return None;
    }
    let p = kept_in(d, key)?;
    let ext = p.extension()?.to_str()?;
    let ct = KINDS.iter().find(|(e, _)| *e == ext)?.1;
    Some((std::fs::read(&p).ok()?, ct))
}

/// A kept icon's bytes and content type (for the protocol).
pub(super) fn read(key: &str) -> Option<(Vec<u8>, &'static str)> {
    read_in(&dir()?, key)
}

/// Whether the icon of `key` could not be had lately (its `.none` mark).
fn none_lately(d: &Path, key: &str) -> bool {
    std::fs::metadata(d.join(format!("{}.none", key)))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < NONE_FOR)
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(super::rb::USER_AGENT)
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // Asked for as itself: no page it was seen on (and no cookies:
            // this client keeps none).
            .referer(false)
            .redirect(reqwest::redirect::Policy::limited(4))
            .build()
            .unwrap_or_default()
    })
}

fn gate() -> &'static tokio::sync::Semaphore {
    static G: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Semaphore::new(AT_ONCE))
}

/// Why an icon did not come: `lasting` when asking again soon would not help
/// (refused, not an image, too large); a network that failed may do better
/// next time.
struct NoIcon {
    lasting: bool,
    why: String,
}

fn lasting(why: &str) -> NoIcon {
    NoIcon { lasting: true, why: why.to_string() }
}

async fn fetch(url: &str) -> Result<(Vec<u8>, &'static str), NoIcon> {
    let passing = |e: reqwest::Error| NoIcon { lasting: false, why: e.to_string() };
    let mut resp = client()
        .get(url)
        .header(reqwest::header::ACCEPT, "image/png,image/jpeg,image/gif,image/x-icon,image/webp")
        .send()
        .await
        .map_err(passing)?;
    let st = resp.status();
    if !st.is_success() {
        let why = format!("HTTP {}", st.as_u16());
        return Err(NoIcon { lasting: !st.is_server_error() && st.as_u16() != 429, why });
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        return Err(lasting("too large"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(passing)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            return Err(lasting("too large"));
        }
    }
    let (ext, _) = sniff(&body).ok_or_else(|| lasting("not a raster image"))?;
    Ok((body, ext))
}

/// The key of the icon at `url`, fetched now when it is not kept yet; ""
/// when there is none to show.
pub(super) async fn icon(url: String) -> String {
    let url = url.trim().to_string();
    let l = url.to_ascii_lowercase();
    if !(l.starts_with("http://") || l.starts_with("https://")) || url.len() > 2048 {
        return String::new();
    }
    let Some(d) = dir() else { return String::new() };
    let key = key_of(&url);
    if kept_in(&d, &key).is_some() {
        return key;
    }
    if none_lately(&d, &key) {
        return String::new();
    }
    let _turn = gate().acquire().await.ok();
    if kept_in(&d, &key).is_some() {
        return key;
    }
    match fetch(&url).await {
        Ok((body, ext)) => match super::store::write_atomic(&d.join(format!("{}.{}", key, ext)), &body) {
            Ok(()) => key,
            Err(e) => {
                crate::aelog!("[CATALOG] icon not kept: {}", e);
                String::new()
            }
        },
        Err(e) => {
            crate::aelog!("[CATALOG] no icon from {}: {}", url, e.why);
            if e.lasting {
                let _ = super::store::write_atomic(&d.join(format!("{}.none", key)), b"");
            }
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_raster_images_are_kept() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]), Some(("png", "image/png")));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]), Some(("jpg", "image/jpeg")));
        assert_eq!(sniff(b"GIF89a\x01\x00"), Some(("gif", "image/gif")));
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0, 16, 16]), Some(("ico", "image/x-icon")));
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "), Some(("webp", "image/webp")));
        // Not an image, or not a raster one: SVG, a page, an empty icon file.
        assert_eq!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), None);
        assert_eq!(sniff(b"<!DOCTYPE html><html>"), None);
        assert_eq!(sniff(&[0, 0, 1, 0, 0, 0]), None);
        assert_eq!(sniff(&[]), None);
    }

    #[test]
    fn an_icon_is_kept_under_its_addresss_key() {
        let k = key_of("https://radioparadise.com/apple-touch-icon.png");
        assert!(is_key(&k));
        assert_eq!(k, key_of("https://radioparadise.com/apple-touch-icon.png"));
        assert_ne!(k, key_of("https://radioparadise.com/favicon.ico"));
        assert!(!is_key("../stations") && !is_key(&k.to_uppercase()) && !is_key(""));

        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_in(d.path(), &k), None);
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        std::fs::write(d.path().join(format!("{k}.png")), png).unwrap();
        assert_eq!(read_in(d.path(), &k), Some((png.to_vec(), "image/png")));
        // A key that is not one reads nothing, whatever is on disk.
        std::fs::write(d.path().join("stations.png"), png).unwrap();
        assert_eq!(read_in(d.path(), "stations"), None);
        // An icon that could not be had is not asked for again soon.
        assert!(!none_lately(d.path(), &k));
        std::fs::write(d.path().join(format!("{k}.none")), b"").unwrap();
        assert!(none_lately(d.path(), &k));
    }
}
