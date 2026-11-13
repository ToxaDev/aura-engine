//! Aura Streamer: the engine with no window, run from a browser on the home
//! network. It sits on the PC next to the DAC; a phone or a tablet opens its page.
//!
//! Stays a console program for now: the console is the audit log, as in the app.

mod assets;
mod dispatch;
mod kv;
mod library;
mod server;
mod update;
mod upnp;

use aura_core::events::Events;
use aura_core::{audio, player, spatial, watchdog};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// The port of a plain address, and the one used when that is taken.
const PORTS: [u16; 2] = [80, 8088];

/// How long a stop may take on the way out: the output closed in time puts the
/// device back as it was.
const QUIT_STOP_WAIT: Duration = Duration::from_secs(3);

/// How long a copy just updated waits for the old one to let go of the port.
const AFTER_UPDATE_WAIT: Duration = Duration::from_secs(20);

fn main() {
    // The panic reaches the session log before the process goes.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        aura_core::aelog!("[PANIC] {} in thread {}", info, std::thread::current().name().unwrap_or("unnamed"));
        previous(info);
    }));

    // Rayon's global pool below normal priority, as in the app: with every core
    // busy at normal priority the video card's driver waits, and the player with it.
    let _ = rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("aura-bg-{i}"))
        .start_handler(|_| player::analytics::track::set_below_normal_priority())
        .build_global();

    // The player's diagnostics.
    if player::selftest::handle_args() || player::render_bench::handle_args() {
        return;
    }

    update::clean_up();
    update::lend_to_filter_download();
    let session_log = audio::logging::init_session_log();
    watchdog::session_begin(session_log.as_deref());
    watchdog::watch_console_close();
    watchdog::start();
    audio::converter::album::install_peak_probe(player::album_probe::converter_probe);
    player::analytics::hub::init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Aura Streamer cannot start: {e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(run(session_log));

    player::controller::get().stop_and_wait(QUIT_STOP_WAIT);
    player::input::shutdown();
    player::output::restore_devices();
    // Every connection closed before an update's new version starts: it would
    // inherit them (Windows).
    runtime.shutdown_timeout(Duration::from_secs(1));
    update::restart();
    watchdog::session_end();
    std::process::exit(code);
}

async fn run(session_log: Option<PathBuf>) -> i32 {
    let Some(listener) = bind().await else { return 1 };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);

    let data = aura_core::app_dir::root().map(|d| d.join("streamer"));
    let kv = kv::Kv::open(data.as_ref().map(|d| d.join("pages.json")));
    kv.keep_saved();
    let lib = library::Library::open(data.as_ref().map(|d| d.join("music-folders.json")), library::default_music_dir());
    let (events, _) = broadcast::channel(256);
    let shared = server::Shared { lib, kv: kv.clone(), events };

    // The engine's events, to every open page.
    let to_pages = {
        let shared = shared.clone();
        Events::new(move |name, payload| shared.emit(name, payload))
    };
    if spatial::pack::start() {
        tokio::spawn(spatial::pack::keep_up_to_date(to_pages.clone()));
    }
    player::controller::start_input();
    {
        let shared = shared.clone();
        aura_core::perf::start(move |sample| {
            shared.emit("perf:tick", serde_json::to_value(sample).unwrap_or_default());
        });
    }
    aura_core::gpu_sched::start();

    upnp::start(data.clone(), port).await;
    {
        let shared = shared.clone();
        tokio::spawn(update::keep_checking(move |v| shared.emit("update:available", v)));
    }

    banner(port, session_log.as_deref());

    // Ctrl+C, or an update put in place. The pages' event streams never end
    // by themselves: after the stop, a few seconds for the rest, then out.
    let (stopping, stopped) = tokio::sync::oneshot::channel::<()>();
    let stop = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => aura_core::aelog!("[STREAMER] stopping"),
            _ = update::quit_requested() => aura_core::aelog!("[STREAMER] stopping for the update"),
        }
        let _ = stopping.send(());
    };
    let serving = server::serve(listener, shared, stop);
    let served = tokio::select! {
        r = serving => r,
        _ = async {
            if stopped.await.is_ok() {
                tokio::time::sleep(QUIT_STOP_WAIT).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Ok(()),
    };
    upnp::stop();
    kv.flush();
    match served {
        Ok(()) => 0,
        Err(e) => {
            aura_core::aelog!("[STREAMER] the server stopped: {e}");
            1
        }
    }
}

/// `AURA_PORT`, else the first free of `PORTS`. One copy per machine: another
/// streamer answering on a port means this one is not needed.
async fn bind() -> Option<TcpListener> {
    let asked = std::env::var("AURA_PORT").ok().and_then(|p| p.parse::<u16>().ok());
    let mut ports: Vec<u16> = asked.map_or(PORTS.to_vec(), |p| vec![p]);
    // Started by the copy it replaces: that one is on its way out, and its
    // port is the one to take.
    let after_update = update::after_update();
    let until = std::time::Instant::now() + AFTER_UPDATE_WAIT;
    let waiting = || after_update.is_some() && std::time::Instant::now() < until;
    if let Some(pid) = after_update {
        aura_core::aelog!("[STREAMER] updated to {}, waiting for the old copy ({pid}) to go", env!("CARGO_PKG_VERSION"));
    }
    for port in ports.clone() {
        let mut seen = false;
        while let Some(url) = streamer_at(port).await {
            if !waiting() {
                eprintln!("Aura Streamer is already running: {url}");
                return None;
            }
            seen = true;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if seen {
            ports = vec![port];
            break;
        }
    }
    let mut why = String::new();
    loop {
        for &port in &ports {
            match TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).await {
                Ok(l) => return Some(l),
                Err(e) => why = format!("port {port}: {e}"),
            }
        }
        if !waiting() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    eprintln!("Aura Streamer cannot listen ({why}). Set AURA_PORT to a free port.");
    None
}

/// The address of a streamer answering on this machine's `port`.
async fn streamer_at(port: u16) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ask = async {
        let mut c = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.ok()?;
        c.write_all(b"GET /api/whoami HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut reply = Vec::new();
        c.read_to_end(&mut reply).await.ok()?;
        String::from_utf8_lossy(&reply).contains(server::WHO).then(|| format!("http://localhost:{port}"))
    };
    tokio::time::timeout(Duration::from_secs(2), ask).await.ok().flatten()
}

/// The machine's name on the network.
pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default()
}

/// The address other machines reach this one at: the one the system would send
/// from (no packet goes out).
fn lan_address() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 168, 0, 1), 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn banner(port: u16, session_log: Option<&std::path::Path>) {
    const RULE: &str = "===============================================================";
    let at = |host: &str| if port == 80 { format!("http://{host}") } else { format!("http://{host}:{port}") };
    let mut out = vec![
        RULE.to_string(),
        format!("  Aura Streamer {}", env!("CARGO_PKG_VERSION")),
        RULE.to_string(),
        String::new(),
        "  Open it from a phone, a tablet or a computer on this network:".to_string(),
    ];
    if let Some(ip) = lan_address() {
        out.push(format!("    {}", at(&ip.to_string())));
    }
    let name = host_name();
    if !name.is_empty() {
        out.push(format!("    {}", at(&name.to_lowercase())));
    }
    out.push(String::new());
    out.push("  This window is the engine's log. Closing it stops the music.".to_string());
    let present = audio::converter::dsp::filter::inventory();
    out.push(format!("  Filters: {} on disk", present.len()));
    if let Some(p) = session_log {
        out.push(format!("  Session log: {}", p.display()));
    }
    out.push(RULE.to_string());
    out.push(String::new());
    audio::logging::banner(&out);
}
