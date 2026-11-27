//! Whether this PC carries each real-time DSD modulator at each DoP rate: a
//! short measurement on this machine (a block of a synthetic signal through
//! one channel's modulator, nanoseconds per output bit), turned into the load
//! one channel puts on its modulator thread — a sequential loop has one core
//! a channel at most (`dsd_worker`). Measured once a session, lazily (the
//! first ask), one kind and rate at a time on one thread, again on demand.
//! The thread runs at the modulator threads' priority (`WORKER_PRIORITY`),
//! so a busy PC takes no more time from it than from a modulator thread. It
//! is one thread for a second or two, and modulator threads run at this
//! priority beside the music anyway. The player's list greys out what is
//! too heavy.
//!
//! A measurement only ever reads high (whatever else the PC does adds to it,
//! a core still at a low clock too), so the thread first brings its core's
//! clock up (`RAMP_MS`), and each kind keeps the fastest it ever measured on
//! this PC in this version (`dsd-cpu.json` in the app's folder): every start
//! and every re-check on demand measures again and can only correct the list.
//! What played tells more than any measurement: a kind
//! that ran two minutes of music at a rate with no step down
//! (`note_played`, from the player's status) is listed as fine here, and one
//! that fell behind loses that.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use serde_json::{json, Value};

use crate::audio::dsd::{encoder::{new_encoder, same_as}, DsdEncoder, DsdError, DsdRate, ModulatorKind, CATALOG};

/// The DSD rates the player offers: DoP's, then the native ones (an ASIO
/// driver's DSD mode; the page lists them for an ASIO device only).
const MULTIPLES: [u32; 5] = [64, 128, 256, 512, 1024];
const BASES: [u32; 2] = [44_100, 48_000];

/// Seconds of signal a measurement modulates (each of its runs).
const BLOCK_S: f64 = 0.05;

/// Milliseconds of busy work before the measurements: a core resting at a
/// low clock is at its full one within this (Windows' and the CPU's own
/// ramp, tens of milliseconds).
const RAMP_MS: u128 = 150;

/// Seconds of music a kind plays at a rate with no step down before it is
/// listed as fine on this PC whatever the measurement said.
pub const PROVEN_S: f64 = 120.0;

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
/// From four cores up, a kind is too heavy only past 90 % of a core: its
/// thread is its own at a high priority, it runs 0.15–0.25 s ahead of the
/// device (`dsd_worker::lead_s`), and if it still falls behind the player
/// steps down to a lighter kind by itself.
pub fn limits(cores: usize) -> (f64, f64) {
    if cores < 4 {
        (0.3, 0.6)
    } else {
        (0.5, 0.9)
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
    /// Some kind's figure is an earlier session's (faster than this one's).
    from_earlier: bool,
    /// What played without a step down (`Store::proven`).
    proven: BTreeSet<String>,
}

static CACHE: Mutex<Option<Report>> = Mutex::new(None);

/// What this PC showed before, kept between sessions: the fastest ns per bit
/// of every kind at every multiple, and the kinds that played at a rate
/// without falling behind. Kept for one version and one core count only (a
/// new version may search differently).
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
struct Store {
    version: String,
    cores: usize,
    /// "sdm7LaPlus@128" → ns per bit.
    best: BTreeMap<String, f64>,
    /// "sdm7LaPlus@128@44100".
    proven: BTreeSet<String>,
}

/// The store file; none in the tests (they leave the app's folder alone).
fn store_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        crate::app_dir::root().map(|d| d.join("dsd-cpu.json"))
    }
}

/// The store of this version on this many cores, or an empty one.
fn load_store(cores: usize) -> Store {
    let version = crate::audio::converter::stamp::APP_VERSION.to_string();
    let read = store_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Store>(&s).ok())
        .filter(|s| s.version == version && s.cores == cores);
    read.unwrap_or(Store { version, cores, ..Store::default() })
}

fn save_store(s: &Store) {
    let Some(p) = store_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// The kind's name as the page knows it ("sdm7LaPlus").
fn kind_name(kind: ModulatorKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{kind:?}"))
}

fn best_key(kind: ModulatorKind, multiple: u32) -> String {
    format!("{}@{multiple}", kind_name(kind))
}

fn proven_key(kind: ModulatorKind, rate: DsdRate) -> String {
    format!("{}@{}@{}", kind_name(kind), rate.multiple, rate.base)
}

/// Each kind's figure the fastest of `fresh` and `earlier` (ns per bit);
/// true if any came from `earlier`.
fn fastest(fresh: &mut [Measured], earlier: &BTreeMap<String, f64>) -> bool {
    let mut any = false;
    for m in fresh.iter_mut() {
        if let (Ok(ns), Some(&old)) = (m.cost, earlier.get(&best_key(m.kind, m.multiple))) {
            if old > 0.0 && old < ns {
                m.cost = Ok(old);
                any = true;
            }
        }
    }
    any
}

/// What playing showed: `kind` at `rate` played `PROVEN_S` of music with no
/// step down (`kept_up`), or fell behind. Kept for the next sessions and
/// shown in the list at once. Called off the player's lock (it writes a
/// file).
pub fn note_played(kind: ModulatorKind, rate: DsdRate, kept_up: bool) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let key = proven_key(kind, rate);
    if kept_up {
        crate::aelog!("[DSD] {key}: {PROVEN_S:.0} s of music with no step down, listed as fine on this PC");
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store(cores);
    let changed = if kept_up { store.proven.insert(key.clone()) } else { store.proven.remove(&key) };
    if let Some(r) = cache.as_mut() {
        if kept_up {
            r.proven.insert(key);
        } else {
            r.proven.remove(&key);
        }
    }
    if changed {
        save_store(&store);
    }
}

/// Busy work for `RAMP_MS`: the core's clock up before anything is timed.
fn ramp_up() {
    let t0 = Instant::now();
    let mut x = 1.0f64;
    while t0.elapsed().as_millis() < RAMP_MS {
        for _ in 0..10_000 {
            x = std::hint::black_box(x * 1.000_000_1 + 1e-9);
        }
    }
    std::hint::black_box(x);
}

/// Runs of a block each measurement times; the fastest one counts.
const RUNS: usize = 6;

/// Milliseconds between two runs: a burst of other work, or a slow or shared
/// core the thread landed on, is likely gone by the next run (a sleep lets
/// the scheduler place the thread afresh). Three runs back to back read
/// SDM9 LA at DSD256 at 48 ns a bit on a PC busy at ~40 %, ≥ 10 % above
/// what it measured another time (Anton 23.11: one check said heavy, the
/// other fine).
const RUN_GAP_MS: u64 = 12;

/// Nanoseconds per output bit of `enc` on a block of `input_rate` PCM: a
/// −6 dB 1 kHz sine with a little noise, after a short warm-up, the fastest of
/// `RUNS` runs spread over a little time (the least disturbed by the rest of
/// the machine).
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
    for run in 0..RUNS {
        if run > 0 {
            std::thread::sleep(std::time::Duration::from_millis(RUN_GAP_MS));
        }
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
                // The chain's rate: the DoP carrier, ×16 past DSD256.
                let input = super::settings::dsd_chain_rate(rate);
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
        // One thread at a modulator thread's priority: what it measures is
        // what a modulator thread would get.
        let mut measured = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("aura-dsd-cpu".into())
                .spawn_scoped(s, || {
                    super::source_stages::set_thread_priority(super::dsd_worker::WORKER_PRIORITY);
                    ramp_up();
                    measure(make)
                })
                .ok()
                .and_then(|h| h.join().ok())
        })
        .unwrap_or_default();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut store = load_store(cores);
        // A start and a re-check alike: the fastest of now and the earlier
        // checks of this version. A measurement only ever reads high, so a
        // re-check on a busy PC took back what it had shown before (Anton
        // 23.11: SDM9 LA at DSD256 heavy by one check, fine by another).
        let from_earlier = fastest(&mut measured, &store.best);
        store.best = measured
            .iter()
            .filter_map(|m| m.cost.ok().map(|ns| (best_key(m.kind, m.multiple), ns)))
            .collect();
        if !measured.is_empty() {
            save_store(&store);
        }
        let report = Report {
            measured,
            while_playing: playing,
            cores,
            took_ms: t0.elapsed().as_secs_f64() * 1000.0,
            from_earlier,
            proven: store.proven,
        };
        crate::aelog!("[DSD] check of this PC{}: {}", if remeasure { " (again)" } else { "" }, summary(&report));
        *cache = Some(report);
    }
    payload(cache.as_ref().expect("measured"))
}

/// The check in one line for the session's log: each kind's share of a core
/// per channel at the 44.1 kHz family's rates ("SDM9 LA 128:28 256:54"),
/// and how it was taken.
fn summary(r: &Report) -> String {
    let mut parts = Vec::new();
    for info in CATALOG.iter().filter(|i| i.realtime) {
        let loads: Vec<String> = r
            .measured
            .iter()
            .filter(|m| m.kind == info.kind)
            .filter_map(|m| {
                let ns = m.cost.ok()?;
                let rate = DsdRate::new(44_100, m.multiple)?;
                Some(format!("{}:{:.0}", m.multiple, ns * rate.bit_rate() as f64 / 1e9 * 100.0))
            })
            .collect();
        if !loads.is_empty() {
            parts.push(format!("{} {}", info.label, loads.join(" ")));
        }
    }
    let (ok, heavy) = limits(r.cores);
    format!(
        "% of a core per channel (fine to {:.0}, heavy to {:.0}): {}; {} cores, {:.0} ms{}{}",
        ok * 100.0,
        heavy * 100.0,
        parts.join(", "),
        r.cores,
        r.took_ms,
        if r.from_earlier { ", some figures from an earlier check" } else { "" },
        if r.while_playing { ", while playing" } else { "" },
    )
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
                            let proven = r.proven.contains(&proven_key(info.kind, rate));
                            let reason = match fit {
                                Fit::Ok => format!("about {} of a core per channel", percent(load)),
                                // Played here without falling behind: fine,
                                // whatever the check said.
                                _ if proven => format!(
                                    "about {} of a core per channel by the check, but it played here without falling behind",
                                    percent(load)
                                ),
                                Fit::Heavy => format!("about {} of a core per channel: a busy PC may make it step down", percent(load)),
                                _ => format!("needs about {} of a core per channel: too heavy for this PC", percent(load)),
                            };
                            (if proven { Fit::Ok } else { fit }, Some(load), Some(ns), reason)
                        }
                        Err(Fit::NotForRate) if m.multiple > info.max_multiple => {
                            (Fit::NotForRate, None, None, format!("built up to DSD{}", info.max_multiple))
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
                        // at DSD256 is SDM7 LA without AVX2 + FMA): the list
                        // shows that one only.
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
        "fromEarlier": r.from_earlier,
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
        assert_eq!(classify(0.9, 8), Fit::Heavy);
        assert_eq!(classify(0.95, 8), Fit::TooHeavy);
        assert_eq!(classify(1.13, 8), Fit::TooHeavy);
        // Two cores: a class stricter.
        assert_eq!(classify(0.4, 2), Fit::Heavy);
        assert_eq!(classify(0.7, 2), Fit::TooHeavy);
    }

    #[test]
    fn a_kind_is_the_same_as_a_plainer_one_only_where_its_search_is_identical() {
        use ModulatorKind as K;
        // SDM7 LA+ at DSD256: its own 16-path search where the player runs
        // the single-precision ones, else SDM7 LA's.
        let plus256 = if crate::audio::dsd::encoder::single_precision() { None } else { Some(K::Sdm7La) };
        assert_eq!(same_as(K::Sdm7LaPlus, 256), plus256);
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
            assert_eq!(rates.len(), 2 * MULTIPLES.len(), "{}", k["label"]);
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
        // SDM7 LA+ at DSD256 is SDM7 LA without the single-precision
        // searches: said, so the list shows one row.
        let plus = kinds.iter().find(|k| k["kind"] == "sdm7LaPlus").unwrap();
        let single = crate::audio::dsd::encoder::single_precision();
        for r in plus["rates"].as_array().unwrap().iter().filter(|r| r["multiple"].as_u64().unwrap() <= 256) {
            let same = if r["multiple"] == 256 && !single { json!("sdm7La") } else { Value::Null };
            assert_eq!(r["sameAs"], same, "{}", r["label"]);
        }
        let dop = |r: &&Value| r["multiple"].as_u64().unwrap() <= 256;
        assert!(kinds.iter().filter(|k| k["kind"] != "sdm7LaPlus").all(|k| k["rates"].as_array().unwrap().iter().filter(dop).all(|r| r["sameAs"].is_null())));
        let sdm9 = kinds.iter().find(|k| k["kind"] == "sdm9").unwrap();
        assert!(sdm9["rates"].as_array().unwrap().iter().filter(dop).all(|r| r["state"] == "notForRate"));
        // SDM9 is built for the native rates: measured there.
        assert!(sdm9["rates"].as_array().unwrap().iter().filter(|r| !dop(r)).all(|r| r["load"].is_number()), "{sdm9}");
        // SDM5 stops at DSD256; SDM7 goes on, heavier at DSD512.
        assert_eq!(at(44_100, 512)["state"], "notForRate");
        assert_eq!(at(44_100, 1024)["reason"], "built up to DSD256");
        let sdm7 = kinds.iter().find(|k| k["kind"] == "sdm7").unwrap();
        let at7 = |m: u64| sdm7["rates"].as_array().unwrap().iter().find(|r| r["base"] == 44_100 && r["multiple"] == m).unwrap()["load"].as_f64().unwrap();
        assert!(at7(512) > at7(256) && at7(1024) > at7(512), "DSD512 heavier than DSD256, DSD1024 than DSD512");
        // Cached: the second ask measures nothing.
        let again = capabilities_with(false, false, &|_, _, _| panic!("measured again"));
        assert_eq!(again["measuredWhilePlaying"], true);
    }

    /// A burst of other work over the first runs (here: the first four
    /// slowed by 20 ms) does not make the figure: the fastest of the runs,
    /// spread over a little time, does.
    #[test]
    fn a_busy_moment_does_not_make_the_figure() {
        struct Busy(usize);
        impl DsdEncoder for Busy {
            fn ratio(&self) -> usize {
                8
            }
            fn process(&mut self, _pcm: &[f64], out: &mut [u8]) {
                self.0 += 1;
                // Call 1 is the warm-up; calls 2–5 are the first four runs.
                if (2..=5).contains(&self.0) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                out.fill(0x69);
            }
            fn reset(&mut self) {}
            fn overloads(&self) -> u64 {
                0
            }
        }
        let mut enc = Busy(0);
        let ns = ns_per_bit(&mut enc, 8_000);
        assert_eq!(enc.0, 1 + RUNS, "a warm-up and {RUNS} runs");
        // 20 ms over 400 × 8 bits would be ~6 µs a bit.
        assert!(ns < 1_000.0, "{ns} ns a bit: a slowed run counted");
    }

    /// A start keeps each kind's fastest figure of this session and the
    /// earlier ones; what played without a step down is listed as fine.
    #[test]
    fn earlier_sessions_and_playing_can_only_correct_the_list() {
        use ModulatorKind as K;
        let mut m = vec![
            Measured { kind: K::Sdm7LaPlus, multiple: 128, cost: Ok(150.0) },
            Measured { kind: K::Sdm7La, multiple: 128, cost: Ok(20.0) },
            Measured { kind: K::Sdm9, multiple: 128, cost: Err(Fit::NotForRate) },
        ];
        let earlier: BTreeMap<String, f64> =
            [("sdm7LaPlus@128".to_string(), 60.0), ("sdm7La@128".to_string(), 30.0), ("sdm9@128".to_string(), 1.0)].into();
        assert!(fastest(&mut m, &earlier));
        assert_eq!(m[0].cost.unwrap(), 60.0);
        assert_eq!(m[1].cost.unwrap(), 20.0);
        assert!(m[2].cost.is_err());
        assert!(!fastest(&mut m, &BTreeMap::new()));

        // 150 ns a bit at DSD128 is ~85 % of a core: heavy; at DSD256 too
        // heavy, until it played there.
        let measured = vec![
            Measured { kind: K::Sdm7LaPlus, multiple: 128, cost: Ok(150.0) },
            Measured { kind: K::Sdm7LaPlus, multiple: 256, cost: Ok(150.0) },
        ];
        let rate256 = DsdRate::new(44_100, 256).unwrap();
        let mut r = Report { measured, while_playing: false, cores: 8, took_ms: 1.0, from_earlier: false, proven: BTreeSet::new() };
        let state = |r: &Report, m: u64, base: u64| {
            let v = payload(r);
            let plus = v["kinds"].as_array().unwrap().iter().find(|k| k["kind"] == "sdm7LaPlus").unwrap().clone();
            let row = plus["rates"].as_array().unwrap().iter().find(|x| x["multiple"] == m && x["base"] == base).unwrap().clone();
            (row["state"].as_str().unwrap().to_string(), row["reason"].as_str().unwrap().to_string())
        };
        assert_eq!(state(&r, 128, 44_100).0, "heavy");
        assert_eq!(state(&r, 256, 44_100).0, "tooHeavy");
        // The session's log line: each kind's share at each rate.
        let line = summary(&r);
        assert!(line.contains("SDM7 LA+ 128:85 256:169"), "{line}");
        assert!(line.contains("fine to 50, heavy to 90"), "{line}");
        r.proven.insert(proven_key(K::Sdm7LaPlus, rate256));
        let (s, why) = state(&r, 256, 44_100);
        assert_eq!(s, "ok");
        assert!(why.contains("played here"), "{why}");
        // The other family is not vouched for.
        assert_eq!(state(&r, 256, 48_000).0, "tooHeavy");
        assert_eq!(proven_key(K::Sdm7LaPlus, rate256), "sdm7LaPlus@256@44100");
    }
}
