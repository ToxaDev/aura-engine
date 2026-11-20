//! The renderer's HTTP side: its descriptions, the SOAP control of each
//! service, and event subscriptions (SUBSCRIBE and UNSUBSCRIBE, which are
//! not methods a router knows: one handler takes every request).
//!
//! Who may ask: the home network only, by an address or a local name. A
//! control request carries a SOAPACTION header, which no web page can send
//! here without a preflight nobody answers.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use super::av::{self, Fault, Reply};
use super::desc::{self, Service};
use super::gena::{self, Subs};
use super::renderer::Renderer;
use super::xml;

const ICON: &[u8] = include_bytes!("../../../../desktop-app/src-tauri/icons/128x128.png");
const MAX_BODY: usize = 1 << 20;

pub struct Ctx {
    pub r: Arc<Renderer>,
    pub subs: Arc<Subs>,
    pub ident: desc::Identity,
    pub web_port: u16,
}

pub async fn serve(listener: tokio::net::TcpListener, ctx: Arc<Ctx>) {
    let app = Router::new().fallback(handle).with_state(ctx).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app).await {
        aura_core::aelog!("[UPNP] the server stopped: {e}");
    }
}

fn host_of(headers: &HeaderMap) -> String {
    let h = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        h.rsplit_once(':').map_or(h, |(a, _)| a)
    };
    host.to_ascii_lowercase()
}

/// An address on the home network, or a name only it has (DNS rebinding:
/// a web page's own domain pointed at this machine is turned away).
fn local_host(host: &str) -> bool {
    if host.is_empty() {
        // HTTP/1.0 control points may send none.
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => super::private_ip(ip),
        Err(_) => !host.contains('.') || host.ends_with(".local") || host.ends_with(".localhost"),
    }
}

fn plain(status: StatusCode) -> Response {
    status.into_response()
}

fn xml_reply(status: StatusCode, body: String) -> Response {
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=\"utf-8\""));
    h.insert("EXT", HeaderValue::from_static(""));
    if let Ok(v) = HeaderValue::from_str(&server_word()) {
        h.insert(header::SERVER, v);
    }
    r
}

fn server_word() -> String {
    format!("{}/1.0 UPnP/1.0 AuraStreamer/{}", std::env::consts::OS, env!("CARGO_PKG_VERSION"))
}

async fn handle(State(ctx): State<Arc<Ctx>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request) -> Response {
    if !super::private_ip(peer.ip()) || !local_host(&host_of(req.headers())) {
        return plain(StatusCode::FORBIDDEN);
    }
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_ascii_uppercase();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET" | "HEAD", ["upnp", "device.xml"]) => {
            // The page's address as this control point reaches the machine.
            let host = host_of(req.headers());
            let mut ident = desc::Identity {
                uuid: ctx.ident.uuid.clone(),
                friendly_name: ctx.ident.friendly_name.clone(),
                version: ctx.ident.version,
                presentation: None,
            };
            if !host.is_empty() {
                ident.presentation = Some(super::page_address(&host, ctx.web_port));
            }
            xml_reply(StatusCode::OK, desc::device(&ident, super::services()))
        }
        ("GET" | "HEAD", ["upnp", "icon.png"]) => ([(header::CONTENT_TYPE, "image/png")], ICON).into_response(),
        ("GET" | "HEAD", ["upnp", short, "scpd.xml"]) => match service(short) {
            Some(s) => xml_reply(StatusCode::OK, desc::scpd(s)),
            None => plain(StatusCode::NOT_FOUND),
        },
        ("POST", ["upnp", short, "control"]) => {
            let Some(s) = service(short) else { return plain(StatusCode::NOT_FOUND) };
            control(ctx, s, req).await
        }
        ("SUBSCRIBE", ["upnp", short, "event"]) => {
            let Some(s) = service(short) else { return plain(StatusCode::NOT_FOUND) };
            subscribe(&ctx, s, req.headers())
        }
        ("UNSUBSCRIBE", ["upnp", short, "event"]) => {
            if service(short).is_none() {
                return plain(StatusCode::NOT_FOUND);
            }
            let sid = req.headers().get("SID").and_then(|v| v.to_str().ok()).unwrap_or("");
            if ctx.subs.unsubscribe(sid.trim()) {
                plain(StatusCode::OK)
            } else {
                plain(StatusCode::PRECONDITION_FAILED)
            }
        }
        _ => plain(StatusCode::NOT_FOUND),
    }
}

fn service(short: &str) -> Option<&'static Service> {
    super::services().iter().copied().find(|s| s.short == short)
}

fn subscribe(ctx: &Ctx, s: &'static Service, h: &HeaderMap) -> Response {
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
    let timeout = gena::timeout_of(get("TIMEOUT"));
    let sid = match (get("SID"), get("CALLBACK"), get("NT")) {
        // A renewal.
        (Some(sid), None, None) => {
            if !ctx.subs.renew(sid, timeout) {
                return plain(StatusCode::PRECONDITION_FAILED);
            }
            sid.to_string()
        }
        (None, Some(cb), Some("upnp:event")) => {
            let callbacks = gena::callbacks(cb);
            if callbacks.is_empty() {
                return plain(StatusCode::PRECONDITION_FAILED);
            }
            let sid = ctx.subs.subscribe(s.short, callbacks, timeout);
            // The first event goes out after this answer.
            let r = ctx.r.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                r.changed.notify_waiters();
            });
            sid
        }
        (Some(_), _, _) => return plain(StatusCode::BAD_REQUEST),
        _ => return plain(StatusCode::PRECONDITION_FAILED),
    };
    let mut r = plain(StatusCode::OK);
    let hm = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&sid) {
        hm.insert("SID", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("Second-{timeout}")) {
        hm.insert("TIMEOUT", v);
    }
    if let Ok(v) = HeaderValue::from_str(&server_word()) {
        hm.insert(header::SERVER, v);
    }
    hm.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    r
}

async fn control(ctx: Arc<Ctx>, s: &'static Service, req: Request) -> Response {
    let soap_action = req
        .headers()
        .get("SOAPACTION")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().trim_matches('"').to_string());
    let Some(soap_action) = soap_action else {
        return plain(StatusCode::BAD_REQUEST);
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let Some(call) = xml::soap_call(&body) else {
        return fault(av::INVALID_ACTION);
    };
    // The header names the service and the action the body carries.
    let named = soap_action.rsplit_once('#').map(|(_, a)| a == call.action).unwrap_or(false);
    if !named {
        return fault(av::INVALID_ACTION);
    }
    let action = call.action.clone();
    let reply: Reply = {
        let r = ctx.r.clone();
        let short = s.short;
        tokio::task::spawn_blocking(move || match short {
            "avt" => av::av_transport(&r, &call),
            "rc" => av::rendering_control(&r, &call),
            "cm" => av::connection_manager(&r, &call),
            "ohproduct" => match super::NAMES.get() {
                Some(n) => super::oh::product(&call, n),
                None => Err(av::INVALID_ACTION),
            },
            "ohplaylist" => super::oh::playlist(&r, &call),
            "ohtime" => super::oh::time(&r, &call),
            "ohvolume" => super::oh::volume(&r, &call),
            "ohinfo" => super::oh::info(&r, &call),
            _ => Err(av::INVALID_ACTION),
        })
        .await
        .unwrap_or(Err((501, "Action Failed")))
    };
    match reply {
        Ok(out) => xml_reply(StatusCode::OK, envelope(&action, s.urn, &out)),
        Err(f) => fault(f),
    }
}

const ENVELOPE_OPEN: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
     s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>";

fn envelope(action: &str, urn: &str, out: &[(&str, String)]) -> String {
    let mut x = String::from(ENVELOPE_OPEN);
    x.push_str(&format!("<u:{action}Response xmlns:u=\"{urn}\">"));
    for (name, value) in out {
        x.push_str(&format!("<{name}>{}</{name}>", xml::esc(value)));
    }
    x.push_str(&format!("</u:{action}Response></s:Body></s:Envelope>"));
    x
}

fn fault((code, words): Fault) -> Response {
    let body = format!(
        "{ENVELOPE_OPEN}<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
         <errorDescription>{words}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    );
    xml_reply(StatusCode::INTERNAL_SERVER_ERROR, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_names_are_answered() {
        for h in ["192.168.1.4", "aura-pc", "aura.local", "", "127.0.0.1"] {
            assert!(local_host(h), "{h}");
        }
        for h in ["evil.example", "8.8.8.8"] {
            assert!(!local_host(h), "{h}");
        }
    }

    #[test]
    fn an_answer_and_a_fault_are_soap() {
        let e = envelope("GetVolume", "urn:schemas-upnp-org:service:RenderingControl:1", &[("CurrentVolume", "40".into())]);
        let c = xml::soap_call(&e).unwrap();
        assert_eq!(c.action, "GetVolumeResponse");
        assert_eq!(c.arg("CurrentVolume"), "40");
    }
}
