//! `[firperf]` lines: what the long filter's convolution costs as it plays.
//!
//! A stream (`PolyStream`, `GpuPolyStream`) keeps a `StreamPerf`: one line
//! when its first block is done (the filter, the ratio, the prime, the first
//! block, how long since the click that started it), one every ~10 s of
//! wall time (blocks, ms per block, partitions summed of all, ms per second
//! of music, how often the render fell behind meanwhile) and one when it is
//! dropped. The lines read the same with `AURA_FIR_OPT=0` and `1` but for
//! the numbers, for an A/B of the speed from the log.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::convolver::{Bank, Input, NBIN};

/// Seconds of wall time between a stream's lines.
const EVERY_S: f64 = 10.0;

fn epoch() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// Nanoseconds since `epoch` + 1 of the last `mark` (0: none yet).
static MARK: AtomicU64 = AtomicU64::new(0);
/// Times the render fell behind (`starve`'s hold engaged).
static BEHIND: AtomicU64 = AtomicU64::new(0);

/// A click that starts music: play, seek, next, previous.
pub fn mark() {
    MARK.store(epoch().elapsed().as_nanos() as u64 + 1, Ordering::Relaxed);
}

/// Milliseconds since the last `mark`, if there was one.
pub fn since_mark_ms() -> Option<f64> {
    match MARK.load(Ordering::Relaxed) {
        0 => None,
        m => Some((epoch().elapsed().as_nanos() as u64).saturating_sub(m - 1) as f64 / 1e6),
    }
}

/// The render fell behind (the starved hold engaged).
pub fn fell_behind() {
    BEHIND.fetch_add(1, Ordering::Relaxed);
}

/// Partitions the CPU streams of the player summed, and of how many the old
/// work would have, since the process started (`count_parts`).
static PARTS_SUMMED: AtomicU64 = AtomicU64::new(0);
static PARTS_ALL: AtomicU64 = AtomicU64::new(0);
/// The share of the last live window (`set_window_frac`), f64 bits; 0 = none.
static WINDOW_FRAC: AtomicU64 = AtomicU64::new(0);

/// A CPU block of the player summed `parts.0` partitions of `parts.1`.
pub fn count_parts(parts: (usize, usize)) {
    if parts.1 > 0 {
        PARTS_SUMMED.fetch_add(parts.0 as u64, Ordering::Relaxed);
        PARTS_ALL.fetch_add(parts.1 as u64, Ordering::Relaxed);
    }
}

/// Blocks the render took from stage 0 ahead (`stage0_ahead`) instead of
/// summing them, since the process started.
static SERVED: AtomicU64 = AtomicU64::new(0);

pub fn count_served() {
    SERVED.fetch_add(1, Ordering::Relaxed);
}

/// Blocks served ahead so far: a live window with any is not the chain's
/// cost (the calibration skips it).
pub fn served_taken() -> u64 {
    SERVED.load(Ordering::Relaxed)
}

/// (summed, all) so far: two readings apart give a window's share.
pub fn parts_taken() -> (u64, u64) {
    (PARTS_SUMMED.load(Ordering::Relaxed), PARTS_ALL.load(Ordering::Relaxed))
}

/// The share of stage 0's partitions summed between two `parts_taken`
/// readings (1 when nothing was counted: the whole filter).
pub fn frac_between(a: (u64, u64), b: (u64, u64)) -> f64 {
    let all = b.1.saturating_sub(a.1);
    if all == 0 {
        return 1.0;
    }
    (b.0.saturating_sub(a.0) as f64 / all as f64).clamp(0.0, 1.0)
}

/// The render's last window summed `frac` of its partitions.
pub fn set_window_frac(frac: f64) {
    WINDOW_FRAC.store(frac.to_bits(), Ordering::Relaxed);
}

/// The share of the last live window (1 before the first one).
pub fn window_frac() -> f64 {
    match WINDOW_FRAC.load(Ordering::Relaxed) {
        0 => 1.0,
        b => f64::from_bits(b),
    }
}

fn behind_count() -> u64 {
    BEHIND.load(Ordering::Relaxed)
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// What the lines of the interval since the last one add up.
#[derive(Default, Clone, Copy)]
struct Span {
    steps: u64,
    frames: u64,
    total_ns: u64,
    max_ns: u64,
    cards: u64,
    card_ns: u64,
    card_max_ns: u64,
    parts: u64,
    parts_full: u64,
    submits: u64,
}

impl Span {
    fn add(&mut self, frames: usize, total_ns: u64, card_ns: Option<u64>, parts: (usize, usize), submits: usize) {
        self.steps += 1;
        self.frames += frames as u64;
        self.total_ns += total_ns;
        self.max_ns = self.max_ns.max(total_ns);
        if let Some(c) = card_ns {
            self.cards += 1;
            self.card_ns += c;
            self.card_max_ns = self.card_max_ns.max(c);
        }
        self.parts += parts.0 as u64;
        self.parts_full += parts.1 as u64;
        self.submits += submits as u64;
    }
}

/// One stream's `[firperf]` lines.
/// Streams of shorter filters (the detector's, the short rungs) log
/// nothing: the lines are for the long ones.
const MIN_PARTITIONS: usize = 16;

pub struct StreamPerf {
    kind: &'static str,
    filter: String,
    l: usize,
    delay: usize,
    p: usize,
    rate: u32,
    track_blocks: Option<i64>,
    j0: i64,
    /// `fir_opt` as the stream runs (the tests set it per stream).
    pub opt: bool,
    bank_bytes: u64,
    fdl_bytes: u64,
    vram_bytes: Option<u64>,
    prime: Option<Duration>,
    since_mark: Option<f64>,
    started: bool,
    t_last: Instant,
    behind_last: u64,
    span: Span,
    whole: Span,
}

impl StreamPerf {
    /// A stream of `kind` ("cpu", "gpu") over `bank`, starting at block `j0`.
    pub fn new(kind: &'static str, bank: &Bank, input: &Input, j0: i64) -> StreamPerf {
        let p = bank.max_partitions();
        let filter = std::path::Path::new(&bank.path)
            .file_name()
            .map_or_else(|| bank.path.clone(), |n| n.to_string_lossy().into_owned());
        StreamPerf {
            kind,
            filter,
            l: bank.l,
            delay: bank.delay,
            p,
            rate: input.rate(),
            track_blocks: input.track_blocks(),
            j0,
            opt: crate::audio::dsp_core::fir_opt(),
            bank_bytes: bank.bytes() as u64,
            // Every slot's two spectra; with `fir_opt` a silent one is empty.
            fdl_bytes: (p * 2 * NBIN * std::mem::size_of::<rustfft::num_complex::Complex<f64>>()) as u64,
            vram_bytes: None,
            prime: None,
            since_mark: since_mark_ms(),
            started: false,
            t_last: Instant::now(),
            behind_last: behind_count(),
            span: Span::default(),
            whole: Span::default(),
        }
    }

    /// The card's memory the stream holds (filter and FDL ring), for a GPU
    /// stream's start line.
    pub fn set_vram(&mut self, bytes: u64) {
        self.vram_bytes = Some(bytes);
    }

    /// The prime (the history before the first block) took `d`.
    pub fn primed(&mut self, d: Duration) {
        self.prime = Some(d);
    }

    /// A step done: `frames` source frames (a block, or a live stream's
    /// piece of one) in `total_ns`, `card_ns` of it on the card (GPU),
    /// `parts` partitions summed of all, in `submits` submits.
    pub fn block(&mut self, frames: usize, total_ns: u64, card_ns: Option<u64>, parts: (usize, usize), submits: usize) {
        if self.p < MIN_PARTITIONS {
            return;
        }
        self.span.add(frames, total_ns, card_ns, parts, submits);
        self.whole.add(frames, total_ns, card_ns, parts, submits);
        if !self.started {
            self.started = true;
            crate::aelog!("{}", self.start_line(total_ns));
            self.t_last = Instant::now();
            self.span = Span::default();
            return;
        }
        if self.t_last.elapsed().as_secs_f64() >= EVERY_S {
            crate::aelog!("{}", self.span_line("every", self.span, true));
            self.t_last = Instant::now();
            self.span = Span::default();
        }
    }

    fn start_line(&self, first_ns: u64) -> String {
        let opt_ms = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.0}"));
        format!(
            "[firperf] {} start filter={} L={} delay={} P={} track_blocks={} j0={} opt={} prime_ms={} first_ms={:.1} \
             since_click_ms={} bank_mb={:.0} vram_mb={} fdl_mb={:.0}",
            self.kind,
            self.filter,
            self.l,
            self.delay,
            self.p,
            self.track_blocks.map_or_else(|| "live".to_string(), |n| n.to_string()),
            self.j0,
            self.opt as u8,
            opt_ms(self.prime.map(|d| d.as_secs_f64() * 1e3)),
            first_ns as f64 / 1e6,
            opt_ms(self.since_mark),
            mb(self.bank_bytes),
            self.vram_bytes.map_or_else(|| "-".to_string(), |b| format!("{:.0}", mb(b))),
            mb(self.fdl_bytes),
        )
    }

    /// The line over `s`; `behind` takes the fell-behind count since the
    /// last line (and moves the mark on).
    fn span_line(&mut self, what: &str, s: Span, behind: bool) -> String {
        let steps = s.steps.max(1) as f64;
        let music_s = s.frames as f64 / self.rate.max(1) as f64;
        let card = if s.cards > 0 {
            format!("{:.1}/{:.1}", s.card_ns as f64 / s.cards as f64 / 1e6, s.card_max_ns as f64 / 1e6)
        } else {
            "-".to_string()
        };
        let now = behind_count();
        let fell = if behind { now - self.behind_last } else { 0 };
        self.behind_last = now;
        format!(
            "[firperf] {} {} filter={} delay={} L={} opt={} steps={} ms_avg_max={:.1}/{:.1} card_ms_avg_max={} parts={}/{} \
             ms_per_music_s={:.1} submits_per_step={:.1} fell_behind=+{}",
            self.kind,
            what,
            self.filter,
            self.delay,
            self.l,
            self.opt as u8,
            s.steps,
            s.total_ns as f64 / steps / 1e6,
            s.max_ns as f64 / 1e6,
            card,
            s.parts,
            s.parts_full,
            if music_s > 0.0 { s.total_ns as f64 / 1e6 / music_s } else { 0.0 },
            s.submits as f64 / steps,
            fell,
        )
    }
}

impl Drop for StreamPerf {
    fn drop(&mut self) {
        if self.started {
            let whole = self.whole;
            let line = self.span_line("end", whole, true);
            crate::aelog!("{}", line);
        }
    }
}

/// A converter pass's line: `secs` for `l` phases over `frames` source
/// frames each, `parts` summed of all over them
/// (`DspProcessor::partition_counts`).
pub fn conv_line(what: &str, l: usize, secs: f64, parts: (u64, u64), frames: usize) -> String {
    format!(
        "[firperf] conv {} L={} opt={} s={:.2} frames={} parts={}/{}",
        what,
        l,
        crate::audio::dsp_core::fir_opt() as u8,
        secs,
        frames,
        parts.0,
        parts.1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::convolver::BLOCK;

    /// The key=value skeleton of a line: the numbers out.
    fn shape(s: &str) -> String {
        s.chars().map(|c| if c.is_ascii_digit() || c == '.' { '#' } else { c }).collect::<String>().replace("##", "#")
    }

    #[test]
    fn lines_read_the_same_both_ways() {
        let span = Span { steps: 3, frames: 3 * BLOCK as u64, total_ns: 9_000_000, max_ns: 4_000_000, parts: 10, parts_full: 30, ..Span::default() };
        let mut lines = Vec::new();
        for opt in [false, true] {
            let mut p = StreamPerf {
                kind: "cpu",
                filter: "f.npy".into(),
                l: 16,
                delay: 0,
                p: 12,
                rate: 44_100,
                track_blocks: Some(4),
                j0: 0,
                opt,
                bank_bytes: 1 << 20,
                fdl_bytes: 1 << 20,
                vram_bytes: None,
                prime: Some(Duration::from_millis(3)),
                since_mark: None,
                started: false,
                t_last: Instant::now(),
                behind_last: behind_count(),
                span: Span::default(),
                whole: Span::default(),
            };
            let a = p.start_line(1_000_000);
            let b = p.span_line("end", span, false);
            lines.push((shape(&a), shape(&b)));
        }
        assert_eq!(lines[0], lines[1]);
    }
}
