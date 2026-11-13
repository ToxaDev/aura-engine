//! A track sent by address, fetched into the cache for the engine to open as
//! a file; a live stream is told apart and left to the engine's radio.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use super::renderer::Meta;

/// The largest track kept (a long DSD256 file is a couple of gigabytes).
const MAX_BYTES: u64 = 8 << 30;

pub enum Fetched {
    File(PathBuf),
    /// No end: a station.
    Stream,
}

/// Whether an address may be fetched: http or https, nothing else (no
/// `file:`, no other scheme a control point might try).
pub fn fetchable(uri: &str) -> bool {
    let u = uri.trim().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://")
}

/// The file name's extension the engine's probe goes by.
fn extension(mime: &str, uri: &str) -> Option<&'static str> {
    let by_mime = match mime.split(';').next().unwrap_or("").trim() {
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Some("wav"),
        "audio/aiff" | "audio/x-aiff" => Some("aiff"),
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => Some("mp3"),
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "audio/aac" | "audio/x-aac" => Some("m4a"),
        "audio/ogg" | "audio/x-ogg" | "application/ogg" => Some("ogg"),
        "audio/dsf" | "audio/x-dsf" => Some("dsf"),
        "audio/dff" | "audio/x-dff" => Some("dff"),
        _ => None,
    };
    by_mime.or_else(|| {
        let path = uri.split(['?', '#']).next().unwrap_or("");
        let ext = path.rsplit('/').next()?.rsplit_once('.')?.1.to_ascii_lowercase();
        ["flac", "wav", "aif", "aiff", "mp3", "m4a", "mp4", "aac", "ogg", "dsf", "dff", "wv", "ape"]
            .into_iter()
            .find(|e| *e == ext)
    })
}

/// Whether an answer is a live stream rather than a file: no length, and a
/// station's signs (ICY headers, a playlist, the usual stream types).
fn looks_live(uri: &str, mime: &str, length: Option<u64>, icy: bool) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".pls") || path.ends_with(".m3u") {
        return true;
    }
    let m = mime.split(';').next().unwrap_or("").trim();
    let playlist = matches!(m, "audio/x-mpegurl" | "audio/mpegurl" | "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/x-scpls");
    playlist || (length.is_none() && (icy || matches!(m, "audio/aac" | "audio/aacp" | "audio/mpeg" | "audio/ogg")))
}

/// Fetch `uri` into `dir` as entry `id`'s file.
pub async fn fetch(client: &reqwest::Client, uri: &str, meta: &Meta, dir: &Path, id: u32) -> Result<Fetched, String> {
    if !fetchable(uri) {
        return Err("Only http and https addresses are played".into());
    }
    if meta.is_broadcast() {
        return Ok(Fetched::Stream);
    }
    let resp = client
        .get(uri)
        .header("Icy-MetaData", "0")
        .send()
        .await
        .map_err(|e| format!("cannot reach the track: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("the server answered {}", resp.status()));
    }
    let header = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mime = header("content-type");
    let icy = resp.headers().keys().any(|k| k.as_str().starts_with("icy-"));
    let length = resp.content_length();
    if looks_live(uri, &mime, length, icy) {
        return Ok(Fetched::Stream);
    }
    if length.is_some_and(|l| l > MAX_BYTES) {
        return Err("the track is too large".into());
    }
    let ext = extension(&meta.mime, uri).or_else(|| extension(&mime, uri));
    let path = match ext {
        Some(e) => dir.join(format!("track-{id}.{e}")),
        None => dir.join(format!("track-{id}")),
    };
    let started = std::time::Instant::now();
    let mut file = tokio::fs::File::create(&path).await.map_err(|e| format!("cannot write the cache: {e}"))?;
    let mut got: u64 = 0;
    let mut body = resp.bytes_stream();
    let r: Result<(), String> = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("the track broke off: {e}"))?;
            got += chunk.len() as u64;
            if got > MAX_BYTES {
                return Err("the track is too large".into());
            }
            file.write_all(&chunk).await.map_err(|e| format!("cannot write the cache: {e}"))?;
        }
        file.flush().await.map_err(|e| format!("cannot write the cache: {e}"))
    }
    .await;
    drop(file);
    if let Err(e) = r {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    if length.is_some_and(|l| l != got) {
        let _ = tokio::fs::remove_file(&path).await;
        return Err("the track came short".into());
    }
    aura_core::aelog!(
        "[UPNP] fetched {:.1} MB in {:.1} s",
        got as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    Ok(Fetched::File(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_are_fetched() {
        assert!(fetchable("http://192.168.1.2:5000/a.flac"));
        assert!(fetchable("HTTPS://x/a"));
        assert!(!fetchable("file:///C:/Windows/win.ini"));
        assert!(!fetchable("smb://nas/a.flac"));
        assert!(!fetchable(""));
    }

    #[test]
    fn the_extension_comes_from_the_type_then_the_address() {
        assert_eq!(extension("audio/x-flac", "http://h/stream?id=4"), Some("flac"));
        assert_eq!(extension("", "http://h/Music/a%20b.DSF?x=1"), Some("dsf"));
        assert_eq!(extension("application/octet-stream", "http://h/12345"), None);
    }

    #[test]
    fn a_station_is_told_from_a_file() {
        assert!(looks_live("http://h/live", "audio/mpeg", None, false));
        assert!(looks_live("http://h/live", "audio/aacp", None, true));
        assert!(looks_live("http://h/radio.m3u8", "", Some(300), false));
        assert!(!looks_live("http://h/a.mp3", "audio/mpeg", Some(5_000_000), false));
        assert!(!looks_live("http://h/a.flac", "audio/flac", None, false), "a file sent without its length (chunked)");
    }
}
