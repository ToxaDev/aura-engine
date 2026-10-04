//! Non-blocking HTTP route dispatch for the analytics channel.
//!
//! All handlers return within microseconds. They NEVER compute, NEVER block,
//! NEVER call into the engine — they only clone an `Arc<[u8]>` from a
//! prebuilt buffer that the background thread keeps fresh.
//!
//! ## URL scheme (PROTOCOL.md §1)
//!
//! ```text
//! GET /player/an/live?since=<u32>                  → AAN1
//! GET /player/an/track?sid=<u32>&rev=<u32>         → AAN2
//! GET /player/an/resp?sid=<u32>&f0=&f1=&n=&log=    → AAN3
//! GET /player/an/wave?sid=<u32>&src=S&lod=0&i=0    → AAWT
//! GET /player/an/spec?sid=<u32>&src=S&lod=0&i=0    → AAST
//! GET /player/an/specz?sid=&src=S&lod=&i0=&i1=&got=<hex> → AAZR (zoomed tiles)
//! GET /player/an/selstat?sid=&src=S&s0=&s1=          → AASL (a selection's statistics)
//! GET /player/an/stream?sid=[&reset=1]              → JSON: a stream's session, songs, bandwidth (↺: reset=1)
//! ```
//!
//! Routes that cannot find the requested data return `None`; the caller
//! (the tauri `asset_protocol` handler or a dev-mode HTTP server) maps that
//! to a 404 response.

use std::sync::Arc;

use super::hub;
use super::proto::{TileSrc, TileKind};

// ─── Query parameter helpers ─────────────────────────────────────────────────

/// Minimal zero-copy query-string parser. Returns the value of the first
/// occurrence of `key` in `query` (`key=value&…`), or `None`.
fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for part in query.split('&') {
        if let Some(rest) = part.strip_prefix(key) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val);
            }
        }
    }
    None
}

fn parse_u32(query: &str, key: &str) -> Option<u32> {
    param(query, key)?.parse().ok()
}

fn parse_u16(query: &str, key: &str) -> Option<u16> {
    param(query, key)?.parse().ok()
}

fn parse_f32(query: &str, key: &str) -> Option<f32> {
    param(query, key)?.parse().ok()
}

fn parse_bool(query: &str, key: &str) -> bool {
    matches!(param(query, key), Some("1") | Some("true") | Some("yes"))
}

fn parse_tile_src(query: &str) -> TileSrc {
    match param(query, "src") {
        Some("B") => TileSrc::B,
        Some("O") => TileSrc::O,
        _         => TileSrc::S,
    }
}

// ─── Route dispatch ───────────────────────────────────────────────────────────

/// Dispatch a `/player/an/*` request.
///
/// `path` is the trailing part after `/player/an/` (e.g. `"live"`).
/// `query` is the raw query string (e.g. `"since=7"`).
///
/// Returns `Some(bytes)` on success, `None` for 404.
pub fn handle(path: &str, query: &str) -> Option<Arc<[u8]>> {
    match path {
        "live"  => route_live(query),
        "track" => route_track(query),
        "resp"  => route_resp(query),
        "wave"  => route_wave(query),
        "spec"  => route_spec(query),
        "specz" => route_specz(query),
        "selstat" => route_selstat(query),
        "wavez" => route_wavez(query),
        "vec" => route_vec(query),
        "stream" => route_stream(query),
        _       => None,
    }
}

// ─── Individual route handlers ────────────────────────────────────────────────

/// `GET /player/an/live?sid=<u32>&since=<u32>`
///
/// Returns the latest AAN1 frame (or `None` if no frame exists yet).
/// The `since` parameter is advisory — the ring always returns the freshest
/// frame regardless.  `sid=` is required per ERRATA E3 (every /player/an/*
/// route takes sid=).
// analytics: E3 — sid is now required; hardcoded LIVE_SID removed
fn route_live(query: &str) -> Option<Arc<[u8]>> {
    let sid   = parse_u32(query, "sid")?;
    let since = parse_u32(query, "since").unwrap_or(0);
    hub::get().aan1_bytes(sid, since)
}

/// `GET /player/an/track?sid=<u32>&rev=<u32>`
///
/// Returns the latest AAN2 frame, or the 8-byte unchanged stub when the
/// client's `rev` matches the current revision.
fn route_track(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let rev = parse_u32(query, "rev").unwrap_or(0);
    hub::get().aan2_bytes(sid, rev)
}

/// `GET /player/an/resp?sid=<u32>&f0=<f32>&f1=<f32>&n=<u16>&log=<bool>`
///
/// Returns a decimated AAN3 frame. Decimation is O(n_pts) and is the only
/// non-trivial work in a route handler; it runs in the request thread, not
/// the background thread.
fn route_resp(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let f0  = parse_f32(query, "f0").unwrap_or(20.0);
    let f1  = parse_f32(query, "f1").unwrap_or(20_000.0);
    let n   = parse_u16(query, "n").unwrap_or(512);
    // The page asks with `scale=log`.
    let log = parse_bool(query, "log") || query.split('&').any(|kv| kv == "scale=log");
    hub::get().aan3_bytes(sid, f0, f1, n, log)
}

/// `GET /player/an/wave?sid=<u32>&src=S&lod=<u8>&i=<u32>`
fn route_wave(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let src = parse_tile_src(query);
    let lod = param(query, "lod").and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
    let i   = parse_u32(query, "i").unwrap_or(0);
    hub::get().tile_bytes(sid, src, TileKind::Wave, lod, i)
}

/// `GET /player/an/spec?sid=<u32>&src=S&lod=<u8>&i=<u32>`
fn route_spec(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let src = parse_tile_src(query);
    let lod = param(query, "lod").and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
    let i   = parse_u32(query, "i").unwrap_or(0);
    hub::get().tile_bytes(sid, src, TileKind::Spec, lod, i)
}

/// `GET /player/an/specz?sid=<u32>&src=S|B|O&lod=<u8>&i0=<u32>&i1=<u32>&got=<hex u64>`
///
/// The zoomed tiles `i0..i1` of `src` at a hop of 2^lod source samples that
/// are ready (bit k of `got` set: the page holds tile i0 + k). Records the
/// ask; the zoom thread computes the rest.
fn route_specz(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let src = parse_tile_src(query);
    let lod = param(query, "lod").and_then(|s| s.parse::<u8>().ok())?;
    let i0  = parse_u32(query, "i0")?;
    let i1  = parse_u32(query, "i1")?;
    let got = param(query, "got").and_then(|s| u64::from_str_radix(s, 16).ok()).unwrap_or(0);
    hub::get().zoom_bytes(sid, src, lod, i0, i1, got)
}

/// `GET /player/an/selstat?sid=<u32>&src=S|B|O&s0=<u64>&s1=<u64>`
///
/// The whole-track metrics of `src` over source samples `[s0, s1)` when they
/// are done; otherwise they are asked for (a thread computes them).
fn route_selstat(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let src = parse_tile_src(query);
    let s0 = param(query, "s0").and_then(|s| s.parse::<u64>().ok())?;
    let s1 = param(query, "s1").and_then(|s| s.parse::<u64>().ok())?;
    hub::get().stat_bytes(sid, src, s0, s1)
}

/// `GET /player/an/stream?sid=<u32>[&reset=1]` — a stream's session, its
/// songs and bandwidth as JSON (stream_totals.rs, built once a second).
/// `reset=1` (↺) asks the live thread to start the session's totals afresh.
fn route_stream(query: &str) -> Option<Arc<[u8]>> {
    if parse_bool(query, "reset") {
        super::stream_totals::ask_reset();
    }
    super::stream_totals::json_bytes()
}

/// `GET /player/an/vec?sid=<u32>` → AAVS (the live output's stereo picture)
fn route_vec(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    hub::get().vec_bytes(sid)
}

/// `GET /player/an/wavez?sid=<u32>&src=S|B|O&s0=<u64>&s1=<u64>&n=<u32>` → AAWZ
fn route_wavez(query: &str) -> Option<Arc<[u8]>> {
    let sid = parse_u32(query, "sid")?;
    let src = parse_tile_src(query);
    let s0 = param(query, "s0").and_then(|s| s.parse::<u64>().ok())?;
    let s1 = param(query, "s1").and_then(|s| s.parse::<u64>().ok())?;
    let n = parse_u32(query, "n")?;
    hub::get().wavez_bytes(sid, src, s0, s1, n)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── param helper ───────────────────────────────────────────────────────

    #[test]
    fn param_finds_key() {
        assert_eq!(param("sid=42&rev=7", "sid"), Some("42"));
        assert_eq!(param("sid=42&rev=7", "rev"), Some("7"));
        assert_eq!(param("sid=42&rev=7", "xxx"), None);
    }

    #[test]
    fn param_empty_query() {
        assert_eq!(param("", "sid"), None);
    }

    #[test]
    fn parse_u32_ok() {
        assert_eq!(parse_u32("sid=99", "sid"), Some(99));
        assert_eq!(parse_u32("sid=xyz", "sid"), None);
    }

    #[test]
    fn parse_bool_truthy_values() {
        assert!(parse_bool("log=1", "log"));
        assert!(parse_bool("log=true", "log"));
        assert!(!parse_bool("log=0", "log"));
        assert!(!parse_bool("", "log"));
    }

    #[test]
    fn parse_tile_src_defaults_to_s() {
        assert_eq!(parse_tile_src("src=S"), TileSrc::S);
        assert_eq!(parse_tile_src("src=B"), TileSrc::B);
        assert_eq!(parse_tile_src("src=O"), TileSrc::O);
        assert_eq!(parse_tile_src(""),      TileSrc::S);
        assert_eq!(parse_tile_src("src=X"), TileSrc::S);
    }

    // ── handle dispatch ────────────────────────────────────────────────────

    #[test]
    fn handle_unknown_path_returns_none() {
        assert!(handle("bogus", "").is_none());
    }

    #[test]
    fn handle_live_missing_sid_returns_none() {
        // sid= is required per ERRATA E3; without it → None.
        assert!(handle("live", "since=0").is_none());
    }

    #[test]
    fn handle_live_with_sid_no_panic() {
        // Hub may not be initialized; if not the call returns None cleanly.
        let _ = handle("live", "sid=4294967294&since=0");
    }

    #[test]
    fn handle_track_missing_sid_returns_none() {
        // `route_track` requires a sid= query param; without it → None.
        assert!(handle("track", "rev=0").is_none());
    }

    #[test]
    fn handle_resp_missing_sid_returns_none() {
        assert!(handle("resp", "f0=20&f1=20000&n=512&log=0").is_none());
    }

    #[test]
    fn handle_wave_missing_sid_returns_none() {
        assert!(handle("wave", "src=S&lod=0&i=0").is_none());
    }

    #[test]
    fn handle_spec_missing_sid_returns_none() {
        assert!(handle("spec", "src=S&lod=0&i=0").is_none());
    }
}
