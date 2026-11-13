//! The engine's side of the `aura` protocol: the player's status, the meters,
//! covers, the analyzer's tiles. The desktop app answers it to its webviews and
//! adds its own routes (update and download progress, radio icons); the
//! streamer answers it over HTTP.
//!
//! The pages ask it up to a hundred times a second. It used to be answered
//! on the thread that runs the windows, so a request waiting for the
//! player's state — a rack being rebuilt, a machine under full load — held
//! up painting and input. Now each request goes to one of two small lanes of
//! threads and the answer is handed back to the webview from there; every
//! webview (WebView2, WebKitGTK, WKWebView) takes an answer from any thread.
//!
//! The meters (the spectrum, HP's lamp and background, the scenes' spans)
//! have a lane of their own, so however long they wait for the player, the
//! status, the covers and the progress lines are answered by the other.
//! Every route only reads, and each of the page's pollers keeps one request
//! in flight, so answering them side by side changes no order the page sees.
//!
//! Every request is answered: one left without an answer would hang in the
//! webview for good. A handler that panics answers 500 and its thread goes
//! on to the next request.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

pub type Response = http::Response<Vec<u8>>;
type Reply = Box<dyn FnOnce(Response) + Send>;

/// Threads in each lane. A lane's pollers keep one request in flight each,
/// so two threads let one wait for the player while the other answers.
const LANE_THREADS: usize = 2;

/// The two lanes answering one front's requests with `route`.
pub struct Lanes {
    meters: Lane,
    rest: Lane,
}

impl Lanes {
    pub fn new(route: fn(&str) -> Response) -> Lanes {
        Lanes {
            meters: Lane::new("aura-meter", LANE_THREADS, route),
            rest: Lane::new("aura-proto", LANE_THREADS, route),
        }
    }

    /// Answer `uri` on a protocol thread; `reply` gets the answer there.
    pub fn serve(&self, uri: String, reply: impl FnOnce(Response) + Send + 'static) {
        let lane = if is_meter(split(&uri).0) { &self.meters } else { &self.rest };
        lane.serve(uri, Box::new(reply));
    }
}

/// The routes that read the playing sound's meters, at the page's frame rate.
fn is_meter(path: &str) -> bool {
    matches!(
        path,
        "/player/spectrum"
            | "/player/eq_spectrum"
            | "/player/phase_lamp"
            | "/player/phase_view"
            | "/player/spectrum_span"
            | "/player/wave_span"
            | "/player/spatial_span"
            | "/player/spatial_map"
    )
}

/// A few threads taking requests from one queue.
struct Lane {
    tx: mpsc::Sender<(String, Reply)>,
    work: fn(&str) -> Response,
}

impl Lane {
    fn new(name: &str, threads: usize, work: fn(&str) -> Response) -> Lane {
        let (tx, rx) = mpsc::channel::<(String, Reply)>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new().name(format!("{name}-{i}")).spawn(move || loop {
                // The lock is let go before the request is answered.
                let job = match rx.lock().unwrap_or_else(|p| p.into_inner()).recv() {
                    Ok(job) => job,
                    Err(_) => return,
                };
                answer(work, job);
            });
        }
        // Not one thread started: the queue's receiver is gone with them,
        // and `serve` answers on the caller's thread, as before.
        Lane { tx, work }
    }

    fn serve(&self, uri: String, reply: Reply) {
        if let Err(mpsc::SendError(job)) = self.tx.send((uri, reply)) {
            answer(self.work, job);
        }
    }
}

fn answer(work: fn(&str) -> Response, (uri, reply): (String, Reply)) {
    let r = catch_unwind(AssertUnwindSafe(|| work(&uri))).unwrap_or_else(|_| {
        crate::aelog!("[AURA] {}: the handler failed; answered 500", uri);
        status(500, b"internal error")
    });
    // Handing the answer over must not end the thread either.
    let _ = catch_unwind(AssertUnwindSafe(move || reply(r)));
}

/// An answer with no more to it than its code and a line of text.
pub fn status(code: u16, body: &[u8]) -> Response {
    aura_reply(http::Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .status(code)
        .body(body.to_vec()))
}

/// The path and the query of a request's address.
pub fn split(uri: &str) -> (&str, &str) {
    // On Windows, WebView2 maps the custom scheme aura:// to
    // https://aura.localhost/… before calling this handler.
    // Some wry versions hand it as aura://localhost/… instead.
    // Either way: strip everything up to and including the host.
    let after_host = uri
        .find("aura.localhost")
        .map(|p| &uri[p + "aura.localhost".len()..])
        .or_else(|| {
            // Fallback: aura://localhost/path or aura:///path
            uri.find("://")
                .and_then(|p| uri[p + 3..].find('/').map(|q| &uri[p + 3 + q..]))
        })
        .unwrap_or("/not-found");
    match after_host.find('?') {
        Some(q) => (&after_host[..q], &after_host[q + 1..]),
        None => (after_host, ""),
    }
}

// ── Cover cache ────────────────────────────────────────────────────────────
//
// The showcase mode of the player strip displays album art for the current
// (and potentially the next) track.  Loading a cover on every request would
// re-open and re-probe the file each time; this two-slot MRU keeps the most
// recently requested covers in memory so the strip's periodic repaint is free.
// The cache holds at most two entries so it never retains artwork for the
// whole queue.

struct CoverCache {
    /// Entries in MRU order: index 0 is the most recently requested.
    slots: Vec<(u64, Vec<u8>, String)>,
}

impl CoverCache {
    fn new() -> Self {
        Self { slots: Vec::with_capacity(2) }
    }

    fn get(&self, id: u64) -> Option<(Vec<u8>, String)> {
        self.slots
            .iter()
            .find(|(i, _, _)| *i == id)
            .map(|(_, d, ct)| (d.clone(), ct.clone()))
    }

    fn put(&mut self, id: u64, data: Vec<u8>, ct: String) {
        self.slots.retain(|(i, _, _)| *i != id);
        self.slots.insert(0, (id, data, ct));
        self.slots.truncate(2);
    }
}

static COVER_CACHE: OnceLock<Mutex<CoverCache>> = OnceLock::new();

fn cover_cache() -> &'static Mutex<CoverCache> {
    COVER_CACHE.get_or_init(|| Mutex::new(CoverCache::new()))
}

/// An answer of the `aura` protocol. A header the response cannot carry (a
/// cover's content type read from the file, say) is a 500, not a panic.
pub fn aura_reply(r: http::Result<Response>) -> Response {
    r.unwrap_or_else(|_| {
        let mut bad = http::Response::new(b"bad response".to_vec());
        *bad.status_mut() = http::StatusCode::INTERNAL_SERVER_ERROR;
        bad
    })
}

/// A 200 that is not kept: everything here changes from one asking to the next.
pub fn ok(body: Vec<u8>, ct: &str) -> Response {
    aura_reply(http::Response::builder()
        .header("Content-Type", ct)
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .status(200)
        .body(body))
}

/// The engine's answer to `path`, whatever thread asks; 404 when it has none.
pub fn engine_route(path: &str, query_str: &str) -> Response {
    use crate::{player, spatial};

    let fail = status;

    if path == "/player/status" {
        let v = player::controller::get().status();
        let json = serde_json::to_vec(&v).unwrap_or_default();
        return ok(json, "application/json");
    }

    if path == "/player/hp_prepare" {
        // The Hybrid-Phase lists the start waits for: {"state","progress",
        // "stage","track","eta_s"} (polled while "preparing").
        let json = serde_json::to_vec(&player::controller::get().hp_prepare()).unwrap_or_default();
        return ok(json, "application/json");
    }

    if path == "/player/cover" {
        // Serve the album-art image for a queued track.
        // The id is a track id that must exist in the current queue;
        // arbitrary paths are never accepted.
        let id: Option<u64> = query_str
            .split('&')
            .find_map(|kv| kv.strip_prefix("id="))
            .and_then(|v| v.parse().ok());
        let Some(id) = id else {
            return fail(400, b"bad request");
        };

        // Fast path: cover already cached.
        if let Some((data, ct)) = cover_cache().lock().unwrap_or_else(|p| p.into_inner()).get(id) {
            return ok(data, &ct);
        }

        // Look up the track path from the current queue.
        let track_path: Option<std::path::PathBuf> = player::controller::get()
            .queue()
            .into_iter()
            .find(|t| t.id == id)
            .map(|t| std::path::PathBuf::from(&t.path));
        let Some(track_path) = track_path else {
            return fail(404, b"not found");
        };

        // Load the cover (no lock held while reading from disk).
        match player::probe::probe_cover(&track_path) {
            Some((data, ct)) => {
                cover_cache().lock().unwrap_or_else(|p| p.into_inner()).put(id, data.clone(), ct.clone());
                return ok(data, &ct);
            }
            None => {
                return fail(404, b"no cover");
            }
        }
    }

    if path == "/player/spectrum" {
        // Parse optional ?bands=N&ahead_ms=M query parameters.
        let bands: usize = query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix("bands="))
            .and_then(|v: &str| v.parse::<usize>().ok())
            .unwrap_or(48)
            .min(128)
            .max(1);
        let ahead_ms: f64 = query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix("ahead_ms="))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(35.0);
        let spectrum = player::controller::get().spectrum(bands, ahead_ms);
        return ok(spectrum, "application/octet-stream");
    }

    if path == "/player/eq_spectrum" {
        // ?n=N&res=R&ahead_ms=M — the EQ window's analyzer, before and
        // after the EQ (`player::eq_spectrum`).
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let n = (num("n=", 320.0) as usize).clamp(2, player::eq_spectrum::MAX_POINTS);
        let res = num("res=", 1.0).clamp(0.0, 3.0) as u8;
        let reply = player::controller::get().eq_spectrum(n, res, num("ahead_ms=", 35.0));
        return ok(reply, "application/octet-stream");
    }

    if path == "/player/phase_lamp" {
        // ?ahead_ms=M&back_ms=W: [HP, αHP, state], the lamps 0..255
        // each, at the frame /player/spectrum centres on, the peak over
        // the W ms before it (the time since the page last asked,
        // 8..50 ms); state: where they come from (`PlanState`: 0 live,
        // 1 a converted file's timeline, 2 pending, 3.. why none).
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let lamp = player::controller::get().phase_lamp(
            num("ahead_ms=", 35.0), num("back_ms=", 16.0).clamp(8.0, 50.0));
        return ok(lamp.to_vec(), "application/octet-stream");
    }

    if path == "/player/phase_view" {
        // ?ahead_ms=M&from_ms=A&to_ms=B&n=N — the rack's background
        // under HP: N bins of the switch (or α) and the detector
        // envelope from A to B ms around the lamp's frame
        // (`PhaseMeter::view`); empty while nothing blends. The
        // last byte, after the bins, is the sensitivity step in
        // effect (i8, −3..+3: the live chain's, or the one a
        // converted file was made with).
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let view = player::controller::get().phase_view(
            num("ahead_ms=", 35.0), num("from_ms=", -1600.0), num("to_ms=", 400.0),
            num("n=", 200.0) as usize);
        return ok(view, "application/octet-stream");
    }

    if path == "/player/spectrum_span" {
        // ?bands=N&from_ms=A&to_ms=B&step_ms=S — the big player's waves.
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let bands = (num("bands=", 48.0) as usize).clamp(1, 128);
        let span = player::controller::get().spectrum_span(
            bands, num("from_ms=", 0.0), num("to_ms=", 2000.0), num("step_ms=", 40.0),
            num("extra=", 0.0) > 0.0);
        return ok(span, "application/octet-stream");
    }

    if path == "/player/wave_span" {
        // ?from_ms=A&to_ms=B&rate=R — the visualization scenes' waveform.
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let span = player::controller::get().wave_span(
            num("from_ms=", -250.0), num("to_ms=", 500.0), num("rate=", 24000.0));
        return ok(span, "application/octet-stream");
    }

    if path == "/player/spatial_span" {
        // ?from_ms=A&to_ms=B — the spatial scenes' sound objects.
        let num = |key: &str, dflt: f64| query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix(key))
            .and_then(|v: &str| v.parse::<f64>().ok())
            .unwrap_or(dflt);
        let span = spatial::span(num("from_ms=", -6000.0), num("to_ms=", 3500.0));
        return ok(span, "application/octet-stream");
    }

    if path == "/player/spatial_map" {
        // ?id=N — a track map's objects and notes (once per map; span's last header field)
        let id = query_str.split('&')
            .find_map(|kv: &str| kv.strip_prefix("id="))
            .and_then(|v: &str| v.parse::<u32>().ok())
            .unwrap_or(0);
        if let Some(j) = spatial::map_json(id) {
            return ok(j.into_bytes(), "application/json");
        }
    }

    // analytics: dispatch to analyzer routes (§2.2)
    if let Some(tail) = path.strip_prefix("/player/an/") {
        if let Some(bytes) = player::analytics::routes::handle(tail, query_str) {
            return ok(bytes.to_vec(), "application/octet-stream");
        }
    }

    fail(404, b"not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn header<'a>(r: &'a Response, name: &str) -> &'a str {
        r.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    /// Ask `lane` for `uri`; the answer and the thread that gave it.
    fn ask(lane: &Lane, uri: &str) -> (Response, String) {
        let (tx, rx) = mpsc::channel();
        lane.serve(uri.to_string(), Box::new(move |r| {
            let _ = tx.send((r, std::thread::current().name().unwrap_or("").to_string()));
        }));
        rx.recv_timeout(Duration::from_secs(5)).expect("every request is answered")
    }

    fn echo(uri: &str) -> Response {
        if uri.ends_with("/boom") {
            panic!("a handler's bug");
        }
        status(200, uri.as_bytes())
    }

    #[test]
    fn both_forms_of_the_address_give_the_path_and_the_query() {
        assert_eq!(split("https://aura.localhost/player/status"), ("/player/status", ""));
        assert_eq!(split("http://aura.localhost/player/cover?id=7"), ("/player/cover", "id=7"));
        assert_eq!(split("aura://localhost/player/spectrum?bands=48&ahead_ms=3"),
            ("/player/spectrum", "bands=48&ahead_ms=3"));
        assert_eq!(split("aura:///update/status"), ("/update/status", ""));
        assert_eq!(split("nonsense"), ("/not-found", ""));
    }

    #[test]
    fn the_meters_have_their_own_lane_and_the_status_does_not_share_it() {
        for p in ["/player/spectrum", "/player/eq_spectrum", "/player/phase_lamp", "/player/phase_view", "/player/spectrum_span",
            "/player/wave_span", "/player/spatial_span", "/player/spatial_map"] {
            assert!(is_meter(p), "{p}");
        }
        for p in ["/player/status", "/player/hp_prepare", "/update/status", "/filters/status", "/player/cover", "/radio/icon",
            "/player/an/live", "/player/an/stream"] {
            assert!(!is_meter(p), "{p}");
        }
    }

    #[test]
    fn routes_answer_with_their_codes_and_headers_as_before() {
        let r = engine_route("/no/such/route", "");
        assert_eq!(r.status(), 404);
        assert_eq!(header(&r, "Access-Control-Allow-Origin"), "*");
        assert_eq!(r.body().as_slice(), b"not found");

        let r = engine_route("/player/cover", "");
        assert_eq!(r.status(), 400, "a cover without an id");
        assert_eq!(header(&r, "Access-Control-Allow-Origin"), "*");
    }

    #[test]
    fn a_handler_that_panics_answers_500_and_its_thread_goes_on() {
        let lane = Lane::new("aura-test-panic", 1, echo);
        let (r, _) = ask(&lane, "https://aura.localhost/boom");
        assert_eq!(r.status(), 500);
        assert_eq!(header(&r, "Access-Control-Allow-Origin"), "*");
        let (r, who) = ask(&lane, "https://aura.localhost/after");
        assert_eq!(r.status(), 200, "the lane's only thread is still there");
        assert_eq!(who, "aura-test-panic-0");
    }

    #[test]
    fn requests_from_many_threads_are_all_answered_off_the_asking_thread() {
        let lane = Arc::new(Lane::new("aura-test-many", 2, echo));
        let (tx, rx) = mpsc::channel();
        let askers: Vec<_> = (0..4).map(|t| {
            let (lane, tx) = (Arc::clone(&lane), tx.clone());
            std::thread::spawn(move || for i in 0..50 {
                let tx = tx.clone();
                lane.serve(format!("https://aura.localhost/{t}/{i}"), Box::new(move |r| {
                    let _ = tx.send((r.status(), std::thread::current().name().map(str::to_string)));
                }));
            })
        }).collect();
        for a in askers { a.join().unwrap(); }
        for _ in 0..200 {
            let (code, who) = rx.recv_timeout(Duration::from_secs(5)).expect("answered");
            assert_eq!(code, 200);
            assert!(who.unwrap_or_default().starts_with("aura-test-many-"));
        }
    }

    #[test]
    fn a_lane_without_threads_answers_on_the_asking_thread() {
        let lane = Lane::new("aura-test-none", 0, echo);
        let (r, who) = ask(&lane, "https://aura.localhost/x");
        assert_eq!(r.status(), 200);
        assert_eq!(who, std::thread::current().name().unwrap_or(""));
    }

    static GATE: Mutex<()> = Mutex::new(());

    fn held(uri: &str) -> Response {
        let _g = GATE.lock().unwrap_or_else(|p| p.into_inner());
        status(200, uri.as_bytes())
    }

    #[test]
    fn a_lane_waiting_for_the_player_leaves_the_other_one_free() {
        let meters = Lane::new("aura-test-held", 2, held);
        let rest = Lane::new("aura-test-free", 2, echo);
        let gate = GATE.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        for i in 0..4 {
            let tx = tx.clone();
            meters.serve(format!("https://aura.localhost/m{i}"), Box::new(move |r| { let _ = tx.send(r.status()); }));
        }
        let (r, _) = ask(&rest, "https://aura.localhost/player/status");
        assert_eq!(r.status(), 200, "answered while every meter thread waits");
        assert!(rx.try_recv().is_err(), "the meters are still waiting");
        drop(gate);
        for _ in 0..4 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 200);
        }
    }
}
