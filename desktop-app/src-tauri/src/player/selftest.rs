//! `aura-engine --player-selftest <file>`: the whole player without the window —
//! controller, render thread and the real output device — at −120 dB, so it
//! can be run on a machine where someone is listening without being heard.

use std::time::Duration;

use super::controller;
use super::settings::{Geometry, Phase, PlayerSettings};

/// The player's command-line diagnostics. `true` when one of them ran and the
/// process should end; the converter's own (`--selftest…`, `--help`) are
/// handled before this, in `startup`.
pub fn handle_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("--player-selftest") => {
            match args.get(1) {
                Some(file) => run(file, args.get(2).map(|s| s.as_str())),
                None => println!("usage: --player-selftest <audio file> [device id from --probe-device]"),
            }
            true
        }
        Some("--probe-device") => {
            probe_device(args.get(1).map(|s| s.as_str()));
            true
        }
        _ => false,
    }
}

/// `--probe-device [device id]`: every render endpoint, then what
/// the chosen one (default: the system default) accepts in exclusive mode at
/// every rate the chain can produce, with the device's own error codes.
pub fn probe_device(device_id: Option<&str>) {
    match super::output::list_devices() {
        Ok(list) => {
            for d in list {
                println!("{} {}  [{}]", if d.is_default { "*" } else { " " }, d.name, d.id);
            }
        }
        Err(e) => println!("list_devices: {e}"),
    }
    println!();
    let rates = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];
    match super::output::diagnose(device_id, &rates) {
        Ok(r) => print!("{r}"),
        Err(e) => println!("diagnose: {e}"),
    }
}

/// `device`: an id from `--probe-device`; the system default when absent.
pub fn run(file: &str, device: Option<&str>) {
    let p = controller::get();
    p.set_volume(-120.0);
    p.set_device(device.map(|d| d.to_string()));
    let (added, errors) = p.add(vec![file.to_string()]);
    assert!(errors.is_empty(), "add: {errors:?}");
    let id = added.first().expect("file could not be read").id;
    println!("added: {:?}", added[0]);
    p.play(Some(id));
    let show = |label: &str, secs: u64| {
        for _ in 0..(secs * 2) {
            std::thread::sleep(Duration::from_millis(500));
            let s = p.status();
            println!(
                "{:<14} {:>8} pos {:>6.2}s  out {:>6}  {} {}  buf {:>4.2}s  rtf {:>5.1}  switch {:>5}ms  under {}  chain [{}]  pending {}  err {}",
                label,
                s["state"].as_str().unwrap_or(""),
                s["positionS"].as_f64().unwrap_or(0.0),
                s["outRate"],
                s["format"].as_str().unwrap_or("-"),
                if s["exclusive"].as_bool().unwrap_or(false) { "excl" } else { "shared" },
                s["bufferS"].as_f64().unwrap_or(0.0),
                s["renderRtf"].as_f64().unwrap_or(0.0),
                s["lastSwitchMs"],
                s["underrunFrames"],
                s["chain"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default(),
                s["pending"],
                s["error"],
            );
        }
    };
    show("start", 5);
    let mut s = PlayerSettings::default();
    s.phase = Phase::Linear;
    p.set_settings(s.clone());
    show("linear", 3);
    s.xtc = true;
    s.xtc_geometry = Geometry { speaker_span_mm: 2000.0, left_distance_mm: 2500.0, right_distance_mm: 2500.0, head_width_mm: 150.0 };
    p.set_settings(s.clone());
    show("xtc", 3);
    p.seek(60.0);
    show("seek 60", 3);
    s.declip = false;
    s.isp = false;
    p.set_settings(s.clone());
    show("dc/isp off", 4);
    p.pause();
    show("paused", 1);
    p.play(None);
    show("resumed", 2);
    p.stop();
    show("stopped", 1);
}
