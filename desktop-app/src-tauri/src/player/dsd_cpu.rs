//! Whether this PC carries each real-time DSD modulator at each DoP rate: a
//! short measurement on this machine (a block of a synthetic signal through
//! one channel's modulator, nanoseconds per output bit), turned into the load
//! one channel puts on its modulator thread — a sequential loop has one core
//! a channel at most (`dsd_worker`). Measured once a session, lazily (the
//! first ask), one kind and rate at a time on one thread at normal priority,
//! so playback around it is not disturbed; again on demand. The player's
//! list greys out what is too heavy.

use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

use crate::audio::dsd::{encoder::{new_encoder, same_as}, DsdEncoder, DsdError, DsdRate, ModulatorKind, CATALOG};

/// The DoP rates the player offers.
const MULTIPLES: [u32; 3] = [64, 128, 256];
const BASES: [u32; 2] = [44_100, 48_000];

/// Seconds of signal a measurement modulates (each of its runs).
const BLOCK_S: f64 = 0.05;

/// How a modulator fits a rate on this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Ok,
    /// Plays, with little to spare: a busy PC may make it step down.
    Heavy,
    TooHeavy,
    /// Not in this version.
    NotAvailable,
    /// Not built for this rate.
    NotForRate,
}

impl Fit {
    fn name(self) -> &'static str {
        match self {
            Fit::Ok => "ok",
            Fit::Heavy => "heavy",
            Fit::TooHeavy => "tooHeavy",
            Fit::NotAvailable => "notAvailable",
            Fit::NotForRate => "notForRate",
        }
    }
}

/// The load (a share of one core, one channel) up to which a kind is `Ok`,
/// and up to which it is `Heavy`, on a PC with `cores` logical cores. Two
/// modulator threads, the audio thread and the render's have to share them:
/// under four, a class stricter.
pub fn limits(cores: usize) -> (f64, f64) {
    if cores < 4 {
        (0.3, 0.6)
    } else {
        (0.5, 0.8)
    }
}

pub fn classify(load: f64, cores: usize) -> Fit {
    let (ok, heavy) = limits(cores);
    if load <= ok {
        Fit::Ok
    } else if load <= heavy {
        Fit::Heavy
    } else {
        Fit::TooHeavy
    }
}

/// One kind at one multiple: nanoseconds per output bit, or why there is none.
#[derive(Clone, Copy, Debug)]
struct Measured {
    kind: ModulatorKind,
    multiple: u32,
    cost: Result<f64, Fit>,
}

struct Report {
    measured: Vec<Measured>,
    while_playing: bool,
    cores: usize,
    took_ms: f64,
}

static CACHE: Mutex<Option<Report>> = Mutex::new(None);

/// Nanoseconds per output bit of `enc` on a block of `input_rate` PCM: a
/// −6 dB 1 kHz sine with a little noise, after a short warm-up, the fastest of
/// three runs (the least disturbed by the rest of the machine).
pub fn ns_per_bit(enc: &mut dyn DsdEncoder, input_rate: u32) -> f64 {
    let n = ((BLOCK_S * input_rate as f64) as usize).max(64);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let pcm: Vec<f64> = (0..n)
        .map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let noise = (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            0.5 * (std::f64::consts::TAU * 1000.0 * i as f64 / input_rate as f64).sin() + 1e-4 * noise
        })
        .collect();
    let mut out = vec![0u8; n * enc.ratio() / 8];
    let warm = n / 5;
    enc.process(&pcm[..warm], &mut out[..warm * enc.ratio() / 8]);
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t0 = Instant::now();
        enc.process(&pcm, &mut out);
        best = best.min(t0.elapsed().as_nanos() as f64);
    }
    std::hint::black_box(&out);
    best / (n * enc.ratio()) as f64
}

/// Every real-time kind at every multiple, `make` building the modulator.
fn measure(make: &dyn Fn(ModulatorKind, u32, DsdRate) -> Result<Box<dyn DsdEncoder>, DsdError>) -> Vec<Measured> {
    let mut v = Vec::new();
    for info in CATALOG.iter().filter(|i| i.realtime) {
        for &multiple in &MULTIPLES {
            let rate = DsdRate::new(44_100, multiple).expect("a DSD rate");
            let cost = if !info.kind.supports(rate) {
                Err(Fit::NotForRate)
            } else {
                // DoP: the chain runs at the carrier, 16 bits a sample.
                let input = rate.dop_carrier();
                match make(info.kind, input, rate) {
                    Ok(mut enc) => Ok(ns_per_bit(enc.as_mut(), input)),
                    Err(_) => Err(Fit::NotAvailable),
                }
            };
            v.push(Measured { kind: info.kind, multiple, cost });
        }
    }
    v
}

/// What the player's list reads (`dsd_capabilities`): for each real-time
/// kind, best first, and each DoP rate, how it fits this PC and why. Measured
/// on the first call, then cached; `remeasure` measures again. `playing`: a
/// stream plays now (the numbers may read a little high), said in the payload.
pub fn capabilities(remeasure: bool, playing: bool) -> Value {
    capabilities_with(remeasure, playing, &|k, input, rate| new_encoder(k, input, rate))
}

fn capabilities_with(
    remeasure: bool,
    playing: bool,
    make: &(dyn Fn(ModulatorKind, u32, DsdRate) -> Result<Box<dyn DsdEncoder>, DsdError> + Sync),
) -> Value {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if remeasure || cache.is_none() {
        let t0 = Instant::now();
        // One thread, normal priority: not a modulator thread's.
        let measured = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("aura-dsd-cpu".into())
                .spawn_scoped(s, || measure(make))
                .ok()
                .and_then(|h| h.join().ok())
        })
        .unwrap_or_default();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        *cache = Some(Report { measured, while_playing: playing, cores, took_ms: t0.elapsed().as_secs_f64() * 1000.0 });
    }
    payload(cache.as_ref().expect("measured"))
}

fn percent(load: f64) -> String {
    format!("{:.0} %", (load * 100.0).max(1.0))
}

fn payload(r: &Report) -> Value {
    let kinds: Vec<Value> = CATALOG
        .iter()
        .filter(|i| i.realtime)
        .map(|info| {
            let mut rates = Vec::new();
            for &base in &BASES {
                for m in r.measured.iter().filter(|m| m.kind == info.kind) {
                    let rate = DsdRate::new(base, m.multiple).expect("a DSD rate");
                    let label = if base == 44_100 { format!("DSD{}", m.multiple) } else { format!("DSD{} (48k)", m.multiple) };
                    let (fit, load, ns, reason) = match m.cost {
                        Ok(ns) => {
                            let load = ns * rate.bit_rate() as f64 / 1e9;
                            let fit = classify(load, r.cores);
                            let reason = match fit {
                                Fit::Ok => format!("about {} of a core per channel", percent(load)),
                                Fit::Heavy => format!("about {} of a core per channel: a busy PC may make it step down", percent(load)),
                                _ => format!("needs about {} of a core per channel: too heavy for this PC", percent(load)),
                            };
                            (fit, Some(load), Some(ns), reason)
                        }
                        Err(Fit::NotForRate) => {
                            (Fit::NotForRate, None, None, format!("built for DSD{} and up", info.min_multiple))
                        }
                        Err(f) => (f, None, None, "not available in this version".to_string()),
                    };
                    rates.push(json!({
                        "base": base,
                        "multiple": m.multiple,
                        "label": label,
                        "state": fit.name(),
                        "load": load,
                        "nsPerBit": ns,
                        "reason": reason,
                        // The same modulator as a plainer kind here (SDM7 LA+
                        // at DSD256 is SDM7 LA): the list shows that one only.
                        "sameAs": same_as(info.kind, m.multiple),
                    }));
                }
            }
            json!({ "kind": info.kind, "label": info.label, "rank": info.rank, "rates": rates })
        })
        .collect();
    let (ok, heavy) = limits(r.cores);
    json!({
        "measured": !r.measured.is_empty(),
        "measuredWhilePlaying": r.while_playing,
        "cores": r.cores,
        "limits": { "ok": ok, "heavy": heavy },
        "tookMs": r.took_ms,
        "kinds": kinds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_are_classed_by_the_share_of_a_core() {
        assert_eq!(classify(0.09, 8), Fit::Ok);
        assert_eq!(classify(0.5, 8), Fit::Ok);
        assert_eq!(classify(0.53, 8), Fit::Heavy);
        assert_eq!(classify(0.8, 8), Fit::Heavy);
        assert_eq!(classify(1.13, 8), Fit::TooHeavy);
        // Two cores: a class stricter.
        assert_eq!(classify(0.4, 2), Fit::Heavy);
        assert_eq!(classify(0.7, 2), Fit::TooHeavy);
    }

    #[test]
    fn a_kind_is_the_same_as_a_plainer_one_only_where_its_search_is_identical() {
        use ModulatorKind as K;
        assert_eq!(same_as(K::Sdm7LaPlus, 256), Some(K::Sdm7La));
        assert_eq!(same_as(K::Sdm7LaPlus, 128), None);
        assert_eq!(same_as(K::Sdm7LaPlus, 64), None);
        // The plainer one is never "the same as" the richer one.
        assert_eq!(same_as(K::Sdm7La, 256), None);
        // The Wide curve is another search; the plain kinds have none.
        assert_eq!(same_as(K::Sdm7WideLa, 256), None);
        assert_eq!(same_as(K::Sdm5, 256), None);
    }

    /// The payload: every real-time kind, best first, at every DoP rate of
    /// both families; SDM5 measured, what is not built "notAvailable", what
    /// is not for the rate "notForRate"; the 48 kHz family a little heavier.
    #[test]
    fn the_payload_names_every_real_time_kind_at_every_dop_rate() {
        let v = capabilities_with(true, true, &|k, input, rate| new_encoder(k, input, rate));
        assert_eq!(v["measuredWhilePlaying"], true);
        assert_eq!(v["measured"], true);
        let kinds = v["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), CATALOG.iter().filter(|i| i.realtime).count());
        let ranks: Vec<u64> = kinds.iter().map(|k| k["rank"].as_u64().unwrap()).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "best first");
        for k in kinds {
            let rates = k["rates"].as_array().unwrap();
            assert_eq!(rates.len(), 6, "{}", k["label"]);
            for r in rates {
                let state = r["state"].as_str().unwrap();
                assert!(["ok", "heavy", "tooHeavy", "notAvailable", "notForRate"].contains(&state));
                assert!(!r["reason"].as_str().unwrap().is_empty());
                assert_eq!(r["load"].is_number(), ["ok", "heavy", "tooHeavy"].contains(&state));
            }
        }
        let sdm5 = kinds.iter().find(|k| k["kind"] == "sdm5").unwrap();
        let at = |base: u64, m: u64| sdm5["rates"].as_array().unwrap().iter().find(|r| r["base"] == base && r["multiple"] == m).unwrap().clone();
        let (a, b) = (at(44_100, 256), at(48_000, 256));
        assert!(a["nsPerBit"].as_f64().unwrap() > 0.0);
        assert!(b["load"].as_f64().unwrap() > a["load"].as_f64().unwrap());
        assert!(at(44_100, 64)["load"].as_f64().unwrap() < a["load"].as_f64().unwrap());
        // SDM7 LA+ above DSD128 is SDM7 LA: said, so the list shows one row.
        let plus = kinds.iter().find(|k| k["kind"] == "sdm7LaPlus").unwrap();
        for r in plus["rates"].as_array().unwrap() {
            let same = if r["multiple"] == 256 { json!("sdm7La") } else { Value::Null };
            assert_eq!(r["sameAs"], same, "{}", r["label"]);
        }
        assert!(kinds.iter().filter(|k| k["kind"] != "sdm7LaPlus").all(|k| k["rates"].as_array().unwrap().iter().all(|r| r["sameAs"].is_null())));
        let sdm9 = kinds.iter().find(|k| k["kind"] == "sdm9").unwrap();
        assert!(sdm9["rates"].as_array().unwrap().iter().all(|r| r["state"] == "notForRate"));
        // Cached: the second ask measures nothing.
        let again = capabilities_with(false, false, &|_, _, _| panic!("measured again"));
        assert_eq!(again["measuredWhilePlaying"], true);
    }
}
