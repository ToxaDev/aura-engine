//! The streamer as a network renderer: control points (JRiver, BubbleUPnP,
//! mConnect, Kazoo…) find it on the home network and send it tracks.
//! UPnP AV (MediaRenderer: AVTransport, RenderingControl, ConnectionManager)
//! and OpenHome (Product, Playlist, Time, Volume, Info) in one device.
//!
//! Its own HTTP port: the control and event requests are SOAP and GENA, not
//! the page's JSON, and are guarded by their own rules (`http`).
//! `AURA_UPNP=0` turns it off, `AURA_UPNP_PORT` picks the port,
//! `AURA_UPNP_NAME` the name control points show.

mod av;
mod desc;
mod gena;
mod http;
mod media;
mod oh;
mod renderer;
mod ssdp;
mod xml;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use renderer::owns;

use desc::Service;
use renderer::Renderer;

/// The port control points are told, when free.
const PORT: u16 = 49494;

/// Every service of the device, in the order the description lists them.
pub fn services() -> &'static [&'static Service] {
    static ALL: [&Service; 8] = [
        &desc::AV_TRANSPORT,
        &desc::RENDERING_CONTROL,
        &desc::CONNECTION_MANAGER,
        &oh::PRODUCT,
        &oh::PLAYLIST,
        &oh::TIME,
        &oh::VOLUME,
        &oh::INFO,
    ];
    &ALL
}

/// Who this renderer is, once started.
static NAMES: OnceLock<oh::Names> = OnceLock::new();
static SSDP: OnceLock<Arc<ssdp::Ssdp>> = OnceLock::new();

/// A service's evented variables now.
fn vars_of(r: &Renderer, short: &str) -> Vec<(&'static str, String)> {
    match short {
        "avt" => av::av_transport_vars(r),
        "rc" => av::rendering_control_vars(r),
        "cm" => av::connection_manager_vars(),
        "ohproduct" => NAMES.get().map(oh::product_vars).unwrap_or_default(),
        "ohplaylist" => oh::playlist_vars(r),
        "ohtime" => oh::time_vars(r),
        "ohvolume" => oh::volume_vars(r),
        "ohinfo" => oh::info_vars(r),
        _ => Vec::new(),
    }
}

/// What an event carries for `changed` variables: the AV services wrap them
/// in LastChange, the others send each one.
fn event_props(short: &str, changed: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    match short {
        "avt" => vec![("LastChange", av::last_change(av::AVT_EVENT_NS, changed, false))],
        "rc" => vec![("LastChange", av::last_change(av::RCS_EVENT_NS, changed, true))],
        _ => changed.to_vec(),
    }
}

/// An address on this machine or on the home network.
pub fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return private_ip(IpAddr::V4(m));
            }
            let first = v.segments()[0];
            v.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// The address other machines reach this one at (the one the system sends
/// from; no packet goes out).
fn lan_ipv4() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 168, 0, 1), 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v) if !v.is_unspecified() => Some(v),
        _ => None,
    }
}

/// A random UUID (version 4).
fn new_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The renderer's UUID, the same from run to run: control points remember
/// a renderer by it.
fn device_uuid(file: Option<&PathBuf>) -> String {
    let read = file.and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| {
        let v: serde_json::Value = serde_json::from_str(&t).ok()?;
        v["uuid"].as_str().filter(|u| u.len() == 36).map(str::to_string)
    });
    read.unwrap_or_else(|| {
        let u = new_uuid();
        if let Some(f) = file {
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(f, serde_json::json!({ "uuid": u }).to_string());
        }
        u
    })
}

/// Start the renderer: its HTTP port, discovery, events and the tick.
/// `data`: the streamer's folder; `web_port`: the page's port (the
/// device's presentation address).
pub async fn start(data: Option<PathBuf>, web_port: u16) {
    if std::env::var("AURA_UPNP").is_ok_and(|v| v == "0") {
        aura_core::aelog!("[UPNP] off (AURA_UPNP=0)");
        return;
    }
    let asked = std::env::var("AURA_UPNP_PORT").ok().and_then(|p| p.parse::<u16>().ok());
    let mut listener = None;
    for port in asked.map_or(vec![PORT, 0], |p| vec![p]) {
        if let Ok(l) = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await {
            listener = Some(l);
            break;
        }
    }
    let Some(listener) = listener else {
        aura_core::aelog!("[UPNP] off: no port to listen on");
        return;
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);

    let host = crate::host_name();
    let name = std::env::var("AURA_UPNP_NAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Aura Streamer".into());
    let uuid = device_uuid(data.as_ref().map(|d| d.join("upnp.json")).as_ref());
    let page = lan_ipv4().map(|ip| page_address(&ip.to_string(), web_port)).unwrap_or_default();
    let _ = NAMES.set(oh::Names { room: if host.is_empty() { name.clone() } else { host.clone() }, name: name.clone(), presentation: page });

    let r = Renderer::new(data.as_ref().map(|d| d.join("upnp-cache")), tokio::runtime::Handle::current());
    renderer::install(r.clone());
    let subs = Arc::new(gena::Subs::default());

    // The tick: a few times a second while the renderer plays, slower idle.
    {
        let r = r.clone();
        let _ = std::thread::Builder::new().name("upnp-tick".into()).spawn(move || loop {
            r.tick();
            let busy = r.transport() != renderer::Transport::NoMedia;
            std::thread::sleep(Duration::from_millis(if busy { 250 } else { 750 }));
        });
    }
    tokio::spawn(gena::run(subs.clone(), r.clone()));

    let ident = desc::Identity { uuid: uuid.clone(), friendly_name: name.clone(), version: env!("CARGO_PKG_VERSION"), presentation: None };
    let ctx = Arc::new(http::Ctx { r, subs, ident, web_port });
    tokio::spawn(http::serve(listener, ctx));

    let mut types = vec!["upnp:rootdevice".to_string(), format!("uuid:{uuid}"), desc::DEVICE_TYPE.to_string()];
    types.extend(services().iter().map(|s| s.urn.to_string()));
    let s = Arc::new(ssdp::Ssdp { uuid, http_port: port, types });
    let _ = SSDP.set(s.clone());
    ssdp::start(s);
    aura_core::aelog!("[UPNP] renderer \"{name}\" on port {port}");
}

fn page_address(host: &str, port: u16) -> String {
    if port == 80 {
        format!("http://{host}/")
    } else {
        format!("http://{host}:{port}/")
    }
}

/// On the way out: control points are told the renderer is gone.
pub fn stop() {
    if let Some(s) = SSDP.get() {
        s.announce(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_version_4_and_kept_from_run_to_run() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        let dir = std::env::temp_dir().join(format!("aura-upnp-test-{}", std::process::id()));
        let f = dir.join("upnp.json");
        let a = device_uuid(Some(&f));
        let b = device_uuid(Some(&f));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(a, b);
    }

    #[test]
    fn only_the_home_network_is_private() {
        for ip in ["192.168.1.5", "10.0.0.1", "172.16.4.4", "127.0.0.1", "169.254.1.1", "fe80::1", "::1"] {
            assert!(private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2001:4860::8888"] {
            assert!(!private_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
