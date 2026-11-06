// Off Windows: Aura Player ASIO is a Windows driver; the input is not offered there (player_input_supported).
#![cfg_attr(not(windows), allow(dead_code))]
//! The input: what another player — foobar2000, JRiver, HQPlayer, Roon… —
//! plays into Aura Player ASIO, played through the same chain as a stream.
//!
//! * `driver` — the driver on this machine: found, this build's own,
//!   installed or removed (elevated regsvr32).
//! * `link` — Aura Player's end of the cable: the server the driver hands
//!   the player's samples to, read at the chain's pace.
//! * `feed` — the input's session fed from it (the radio's live source,
//!   source stages and chain: `radio::Session::start_input`).
//! * `pace` — the reader's own clock once the chain plays: the link read
//!   evenly, so the driver calls the player once a buffer's time.
//!
//! Whether a player starting takes the player over (`auto`, on unless
//! turned off) is kept in `player-input.json` beside the app's settings.

#[cfg(test)]
mod bench;
pub mod calls;
pub mod driver;
pub mod feed;
pub mod link;
pub mod pace;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use self::driver::DriverState;

/// A player starting while the input is not listened to takes the player
/// over.
static AUTO: AtomicBool = AtomicBool::new(true);

pub fn auto() -> bool {
    AUTO.load(Ordering::Acquire)
}

fn prefs_path() -> Option<std::path::PathBuf> {
    crate::app_dir::root().map(|d| d.join("player-input.json"))
}

/// The auto switch on or off, kept for the next start.
pub fn set_auto(on: bool) {
    if AUTO.swap(on, Ordering::AcqRel) == on {
        return;
    }
    crate::aelog!("[INPUT] auto switch {}", if on { "on" } else { "off" });
    #[cfg(not(test))]
    if let Some(p) = prefs_path() {
        let _ = std::fs::write(p, json!({ "auto": on }).to_string());
    }
}

fn load_prefs() {
    let v: Option<Value> = prefs_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok());
    if let Some(on) = v.as_ref().and_then(|v| v["auto"].as_bool()) {
        AUTO.store(on, Ordering::Release);
    }
}

/// The input's link, made once at the app's start; `events` is the
/// player's end of it.
pub fn start(events: Arc<dyn link::Events>) {
    load_prefs();
    link::start(events, auto);
}

/// The app is closing: the link ends, and a connected player hears it at
/// once (it plays on as into an unplugged cable).
pub fn shutdown() {
    if let Some(l) = link::get() {
        l.stop();
    }
}

/// The driver's state is looked at again after this: it reads the registry
/// and both DLLs whole, and the status is asked for many times a second.
const DRIVER_LOOK: Duration = Duration::from_secs(5);

static DRIVER: Mutex<Option<(Instant, DriverState)>> = Mutex::new(None);

/// The driver on this machine (looked at most every few seconds).
pub fn driver_state() -> DriverState {
    let mut d = DRIVER.lock().unwrap_or_else(|e| e.into_inner());
    match *d {
        Some((at, s)) if at.elapsed() < DRIVER_LOOK => s,
        _ => {
            let s = driver::state();
            *d = Some((Instant::now(), s));
            s
        }
    }
}

/// Installed or removed: looked at afresh at the next status.
pub fn driver_changed() {
    *DRIVER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// What an install is doing: "download" (fetching this build's driver) or
/// "install" (Windows asking, then copying and registering).
static INSTALLING: Mutex<Option<&'static str>> = Mutex::new(None);

pub fn set_installing(phase: Option<&'static str>) {
    *INSTALLING.lock().unwrap_or_else(|e| e.into_inner()) = phase;
}

/// The status's `input`: { driver, installing, client, format, listening,
/// held, auto, error } (`input.js` reads it).
pub fn status() -> Value {
    let mut v = match link::get() {
        Some(l) => l.status(),
        None => json!({
            "client": null,
            "format": null,
            "listening": false,
            "held": false,
            "auto": auto(),
            "error": null,
        }),
    };
    v["driver"] = serde_json::to_value(driver_state()).unwrap_or(Value::Null);
    v["installing"] = json!(*INSTALLING.lock().unwrap_or_else(|e| e.into_inner()));
    v
}
