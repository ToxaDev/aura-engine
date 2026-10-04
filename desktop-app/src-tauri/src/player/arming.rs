//! How far the player is with bringing the rack onto the air: the thin bar
//! under the player's badges fills with it.
//!
//! One arming is one pair (track, rack epoch). A new track, a rack change or
//! a stop starts a new epoch and the bar starts from nothing; a seek, the
//! quick variant, the full one's Apply and the Hybrid-Phase job keep it, so a
//! track's start is one arming from its first sound to its last stage.
//!
//! The work is a list of steps, each weighed by what it is expected to take:
//! the full variant's source stages (decode, stats, declip, ISP, SUB, the
//! Adaptive Apodizer's analysis and filter, the true-peak measure), the
//! output-peak probe, the chain's build and its landing on the air; then the
//! Hybrid-Phase envelope, the pair's build and landing; or, for a new output
//! rate, the device's reopening. Which step the player is in is read off its
//! own state, not off a clock: a preparation's current stage (the engine's
//! status lines, `decode::set_status`), the variant cache, the chain heard and
//! the chain placed last (a landing is the rack's own chain placed and not yet
//! heard — a seek's or a power step's swap is not one), the Hybrid-Phase job.
//! Inside a step the clock runs against the step's estimate, learned from
//! earlier runs on this machine; past the estimate the step creeps towards
//! 95 % of its share and never stops. A landing is exact: the audio between
//! the reader and the splice. The fraction never goes back within one arming,
//! and stays at or under 98 % until the rack's chain is heard.
//!
//! When nothing more is coming — the full variant's preparation failed, the
//! pair's job gave up or left the pair to the next track — the arming is not
//! busy any more, and the bar goes as it stands; a newer preparation or job
//! makes it busy again.
//!
//! Nothing here touches the disk on the status path: the estimates are read
//! in at start and written a few seconds after they change, on threads of
//! their own.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::settings::{Mode, Phase, PlayerSettings};

// ── Estimates ────────────────────────────────────────────────────────────

/// How far the newest run moves an estimate.
const ALPHA: f64 = 0.3;
/// Estimates kept; the least recently learned go first.
const MAX_KEYS: usize = 200;
/// The longest key kept (a lab stage's step is named after its status line).
const MAX_KEY_LEN: usize = 96;

/// What each step took on this machine: seconds per minute of source audio
/// for the whole-file work (`prep:<step>:f<n>`, `probe:f<n>`, `env:f<n>`,
/// n = the source rate over its family's base), seconds for a chain's build
/// (`build:<taps>:<cpu|gpu>:<one|pair>`, by the rack's GPU switch).
#[derive(Default)]
pub struct Model {
    /// key → (estimate, when last learned)
    map: std::collections::HashMap<String, (f64, u64)>,
    clock: u64,
}

impl Model {
    pub fn get(&self, key: &str) -> Option<f64> {
        self.map.get(key).map(|e| e.0)
    }

    pub fn learn(&mut self, key: &str, v: f64) {
        if !v.is_finite() || v < 0.0 || key.len() > MAX_KEY_LEN {
            return;
        }
        self.clock += 1;
        let at = self.clock;
        let e = self.map.entry(key.to_string()).or_insert((v, at));
        e.0 += ALPHA * (v - e.0);
        e.1 = at;
        self.evict();
    }

    fn evict(&mut self) {
        while self.map.len() > MAX_KEYS {
            let Some(oldest) = self.map.iter().min_by_key(|(_, e)| e.1).map(|(k, _)| k.clone()) else { break };
            self.map.remove(&oldest);
        }
    }

    /// Oldest first: a reader keeps them in the order they were learned.
    pub fn to_json(&self) -> String {
        let mut list: Vec<_> = self.map.iter().collect();
        list.sort_by_key(|(_, e)| e.1);
        let list: Vec<Value> = list.into_iter().map(|(k, e)| json!([k, e.0])).collect();
        json!({ "version": 1, "estimates": list }).to_string()
    }

    /// A file that does not read as ours (broken, another version, someone
    /// else's) gives no estimates: the defaults stand in until the player
    /// has learned its own.
    pub fn from_json(text: &str) -> Model {
        let mut m = Model::default();
        let Ok(v) = serde_json::from_str::<Value>(text) else { return m };
        if v.get("version").and_then(Value::as_u64) != Some(1) {
            return m;
        }
        let Some(list) = v.get("estimates").and_then(Value::as_array) else { return m };
        for e in list {
            let (Some(k), Some(x)) = (e.get(0).and_then(Value::as_str), e.get(1).and_then(Value::as_f64)) else { continue };
            if x.is_finite() && x >= 0.0 && k.len() <= MAX_KEY_LEN {
                m.clock += 1;
                m.map.insert(k.to_string(), (x, m.clock));
            }
        }
        m.evict();
        m
    }

    /// `older` (read from disk) under what this run has learned already.
    fn merge_under(&mut self, older: Model) {
        let mut mine: Vec<_> = self.map.drain().collect();
        mine.sort_by_key(|(_, e)| e.1);
        let mut theirs: Vec<_> = older.map.into_iter().collect();
        theirs.sort_by_key(|(_, e)| e.1);
        self.clock = 0;
        for (k, (v, _)) in theirs.into_iter().chain(mine) {
            self.clock += 1;
            self.map.insert(k, (v, self.clock));
        }
        self.evict();
    }
}

fn store_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::app_dir::root().map(|d| d.join("player-arming.json"))
}

fn model() -> &'static Mutex<Model> {
    static M: OnceLock<Mutex<Model>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Model::default()))
}

/// The estimates file has been read (or found missing): writing it now
/// cannot lose what it held.
static LOADED: AtomicBool = AtomicBool::new(false);

/// Read the estimates file in, once, on a thread of its own.
pub fn load_in_background() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let Some(path) = store_path() else { return };
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("aura-arming-load".into()).spawn(move || {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let older = Model::from_json(&text);
            model().lock().unwrap_or_else(|e| e.into_inner()).merge_under(older);
        }
        LOADED.store(true, Ordering::Release);
    });
    if spawned.is_err() {
        // No thread: the next estimate learned tries again.
        STARTED.store(false, Ordering::Release);
    }
}

/// Write the estimates a few seconds after they changed — a preparation's
/// steps end in one write — on a thread of its own.
fn save_soon() {
    static PENDING: AtomicBool = AtomicBool::new(false);
    let Some(path) = store_path() else { return };
    if PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("aura-arming-save".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        PENDING.store(false, Ordering::Release);
        if !LOADED.load(Ordering::Acquire) {
            return;
        }
        let text = model().lock().unwrap_or_else(|e| e.into_inner()).to_json();
        if let Err(e) = write_atomic(&path, &text) {
            crate::aelog!("[PLAYER] arming estimates not saved: {}", e);
        }
    });
    if spawned.is_err() {
        // No thread: the next estimate learned asks again.
        PENDING.store(false, Ordering::Release);
    }
}

/// `text` into `path` whole or not at all: a temporary file, then a rename
/// over the old one.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

thread_local! {
    /// This thread prepares ahead of need (the next track's prewarm), below
    /// the playing track's priority and beside it: its times are not this
    /// machine's times for a start, and taught the estimates slower ones —
    /// the bar's ETA, and the prewarm's own lead, grew.
    static AHEAD: Cell<bool> = const { Cell::new(false) };
}

/// The calling thread prepares ahead of need: what it does teaches the
/// estimates nothing (`AHEAD`).
pub fn learn_nothing_here() {
    AHEAD.with(|a| a.set(true));
}

fn learn(key: &str, v: f64) {
    if AHEAD.with(|a| a.get()) {
        return;
    }
    load_in_background();
    model().lock().unwrap_or_else(|e| e.into_inner()).learn(key, v);
    save_soon();
}

/// Seconds a minute of CD-rate audio takes, for a step this machine has not
/// run yet: a little over what it took on 30.09 (2Pac, 4:44 at 44.1 kHz, and
/// Freestyler, 4:07 at 96 kHz, both at FS×8), so a first run does not
/// promise too much. The Hybrid-Phase envelope took 1 to 35 s a track.
fn default_per_minute(step: &str) -> f64 {
    match step {
        "prep:decode" => 0.1,
        "prep:stats" => 0.04,
        "prep:declip" => 0.05,
        "prep:isp" => 0.25,
        "prep:sub" => 0.2,
        "prep:aa" => 0.2,
        "prep:apod-aa" | "prep:apod" => 1.5,
        "prep:measure" => 0.1,
        "probe" => 0.35,
        "env" => 2.0,
        _ => 0.3,
    }
}

/// A chain's build (filter banks, the stages' priming), seconds.
const DEFAULT_BUILD_S: f64 = 0.6;
/// A Hybrid-Phase pair: two banks.
const DEFAULT_PAIR_BUILD_S: f64 = 1.2;
/// A new output rate: the old stream's fade, the device's reopening and the
/// new chain's pre-roll.
const REOPEN_S: f64 = 1.0;
/// A track whose length the probe did not tell counts as this long.
const UNKNOWN_LENGTH_S: f64 = 240.0;

/// The estimate of a whole-file step for a minute of audio at family
/// multiple `fam`: learned at that rate, else at another (scaled by the
/// samples a minute holds), else the default.
fn per_minute(m: &Model, step: &str, fam: u32) -> f64 {
    if let Some(v) = m.get(&format!("{step}:f{fam}")) {
        return v;
    }
    for g in [1u32, 2, 4, 8, 16] {
        if let Some(v) = m.get(&format!("{step}:f{g}")) {
            return v * fam as f64 / g as f64;
        }
    }
    default_per_minute(step) * fam as f64
}

fn build_key(taps: usize, gpu: bool, pair: bool) -> String {
    format!(
        "build:{}:{}:{}",
        crate::audio::converter::dsp::filter::taps_label(taps).unwrap_or("?"),
        if gpu { "gpu" } else { "cpu" },
        if pair { "pair" } else { "one" }
    )
}

/// The family base (44.1 or 48 kHz) of a source rate and the rate as its
/// multiple, when it is in one of the two families.
fn family(src_rate: u32) -> Option<(u32, u32)> {
    for base in [44_100u32, 48_000] {
        let mut m = 1u32;
        while m <= 16 {
            if base * m == src_rate {
                return Some((base, m));
            }
            m *= 2;
        }
    }
    None
}

/// The source rate over its family's base: what the per-minute estimates are
/// kept by (1 outside both families).
pub fn family_multiple(src_rate: u32) -> u32 {
    family(src_rate).map_or(1, |f| f.1)
}

/// The output rate a source plays at with `fs`: its family's base × fs,
/// stepped up while the source is already there (as prepare_variant does).
fn out_rate(src_rate: u32, fs: u32) -> Option<u32> {
    let (base, _) = family(src_rate)?;
    let mut f = fs.max(1);
    while base * f <= src_rate && f < 16 {
        f *= 2;
    }
    (base * f > src_rate).then_some(base * f)
}

fn is_hp(s: &PlayerSettings) -> bool {
    matches!(s.phase, Phase::Hybrid | Phase::Alpha)
}

/// What preparing a track's whole chain is expected to take on this
/// machine, in seconds: its source stages, the output probe, a Hybrid-Phase
/// rack's onset envelope and the chain's build. The next track's prewarm
/// begins well ahead of that before the end (controller.rs).
pub fn expected_prepare_s(s: &PlayerSettings, src_rate: u32, duration_s: f64) -> f64 {
    load_in_background();
    let m = model().lock().unwrap_or_else(|e| e.into_inner());
    expected_s(&m, s, src_rate, duration_s)
}

fn expected_s(m: &Model, s: &PlayerSettings, src_rate: u32, duration_s: f64) -> f64 {
    let fam = family_multiple(src_rate);
    let length_s = if duration_s > 0.0 { duration_s } else { UNKNOWN_LENGTH_S };
    let minutes = (length_s / 60.0).max(0.02);
    let pair = is_hp(s);
    let mut t: f64 = prep_steps(s, src_rate).iter().map(|st| per_minute(m, &format!("prep:{st}"), fam)).sum::<f64>()
        + per_minute(m, "probe", fam);
    if pair {
        t += per_minute(m, "env", fam);
    }
    t * minutes + m.get(&build_key(s.taps, s.use_gpu, pair)).unwrap_or(if pair { DEFAULT_PAIR_BUILD_S } else { DEFAULT_BUILD_S })
}

/// Where a continuous swap lands: this far past the reader (the
/// controller's `lead_s`).
fn lead_s(s: &PlayerSettings) -> f64 {
    let taps = if is_hp(s) { 2 * s.taps } else { s.taps };
    0.45 + taps as f64 / 30_000_000.0 * 0.6
}

// ── Probe, build and envelope times ─────────────────────────────────────

thread_local! {
    /// Seconds of output-peak probes run on this thread so far.
    static PROBE_S: Cell<f64> = const { Cell::new(0.0) };
}

/// An output-peak probe ran over `frames` of source audio at `rate` in `secs`.
pub fn learned_probe(rate: u32, frames: usize, secs: f64) {
    PROBE_S.with(|c| c.set(c.get() + secs));
    let minutes = frames as f64 / rate.max(1) as f64 / 60.0;
    if minutes >= 0.1 {
        learn(&format!("probe:f{}", family_multiple(rate)), secs / minutes);
    }
}

/// The probes this thread has run so far, in seconds: a build counts its
/// own time without the probe it ran on the way.
pub fn probe_secs_here() -> f64 {
    PROBE_S.with(|c| c.get())
}

/// What building a chain of `taps` for a rack with the GPU switch `gpu` is
/// expected to take here, one bank or a Hybrid-Phase pair: learned, else
/// what the bar plans with.
pub fn expected_build_s(taps: usize, gpu: bool, pair: bool) -> f64 {
    let m = model().lock().unwrap_or_else(|e| e.into_inner());
    m.get(&build_key(taps, gpu, pair)).unwrap_or(if pair { DEFAULT_PAIR_BUILD_S } else { DEFAULT_BUILD_S })
}

/// A chain of `taps` for a rack with the GPU switch `gpu` was built in
/// `secs`, one bank or a Hybrid-Phase pair.
pub fn learned_build(taps: usize, gpu: bool, pair: bool, secs: f64) {
    learn(&build_key(taps, gpu, pair), secs);
}

/// The Hybrid-Phase pair's swap: what its build took on the swap's thread,
/// probes and all (`hp:build:…`), and how fast it drew its sound beside the
/// chain it replaces, in seconds of audio a second (`hp:draw:…`), by taps and
/// the rack's GPU switch. None until learned here.
pub fn expected_hp_swap(taps: usize, gpu: bool) -> (Option<f64>, Option<f64>) {
    let m = model().lock().unwrap_or_else(|e| e.into_inner());
    (m.get(&hp_key("build", taps, gpu)), m.get(&hp_key("draw", taps, gpu)))
}

/// The pair was built in `build_s`; it drew at `draw_rtf` (None: it drew
/// nothing worth timing).
pub fn learned_hp_swap(taps: usize, gpu: bool, build_s: f64, draw_rtf: Option<f64>) {
    learn(&hp_key("build", taps, gpu), build_s);
    if let Some(r) = draw_rtf {
        learn(&hp_key("draw", taps, gpu), r);
    }
}

fn hp_key(what: &str, taps: usize, gpu: bool) -> String {
    format!(
        "hp:{}:{}:{}",
        what,
        crate::audio::converter::dsp::filter::taps_label(taps).unwrap_or("?"),
        if gpu { "gpu" } else { "cpu" }
    )
}

/// A Hybrid-Phase onset envelope was computed over `frames` at `rate` in
/// `secs` (not read back from disk).
pub fn learned_env(rate: u32, frames: usize, secs: f64) {
    let minutes = frames as f64 / rate.max(1) as f64 / 60.0;
    if minutes >= 0.1 {
        learn(&format!("env:f{}", family_multiple(rate)), secs / minutes);
    }
}

/// Every preparation and every Hybrid-Phase job has an id of its own: two
/// of the same file and rack at once (a rack clicked away and back, the
/// analyzer's) each write only their own record.
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

// ── Preparations ─────────────────────────────────────────────────────────

/// A preparation of a source variant, as far as it has gone.
#[derive(Clone, Debug)]
pub struct PrepView {
    /// The step it is in (`step_name` of its last status line); empty until
    /// its first.
    pub step: String,
    pub step_started: Instant,
    pub finished: Option<Instant>,
    pub ok: bool,
    /// The Hybrid-Phase envelope of the variant it made is on disk already
    /// (a later session's pair then needs no envelope work).
    pub env_on_disk: bool,
}

struct PrepRun {
    id: u64,
    path: String,
    key: String,
    started: Instant,
    view: PrepView,
}

/// Preparation runs kept, newest last.
const MAX_PREPS: usize = 32;

fn preps() -> &'static Mutex<Vec<PrepRun>> {
    static P: OnceLock<Mutex<Vec<PrepRun>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

/// The preparations of `path` for source key `key` as one: the one running
/// that began first (the furthest on) — else the newest that ended.
fn prep_of(runs: &[PrepRun], path: &str, key: &str) -> Option<PrepView> {
    let mine = || runs.iter().filter(|r| r.path == path && r.key == key);
    mine()
        .filter(|r| r.view.finished.is_none())
        .min_by_key(|r| r.started)
        .or_else(|| mine().max_by_key(|r| r.view.finished))
        .map(|r| r.view.clone())
}

pub fn prep_view(path: &str, key: &str) -> Option<PrepView> {
    prep_of(&preps().lock().unwrap_or_else(|e| e.into_inner()), path, key)
}

/// The step a status line of the engine's preparation begins: the source
/// stages by their own lines, anything else (a lab stage, a wait for memory)
/// by its words up to the first number.
pub fn step_name(line: &str) -> Option<String> {
    const KNOWN: [(&str, &str); 9] = [
        ("Decoding", "decode"),
        ("Lab: analyzing source", "stats"),
        ("Lab: declipping source", "declip"),
        ("Lab: intersample peak scan", "isp"),
        ("Subsonic filter", "sub"),
        ("Adaptive Apodizer: source analysis", "aa"),
        ("Adaptive Apodizer: applying", "apod-aa"),
        ("Applying apodizing pre-filter", "apod"),
        ("Apodizing (GPU", "apod"),
    ];
    let t = line.trim();
    if let Some((_, name)) = KNOWN.iter().find(|(head, _)| t.starts_with(head)) {
        return Some((*name).to_string());
    }
    let words = t.split(|c: char| c.is_ascii_digit()).next().unwrap_or("");
    let words = words.trim_end_matches(|c: char| c == '.' || c == '\u{2026}' || c == ':' || c.is_whitespace());
    (!words.is_empty()).then(|| words.chars().take(48).collect())
}

/// One preparation in progress: its steps go into its own record as they
/// begin, and into the estimates when it ends well.
pub struct Prep {
    id: u64,
    /// Each step and when it began.
    log: Arc<Mutex<Vec<(String, Instant)>>>,
    learn: bool,
    ended: bool,
}

/// A preparation of `path` for source key `key` begins. `learn`: its step
/// times teach the estimates (the full variant's; the quick one runs beside
/// it and is not a step of any arming).
pub fn prep_begin(path: &Path, key: &str, learn: bool) -> Prep {
    let id = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    {
        let mut g = preps().lock().unwrap_or_else(|e| e.into_inner());
        g.push(PrepRun {
            id,
            path: path.to_string_lossy().into_owned(),
            key: key.to_string(),
            started: now,
            view: PrepView { step: String::new(), step_started: now, finished: None, ok: false, env_on_disk: false },
        });
        if g.len() > MAX_PREPS {
            let n = g.len() - MAX_PREPS;
            g.drain(..n);
        }
    }
    Prep { id, log: Arc::new(Mutex::new(Vec::new())), learn, ended: false }
}

fn with_run(id: u64, f: impl FnOnce(&mut PrepView)) {
    let mut g = preps().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = g.iter_mut().rev().find(|r| r.id == id) {
        f(&mut r.view);
    }
}

fn enter(id: u64, log: &Mutex<Vec<(String, Instant)>>, name: String) {
    let now = Instant::now();
    {
        let mut l = log.lock().unwrap_or_else(|e| e.into_inner());
        if l.last().is_some_and(|(n, _)| *n == name) {
            return;
        }
        l.push((name.clone(), now));
    }
    with_run(id, |v| {
        v.step = name;
        v.step_started = now;
    });
}

impl Prep {
    /// For `decode::with_step_listener`: each status line of the
    /// preparation's thread moves it to the step the line names.
    pub fn listener(&self) -> impl FnMut(&str) + 'static {
        let id = self.id;
        let log = self.log.clone();
        move |line: &str| {
            if let Some(name) = step_name(line) {
                enter(id, &log, name);
            }
        }
    }

    /// A step the preparation marks itself (one the engine has no line for).
    pub fn step(&self, name: &str) {
        enter(self.id, &self.log, name.to_string());
    }

    /// The engine refused the rate and the preparation starts over a FS step
    /// up: what it did so far teaches nothing (a second decode would count
    /// as part of the first).
    pub fn retry(&self) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// It ended well, over `frames` of source audio at `rate`.
    pub fn finish(mut self, rate: u32, frames: usize, env_on_disk: bool) {
        self.end(true, Some((rate, frames)), env_on_disk);
    }

    fn end(&mut self, ok: bool, audio: Option<(u32, usize)>, env_on_disk: bool) {
        if self.ended {
            return;
        }
        self.ended = true;
        let now = Instant::now();
        with_run(self.id, |v| {
            v.finished = Some(now);
            v.ok = ok;
            v.env_on_disk = env_on_disk;
        });
        let Some((rate, frames)) = audio.filter(|_| ok && self.learn) else { return };
        let minutes = frames as f64 / rate.max(1) as f64 / 60.0;
        if minutes < 0.1 {
            return;
        }
        let fam = family_multiple(rate);
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (i, (name, t)) in log.iter().enumerate() {
            let end = log.get(i + 1).map_or(now, |n| n.1);
            learn(&format!("prep:{name}:f{fam}"), end.saturating_duration_since(*t).as_secs_f64() / minutes);
        }
    }
}

impl Drop for Prep {
    /// Dropped without `finish`: an error, a cancel, a rate the engine
    /// refused. Nothing is learned from it.
    fn drop(&mut self) {
        self.end(false, None, false);
    }
}

// ── Hybrid-Phase jobs ────────────────────────────────────────────────────

/// How a Hybrid-Phase job (power.rs `hp_swap`) ended, or that it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpEnd {
    Running,
    /// The pair went to the render thread.
    Sent,
    /// The pair waits for the next track (one step down a track).
    Held,
    /// The pair will not come on this arming.
    GaveUp,
    /// Left to another: a newer job, a seek that brings the pair, a new
    /// arming. Not counted.
    Stale,
}

/// A track's Hybrid-Phase job, as far as it has gone.
#[derive(Clone, Copy, Debug)]
pub struct HpJobView {
    pub started: Instant,
    /// Its onset envelope is at hand (computed or read back).
    pub env_done: Option<Instant>,
    pub end: HpEnd,
}

struct HpRun {
    id: u64,
    track: u64,
    epoch: u64,
    view: HpJobView,
}

const MAX_HP_RUNS: usize = 32;

fn hp_runs() -> &'static Mutex<Vec<HpRun>> {
    static H: OnceLock<Mutex<Vec<HpRun>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Vec::new()))
}

/// The job of `track` in rack epoch `epoch` that counts: the newest one
/// that was not left to another.
fn hp_of(runs: &[HpRun], track: u64, epoch: u64) -> Option<HpJobView> {
    runs.iter()
        .filter(|r| r.track == track && r.epoch == epoch && r.view.end != HpEnd::Stale)
        .max_by_key(|r| r.view.started)
        .map(|r| r.view)
}

pub fn hp_job_view(track: u64, epoch: u64) -> Option<HpJobView> {
    hp_of(&hp_runs().lock().unwrap_or_else(|e| e.into_inner()), track, epoch)
}

/// How an HP job that sent nothing ended, by the reason it gave: the ones
/// that leave the pair to another are not a give-up.
fn hp_end_of(why: &str) -> HpEnd {
    match why {
        "one-step-down" => HpEnd::Held,
        "generation" | "track" | "phase" | "full-ready" | "timeline" | "not-deferred" => HpEnd::Stale,
        _ => HpEnd::GaveUp,
    }
}

/// One Hybrid-Phase job in progress. Dropped without `end` — a panic, a path
/// that forgot — it gave up.
pub struct HpJob {
    id: u64,
    ended: bool,
}

/// `track`'s Hybrid-Phase job begins, in rack epoch `epoch`.
pub fn hp_job_begin(track: u64, epoch: u64) -> HpJob {
    let id = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    let mut g = hp_runs().lock().unwrap_or_else(|e| e.into_inner());
    g.push(HpRun { id, track, epoch, view: HpJobView { started: Instant::now(), env_done: None, end: HpEnd::Running } });
    if g.len() > MAX_HP_RUNS {
        let n = g.len() - MAX_HP_RUNS;
        g.drain(..n);
    }
    HpJob { id, ended: false }
}

impl HpJob {
    fn set(&self, f: impl FnOnce(&mut HpJobView)) {
        let mut g = hp_runs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = g.iter_mut().rev().find(|r| r.id == self.id) {
            f(&mut r.view);
        }
    }

    /// Its envelope is at hand: the pair's build and landing are ahead.
    pub fn envelope_ready(&self) {
        let now = Instant::now();
        self.set(|v| v.env_done = Some(now));
    }

    /// It is over: the pair sent (`None`), or the reason it sent nothing.
    pub fn end(mut self, why: Option<&str>) {
        self.ended = true;
        let end = why.map_or(HpEnd::Sent, hp_end_of);
        self.set(|v| v.end = end);
    }
}

impl Drop for HpJob {
    fn drop(&mut self) {
        if !self.ended {
            self.set(|v| v.end = HpEnd::GaveUp);
        }
    }
}

// ── The arming ───────────────────────────────────────────────────────────

/// The most an arming shows before the rack's chain is heard.
const BUSY_CAP: f64 = 0.98;
/// A finished preparation whose variant is not in the cache yet is in its
/// output-peak probe — for this long after it finished; later it is stale.
const PROBE_WINDOW: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Prep,
    Probe,
    Build,
    Land,
    Reopen,
    HpEnv,
    HpBuild,
    HpLand,
}

impl Kind {
    /// The order the steps come in (a landing and a reopening share a place).
    fn rank(self) -> u8 {
        match self {
            Kind::Prep => 0,
            Kind::Probe => 1,
            Kind::Build => 2,
            Kind::Land | Kind::Reopen => 3,
            Kind::HpEnv => 4,
            Kind::HpBuild => 5,
            Kind::HpLand => 6,
        }
    }
}

#[derive(Clone, Debug)]
struct Step {
    kind: Kind,
    name: String,
    /// Expected seconds: the step's weight.
    est: f64,
}

impl Step {
    /// The step as the status names it.
    fn label(&self) -> String {
        if self.kind == Kind::Prep { format!("prep:{}", self.name) } else { self.name.clone() }
    }
}

/// A chain in the timeline (the one heard, or the one placed last).
#[derive(Clone, Debug)]
pub struct Air {
    pub track: u64,
    /// "live", "file" or "direct".
    pub source: &'static str,
    pub quick: bool,
    /// The source key of the settings it was built with.
    pub key: String,
    /// Built for the rack: the same settings, a downgrade's taps and the GPU
    /// switch aside.
    pub same_chain: bool,
    /// Plays linear phase until its Hybrid-Phase envelope is ready.
    pub hp_deferred: bool,
}

impl Air {
    pub fn of(track: u64, d: &super::render::ChainDesc, rack: &PlayerSettings) -> Air {
        Air {
            track,
            source: d.source,
            quick: d.quick,
            key: d.settings.source_key(),
            same_chain: same_chain(&d.settings, rack, d.downgrade.is_some(), d.hp_deferred),
            hp_deferred: d.hp_deferred,
        }
    }
}

/// A chain built with `built` is the rack's chain: a downgrade's taps (the
/// player's own call, final for the track), the GPU switch (a failure moves
/// the chain to the CPU) and a deferred pair's stand-in phase aside.
pub fn same_chain(built: &PlayerSettings, rack: &PlayerSettings, downgraded: bool, hp_deferred: bool) -> bool {
    let mut b = built.clone();
    b.use_gpu = rack.use_gpu;
    if downgraded {
        b.taps = rack.taps;
    }
    if hp_deferred && is_hp(rack) {
        b.phase = rack.phase;
    }
    b == *rack
}

/// What the player's state says, gathered by `Player::status` (memory only).
pub struct Obs {
    /// A track plays, is paused, or is being prepared before its first
    /// sound.
    pub active: bool,
    pub track: u64,
    /// The rack epoch: a new track start, a rack change or a stop moves it —
    /// a seek does not.
    pub epoch: u64,
    /// The rack the track plays with, and its source key (the cache's key).
    pub settings: PlayerSettings,
    pub key: String,
    pub duration_s: f64,
    pub src_rate: u32,
    /// The full variant is in the cache.
    pub full_ready: bool,
    /// The Hybrid-Phase envelope is at hand (in memory, or on disk as the
    /// preparation found it).
    pub env_ready: bool,
    /// Instant start off: the whole chain is made before the first sound — a
    /// Hybrid-Phase rack's envelope in the preparation, before the chain's
    /// build, not after it is heard; nothing stands in on linear phase.
    pub whole_first: bool,
    /// The device's rate now (0: no stream).
    pub stream_rate: u32,
    /// The chain heard now.
    pub air: Option<Air>,
    /// The chain placed last, and how far past the reader its splice lies (s).
    pub newest: Option<(Air, f64)>,
    /// The device is being switched to another rate or mode for this rack.
    pub rate_switch: bool,
    /// How far the render runs ahead of the reader — at a start, where it is
    /// going (its target): the pair goes in just behind the write head.
    pub buffer_s: f64,
    pub prep: Option<PrepView>,
    pub hp: Option<HpJobView>,
}

/// How far the arming is, for the status.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// Which arming: a new one begins from nothing.
    pub seq: u64,
    pub frac: f64,
    pub step: String,
    /// Work is still on its way to the air.
    pub busy: bool,
    /// Seconds still expected.
    pub left_s: f64,
}

impl View {
    pub fn json(&self) -> Value {
        json!({
            "seq": self.seq,
            "frac": (self.frac * 1000.0).round() / 1000.0,
            "step": self.step,
            "busy": self.busy,
            "leftS": (self.left_s * 10.0).round() / 10.0,
        })
    }
}

/// A step's share done after `x` of its estimate: the estimate itself up to
/// 80 %, then creeping towards 95 % — slower and slower, never stopping —
/// for a step that takes longer than expected. The tail is a hyperbola, not
/// an exponential: one that met 80 % at the same pace sat at 95 % once the
/// step had run twice its estimate, and looked stuck.
fn creep(x: f64) -> f64 {
    const K: f64 = 0.15;
    if x.is_nan() || x <= 0.0 {
        0.0
    } else if x <= 0.8 {
        x
    } else {
        0.95 - 0.15 * K / (K + (x - 0.8))
    }
}

/// The source stages a full preparation will run, in order, for the rack
/// `s` (as prepare_audio_phase decides them), and the measure after them.
fn prep_steps(s: &PlayerSettings, src_rate: u32) -> Vec<&'static str> {
    let mut v = vec!["decode"];
    if s.declip || s.adaptive_headroom || s.isp {
        v.push("stats");
    }
    if s.declip {
        v.push("declip");
    }
    if s.isp {
        v.push("isp");
    }
    if s.subsonic_hz != 0 {
        v.push("sub");
    }
    if s.adaptive_apodizer {
        v.push("aa");
        // Above 48 kHz the analysis mostly finds a true hi-res source and
        // leaves it; a filter that does run there comes in as it begins.
        if src_rate <= 48_000 {
            v.push("apod-aa");
        }
    } else if s.apodizing > 0 && src_rate <= 48_000 {
        v.push("apod");
    }
    v.push("measure");
    v
}

/// The two apodizing steps are one place in a plan: the adaptive filter, or
/// the static preset it falls back to.
fn same_step(planned: &str, seen: &str) -> bool {
    planned == seen || (planned.starts_with("apod") && seen.starts_with("apod"))
}

/// The rack's chain for this track: the full variant with the rack's source
/// stages and settings (a downgrade's taps and the GPU switch aside).
fn is_target(o: &Obs, a: &Air) -> bool {
    a.track == o.track && a.source == "live" && !a.quick && a.key == o.key && a.same_chain
}

/// The rack's chain as the arming ends with it: with its Hybrid-Phase pair,
/// not the pair's linear stand-in.
fn is_final(o: &Obs, a: &Air) -> bool {
    is_target(o, a) && !(a.hp_deferred && is_hp(&o.settings))
}

/// The rack's pair stand-in: the rack's chain, playing linear phase until
/// its envelope is ready.
fn is_stand_in(o: &Obs, a: &Air) -> bool {
    is_target(o, a) && a.hp_deferred && is_hp(&o.settings)
}

/// The rack's chain is heard, and nothing but it placed after it.
fn reached(o: &Obs) -> bool {
    o.air.as_ref().is_some_and(|a| is_final(o, a))
        && o.newest.as_ref().is_none_or(|(n, _)| is_final(o, n))
        && !o.rate_switch
}

/// Nothing more is coming on this arming, and why: the stand-in plays and
/// its job left the pair to the next track or gave up; or the full
/// variant's preparation failed and none runs.
fn stalled(o: &Obs) -> Option<&'static str> {
    let stand_in = o.air.as_ref().is_some_and(|a| is_stand_in(o, a))
        && !o.newest.as_ref().is_some_and(|(n, _)| is_final(o, n));
    if stand_in {
        match o.hp.map(|j| j.end) {
            Some(HpEnd::Held) => return Some("held"),
            Some(HpEnd::GaveUp) => return Some("gave-up"),
            _ => {}
        }
    }
    if !o.full_ready && o.prep.as_ref().is_some_and(|p| p.finished.is_some() && !p.ok) {
        return Some("failed");
    }
    None
}

/// The step the state shows: its kind, a preparation step's name, when the
/// state says it began, and for a landing the splice's distance.
struct Seen {
    kind: Kind,
    name: Option<String>,
    began: Option<Instant>,
    splice_s: Option<f64>,
}

fn seen(kind: Kind) -> Seen {
    Seen { kind, name: None, began: None, splice_s: None }
}

fn observed(o: &Obs, now: Instant) -> Seen {
    if o.rate_switch {
        return seen(Kind::Reopen);
    }
    // The rack's own chain placed, not yet heard: its landing. A chain a
    // seek or a power step placed is the rack's only when it is.
    if let Some((_, ahead)) = o.newest.as_ref().filter(|(n, _)| is_final(o, n)) {
        if !o.air.as_ref().is_some_and(|a| is_final(o, a)) {
            let pair = is_hp(&o.settings) && o.air.as_ref().is_some_and(|a| is_stand_in(o, a));
            return Seen { splice_s: Some(*ahead), ..seen(if pair { Kind::HpLand } else { Kind::Land }) };
        }
    }
    // The rack's full chain placed as the pair's stand-in, not yet heard:
    // its landing comes before the pair's work shows (the job has begun
    // meanwhile; its time counts from its start).
    if let Some((_, ahead)) = o.newest.as_ref().filter(|(n, _)| is_stand_in(o, n)) {
        if !o.air.as_ref().is_some_and(|a| is_target(o, a)) {
            return Seen { splice_s: Some(*ahead), ..seen(Kind::Land) };
        }
    }
    // The stand-in plays or is placed: the pair comes, by its job.
    if o.air.as_ref().is_some_and(|a| is_stand_in(o, a)) || o.newest.as_ref().is_some_and(|(n, _)| is_stand_in(o, n)) {
        return match o.hp {
            Some(j) if j.end == HpEnd::Sent => seen(Kind::HpLand),
            Some(j) if j.env_done.is_some() => Seen { began: j.env_done, ..seen(Kind::HpBuild) },
            Some(j) => Seen { began: Some(j.started), ..seen(Kind::HpEnv) },
            None if o.env_ready => seen(Kind::HpBuild),
            None => seen(Kind::HpEnv),
        };
    }
    // Instant start off: the full variant is made, its envelope is being
    // made — before the chain's build.
    if o.whole_first && o.full_ready && is_hp(&o.settings) && !o.env_ready {
        return seen(Kind::HpEnv);
    }
    if o.full_ready {
        return seen(Kind::Build);
    }
    match &o.prep {
        Some(p) if p.finished.is_none() && !p.step.is_empty() => {
            Seen { name: Some(p.step.clone()), began: Some(p.step_started), ..seen(Kind::Prep) }
        }
        Some(p) if p.ok && p.finished.is_some_and(|f| now.saturating_duration_since(f) < PROBE_WINDOW) => {
            Seen { began: p.finished, ..seen(Kind::Probe) }
        }
        _ => seen(Kind::Prep),
    }
}

struct Arming {
    seq: u64,
    track: u64,
    epoch: u64,
    steps: Vec<Step>,
    /// The step the player is in; it only moves on.
    at: usize,
    /// When that step began.
    since: Instant,
    /// The splice's distance when its landing was first seen.
    land_total: Option<f64>,
    frac: f64,
    /// The rack's chain was heard: the arming is over for good.
    done: bool,
    /// Nothing more is coming, for now (a newer preparation or job ends it).
    stall: Option<&'static str>,
    fam: u32,
    minutes: f64,
}

impl Arming {
    fn new(seq: u64, o: &Obs, now: Instant, m: &Model) -> Arming {
        let length_s = if o.duration_s > 0.0 { o.duration_s } else { UNKNOWN_LENGTH_S };
        let mut a = Arming {
            seq,
            track: o.track,
            epoch: o.epoch,
            steps: Vec::new(),
            at: 0,
            since: now,
            land_total: None,
            frac: 0.0,
            done: reached(o),
            stall: None,
            fam: family_multiple(o.src_rate),
            minutes: (length_s / 60.0).max(0.02),
        };
        if !a.done {
            a.steps = a.plan(o, m);
        }
        a
    }

    fn prep_step(&self, name: &str, m: &Model) -> Step {
        Step { kind: Kind::Prep, name: name.to_string(), est: per_minute(m, &format!("prep:{name}"), self.fam) * self.minutes }
    }

    fn hp_steps(&self, o: &Obs, m: &Model) -> [Step; 3] {
        let s = &o.settings;
        [
            Step { kind: Kind::HpEnv, name: "hp:env".into(), est: per_minute(m, "env", self.fam) * self.minutes },
            Step {
                kind: Kind::HpBuild,
                name: "hp:build".into(),
                // The pair's banks, and its own output-peak probe.
                est: m.get(&build_key(s.taps, s.use_gpu, true)).unwrap_or(DEFAULT_PAIR_BUILD_S)
                    + per_minute(m, "probe", self.fam) * self.minutes,
            },
            // The pair goes in just behind the write head: heard when the
            // reader gets there.
            Step { kind: Kind::HpLand, name: "hp:land".into(), est: o.buffer_s.clamp(0.5, 6.0) },
        ]
    }

    fn landing(&self, o: &Obs) -> Step {
        let s = &o.settings;
        let reopen = o.stream_rate != 0 && out_rate(o.src_rate, s.fs_multiplier).is_some_and(|r| r != o.stream_rate);
        if reopen {
            Step { kind: Kind::Reopen, name: "reopen".into(), est: REOPEN_S }
        } else {
            Step { kind: Kind::Land, name: "land".into(), est: lead_s(s) }
        }
    }

    fn plan(&self, o: &Obs, m: &Model) -> Vec<Step> {
        let s = &o.settings;
        let mut steps = Vec::new();
        if !o.full_ready {
            for name in prep_steps(s, o.src_rate) {
                steps.push(self.prep_step(name, m));
            }
        }
        // Instant start off: a Hybrid-Phase rack's envelope comes before the
        // first sound, then its output probe and the pair's own build.
        let env_first = o.whole_first && is_hp(s) && !o.env_ready;
        if env_first {
            steps.push(Step { kind: Kind::HpEnv, name: "hp:env".into(), est: per_minute(m, "env", self.fam) * self.minutes });
        }
        if !o.full_ready || env_first {
            steps.push(Step { kind: Kind::Probe, name: "probe".into(), est: per_minute(m, "probe", self.fam) * self.minutes });
        }
        // The pair is built at the start when it needs no envelope first.
        let pair = is_hp(s) && (o.whole_first || o.env_ready);
        let build_est = m.get(&build_key(s.taps, s.use_gpu, pair)).unwrap_or(if pair { DEFAULT_PAIR_BUILD_S } else { DEFAULT_BUILD_S });
        steps.push(Step { kind: Kind::Build, name: "build".into(), est: build_est });
        steps.push(self.landing(o));
        if is_hp(s) && !o.env_ready && !o.whole_first {
            steps.extend(self.hp_steps(o, m));
        }
        steps
    }

    /// Where in the plan the observed step is: a step the plan did not
    /// expect (a lab stage, the envelope after all, a reopen) goes in at its
    /// place in the order. None: nothing to move to.
    fn slot(&mut self, kind: Kind, name: Option<&str>, o: &Obs, m: &Model) -> Option<usize> {
        if kind == Kind::Prep {
            let name = name?;
            if let Some(j) = self.steps.iter().position(|s| s.kind == Kind::Prep && same_step(&s.name, name)) {
                return Some(j);
            }
            // After the step it is in (a preparation's steps come in order).
            let j = if self.steps.get(self.at).is_some_and(|s| s.kind == Kind::Prep) {
                self.at + 1
            } else {
                self.steps.iter().position(|s| s.kind.rank() > Kind::Prep.rank()).unwrap_or(self.steps.len())
            };
            let step = self.prep_step(name, m);
            self.insert(j, vec![step]);
            return Some(j);
        }
        if let Some(j) = self.steps.iter().position(|s| s.kind == kind) {
            return Some(j);
        }
        let new: Vec<Step> = match kind {
            Kind::HpEnv | Kind::HpBuild | Kind::HpLand => self.hp_steps(o, m).into_iter().collect(),
            Kind::Land => vec![Step { kind, name: "land".into(), est: lead_s(&o.settings) }],
            Kind::Reopen => vec![Step { kind, name: "reopen".into(), est: REOPEN_S }],
            Kind::Probe => vec![Step { kind, name: "probe".into(), est: per_minute(m, "probe", self.fam) * self.minutes }],
            Kind::Build => vec![Step {
                kind,
                name: "build".into(),
                est: m.get(&build_key(o.settings.taps, o.settings.use_gpu, false)).unwrap_or(DEFAULT_BUILD_S),
            }],
            Kind::Prep => unreachable!(),
        };
        let pos = self.steps.iter().position(|s| s.kind.rank() > kind.rank()).unwrap_or(self.steps.len());
        let first = new.iter().position(|s| s.kind == kind).unwrap_or(0);
        self.insert(pos, new);
        Some(pos + first)
    }

    /// `new` into the plan at `pos`; the step the player is in stays its step.
    fn insert(&mut self, pos: usize, new: Vec<Step>) {
        let n = new.len();
        let had = self.steps.len();
        self.steps.splice(pos..pos, new);
        if had > 0 && pos <= self.at {
            self.at += n;
        }
    }

    fn advance(&mut self, o: &Obs, now: Instant, m: &Model) {
        if self.done {
            return;
        }
        if reached(o) {
            self.done = true;
            self.stall = None;
            return;
        }
        self.stall = stalled(o);
        if self.stall.is_some() {
            return;
        }
        if self.steps.is_empty() {
            self.steps = self.plan(o, m);
        }
        let s = observed(o, now);
        let Some(j) = self.slot(s.kind, s.name.as_deref(), o, m) else { return };
        if j > self.at {
            self.at = j;
            self.since = s.began.unwrap_or(now);
            self.land_total = None;
        }
    }

    fn view(&mut self, o: &Obs, now: Instant) -> View {
        if self.done || self.steps.is_empty() {
            self.frac = 1.0;
            return View { seq: self.seq, frac: 1.0, step: "done".into(), busy: false, left_s: 0.0 };
        }
        if let Some(why) = self.stall {
            return View { seq: self.seq, frac: self.frac, step: why.into(), busy: false, left_s: 0.0 };
        }
        let at = self.at.min(self.steps.len() - 1);
        let total: f64 = self.steps.iter().map(|s| s.est).sum::<f64>().max(1e-6);
        let before: f64 = self.steps[..at].iter().map(|s| s.est).sum();
        let after: f64 = self.steps[at + 1..].iter().map(|s| s.est).sum();
        let st = &self.steps[at];
        let splice = match observed(o, now) {
            Seen { kind, splice_s: Some(left), .. } if kind == st.kind => Some(left),
            _ => None,
        };
        let p = match splice {
            Some(left) => {
                let tot = self.land_total.get_or_insert(left);
                if left > *tot {
                    *tot = left;
                }
                if *tot <= 1e-6 { 1.0 } else { (1.0 - left / *tot).clamp(0.0, 1.0) }
            }
            None => creep(now.saturating_duration_since(self.since).as_secs_f64() / st.est.max(1e-3)),
        };
        let f = ((before + st.est * p) / total).min(BUSY_CAP);
        self.frac = self.frac.max(f);
        View { seq: self.seq, frac: self.frac, step: st.label(), busy: true, left_s: st.est * (1.0 - p) + after }
    }
}

/// The armings, one at a time.
#[derive(Default)]
pub struct Tracker {
    cur: Option<Arming>,
    seq: u64,
}

impl Tracker {
    /// The arming the state `o` is in at `now`; None when nothing arms: no
    /// track plays or is being prepared, BIT-PERFECT, or a converted file
    /// plays.
    pub fn observe(&mut self, o: &Obs, now: Instant, m: &Model) -> Option<View> {
        let nothing = !o.active
            || o.settings.mode == Mode::Direct
            || o.air.as_ref().is_some_and(|a| a.track == o.track && a.source != "live");
        if nothing {
            self.cur = None;
            return None;
        }
        if !self.cur.as_ref().is_some_and(|a| a.track == o.track && a.epoch == o.epoch) {
            self.seq += 1;
            self.cur = Some(Arming::new(self.seq, o, now, m));
        }
        let a = self.cur.as_mut()?;
        a.advance(o, now, m);
        Some(a.view(o, now))
    }
}

/// The player's arming, from what its state says now.
pub fn observe(o: &Obs) -> Option<View> {
    static T: OnceLock<Mutex<Tracker>> = OnceLock::new();
    let m = model().lock().unwrap_or_else(|e| e.into_inner());
    T.get_or_init(|| Mutex::new(Tracker::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .observe(o, Instant::now(), &m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    /// A thread that prepares ahead of need (the next track's prewarm, below
    /// the playing track's priority) teaches the estimates nothing: its
    /// slower times made the bar's ETA, and the prewarm's own lead, grow.
    /// Another thread's times do teach them.
    #[test]
    fn a_prewarm_teaches_the_estimates_nothing() {
        // A key no other test learns: a 5k pair on the GPU.
        let key = build_key(5_000, true, true);
        std::thread::spawn(|| {
            learn_nothing_here();
            learned_build(5_000, true, true, 99.0);
        })
        .join()
        .unwrap();
        let ahead = model().lock().unwrap().get(&key);
        std::thread::spawn(|| learned_build(5_000, true, true, 1.0)).join().unwrap();
        let start = model().lock().unwrap().get(&key);

        assert_eq!(ahead, None, "the prewarm's build time is not learned");
        assert!(start.is_some(), "a start's build time is");
    }

    fn rack() -> PlayerSettings {
        PlayerSettings::default()
    }

    fn air(o: &Obs, quick: bool, hp_deferred: bool) -> Air {
        Air { track: o.track, source: "live", quick, key: o.key.clone(), same_chain: true, hp_deferred }
    }

    /// A 4-minute CD track just started: the quick variant plays, nothing
    /// of the full one yet.
    fn start() -> Obs {
        let s = rack();
        let mut o = Obs {
            active: true,
            track: 7,
            epoch: 3,
            key: s.source_key(),
            duration_s: 240.0,
            src_rate: RATE,
            full_ready: false,
            env_ready: false,
            whole_first: false,
            stream_rate: 352_800,
            air: None,
            newest: None,
            rate_switch: false,
            buffer_s: 3.0,
            prep: None,
            hp: None,
            settings: s,
        };
        let q = air(&o, true, false);
        o.newest = Some((q.clone(), 0.0));
        o.air = Some(q);
        o
    }

    fn prep_in(t0: Instant, step: &str, began: f64) -> Option<PrepView> {
        Some(PrepView { step: step.into(), step_started: t0 + secs(began), finished: None, ok: false, env_on_disk: false })
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// The rack's full chain is heard (with its pair: not deferred).
    fn on_air(o: &mut Obs) {
        o.full_ready = true;
        let a = air(o, false, false);
        o.newest = Some((a.clone(), 0.0));
        o.air = Some(a);
    }

    /// The rack's full chain placed `ahead_s` past the reader, not heard yet.
    fn placed(o: &mut Obs, ahead_s: f64, hp_deferred: bool) {
        o.full_ready = true;
        o.newest = Some((air(o, false, hp_deferred), ahead_s));
    }

    fn hp_rack(o: &mut Obs) {
        o.settings.phase = Phase::Hybrid;
        o.key = o.settings.source_key();
    }

    fn job(t0: Instant, env_done: Option<f64>, end: HpEnd) -> Option<HpJobView> {
        Some(HpJobView { started: t0, env_done: env_done.map(|s| t0 + secs(s)), end })
    }

    #[test]
    fn a_track_start_climbs_through_its_steps_to_the_air_and_never_back() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        let mut seen = Vec::new();
        let mut at = |t: &mut Tracker, o: &Obs, s: f64| {
            let v = t.observe(o, t0 + secs(s), &m).expect("a live track arms");
            seen.push(v.frac);
            v
        };
        let v = at(&mut t, &o, 0.0);
        assert!(v.busy && v.frac < 0.01, "a fresh arming starts from nothing: {v:?}");
        o.prep = prep_in(t0, "decode", 0.0);
        let v = at(&mut t, &o, 0.3);
        assert_eq!(v.step, "prep:decode");
        assert!(v.frac > 0.0);
        o.prep = prep_in(t0, "isp", 1.0);
        let v = at(&mut t, &o, 1.5);
        assert_eq!(v.step, "prep:isp");
        o.prep = prep_in(t0, "apod-aa", 3.0);
        at(&mut t, &o, 6.0);
        let mut p = o.prep.clone().unwrap();
        p.finished = Some(t0 + secs(9.0));
        p.ok = true;
        o.prep = Some(p);
        assert_eq!(at(&mut t, &o, 9.2).step, "probe");
        o.full_ready = true;
        assert_eq!(at(&mut t, &o, 10.0).step, "build");
        placed(&mut o, 0.8, false);
        let v = at(&mut t, &o, 10.5);
        assert_eq!(v.step, "land");
        placed(&mut o, 0.1, false);
        let v = at(&mut t, &o, 11.2);
        assert!(v.busy && v.frac <= BUSY_CAP, "not heard yet: at most {BUSY_CAP}, {v:?}");
        on_air(&mut o);
        let v = at(&mut t, &o, 11.4);
        assert_eq!((v.busy, v.frac), (false, 1.0));
        assert!(seen.windows(2).all(|w| w[1] >= w[0]), "the bar went back: {seen:?}");
    }

    /// Instant start off, Hybrid-Phase: the track waits in silence for its
    /// whole chain — the variant, then its envelope, the pair's build and the
    /// device's start — and the bar climbs through them before the first
    /// sound: the envelope's share is filled while the envelope is made, not
    /// jumped over when the pair is heard. One arming, never back.
    #[test]
    fn with_instant_start_off_the_envelope_is_armed_before_the_first_sound() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        hp_rack(&mut o);
        o.whole_first = true;
        // Waiting: no stream, nothing on the air.
        o.stream_rate = 0;
        o.air = None;
        o.newest = None;
        let mut seen = Vec::new();
        let mut at = |t: &mut Tracker, o: &Obs, s: f64| {
            let v = t.observe(o, t0 + secs(s), &m).expect("a track being prepared arms");
            seen.push((v.seq, v.frac));
            v
        };
        o.prep = prep_in(t0, "decode", 0.0);
        at(&mut t, &o, 0.2);
        let mut p = o.prep.clone().unwrap();
        p.finished = Some(t0 + secs(2.0));
        p.ok = true;
        o.prep = Some(p);
        // The variant is cached; its envelope is being made.
        o.full_ready = true;
        let env_start = at(&mut t, &o, 2.1);
        assert_eq!(env_start.step, "hp:env");
        let env_late = at(&mut t, &o, 6.0);
        assert!(env_late.busy && env_late.frac > env_start.frac, "the envelope's share fills while it is made: {env_late:?}");
        // The envelope is ready: the pair is built, then the device starts.
        o.env_ready = true;
        assert_eq!(at(&mut t, &o, 7.0).step, "build");
        o.stream_rate = 352_800;
        placed(&mut o, 0.3, false);
        let land = at(&mut t, &o, 7.5);
        assert_eq!(land.step, "land");
        assert!(land.frac > env_late.frac && land.frac <= BUSY_CAP, "{land:?}");
        on_air(&mut o);
        let v = at(&mut t, &o, 7.9);
        assert_eq!((v.busy, v.frac), (false, 1.0));
        assert!(seen.iter().all(|&(q, _)| q == seen[0].0), "one arming from the press to the sound: {seen:?}");
        assert!(seen.windows(2).all(|w| w[1].1 >= w[0].1), "the bar went back: {seen:?}");
    }

    #[test]
    fn a_new_track_or_rack_starts_over_a_seek_does_not() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "isp", 0.0);
        let a = t.observe(&o, t0 + secs(2.0), &m).unwrap();
        assert!(a.frac > 0.05);
        // A seek: a chain of the quick variant placed further on, the same epoch.
        o.newest = Some((air(&o, true, false), 0.05));
        let b = t.observe(&o, t0 + secs(2.1), &m).unwrap();
        assert_eq!(b.seq, a.seq, "a seek keeps the arming");
        assert!(b.frac >= a.frac);
        o.epoch += 1;
        o.prep = None;
        let c = t.observe(&o, t0 + secs(2.2), &m).unwrap();
        assert_eq!(c.seq, a.seq + 1);
        assert!(c.frac < 0.01, "a rack change begins from nothing: {c:?}");
        o.track = 8;
        o.air = None;
        o.newest = None;
        let d = t.observe(&o, t0 + secs(2.3), &m).unwrap();
        assert_eq!(d.seq, c.seq + 1);
    }

    #[test]
    fn a_step_past_its_estimate_creeps_but_stays_under_its_share() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "decode", 0.0);
        // decode is the first step: its whole share is the bound.
        let a = Arming::new(1, &o, t0, &m);
        let total: f64 = a.steps.iter().map(|s| s.est).sum();
        let share = a.steps[0].est / total;
        let mut last = 0.0;
        for s in [1.0, 5.0, 30.0, 300.0, 3000.0] {
            let v = t.observe(&o, t0 + secs(s), &m).unwrap();
            assert!(v.frac < 0.95 * share + 1e-9, "at {s} s {} ≥ 95 % of the step's share {share}", v.frac);
            assert!(v.frac >= last);
            last = v.frac;
        }
        assert!(creep(10.0) < 0.95 && creep(10.0) > creep(5.0) && creep(5.0) > creep(1.0));
        assert!(creep(2.0) < 0.94, "twice the estimate is not yet the wall: {}", creep(2.0));
        assert!(creep(1e6) < 0.95 && creep(1e6) > creep(1e5), "still moving, never there");
        assert!((creep(0.8) - 0.8).abs() < 1e-12 && creep(0.0) == 0.0 && creep(f64::NAN) == 0.0);
    }

    #[test]
    fn a_step_done_early_moves_the_bar_on() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "decode", 0.0);
        let a = t.observe(&o, t0 + secs(0.1), &m).unwrap();
        // The ISP scan begins at once: decode, stats and declip are behind.
        o.prep = prep_in(t0, "isp", 0.1);
        let b = t.observe(&o, t0 + secs(0.1), &m).unwrap();
        assert!(b.frac > a.frac + 0.01, "{a:?} → {b:?}");
    }

    #[test]
    fn a_landing_is_the_distance_to_the_splice() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        placed(&mut o, 1.0, false);
        let a = t.observe(&o, t0, &m).unwrap();
        assert_eq!(a.step, "land");
        placed(&mut o, 0.5, false);
        let b = t.observe(&o, t0 + secs(0.5), &m).unwrap();
        placed(&mut o, 0.0, false);
        let c = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert!(a.frac < b.frac && b.frac < c.frac, "{a:?} {b:?} {c:?}");
        assert_eq!(c.frac, BUSY_CAP, "landed but not heard yet");
    }

    #[test]
    fn a_seek_or_a_power_step_is_not_the_racks_landing() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "isp", 0.0);
        // A seek places the quick variant again: not the rack's chain.
        o.newest = Some((air(&o, true, false), 0.3));
        let v = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert_eq!(v.step, "prep:isp");
        // The rack's chain heard, then a power step places it at fewer taps:
        // still the rack's chain, the arming stays done.
        on_air(&mut o);
        assert!(!t.observe(&o, t0 + secs(2.0), &m).unwrap().busy);
        o.newest = Some((air(&o, false, false), 0.3));
        assert!(!t.observe(&o, t0 + secs(2.1), &m).unwrap().busy);
    }

    #[test]
    fn a_seek_while_the_stand_in_plays_does_not_jump_to_the_pairs_landing() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        hp_rack(&mut o);
        o.full_ready = true;
        let stand_in = air(&o, false, true);
        o.air = Some(stand_in.clone());
        o.newest = Some((stand_in.clone(), 0.0));
        o.hp = job(t0, None, HpEnd::Running);
        let a = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert_eq!((a.step.as_str(), a.busy), ("hp:env", true));
        // The seek: another stand-in placed (the envelope is not ready yet), a new job.
        o.newest = Some((stand_in.clone(), 0.2));
        o.hp = job(t0 + secs(1.2), None, HpEnd::Running);
        let b = t.observe(&o, t0 + secs(1.3), &m).unwrap();
        assert_eq!(b.step, "hp:env", "a seek's swap is not the pair landing");
        assert!(b.frac >= a.frac);
        // The envelope, the pair placed, the pair heard.
        o.hp = job(t0 + secs(1.2), Some(3.0), HpEnd::Running);
        assert_eq!(t.observe(&o, t0 + secs(4.5), &m).unwrap().step, "hp:build");
        o.hp = job(t0 + secs(1.2), Some(3.0), HpEnd::Sent);
        o.newest = Some((air(&o, false, false), 2.5));
        let c = t.observe(&o, t0 + secs(5.0), &m).unwrap();
        assert_eq!(c.step, "hp:land");
        on_air(&mut o);
        let d = t.observe(&o, t0 + secs(7.5), &m).unwrap();
        assert_eq!((d.busy, d.frac), (false, 1.0));
    }

    #[test]
    fn the_stand_in_lands_before_the_pairs_work_shows() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        hp_rack(&mut o);
        o.full_ready = true;
        // The quick variant plays; the full chain went in as the stand-in, the
        // job has begun.
        o.newest = Some((air(&o, false, true), 0.6));
        o.hp = job(t0, None, HpEnd::Running);
        let a = t.observe(&o, t0 + secs(0.2), &m).unwrap();
        assert_eq!(a.step, "land");
        let stand_in = air(&o, false, true);
        o.air = Some(stand_in.clone());
        o.newest = Some((stand_in, 0.0));
        let b = t.observe(&o, t0 + secs(0.9), &m).unwrap();
        assert_eq!(b.step, "hp:env");
        assert!(b.frac >= a.frac);
    }

    #[test]
    fn a_failed_preparation_is_nothing_more_coming_until_another_runs() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "isp", 0.0);
        let a = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert!(a.busy);
        let mut p = o.prep.clone().unwrap();
        p.finished = Some(t0 + secs(1.5));
        p.ok = false;
        o.prep = Some(p);
        let b = t.observe(&o, t0 + secs(2.0), &m).unwrap();
        assert_eq!((b.busy, b.step.as_str()), (false, "failed"));
        assert_eq!(b.frac, a.frac.max(b.frac), "it stays as it stands");
        // A seek asks for it again: busy once more, the same arming.
        o.prep = prep_in(t0 + secs(3.0), "decode", 3.0);
        let c = t.observe(&o, t0 + secs(3.2), &m).unwrap();
        assert!(c.busy && c.seq == a.seq && c.frac >= b.frac);
    }

    #[test]
    fn a_pair_whose_job_gave_up_or_waits_for_the_next_track_is_not_waited_for() {
        let m = Model::default();
        let t0 = Instant::now();
        for (end, why) in [(HpEnd::GaveUp, "gave-up"), (HpEnd::Held, "held")] {
            let mut t = Tracker::default();
            let mut o = start();
            hp_rack(&mut o);
            o.full_ready = true;
            let stand_in = air(&o, false, true);
            o.air = Some(stand_in.clone());
            o.newest = Some((stand_in, 0.0));
            o.hp = job(t0, None, HpEnd::Running);
            assert!(t.observe(&o, t0 + secs(1.0), &m).unwrap().busy);
            o.hp = job(t0, None, end);
            let v = t.observe(&o, t0 + secs(2.0), &m).unwrap();
            assert_eq!((v.busy, v.step.as_str()), (false, why));
            // A newer job (after a seek) runs: busy again.
            o.hp = job(t0 + secs(3.0), None, HpEnd::Running);
            assert!(t.observe(&o, t0 + secs(3.5), &m).unwrap().busy);
        }
    }

    #[test]
    fn hp_jobs_count_by_track_and_epoch_the_newest_not_left_to_another() {
        let t0 = Instant::now();
        let run = |id, track, epoch, s: f64, end| HpRun {
            id,
            track,
            epoch,
            view: HpJobView { started: t0 + secs(s), env_done: None, end },
        };
        let runs = [
            run(1, 7, 3, 0.0, HpEnd::GaveUp),
            run(2, 7, 3, 1.0, HpEnd::Running),
            run(3, 7, 3, 2.0, HpEnd::Stale),
            run(4, 7, 2, 3.0, HpEnd::Held),
            run(5, 8, 3, 4.0, HpEnd::Sent),
        ];
        assert_eq!(hp_of(&runs, 7, 3).map(|v| v.end), Some(HpEnd::Running), "a stale one does not count");
        assert_eq!(hp_of(&runs, 7, 2).map(|v| v.end), Some(HpEnd::Held));
        assert!(hp_of(&runs, 7, 4).is_none(), "an old play's job is not this arming's");
        assert_eq!(hp_end_of("one-step-down"), HpEnd::Held);
        assert_eq!(hp_end_of("generation"), HpEnd::Stale);
        assert_eq!(hp_end_of("busy"), HpEnd::GaveUp);
        assert_eq!(hp_end_of("build: out of memory"), HpEnd::GaveUp);
    }

    #[test]
    fn a_job_dropped_without_an_end_gave_up() {
        let a = hp_job_begin(90_001, 1);
        a.envelope_ready();
        assert_eq!(hp_job_view(90_001, 1).map(|v| (v.end, v.env_done.is_some())), Some((HpEnd::Running, true)));
        drop(a);
        assert_eq!(hp_job_view(90_001, 1).map(|v| v.end), Some(HpEnd::GaveUp));
        hp_job_begin(90_001, 1).end(None);
        assert_eq!(hp_job_view(90_001, 1).map(|v| v.end), Some(HpEnd::Sent), "the newer job counts");
        hp_job_begin(90_001, 1).end(Some("generation"));
        assert_eq!(hp_job_view(90_001, 1).map(|v| v.end), Some(HpEnd::Sent), "a stale one is left out");
    }

    #[test]
    fn preparations_of_one_file_and_rack_each_keep_their_own_record() {
        let path = Path::new("C:/music/arming-runs.flac");
        let p = path.to_string_lossy();
        let first = prep_begin(path, "k", true);
        first.step("decode");
        let second = prep_begin(path, "k", true);
        second.step("isp");
        // The first is dropped (a cancel): the second still runs, and counts.
        drop(first);
        let v = prep_view(&p, "k").unwrap();
        assert_eq!((v.step.as_str(), v.finished.is_none()), ("isp", true));
        // Its lines move only its own record.
        let mut hear = second.listener();
        hear("Subsonic filter: 15 Hz");
        assert_eq!(prep_view(&p, "k").unwrap().step, "sub");
        second.finish(44_100, 44_100 * 3, false);
        let v = prep_view(&p, "k").unwrap();
        assert!(v.ok && v.finished.is_some(), "the newest that ended: the one that ended well");
    }

    #[test]
    fn the_running_one_that_began_first_is_the_preparation_that_counts() {
        let t0 = Instant::now();
        let run = |id, s: f64, step: &str, finished: Option<f64>, ok| PrepRun {
            id,
            path: "f".into(),
            key: "k".into(),
            started: t0 + secs(s),
            view: PrepView { step: step.into(), step_started: t0, finished: finished.map(|f| t0 + secs(f)), ok, env_on_disk: false },
        };
        let runs = [run(1, 0.0, "aa", None, false), run(2, 1.0, "decode", None, false), run(3, 0.5, "isp", Some(2.0), false)];
        assert_eq!(prep_of(&runs, "f", "k").unwrap().step, "aa");
        let ended = [run(1, 0.0, "aa", Some(3.0), true), run(2, 1.0, "isp", Some(2.0), false)];
        assert!(prep_of(&ended, "f", "k").unwrap().ok, "the newest that ended");
        assert!(prep_of(&ended, "f", "other").is_none());
    }

    #[test]
    fn bit_perfect_a_converted_file_and_a_stop_arm_nothing() {
        let m = Model::default();
        let mut t = Tracker::default();
        let now = Instant::now();
        let mut o = start();
        o.settings.mode = Mode::Direct;
        assert!(t.observe(&o, now, &m).is_none());
        let mut o = start();
        o.air.as_mut().unwrap().source = "file";
        assert!(t.observe(&o, now, &m).is_none());
        let mut o = start();
        o.active = false;
        assert!(t.observe(&o, now, &m).is_none());
    }

    #[test]
    fn a_rack_already_on_the_air_is_done_at_once() {
        let m = Model::default();
        let mut t = Tracker::default();
        let mut o = start();
        on_air(&mut o);
        let v = t.observe(&o, Instant::now(), &m).unwrap();
        assert_eq!((v.busy, v.frac, v.step.as_str()), (false, 1.0, "done"));
    }

    #[test]
    fn a_step_the_plan_did_not_expect_goes_in_without_moving_the_bar_back() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.prep = prep_in(t0, "isp", 0.0);
        let a = t.observe(&o, t0 + secs(2.0), &m).unwrap();
        o.prep = prep_in(t0, "Lab: codec forensics", 2.0);
        let b = t.observe(&o, t0 + secs(2.0), &m).unwrap();
        assert_eq!(b.step, "prep:Lab: codec forensics");
        assert!(b.frac >= a.frac);
        let c = t.observe(&o, t0 + secs(3.0), &m).unwrap();
        assert!(c.frac >= b.frac);
    }

    #[test]
    fn a_step_that_goes_in_before_the_current_one_does_not_move_it() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.full_ready = true;
        let a = t.observe(&o, t0, &m).unwrap();
        assert_eq!(a.step, "build");
        // Gone from the cache and prepared again: the bar does not go back.
        o.full_ready = false;
        o.prep = prep_in(t0, "isp", 0.5);
        let b = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert_eq!(b.step, "build");
        assert!(b.frac >= a.frac);
    }

    #[test]
    fn a_device_switch_is_a_reopen() {
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.full_ready = true;
        o.rate_switch = true;
        assert_eq!(t.observe(&o, t0, &m).unwrap().step, "reopen");
    }

    #[test]
    fn a_downgrade_or_the_gpu_switch_is_still_the_racks_chain() {
        let r = PlayerSettings { taps: 30_000_000, use_gpu: true, phase: Phase::Hybrid, ..rack() };
        let built = PlayerSettings { taps: 10_000_000, use_gpu: false, phase: Phase::Linear, ..r.clone() };
        assert!(same_chain(&built, &r, true, true));
        assert!(!same_chain(&built, &r, false, true), "fewer taps without a downgrade is another chain");
        let xtc = PlayerSettings { xtc: true, ..r.clone() };
        assert!(!same_chain(&xtc, &r, false, false));
    }

    #[test]
    fn a_new_output_rate_is_a_reopen_not_a_landing() {
        let m = Model::default();
        let mut o = start();
        o.settings.fs_multiplier = 2;
        o.key = o.settings.source_key();
        let a = Arming::new(1, &o, Instant::now(), &m);
        assert!(a.steps.iter().any(|s| s.kind == Kind::Reopen));
        assert!(!a.steps.iter().any(|s| s.kind == Kind::Land));
        assert_eq!(out_rate(44_100, 8), Some(352_800));
        assert_eq!(out_rate(96_000, 2), Some(192_000), "a source already at the rate steps up");
        assert_eq!(out_rate(12_345, 8), None);
    }

    #[test]
    fn an_unknown_length_does_not_weigh_the_work_as_nothing() {
        let m = Model::default();
        let mut o = start();
        let known = Arming::new(1, &o, Instant::now(), &m);
        o.duration_s = 0.0;
        let unknown = Arming::new(1, &o, Instant::now(), &m);
        let sum = |a: &Arming| a.steps.iter().filter(|s| s.kind == Kind::Prep).map(|s| s.est).sum::<f64>();
        assert!((sum(&unknown) - sum(&known)).abs() < 1e-9, "counted as {UNKNOWN_LENGTH_S} s");
    }

    #[test]
    fn the_plan_runs_the_stages_the_rack_has_on() {
        let s = PlayerSettings { declip: false, isp: false, adaptive_headroom: false, subsonic_hz: 0, adaptive_apodizer: false, apodizing: 0, ..rack() };
        assert_eq!(prep_steps(&s, 44_100), ["decode", "measure"]);
        assert_eq!(prep_steps(&rack(), 44_100), ["decode", "stats", "declip", "isp", "sub", "aa", "apod-aa", "measure"]);
        assert_eq!(prep_steps(&rack(), 96_000), ["decode", "stats", "declip", "isp", "sub", "aa", "measure"]);
        let st = PlayerSettings { adaptive_apodizer: false, apodizing: 2, ..rack() };
        assert!(prep_steps(&st, 48_000).contains(&"apod"));
        assert!(same_step("apod-aa", "apod"));
    }

    #[test]
    fn the_status_lines_name_the_steps() {
        assert_eq!(step_name("Decoding: 2Pac - California Love.mp3").as_deref(), Some("decode"));
        assert_eq!(step_name("Lab: intersample peak scan...").as_deref(), Some("isp"));
        assert_eq!(step_name("Subsonic filter: 15 Hz").as_deref(), Some("sub"));
        assert_eq!(step_name("Adaptive Apodizer: applying...").as_deref(), Some("apod-aa"));
        assert_eq!(step_name("Applying apodizing pre-filter (FFT+Rayon)...").as_deref(), Some("apod"));
        assert_eq!(step_name("Lab: codec forensics...").as_deref(), Some("Lab: codec forensics"));
        assert_eq!(step_name("SYSTEM RAM LOW. prep waiting for 3.2 GB free...").as_deref(), Some("SYSTEM RAM LOW. prep waiting for"));
        assert_eq!(step_name("  ").as_deref(), None);
    }

    #[test]
    fn estimates_learn_a_moving_average_and_keep_the_newest() {
        let mut m = Model::default();
        m.learn("prep:isp:f1", 1.0);
        assert_eq!(m.get("prep:isp:f1"), Some(1.0));
        m.learn("prep:isp:f1", 2.0);
        assert!((m.get("prep:isp:f1").unwrap() - 1.3).abs() < 1e-12);
        m.learn("prep:isp:f1", f64::NAN);
        m.learn("prep:isp:f1", -1.0);
        assert!((m.get("prep:isp:f1").unwrap() - 1.3).abs() < 1e-12, "nonsense is not learned");
        for i in 0..MAX_KEYS + 10 {
            m.learn(&format!("k{i}"), i as f64);
        }
        assert_eq!(m.map.len(), MAX_KEYS);
        assert!(m.get("prep:isp:f1").is_none() && m.get("k0").is_none(), "the oldest go first");
        assert_eq!(m.get(&format!("k{}", MAX_KEYS + 9)), Some((MAX_KEYS + 9) as f64));
    }

    #[test]
    fn estimates_survive_the_file_and_a_bad_file_gives_none() {
        let mut m = Model::default();
        m.learn("prep:decode:f1", 0.07);
        m.learn("build:30M:cpu:one", 2.5);
        let back = Model::from_json(&m.to_json());
        assert_eq!(back.get("prep:decode:f1"), Some(0.07));
        assert_eq!(back.get("build:30M:cpu:one"), Some(2.5));
        for bad in ["", "{", "[]", r#"{"version":2,"estimates":[["a",1]]}"#, r#"{"version":1,"estimates":[["a",-1],["b","x"],[3,1]]}"#] {
            assert!(Model::from_json(bad).map.is_empty(), "{bad:?}");
        }
        let mut mine = Model::default();
        mine.learn("prep:decode:f1", 0.2);
        mine.merge_under(back);
        assert_eq!(mine.get("prep:decode:f1"), Some(0.2), "this run's estimate stays over the file's");
        assert_eq!(mine.get("build:30M:cpu:one"), Some(2.5));
    }

    #[test]
    fn the_file_is_written_whole_over_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("player-arming.json");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_minute_is_counted_at_the_family_rate() {
        assert_eq!(family_multiple(44_100), 1);
        assert_eq!(family_multiple(96_000), 2);
        assert_eq!(family_multiple(352_800), 8);
        assert_eq!(family_multiple(22_050), 1);
        let mut m = Model::default();
        m.learn("prep:isp:f1", 0.2);
        assert!((per_minute(&m, "prep:isp", 2) - 0.4).abs() < 1e-12, "another family's, by its samples");
        assert!((per_minute(&Model::default(), "env", 1) - 2.0).abs() < 1e-12);
    }

    /// The prewarm's estimate of a whole chain: the rack's source stages, the
    /// output probe, a Hybrid-Phase rack's envelope, all by the minute at the
    /// family rate, and the build — learned figures where there are some.
    #[test]
    fn a_whole_chain_is_expected_to_take_its_steps_together() {
        let lin = PlayerSettings { phase: Phase::Linear, ..PlayerSettings::default() };
        let hp = PlayerSettings { phase: Phase::Hybrid, ..lin.clone() };
        let m = Model::default();
        let per_min: f64 = prep_steps(&lin, 44_100).iter().map(|st| default_per_minute(&format!("prep:{st}"))).sum::<f64>()
            + default_per_minute("probe");
        let want = per_min * 4.0 + DEFAULT_BUILD_S;
        assert!((expected_s(&m, &lin, 44_100, 240.0) - want).abs() < 1e-9, "four minutes, linear phase");
        let want_hp = (per_min + default_per_minute("env")) * 4.0 + DEFAULT_PAIR_BUILD_S;
        assert!((expected_s(&m, &hp, 44_100, 240.0) - want_hp).abs() < 1e-9, "the pair: its envelope and two banks");
        let mut learned = Model::default();
        learned.learn("env:f1", 0.5);
        learned.learn(&build_key(hp.taps, hp.use_gpu, true), 4.0);
        let d = expected_s(&learned, &hp, 96_000, 240.0) - expected_s(&m, &hp, 96_000, 240.0);
        let want_d = (0.5 * 2.0 - default_per_minute("env") * 2.0) * 4.0 + (4.0 - DEFAULT_PAIR_BUILD_S);
        assert!((d - want_d).abs() < 1e-9, "learned: the envelope at 96 kHz by its samples, the pair's build");
        assert!(expected_s(&m, &lin, 44_100, 0.0) > expected_s(&m, &lin, 44_100, 60.0), "an unknown length is not nothing");
    }

    #[test]
    fn a_preparation_reports_its_steps_and_ends_well_or_not() {
        let path = Path::new("C:/music/arming-test.flac");
        let at = || prep_view(&path.to_string_lossy(), "k-ok").unwrap();
        let p = prep_begin(path, "k-ok", false);
        let mut hear = p.listener();
        hear("Decoding: arming-test.flac");
        assert_eq!(at().step, "decode");
        hear("Lab: intersample peak scan...");
        let isp = at();
        hear("Lab: intersample peak scan...");
        assert_eq!((at().step, at().step_started), (isp.step.clone(), isp.step_started), "the same line again is the same step");
        assert_eq!(isp.step, "isp");
        p.retry();
        p.step("measure");
        assert_eq!((at().step.as_str(), at().finished.is_none()), ("measure", true));
        p.finish(44_100, 44_100 * 3, true);
        let v = at();
        assert!(v.ok && v.finished.is_some() && v.env_on_disk);
        drop(prep_begin(path, "k-cancelled", true));
        let v = prep_view(&path.to_string_lossy(), "k-cancelled").unwrap();
        assert!(!v.ok && v.finished.is_some(), "dropped without finish: it failed");
    }

    #[test]
    fn a_listener_hears_its_own_threads_status_lines_only() {
        use crate::audio::converter::decode::{set_status, with_step_listener};
        let heard = Arc::new(Mutex::new(Vec::<String>::new()));
        let h = heard.clone();
        with_step_listener(move |s| h.lock().unwrap().push(s.to_string()), || {
            set_status("Lab: analyzing source...");
            std::thread::spawn(|| set_status("Decoding: another thread's file")).join().unwrap();
        });
        set_status("Decoding: after the listener");
        assert_eq!(*heard.lock().unwrap(), ["Lab: analyzing source..."]);
    }

    #[test]
    fn a_track_being_prepared_before_its_first_sound_arms_too() {
        // No stream yet, nothing on the air: a start that waits for every
        // stage (or a quick start's first moment).
        let m = Model::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut o = start();
        o.air = None;
        o.newest = None;
        o.stream_rate = 0;
        o.prep = prep_in(t0, "isp", 0.5);
        let v = t.observe(&o, t0 + secs(1.0), &m).unwrap();
        assert!(v.busy && v.frac > 0.0 && v.step == "prep:isp", "{v:?}");
    }
}
