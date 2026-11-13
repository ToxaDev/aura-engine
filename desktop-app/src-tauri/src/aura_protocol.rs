//! The app's `aura` protocol: the engine's routes (`aura_core::protocol`), and
//! the app's own: update and download progress, radio icons.

use aura_core::protocol::{self, Lanes, Response};
use std::sync::OnceLock;

/// Answer `uri` on a protocol thread; `reply` gets the answer there.
pub fn serve(uri: String, reply: impl FnOnce(Response) + Send + 'static) {
    static LANES: OnceLock<Lanes> = OnceLock::new();
    LANES.get_or_init(|| Lanes::new(route)).serve(uri, reply);
}

/// The answer to one request, whatever thread asks.
fn route(uri: &str) -> Response {
    use crate::{filters_dl, radio_catalog, updater};

    let (path, query_str) = protocol::split(uri);

    if path == "/update/status" {
        return protocol::ok(updater::status_json(), "application/json");
    }

    if path == "/filters/status" {
        return protocol::ok(filters_dl::status_json(), "application/json");
    }

    if path == "/radio/icon" {
        // A station's icon, from the catalog's own folder only
        // (`radio_icon` fetched it): this never waits for the network.
        let key = query_str.split('&').find_map(|kv| kv.strip_prefix("k=")).unwrap_or("");
        return match radio_catalog::icon_file(key) {
            Some((data, ct)) => protocol::aura_reply(tauri::http::Response::builder()
                .header("Content-Type", ct)
                .header("X-Content-Type-Options", "nosniff")
                .header("Cache-Control", "max-age=86400")
                .header("Access-Control-Allow-Origin", "*")
                .status(200)
                .body(data)),
            None => protocol::status(404, b"no icon"),
        };
    }

    protocol::engine_route(path, query_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(r: &'a Response, name: &str) -> &'a str {
        r.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    #[test]
    fn the_apps_routes_answer_and_the_rest_go_to_the_engine() {
        let r = route("aura://localhost/update/status");
        assert_eq!(r.status(), 200);
        assert_eq!(header(&r, "Content-Type"), "application/json");
        assert_eq!(header(&r, "Cache-Control"), "no-store");
        assert_eq!(header(&r, "Access-Control-Allow-Origin"), "*");

        let r = route("https://aura.localhost/no/such/route");
        assert_eq!(r.status(), 404);
        assert_eq!(r.body().as_slice(), b"not found");
    }
}
