//! The HTTP server: the web remote's files, its commands, the engine's `aura`
//! routes and its events. For the home network only (`guard`).

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aura_core::protocol::{self, Lanes};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::kv::Kv;
use crate::library::Library;
use crate::{assets, dispatch};

/// What identifies this program to anyone who asks `/api/whoami`.
pub const WHO: &str = "aura-streamer";

/// The largest request: a playlist can be long.
const MAX_BODY: usize = 16 << 20;

#[derive(Clone)]
pub struct Shared {
    pub lib: Arc<Library>,
    pub kv: Arc<Kv>,
    pub events: broadcast::Sender<String>,
}

impl Shared {
    /// To every open page.
    pub fn emit(&self, name: &str, payload: Value) {
        // Nobody listening is not a failure.
        let _ = self.events.send(json!({ "event": name, "payload": payload }).to_string());
    }
}

pub fn router(shared: Shared) -> Router {
    Router::new()
        .route("/api/whoami", get(whoami))
        .route("/api/invoke/{cmd}", post(invoke))
        .route("/api/kv", get(kv_all).post(kv_set))
        .route("/api/events", get(events))
        .route("/aura/{*rest}", get(aura))
        .route("/api/lib/cover/{id}", get(lib_cover))
        .fallback(file)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(guard))
        .with_state(shared)
}

/// Serve until `stop` resolves. `addr` is bound already.
pub async fn serve(
    listener: tokio::net::TcpListener,
    shared: Shared,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = router(shared).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app).with_graceful_shutdown(stop).await
}

// ── who may ask ────────────────────────────────────────────────────────────

/// A peer on this machine or on the local network.
fn private_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return private_peer(IpAddr::V4(m));
            }
            let first = v.segments()[0];
            v.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// The host part of a `Host` header or of an origin's address, lower case.
fn host_of(s: &str) -> String {
    let s = s.split("://").last().unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    let host = if let Some(rest) = s.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        s.rsplit_once(':').map_or(s, |(h, _)| h)
    };
    host.to_ascii_lowercase()
}

/// A name that can only mean something on the home network: an address, the
/// machine's bare name, `localhost`, `*.local`, `*.localhost`. Any other name
/// that reaches here is a web page's own domain pointed at this machine (DNS
/// rebinding), and is turned away.
fn local_host_name(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return private_peer(ip);
    }
    !host.contains('.') || host.ends_with(".local") || host.ends_with(".localhost")
}

/// Who may ask: a peer on the local network, by a local name. A request that
/// changes something must come from this server's own page: its `Origin` names
/// this host, and it carries JSON (which a foreign page cannot send here
/// without a preflight nobody answers).
fn allowed(peer: IpAddr, method: &Method, headers: &HeaderMap) -> Result<(), &'static str> {
    if !private_peer(peer) {
        return Err("This server answers the home network only");
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(host_of).unwrap_or_default();
    if !local_host_name(&host) {
        return Err("Unknown host name");
    }
    if method != Method::GET && method != Method::HEAD {
        if let Some(o) = headers.get(header::ORIGIN) {
            if o.to_str().map(host_of).unwrap_or_default() != host {
                return Err("A request from another site");
            }
        }
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("application/json"));
        if !json {
            return Err("A request must carry JSON");
        }
    }
    Ok(())
}

async fn guard(ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if let Err(why) = allowed(peer.ip(), req.method(), req.headers()) {
        aura_core::aelog!("[STREAMER] refused {} {} from {}: {}", req.method(), req.uri().path(), peer.ip(), why);
        return (StatusCode::FORBIDDEN, why).into_response();
    }
    next.run(req).await
}

// ── the API ────────────────────────────────────────────────────────────────

async fn whoami() -> Json<Value> {
    Json(json!({
        "who": WHO,
        "version": env!("CARGO_PKG_VERSION"),
        "name": crate::host_name(),
    }))
}

/// A command, on the blocking pool: it may wait for the player or the disk.
/// Tauri's way of answering: the value, or the error as a string.
async fn invoke(State(s): State<Shared>, Path(cmd): Path<String>, body: axum::body::Bytes) -> Response {
    let args: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!(format!("bad JSON: {e}")))).into_response(),
        }
    };
    let lib = s.lib.clone();
    let r = tokio::task::spawn_blocking(move || dispatch::dispatch(&lib, &cmd, args)).await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!(e))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!(e.to_string()))).into_response(),
    }
}

async fn kv_all(State(s): State<Shared>) -> Response {
    let mut r = Json(s.kv.all()).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    r
}

#[derive(serde::Deserialize)]
struct KvWrite {
    key: String,
    value: Option<String>,
    /// The page that wrote it: it does not need telling.
    from: Option<String>,
}

async fn kv_set(State(s): State<Shared>, Json(w): Json<KvWrite>) -> Response {
    let changed = json!({ "key": w.key, "value": w.value, "from": w.from });
    match s.kv.set(w.key, w.value) {
        Ok(()) => {
            s.emit("kv:changed", changed);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::PAYLOAD_TOO_LARGE, Json(json!(e))).into_response(),
    }
}

/// The engine's events and other pages' writes, as they happen.
async fn events(State(s): State<Shared>) -> impl IntoResponse {
    let rx = s.events.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(text) => return Some((Ok::<_, Infallible>(Event::default().data(text)), rx)),
                // Too slow for a moment: what it missed is gone, the next one comes.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// An album's cover, small. Its address changes with the picture, so it keeps.
async fn lib_cover(State(s): State<Shared>, Path(id): Path<String>) -> Response {
    let lib = s.lib.clone();
    match tokio::task::spawn_blocking(move || lib.index.cover(&id)).await {
        Ok(Some(jpg)) => (
            [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "max-age=2592000, immutable")],
            jpg,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── the engine's routes ────────────────────────────────────────────────────

fn route(uri: &str) -> protocol::Response {
    let read = clock_ms();
    let (path, query) = protocol::split(uri);
    let mut r = protocol::engine_route(path, query);
    // When the engine took up the request (a meter reads the heard frame
    // first): the page times its blocks by it (`stamp`).
    r.extensions_mut().insert(ReadAt(read));
    r
}

/// The server's own clock, ms since it first looked: the web remote sets
/// its clock of the heard frame by it (meterfeed.js `noteClock`).
fn clock_ms() -> f64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// When the engine took up a request, on `clock_ms`.
#[derive(Clone, Copy)]
struct ReadAt(f64);

/// The header the remote reads: `<read>;<send>` on `clock_ms` — when the
/// engine took the request up, and when the answer left. The page bounds
/// the offset between the two clocks from many answers (the request cannot
/// arrive before it was sent, the answer not before it left), so an answer
/// slow one way (a phone's Wi-Fi sleeping between packets) no longer moves
/// its meters late by half its round trip (Anton 11.11: tens of ms behind
/// the PC).
const CLOCK_HEADER: &str = "x-aura-clock";

fn stamp(headers: &mut HeaderMap, read: f64) {
    if let Ok(v) = format!("{:.3};{:.3}", read, clock_ms()).parse() {
        headers.insert(CLOCK_HEADER, v);
    }
}

/// `/aura/player/status` is the protocol's `/player/status`, answered on its
/// lanes: a meter that waits for the player holds up nothing else.
async fn aura(uri: Uri) -> Response {
    static LANES: OnceLock<Lanes> = OnceLock::new();
    let tail = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let tail = tail.strip_prefix("/aura").unwrap_or(tail);
    // The streamer's own update and the filter download, followed by the page.
    let status = match tail.split('?').next() {
        Some("/update/status") => Some(crate::update::status_json()),
        Some("/filters/status") => Some(aura_core::filters_dl::status_json()),
        _ => None,
    };
    if let Some(body) = status {
        return ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response();
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    LANES.get_or_init(|| Lanes::new(route)).serve(format!("aura://localhost{tail}"), move |r| {
        let _ = tx.send(r);
    });
    match rx.await {
        Ok(r) => {
            let (mut parts, body) = r.into_parts();
            // The webview needs it; on a network it would let any site read these.
            parts.headers.remove(header::ACCESS_CONTROL_ALLOW_ORIGIN);
            if let Some(ReadAt(read)) = parts.extensions.remove::<ReadAt>() {
                stamp(&mut parts.headers, read);
            }
            Response::from_parts(parts, Body::from(body))
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ── the files ──────────────────────────────────────────────────────────────

async fn file(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some(bytes) = assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ct = assets::content_type(path);
    let body = if path.ends_with(".html") {
        Body::from(assets::serve_page(path, &String::from_utf8_lossy(&bytes)))
    } else {
        Body::from(bytes.into_owned())
    };
    Response::builder()
        .header(header::CONTENT_TYPE, ct)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(list: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in list {
            h.insert(k, v.parse().unwrap());
        }
        h
    }

    const LAN: &str = "192.168.1.5";

    fn peer() -> IpAddr {
        LAN.parse().unwrap()
    }

    /// The engine's answers carry the clock: when the request was taken up,
    /// then when the answer left, on the server's own clock.
    #[test]
    fn an_engine_answer_carries_the_servers_clock() {
        let r = route("aura://localhost/player/nothing-here");
        let ReadAt(read) = *r.extensions().get::<ReadAt>().expect("the read time");
        let mut h = HeaderMap::new();
        stamp(&mut h, read);
        let v = h.get(CLOCK_HEADER).and_then(|v| v.to_str().ok()).expect("the header").to_string();
        let (a, b) = v.split_once(';').expect("read;send");
        let (a, b): (f64, f64) = (a.parse().unwrap(), b.parse().unwrap());
        assert!(a >= 0.0 && b >= a && (a - read).abs() < 1e-3, "{v}");
    }

    #[test]
    fn only_the_local_network_is_answered() {
        for ip in ["127.0.0.1", "192.168.1.5", "10.0.0.2", "172.16.3.4", "169.254.1.1", "::1", "fe80::1", "fd00::1", "::ffff:192.168.1.5"] {
            assert!(private_peer(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!private_peer(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_local_name_passes_and_a_web_domain_does_not() {
        for h in ["192.168.1.40:8088", "192.168.1.40", "localhost:80", "aura.local", "AURA.local:80", "DESKTOP-1", "[::1]:80", "x.localhost"] {
            let got = allowed(peer(), &Method::GET, &headers(&[(header::HOST, h)]));
            assert!(got.is_ok(), "{h}: {got:?}");
        }
        for h in ["evil.example.com", "evil.example.com:80", "8.8.8.8", ""] {
            assert!(allowed(peer(), &Method::GET, &headers(&[(header::HOST, h)])).is_err(), "{h:?}");
        }
        assert!(allowed(peer(), &Method::GET, &HeaderMap::new()).is_err(), "no Host at all");
    }

    #[test]
    fn a_request_from_outside_is_refused_whatever_it_says() {
        let h = headers(&[(header::HOST, "192.168.1.40")]);
        assert!(allowed("8.8.8.8".parse().unwrap(), &Method::GET, &h).is_err());
    }

    #[test]
    fn a_change_must_come_from_this_servers_own_page_with_json() {
        let json_ct = (header::CONTENT_TYPE, "application/json");
        let host = (header::HOST, "192.168.1.40:8088");
        let own = (header::ORIGIN, "http://192.168.1.40:8088");
        let foreign = (header::ORIGIN, "https://evil.example.com");
        assert!(allowed(peer(), &Method::POST, &headers(&[host.clone(), own.clone(), json_ct.clone()])).is_ok());
        assert!(allowed(peer(), &Method::POST, &headers(&[host.clone(), json_ct.clone()])).is_ok(), "no origin: not a browser's cross-site request");
        assert!(allowed(peer(), &Method::POST, &headers(&[host.clone(), foreign, json_ct.clone()])).is_err());
        assert!(allowed(peer(), &Method::POST, &headers(&[host.clone(), own, (header::CONTENT_TYPE, "text/plain")])).is_err());
        assert!(allowed(peer(), &Method::POST, &headers(&[host])).is_err(), "no JSON");
    }
}
