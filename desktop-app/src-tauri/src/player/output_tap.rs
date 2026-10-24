//! A measuring tap on what the device plays. `AURA_PLAYER_TAP=<folder>`
//! writes every output stream's frames — after the chain and the ramps,
//! before the volume and the dither — to `<folder>\tap-<unix ms>-<rate>.f32`
//! (interleaved f32 L R) with a `.idx` beside it (`<unix ms> <frames before>`
//! per device period), so a live chain can be compared with its converted
//! file on the real device. Unset: nothing is spawned or allocated, and the
//! output thread pays one load per period.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

enum Cmd {
    Open(u32),
    Frames(Vec<f32>),
}

static TAP: OnceLock<Option<Sender<Cmd>>> = OnceLock::new();

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn tap() -> Option<&'static Sender<Cmd>> {
    TAP.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("AURA_PLAYER_TAP")?);
        std::fs::create_dir_all(&dir).ok()?;
        let (tx, rx) = channel::<Cmd>();
        std::thread::Builder::new()
            .name("aura-output-tap".into())
            .spawn(move || {
                let mut out: Option<(std::io::BufWriter<std::fs::File>, std::fs::File, u64)> = None;
                for cmd in rx {
                    match cmd {
                        Cmd::Open(rate) => {
                            let stem = format!("tap-{}-{}", unix_ms(), rate);
                            let data = std::fs::File::create(dir.join(format!("{stem}.f32")));
                            let idx = std::fs::File::create(dir.join(format!("{stem}.idx")));
                            out = match (data, idx) {
                                (Ok(d), Ok(i)) => Some((std::io::BufWriter::with_capacity(1 << 20, d), i, 0)),
                                _ => None,
                            };
                        }
                        Cmd::Frames(v) => {
                            if let Some((data, idx, frames)) = out.as_mut() {
                                let _ = writeln!(idx, "{} {}", unix_ms(), frames);
                                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                                let _ = data.write_all(&bytes);
                                *frames += (v.len() / 2) as u64;
                            }
                        }
                    }
                }
            })
            .ok()?;
        Some(tx)
    })
    .as_ref()
}

/// A new output stream at `rate`: the frames after this go to a new file.
pub fn open(rate: u32) {
    if let Some(t) = tap() {
        let _ = t.send(Cmd::Open(rate));
    }
}

/// One device period, pre-volume.
pub fn frames(l: &[f64], r: &[f64]) {
    if let Some(t) = tap() {
        let mut v = Vec::with_capacity(l.len() * 2);
        for (a, b) in l.iter().zip(r) {
            v.push(*a as f32);
            v.push(*b as f32);
        }
        let _ = t.send(Cmd::Frames(v));
    }
}
