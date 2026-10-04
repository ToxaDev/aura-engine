//! The source stages on a stream: what the engine does to a track before the
//! filter (`prepare_audio_phase`), done as the audio arrives instead of on the
//! whole track — for a live stream, and for a file that starts playing before
//! it is prepared.
//!
//! In the engine's order: DC (the 2 Hz high-pass), the intersample repair
//! (`isp::IspStream`), the subsonic filter (`StreamingLinearFir`, the file
//! path's partitions), the static apodizer (the file path's blocks, minimum
//! phase). Each
//! stage's arithmetic is the file path's, so a stream that starts at a track's
//! first sample comes out as `prepare_audio_phase` leaves the track, bit for
//! bit (the test below). What needs the whole track does not run: declip; the
//! adaptive apodizer, whose static preset stands in as on a track it declines;
//! and the static DC — a stream has no mean to subtract, the 2 Hz high-pass
//! stands in for it. The repair leaves whole the clusters that reach the
//! source's samples over full scale (`isp::hot_spans`): a live stream's are
//! found on each piece before the high-pass, a file's on the whole track
//! before its DC removal.
//!
//! The stages hold some of the stream back: the repair a few samples (its
//! cluster in flight), the subsonic filter a 93 ms partition and half its
//! length (0.43–0.77 s), the apodizer a block (≤ 93 ms). Samples go in and
//! come out as f64, and are kept so: a live source and a growing file hold
//! them as the stages computed them.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

use crate::audio::converter::apodize::{
    design_subsonic_highpass, generate_apodizing_coeffs, subsonic_block_len, StreamingLinearFir, SUBSONIC_CORNERS_HZ,
};
use crate::audio::converter::dsp::lab::isp::{hot_spans, IspReport, IspStream};
use crate::audio::converter::dsp::lab::{CHAIN_DECLINED, CHAIN_RAN};

use super::settings::PlayerSettings;

/// What a stream's source stages run, from a rack's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct SourcePlan {
    pub rate: u32,
    pub isp: bool,
    /// Subsonic corner (10, 15, 20 Hz), 0 for none.
    pub subsonic_hz: u32,
    /// Static apodizer strength (1–3), 0 for none (always 0 above 48 kHz).
    pub apodizing: u32,
    /// Asked for by the rack, not run on a stream.
    pub declip_asked: bool,
    pub aa_asked: bool,
    pub static_dc_asked: bool,
}

impl SourcePlan {
    pub fn new(s: &PlayerSettings, rate: u32) -> SourcePlan {
        SourcePlan {
            rate,
            isp: s.isp,
            subsonic_hz: if SUBSONIC_CORNERS_HZ.contains(&s.subsonic_hz) { s.subsonic_hz } else { 0 },
            apodizing: if rate <= 48_000 && (1..=3).contains(&s.apodizing) { s.apodizing } else { 0 },
            declip_asked: s.declip,
            aa_asked: s.adaptive_apodizer,
            static_dc_asked: !s.iir_dc_blocking,
        }
    }

    /// What the source of a stream depends on: a rack with another key needs
    /// the stream's source made again.
    pub fn key(&self) -> String {
        format!("{}|{}|{}|{}", self.rate, self.isp as u8, self.subsonic_hz, self.apodizing)
    }

    /// The most the stages hold a stream back, frames: the subsonic filter's
    /// partition and half its length, the apodizer's block, the repair's
    /// reach. They hand audio on in block-sized steps (93 ms for the
    /// subsonic filter's partitions), so a live stream's start waits this
    /// much more than its margin, or the reader runs into the steps and
    /// starves.
    pub fn latency_frames(&self) -> usize {
        let mut n = if self.isp { 64 } else { 0 };
        if self.subsonic_hz != 0 {
            let h = design_subsonic_highpass(self.rate, self.subsonic_hz).len();
            n += subsonic_block_len(self.rate) + h / 2;
        }
        // generate_apodizing_coeffs: 2048 taps for strength 1, 4096 above.
        match self.apodizing {
            0 => {}
            1 => n += 2_048,
            _ => n += 4_096,
        }
        n
    }

    /// The stages' tokens on the chain, in the engine's order.
    pub fn tokens(&self) -> Vec<String> {
        let mut t = Vec::new();
        if self.isp {
            t.push("ISP".to_string());
        }
        if self.subsonic_hz != 0 {
            t.push(format!("SUB{}", self.subsonic_hz));
        }
        match self.apodizing {
            1 => t.push("Apod".into()),
            2 => t.push("Apod-M".into()),
            3 => t.push("Apod-S".into()),
            _ => {}
        }
        t
    }

    /// The source half of the chain as a converted row lists it: `(token,
    /// state, why)` for each stage the rack has on.
    pub fn stages(&self) -> Vec<(String, u8, String)> {
        let mut out = Vec::new();
        if self.declip_asked {
            out.push(("DC".into(), CHAIN_DECLINED, "declip needs the whole track: off on a stream".into()));
        }
        if self.isp {
            out.push(("ISP".into(), CHAIN_RAN, "intersample overs repaired as the stream arrives".into()));
        }
        if self.subsonic_hz != 0 {
            out.push((
                "SUB".into(),
                CHAIN_RAN,
                format!("linear-phase high-pass below {} Hz on the stream", self.subsonic_hz),
            ));
        }
        if self.aa_asked {
            let why = if self.apodizing > 0 {
                "the adaptive apodizer needs the whole track: the static preset runs instead"
            } else {
                "the adaptive apodizer needs the whole track: off on a stream"
            };
            out.push(("AA".into(), CHAIN_DECLINED, why.into()));
        }
        out
    }
}

/// What a stream's source stages have done, over all its connections.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTally {
    pub frames_in: u64,
    pub frames_out: u64,
    pub isp_clusters: usize,
    pub isp_fixed: usize,
    pub isp_unfixed: usize,
    /// Clusters left whole at the source's samples over full scale.
    pub isp_hot: usize,
    /// True peak of the stream before the repair, dBTP (None before any).
    pub isp_max_dbtp: Option<f64>,
}

/// The rayon pool a file's source stages run on while a listener waits for
/// them (Instant start's first sound, a track waiting for its whole chain):
/// their own, so that their blocks never queue behind a preparation's in the
/// global pool — the stream of a track's first variant stood there behind
/// its full variant's. Half the logical cores, two to eight threads, above
/// normal priority (below the render's). None if it cannot be made.
pub fn stream_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, |x| x.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads((n / 2).clamp(2, 8))
            .thread_name(|i| format!("aura-source-stages-{i}"))
            .start_handler(|_| set_thread_priority(1))
            .build()
            .ok()
    })
    .as_ref()
}

/// The calling thread's scheduling priority (Windows' scale: −1 below
/// normal, 0 normal, 1 above normal); 0 elsewhere.
pub fn thread_priority() -> i32 {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, GetThreadPriority};
        GetThreadPriority(GetCurrentThread())
    }
    #[cfg(not(windows))]
    0
}

/// Set the calling thread's priority (`thread_priority`'s scale).
pub fn set_thread_priority(p: i32) {
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
        SetThreadPriority(GetCurrentThread(), p);
    }
    #[cfg(not(windows))]
    let _ = p;
}

/// Blocks of `block` frames a file's subsonic filter or apodizer transforms
/// at once: two for each of `threads`, under 256 MB in flight (56 bytes an
/// FFT bin a block, as the converter counts them). One for a single thread.
fn file_batch(block: usize, threads: usize) -> usize {
    if threads <= 1 {
        return 1;
    }
    let n_fft = 2 * block;
    (2 * threads).min((256usize << 20) / (56 * n_fft)).max(1)
}

/// One stage: takes samples, hands on what is ready.
trait Step: Send {
    fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64]));
    fn finish(&mut self, out: &mut dyn FnMut(&[f64], &[f64]));
    /// The repair's report, once finished (only the repair keeps one).
    fn take_report(&mut self) -> Option<IspReport> {
        None
    }
    /// A filter's samples and the energy it took out of each channel, once
    /// finished (`StreamingLinearFir::finish`).
    fn take_removed(&mut self) -> Option<(usize, f64, f64)> {
        None
    }
    /// The source's samples over full scale in samples still to come (only
    /// the repair takes them: `IspStream::add_hot`).
    fn add_hot(&mut self, _l: &[(usize, usize)], _r: &[(usize, usize)]) {}
}

/// `prepare_audio_phase`'s 2 Hz DC high-pass, its state carried over.
struct Dc {
    r: f64,
    x_prev: Option<(f64, f64)>,
    y: (f64, f64),
    bl: Vec<f64>,
    br: Vec<f64>,
}

impl Step for Dc {
    fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
        let (mut x_prev_l, mut x_prev_r) = self.x_prev.unwrap_or((l[0], r[0]));
        let (mut y_l, mut y_r) = self.y;
        self.bl.clear();
        self.br.clear();
        for (&x_l, &x_r) in l.iter().zip(r) {
            y_l = x_l - x_prev_l + self.r * y_l;
            y_r = x_r - x_prev_r + self.r * y_r;
            x_prev_l = x_l;
            x_prev_r = x_r;
            self.bl.push(y_l);
            self.br.push(y_r);
        }
        self.x_prev = Some((x_prev_l, x_prev_r));
        self.y = (y_l, y_r);
        out(&self.bl, &self.br);
    }

    fn finish(&mut self, _out: &mut dyn FnMut(&[f64], &[f64])) {}
}

struct Isp {
    s: Option<IspStream>,
    tally: Arc<Mutex<SourceTally>>,
    /// The tally's ISP counts before this stream: clusters, fixed, unfixed, hot.
    before: (usize, usize, usize, usize),
    report: Option<IspReport>,
}

impl Isp {
    fn count(&self) {
        let Some(s) = &self.s else { return };
        let st = s.stats();
        let mut t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
        t.isp_clusters = self.before.0 + st.clusters;
        t.isp_fixed = self.before.1 + st.fixed;
        t.isp_unfixed = self.before.2 + st.unfixed;
        t.isp_hot = self.before.3 + st.hot;
        if st.max_dbtp > -200.0 {
            t.isp_max_dbtp = Some(t.isp_max_dbtp.map_or(st.max_dbtp, |m| m.max(st.max_dbtp)));
        }
    }
}

impl Step for Isp {
    fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
        if let Some(s) = self.s.as_mut() {
            s.push(l, r, out);
        }
        self.count();
    }

    fn finish(&mut self, out: &mut dyn FnMut(&[f64], &[f64])) {
        self.count();
        if let Some(s) = self.s.take() {
            self.report = s.finish(out);
        }
        // The clusters the end closed are in the report, not in the last count.
        if let Some(r) = &self.report {
            let mut t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            t.isp_clusters = self.before.0 + r.clusters;
            t.isp_fixed = self.before.1 + r.fixed;
            t.isp_unfixed = self.before.2 + r.unfixed;
            t.isp_hot = self.before.3 + r.hot;
        }
    }

    fn take_report(&mut self) -> Option<IspReport> {
        self.report.take()
    }

    fn add_hot(&mut self, l: &[(usize, usize)], r: &[(usize, usize)]) {
        if let Some(s) = self.s.as_mut() {
            s.add_hot(l, r);
        }
    }
}

/// The subsonic filter (linear phase) or the static apodizer (minimum phase):
/// the file path's FFT blocks, one at a time.
struct Fir {
    f: Option<StreamingLinearFir>,
    removed: Option<(usize, f64, f64)>,
}

impl Fir {
    /// Batches growing from one block to `batch` (`ramp_batch_to`): the
    /// first samples come out as soon as on a live stream. A file's batches
    /// keep their buffers (`keep_buffers`): it streams for a few seconds; and
    /// only the subsonic filter's account (`account`) is summed there.
    fn new(mut f: StreamingLinearFir, batch: usize, account: bool) -> Fir {
        f.ramp_batch_to(batch);
        if batch > 1 {
            f.keep_buffers();
            if !account {
                f.skip_removed();
            }
        }
        Fir { f: Some(f), removed: None }
    }
}

impl Step for Fir {
    fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
        let cancel = AtomicBool::new(false);
        if let Some(f) = self.f.as_mut() {
            let _ = f.push(l, r, &cancel, &mut |a, b| {
                out(a, b);
                Ok(())
            });
        }
    }

    fn finish(&mut self, out: &mut dyn FnMut(&[f64], &[f64])) {
        let cancel = AtomicBool::new(false);
        if let Some(f) = self.f.take() {
            self.removed = f
                .finish(&cancel, &mut |a, b| {
                    out(a, b);
                    Ok(())
                })
                .ok();
        }
    }

    fn take_removed(&mut self) -> Option<(usize, f64, f64)> {
        self.removed.take()
    }
}

/// The samples as they reach the static apodizer, copied aside: a file's
/// full variant puts the Adaptive Apodizer there in its place.
struct Tee {
    to: Arc<super::grow::GrowSource>,
}

impl Step for Tee {
    fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
        self.to.push(l, r);
        out(l, r);
    }

    fn finish(&mut self, _out: &mut dyn FnMut(&[f64], &[f64])) {}
}

/// What a stream's stages report at its end: the repair's report, as
/// `isp::run` gives it, and the subsonic filter's samples and removed energy
/// per channel, as its whole-buffer run sums them.
pub struct StagesEnd {
    pub isp: Option<IspReport>,
    pub sub_removed: Option<(usize, f64, f64)>,
}

/// Push `l`/`r` through `steps` in order; what the last hands on goes to `out`.
fn run_steps(steps: &mut [Box<dyn Step>], l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
    match steps.split_first_mut() {
        None => out(l, r),
        Some((first, rest)) => first.push(l, r, &mut |a, b| run_steps(rest, a, b, out)),
    }
}

/// A stream's source stages, from its first sample on.
pub struct SourceStages {
    steps: Vec<Box<dyn Step>>,
    isp_at: Option<usize>,
    sub_at: Option<usize>,
    apod_at: Option<usize>,
    tally: Arc<Mutex<SourceTally>>,
    /// A live stream with the repair on: each piece's samples over full
    /// scale are found as it comes in, before the DC step moves them, and
    /// handed to the repair (`isp::hot_spans`). A file's came with it.
    hot_from_input: bool,
    /// Frames pushed so far: where the next piece starts in the stream.
    pos: usize,
}

impl SourceStages {
    /// The stages `plan` asks for, counting into `tally` (which may already
    /// hold an earlier connection's counts: they are added to).
    pub fn new(plan: &SourcePlan, tally: Arc<Mutex<SourceTally>>) -> SourceStages {
        SourceStages::build(plan, true, &[], &[], &[], &[], tally, 1)
    }

    /// A file's stages once what needs the whole track has run on all of it
    /// (`prepare::source_head`: the statistics, DC removal, declip): no DC
    /// step here, and the repair leaves declip's spans alone, and the
    /// clusters that reach the source's samples over full scale (`hot_l`,
    /// `hot_r`, found before DC removal), as the file path does. From the
    /// track's first sample, the output is `prepare_audio_phase`'s to the bit
    /// (the adaptive apodizer aside).
    ///
    /// A file comes many times faster than it plays: with `threads` over
    /// one, the repair takes its two channels side by side and the filters
    /// transform `file_batch` blocks at once, on the rayon pool the caller
    /// runs in (`stream_pool`) — the same arithmetic, so the same samples.
    pub fn after_repairs(
        plan: &SourcePlan,
        skip_l: &[(usize, usize)],
        skip_r: &[(usize, usize)],
        hot_l: &[(usize, usize)],
        hot_r: &[(usize, usize)],
        tally: Arc<Mutex<SourceTally>>,
        threads: usize,
    ) -> SourceStages {
        SourceStages::build(plan, false, skip_l, skip_r, hot_l, hot_r, tally, threads)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        plan: &SourcePlan,
        dc: bool,
        skip_l: &[(usize, usize)],
        skip_r: &[(usize, usize)],
        hot_l: &[(usize, usize)],
        hot_r: &[(usize, usize)],
        tally: Arc<Mutex<SourceTally>>,
        threads: usize,
    ) -> SourceStages {
        let mut steps: Vec<Box<dyn Step>> = Vec::new();
        if dc {
            // prepare_audio_phase's coefficient, the same expression.
            let fc = 2.0_f64;
            let r = (-2.0 * std::f64::consts::PI * fc / plan.rate as f64).exp();
            steps.push(Box::new(Dc { r, x_prev: None, y: (0.0, 0.0), bl: Vec::new(), br: Vec::new() }));
        }
        let mut isp_at = None;
        if plan.isp {
            let before = {
                let t = tally.lock().unwrap_or_else(|e| e.into_inner());
                (t.isp_clusters, t.isp_fixed, t.isp_unfixed, t.isp_hot)
            };
            isp_at = Some(steps.len());
            let s = IspStream::with_skips(skip_l, skip_r).with_hot(hot_l, hot_r);
            let s = if threads > 1 { s.in_parallel() } else { s };
            steps.push(Box::new(Isp { s: Some(s), tally: tally.clone(), before, report: None }));
        }
        let mut sub_at = None;
        if plan.subsonic_hz != 0 {
            let h = design_subsonic_highpass(plan.rate, plan.subsonic_hz);
            sub_at = Some(steps.len());
            let f = StreamingLinearFir::with_batch(&h, plan.rate, 1);
            steps.push(Box::new(Fir::new(f, file_batch(subsonic_block_len(plan.rate), threads), true)));
        }
        let mut apod_at = None;
        if plan.apodizing != 0 {
            let h = generate_apodizing_coeffs(plan.rate, plan.apodizing);
            if h.len() > 1 {
                let f = StreamingLinearFir::minimum_phase(&h, 1);
                apod_at = Some(steps.len());
                steps.push(Box::new(Fir::new(f, file_batch(h.len().next_power_of_two().max(512), threads), false)));
            }
        }
        SourceStages { steps, isp_at, sub_at, apod_at, tally, hot_from_input: dc && isp_at.is_some(), pos: 0 }
    }

    /// What reaches the static apodizer also goes to `to`, as it passes:
    /// where the full variant's Adaptive Apodizer takes over. Nothing when
    /// the stages have no apodizer (what comes out is that already).
    pub fn with_tee_before_apodizer(mut self, to: Arc<super::grow::GrowSource>) -> SourceStages {
        if let Some(at) = self.apod_at {
            self.steps.insert(at, Box::new(Tee { to }));
            self.apod_at = Some(at + 1);
        }
        self
    }

    /// Take the next decoded samples; `out` gets what the stages have ready,
    /// in order (possibly nothing yet, possibly more than came in).
    pub fn push(&mut self, l: &[f64], r: &[f64], out: &mut dyn FnMut(&[f64], &[f64])) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        if let (true, Some(at)) = (self.hot_from_input, self.isp_at) {
            // The piece as decoded, before the DC step: it hands the repair
            // these same samples, none held back, so the spans count alike.
            let (hl, hr) = (hot_spans(&l[..n], self.pos), hot_spans(&r[..n], self.pos));
            if !hl.is_empty() || !hr.is_empty() {
                self.steps[at].add_hot(&hl, &hr);
            }
        }
        self.pos += n;
        let tally = self.tally.clone();
        tally.lock().unwrap_or_else(|e| e.into_inner()).frames_in += n as u64;
        run_steps(&mut self.steps, &l[..n], &r[..n], &mut |a, b| {
            tally.lock().unwrap_or_else(|e| e.into_inner()).frames_out += a.len() as u64;
            out(a, b)
        });
    }

    /// The stream is over (or its connection broke): everything the stages
    /// still hold is handed on. Returns the repair's report on this stream,
    /// as `isp::run` would give it on the same samples.
    pub fn finish(self, out: &mut dyn FnMut(&[f64], &[f64])) -> Option<IspReport> {
        self.finish_with_reports(out).isp
    }

    /// `finish`, with the subsonic filter's account as well (`StagesEnd`).
    pub fn finish_with_reports(mut self, out: &mut dyn FnMut(&[f64], &[f64])) -> StagesEnd {
        let tally = self.tally.clone();
        let mut out = |a: &[f64], b: &[f64]| {
            tally.lock().unwrap_or_else(|e| e.into_inner()).frames_out += a.len() as u64;
            out(a, b)
        };
        for i in 0..self.steps.len() {
            let (head, tail) = self.steps.split_at_mut(i + 1);
            head[i].finish(&mut |a, b| run_steps(tail, a, b, &mut out));
        }
        StagesEnd {
            isp: self.isp_at.and_then(|at| self.steps[at].take_report()),
            sub_removed: self.sub_at.and_then(|at| self.steps[at].take_removed()),
        }
    }

    /// A whole decoded track through the stages, each on a thread of its own
    /// so that they work on consecutive pieces side by side: every piece goes
    /// through them in order, as `push` takes it, and `out` (on the calling
    /// thread) gets what `push` and `finish` would hand on, in the same
    /// order. Their work runs on `pool` when there is one (`stream_pool`) —
    /// borrowed a piece at a time, never while a stage waits for the next.
    /// `go_on` is asked before each piece: false stops the stream where it
    /// is, unfinished (None).
    pub fn run_file(
        self,
        l: &[f64],
        r: &[f64],
        pool: Option<&rayon::ThreadPool>,
        go_on: &(dyn Fn() -> bool + Sync),
        out: &mut dyn FnMut(&[f64], &[f64]),
    ) -> Option<StagesEnd> {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::sync_channel;
        type Piece = (Vec<f64>, Vec<f64>);
        const PIECE: usize = 1 << 16;
        // Pieces waiting between two stages: room for whole batches of the
        // subsonic filter (16 partitions of 4096 at 44.1 kHz, one piece) and
        // more, so the repair goes on while the filter works through one.
        const QUEUE: usize = 32;
        fn in_pool<R: Send>(pool: Option<&rayon::ThreadPool>, f: impl FnOnce() -> R + Send) -> R {
            match pool {
                Some(p) => p.install(f),
                None => f(),
            }
        }
        // A live stream's samples over full scale are found in `push`, which
        // this does not go through: it is for a file, whose came with it.
        assert!(!self.hot_from_input, "run_file streams a file after its whole-track stages");
        let SourceStages { steps, isp_at, sub_at, tally, .. } = self;
        let n = l.len().min(r.len());
        let stopped = AtomicBool::new(false);
        // The stages run at the caller's priority (a prewarm's below normal).
        let prio = thread_priority();
        let mut end = StagesEnd { isp: None, sub_removed: None };
        std::thread::scope(|sc| {
            let (feed, mut rx) = sync_channel::<Piece>(QUEUE);
            let mut stages = Vec::new();
            for (i, mut step) in steps.into_iter().enumerate() {
                let (tx, next) = sync_channel::<Piece>(QUEUE);
                let input = std::mem::replace(&mut rx, next);
                let stopped = &stopped;
                stages.push(sc.spawn(move || {
                    set_thread_priority(prio);
                    // A stage's output is sent after its work, outside the
                    // pool: a worker never waits there for the next stage.
                    for (a, b) in input.iter() {
                        let mut made: Vec<Piece> = Vec::new();
                        in_pool(pool, || step.push(&a, &b, &mut |x, y| made.push((x.to_vec(), y.to_vec()))));
                        for p in made {
                            let _ = tx.send(p);
                        }
                    }
                    if !stopped.load(Ordering::Acquire) {
                        let mut made: Vec<Piece> = Vec::new();
                        in_pool(pool, || step.finish(&mut |x, y| made.push((x.to_vec(), y.to_vec()))));
                        for p in made {
                            let _ = tx.send(p);
                        }
                    }
                    (i, step.take_report(), step.take_removed())
                }));
            }
            let tally_in = tally.clone();
            let stopped_feed = &stopped;
            sc.spawn(move || {
                set_thread_priority(prio);
                let mut at = 0;
                while at < n {
                    if !go_on() {
                        stopped_feed.store(true, Ordering::Release);
                        break;
                    }
                    let e = (at + PIECE).min(n);
                    tally_in.lock().unwrap_or_else(|e| e.into_inner()).frames_in += (e - at) as u64;
                    if feed.send((l[at..e].to_vec(), r[at..e].to_vec())).is_err() {
                        break;
                    }
                    at = e;
                }
            });
            for (a, b) in rx.iter() {
                tally.lock().unwrap_or_else(|e| e.into_inner()).frames_out += a.len() as u64;
                out(&a, &b);
            }
            for h in stages {
                if let Ok((i, isp, removed)) = h.join() {
                    if Some(i) == isp_at {
                        end.isp = isp;
                    }
                    if Some(i) == sub_at {
                        end.sub_removed = removed;
                    }
                }
            }
        });
        (!stopped.load(Ordering::Acquire)).then_some(end)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    pub(crate) fn write_wav16(path: &std::path::Path, rate: u32, l: &[f64], r: &[f64]) {
        let n = l.len();
        let mut b = Vec::with_capacity(44 + n * 4);
        let data = (n * 4) as u32;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 4).to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            for v in [l[i], r[i]] {
                b.extend_from_slice(&((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
            }
        }
        std::fs::write(path, b).expect("write the test file");
    }

    /// Everything the stages work on: a DC offset, rumble under the subsonic
    /// corner, content near Nyquist for the apodizer, and intersample overs
    /// for the repair — at the start, the end and across the chunk edges.
    pub(crate) fn programme(n: usize, rate: u32) -> (Vec<f64>, Vec<f64>) {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        let t = |i: usize| i as f64 / rate as f64;
        let mut l: Vec<f64> = (0..n)
            .map(|i| 0.25 * (2.0 * PI * 440.0 * t(i)).sin() + 0.12 * (2.0 * PI * 19_000.0 * t(i)).sin()
                + 0.05 * (2.0 * PI * 3.0 * t(i)).sin() + 0.02 + 0.01 * noise())
            .collect();
        let mut r: Vec<f64> = (0..n)
            .map(|i| 0.2 * (2.0 * PI * 330.0 * t(i)).sin() + 0.04 * (2.0 * PI * 5.0 * t(i)).sin() - 0.015 + 0.01 * noise())
            .collect();
        let signs = [-1.0_f64, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -1.0];
        for (x, spots) in [(&mut l, [3usize, 4_093, 65_530, 100_000, n - 6]), (&mut r, [0, 4_094, 65_533, 90_000, n - 9])] {
            for &at in &spots {
                for (k, s) in signs.iter().enumerate() {
                    if at + k < n {
                        x[at + k] = 0.68 * s;
                    }
                }
            }
        }
        (l, r)
    }

    fn stream(plan: &SourcePlan, l: &[f64], r: &[f64], chunks: &[usize]) -> (Vec<f64>, Vec<f64>, Option<IspReport>, SourceTally) {
        let tally = Arc::new(Mutex::new(SourceTally::default()));
        let mut st = SourceStages::new(plan, tally.clone());
        let (mut ol, mut or) = (Vec::new(), Vec::new());
        let (mut at, mut k) = (0usize, 0usize);
        while at < l.len() {
            let c = chunks[k % chunks.len()].min(l.len() - at);
            st.push(&l[at..at + c], &r[at..at + c], &mut |a, b| {
                ol.extend_from_slice(a);
                or.extend_from_slice(b);
            });
            at += c;
            k += 1;
        }
        let rep = st.finish(&mut |a, b| {
            ol.extend_from_slice(a);
            or.extend_from_slice(b);
        });
        let t = tally.lock().unwrap().clone();
        (ol, or, rep, t)
    }

    pub(crate) fn same_bits(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    /// A stream that starts at a track's first sample comes out of the stages
    /// exactly as the file path prepares the track (`prepare_audio_phase`, the
    /// player's own preparation): DC high-pass, intersample repair, subsonic
    /// filter and static apodizer, every sample to the bit, whatever sizes the
    /// stream arrives in; and the repair reports what `isp::run` reported.
    #[test]
    fn a_stream_from_the_first_sample_is_the_prepared_track_bit_for_bit() {
        let rate = 44_100u32;
        let n = 3 * rate as usize;
        let (l, r) = programme(n, rate);
        let path = std::env::temp_dir().join(format!("aura-source-stages-{}.wav", std::process::id()));
        write_wav16(&path, rate, &l, &r);
        let cancel = AtomicBool::new(false);
        let a = crate::audio::converter::decode::decode_file(&path).expect("decode the test file");
        for (isp, sub, apod) in [(true, 15u32, 2u32), (false, 20, 0), (true, 0, 1)] {
            let s = PlayerSettings {
                iir_dc_blocking: true,
                isp,
                subsonic_hz: sub,
                apodizing: apod,
                adaptive_apodizer: false,
                declip: false,
                adaptive_headroom: false,
                fs_multiplier: 2,
                ..PlayerSettings::default()
            };
            let mut eng = s.to_engine();
            let p = crate::audio::converter::pipeline::prepare::prepare_audio_phase(&path, &mut eng, &cancel, None)
                .expect("prepare the test file");
            let plan = SourcePlan::new(&s, rate);
            for chunks in [&[4_096usize][..], &[1, 777, 65_536, 3][..]] {
                let (sl, sr, rep, t) = stream(&plan, &a.samples_l, &a.samples_r, chunks);
                assert!(same_bits(&sl, &p.audio_l) && same_bits(&sr, &p.audio_r),
                    "isp {isp} sub {sub} apod {apod}, chunks {chunks:?}: the stream differs from the prepared track");
                assert_eq!((t.frames_in, t.frames_out), (n as u64, n as u64));
                if isp {
                    let rep = rep.expect("the programme has intersample overs");
                    assert!(rep.clusters >= 8 && rep.fixed >= 8, "clusters {} fixed {}", rep.clusters, rep.fixed);
                    assert_eq!((t.isp_clusters, t.isp_fixed, t.isp_unfixed), (rep.clusters, rep.fixed, rep.unfixed));
                    let tag = p.lab_tags.iter().find(|(k, _)| k == "AURA_ISP").map(|(_, v)| v.clone()).unwrap_or_default();
                    assert!(tag.contains(&format!("clusters={};fixed={};unfixed={}", rep.clusters, rep.fixed, rep.unfixed)),
                        "the file path's ISP tag says {tag}");
                } else {
                    assert!(rep.is_none());
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Instant start's way for a file: what needs the whole track (the
    /// statistics, DC removal — the track's mean or the 2 Hz high-pass —
    /// and declip) runs on all of it, the rest streams after it. Out comes
    /// the track as `prepare_audio_phase` leaves it, to the bit, whatever
    /// sizes it streams in — declip's repaired arcs, which go over full scale,
    /// left to the output stage by the repair as the file path leaves them.
    #[test]
    fn a_file_streamed_after_its_whole_track_repairs_is_the_prepared_track_bit_for_bit() {
        let rate = 44_100u32;
        let n = 3 * rate as usize;
        let (mut l, r) = programme(n, rate);
        // A clipped stretch on the left: a loud tone cut flat at its rail.
        for (i, x) in l.iter_mut().enumerate().take(130_000).skip(110_000) {
            *x = (1.4 * (2.0 * PI * 220.0 * i as f64 / rate as f64).sin()).clamp(-0.85, 0.85);
        }
        let path = std::env::temp_dir().join(format!("aura-source-stages-file-{}.wav", std::process::id()));
        write_wav16(&path, rate, &l, &r);
        let cancel = AtomicBool::new(false);
        let a = crate::audio::converter::decode::decode_file(&path).expect("decode the test file");
        for (iir, sub, apod) in [(false, 15u32, 2u32), (true, 20, 0)] {
            let s = PlayerSettings {
                iir_dc_blocking: iir,
                declip: true,
                isp: true,
                subsonic_hz: sub,
                apodizing: apod,
                adaptive_apodizer: false,
                adaptive_headroom: false,
                fs_multiplier: 2,
                ..PlayerSettings::default()
            };
            let mut eng = s.to_engine();
            let p = crate::audio::converter::pipeline::prepare::prepare_audio_phase(&path, &mut eng, &cancel, None)
                .expect("prepare the test file");
            let (mut hl, mut hr) = (a.samples_l.clone(), a.samples_r.clone());
            let head = crate::audio::converter::pipeline::prepare::source_head(&mut hl, &mut hr, rate, a.lossy, &eng, &cancel)
                .expect("the whole-track repairs");
            // Behind the 2 Hz high-pass the plateaus are not flat any more and
            // declip finds no clipping — on the file path just the same.
            assert_eq!(head.declip_spans_l.is_empty(), iir, "declip on the clipped stretch: {:?}",
                head.lab_outcomes.iter().map(|o| o.text.clone()).collect::<Vec<_>>());
            let plan = SourcePlan::new(&s, rate);
            // One thread, the stream's own pool (the channels side by side,
            // the filters' blocks in batches), and each stage on a thread of
            // its own (`run_file`, chunks unused).
            for (chunks, threads, piped) in [(&[4_096usize][..], 1usize, false), (&[1, 777, 65_536, 3][..], 1, false),
                (&[32_768][..], 4, false), (&[1, 777, 65_536, 3][..], 4, false), (&[0][..], 4, true), (&[0][..], 1, true)] {
                let tally = Arc::new(Mutex::new(SourceTally::default()));
                let mut st = SourceStages::after_repairs(&plan, &head.declip_spans_l, &head.declip_spans_r, &head.hot_spans_l, &head.hot_spans_r, tally.clone(), threads);
                let (mut sl, mut sr) = (Vec::new(), Vec::new());
                let pool = stream_pool().filter(|_| threads > 1);
                let end = if piped {
                    st.run_file(&hl, &hr, pool, &|| true, &mut |a, b| {
                        sl.extend_from_slice(a);
                        sr.extend_from_slice(b);
                    })
                    .expect("not stopped")
                } else {
                    let mut run = || {
                        let (mut at, mut k) = (0usize, 0usize);
                        while at < n {
                            let c = chunks[k % chunks.len()].min(n - at);
                            st.push(&hl[at..at + c], &hr[at..at + c], &mut |a, b| {
                                sl.extend_from_slice(a);
                                sr.extend_from_slice(b);
                            });
                            at += c;
                            k += 1;
                        }
                    };
                    match pool {
                        Some(pool) => pool.install(run),
                        None => run(),
                    }
                    st.finish_with_reports(&mut |a, b| {
                        sl.extend_from_slice(a);
                        sr.extend_from_slice(b);
                    })
                };
                let t = tally.lock().unwrap().clone();
                assert_eq!((t.frames_in, t.frames_out), (n as u64, n as u64), "piped {piped}");
                assert!(same_bits(&sl, &p.audio_l) && same_bits(&sr, &p.audio_r),
                    "iir {iir} sub {sub} apod {apod}, chunks {chunks:?}, threads {threads}, piped {piped}: the stream differs from the prepared track");
                let rep = end.isp.expect("the programme has intersample overs");
                let tag = p.lab_tags.iter().find(|(k, _)| k == "AURA_ISP").map(|(_, v)| v.clone()).unwrap_or_default();
                assert!(tag.contains(&format!("clusters={};fixed={};unfixed={}", rep.clusters, rep.fixed, rep.unfixed)),
                    "the file path's ISP tag says {tag}, the stream {} {} {}", rep.clusters, rep.fixed, rep.unfixed);
                // The subsonic filter's account: the file path's own line.
                let (frames, e_l, e_r) = end.sub_removed.expect("the subsonic filter ran");
                let rms = |e: f64| 10.0 * (e / frames as f64).log10();
                let line = p.lab_outcomes.iter().find(|o| o.feature == "SUB").map(|o| o.text.clone()).unwrap_or_default();
                assert_eq!(frames, n);
                assert!(line.ends_with(&format!("removed {:.1} / {:.1} dBFS RMS", rms(e_l), rms(e_r))),
                    "the file path says {line}");
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// 32-bit float WAV: samples over full scale stay as they are.
    pub(crate) fn write_wav_f32(path: &std::path::Path, rate: u32, l: &[f64], r: &[f64]) {
        let n = l.len();
        let mut b = Vec::with_capacity(44 + n * 8);
        let data = (n * 8) as u32;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            for v in [l[i], r[i]] {
                b.extend_from_slice(&(v as f32).to_le_bytes());
            }
        }
        std::fs::write(path, b).expect("write the test file");
    }

    /// A source over full scale in places — what a loud lossy decode brings:
    /// a stretch of the programme 4 dB up, its intersample overs with it, and
    /// a loud tone — as a stream from its first sample (its samples over full
    /// scale found on each piece before the 2 Hz high-pass) and as a file
    /// after its whole-track head (found there, before DC removal): both come
    /// out as `prepare_audio_phase` leaves the track, bit for bit, the
    /// clusters at those samples left whole and counted alike.
    #[test]
    fn a_source_over_full_scale_streams_as_the_file_path_prepares_it_bit_for_bit() {
        let rate = 44_100u32;
        let n = 3 * rate as usize;
        let (mut l, mut r) = programme(n, rate);
        for i in 60_000..70_000 {
            l[i] *= 1.6;
            r[i] *= 1.6;
        }
        for (i, x) in l.iter_mut().enumerate().take(112_000).skip(110_000) {
            *x = 1.25 * (2.0 * PI * 100.0 * i as f64 / rate as f64).sin();
        }
        let path = std::env::temp_dir().join(format!("aura-source-stages-hot-{}.wav", std::process::id()));
        write_wav_f32(&path, rate, &l, &r);
        let cancel = AtomicBool::new(false);
        let a = crate::audio::converter::decode::decode_file(&path).expect("decode the test file");
        assert!(a.samples_l.iter().any(|v| v.abs() > 1.0), "the float file keeps its samples over full scale");
        for (iir, sub, apod) in [(true, 15u32, 2u32), (false, 20, 0)] {
            let s = PlayerSettings {
                iir_dc_blocking: iir,
                declip: false,
                isp: true,
                subsonic_hz: sub,
                apodizing: apod,
                adaptive_apodizer: false,
                adaptive_headroom: false,
                fs_multiplier: 2,
                ..PlayerSettings::default()
            };
            let mut eng = s.to_engine();
            let p = crate::audio::converter::pipeline::prepare::prepare_audio_phase(&path, &mut eng, &cancel, None)
                .expect("prepare the test file");
            let tag = p.lab_tags.iter().find(|(k, _)| k == "AURA_ISP").map(|(_, v)| v.clone()).unwrap_or_default();
            assert!(tag.contains(";hot="), "the file path left clusters whole: {tag}");
            let plan = SourcePlan::new(&s, rate);
            let check = |sl: &[f64], sr: &[f64], rep: &IspReport, how: &str| {
                assert!(same_bits(sl, &p.audio_l) && same_bits(sr, &p.audio_r), "iir {iir}, {how}: the stream differs from the prepared track");
                assert!(rep.hot > 0, "iir {iir}, {how}");
                assert!(tag.contains(&format!("clusters={};fixed={};unfixed={}", rep.clusters, rep.fixed, rep.unfixed)) && tag.ends_with(&format!(";hot={}", rep.hot)),
                    "iir {iir}, {how}: the file path's tag says {tag}, the stream {} {} {} hot {}", rep.clusters, rep.fixed, rep.unfixed, rep.hot);
            };
            if iir {
                // A live stream: the 2 Hz high-pass is its DC removal.
                for chunks in [&[4_096usize][..], &[1, 777, 65_536, 3][..]] {
                    let (sl, sr, rep, t) = stream(&plan, &a.samples_l, &a.samples_r, chunks);
                    let rep = rep.expect("overs");
                    check(&sl, &sr, &rep, &format!("live, chunks {chunks:?}"));
                    assert_eq!((t.isp_clusters, t.isp_fixed, t.isp_unfixed, t.isp_hot), (rep.clusters, rep.fixed, rep.unfixed, rep.hot));
                }
            }
            let (mut hl, mut hr) = (a.samples_l.clone(), a.samples_r.clone());
            let head = crate::audio::converter::pipeline::prepare::source_head(&mut hl, &mut hr, rate, a.lossy, &eng, &cancel)
                .expect("the whole-track head");
            assert!(!head.hot_spans_l.is_empty() && !head.hot_spans_r.is_empty());
            for (threads, piped) in [(1usize, false), (4, false), (4, true)] {
                let tally = Arc::new(Mutex::new(SourceTally::default()));
                let st = SourceStages::after_repairs(&plan, &head.declip_spans_l, &head.declip_spans_r, &head.hot_spans_l, &head.hot_spans_r, tally, threads);
                let pool = stream_pool().filter(|_| threads > 1);
                let (mut sl, mut sr) = (Vec::new(), Vec::new());
                let end = if piped {
                    st.run_file(&hl, &hr, pool, &|| true, &mut |a, b| {
                        sl.extend_from_slice(a);
                        sr.extend_from_slice(b);
                    })
                    .expect("not stopped")
                } else {
                    let mut st = st;
                    let mut feed = || {
                        let mut at = 0;
                        while at < n {
                            let e = (at + 32_768).min(n);
                            st.push(&hl[at..e], &hr[at..e], &mut |a, b| {
                                sl.extend_from_slice(a);
                                sr.extend_from_slice(b);
                            });
                            at = e;
                        }
                    };
                    match pool {
                        Some(pool) => pool.install(feed),
                        None => feed(),
                    }
                    st.finish_with_reports(&mut |a, b| {
                        sl.extend_from_slice(a);
                        sr.extend_from_slice(b);
                    })
                };
                check(&sl, &sr, end.isp.as_ref().expect("overs"), &format!("file, threads {threads}, piped {piped}"));
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// How fast a file's stages stream: 170 s of source at 44.1 kHz (a 30M
    /// linear filter's look-ahead at FS×2), each stage and all three, on one
    /// thread and on the stream's pool, one after the other and side by side
    /// (`run_file`). A measurement, not a test:
    /// `cargo test --profile fast --bins bench_stream_stages -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_stream_stages() {
        let rate = 44_100u32;
        let n = 170 * rate as usize;
        let (l, r) = programme(n, rate);
        let pool = stream_pool().expect("the stream pool");
        for threads in [1usize, pool.current_num_threads()] {
            for (name, isp, sub, apod) in [("isp", true, 0u32, 0u32), ("sub15", false, 15, 0), ("apod2", false, 0, 2), ("all", true, 15, 2)] {
                let s = PlayerSettings { isp, subsonic_hz: sub, apodizing: apod, adaptive_apodizer: false, declip: false, ..PlayerSettings::default() };
                let plan = SourcePlan::new(&s, rate);
                let t0 = std::time::Instant::now();
                let tally = Arc::new(Mutex::new(SourceTally::default()));
                let mut st = SourceStages::after_repairs(&plan, &[], &[], &[], &[], tally, threads);
                let mut got = 0usize;
                pool.install(|| {
                    let mut at = 0;
                    while at < n {
                        let e = (at + 32_768).min(n);
                        st.push(&l[at..e], &r[at..e], &mut |a, _| got += a.len());
                        at = e;
                    }
                });
                st.finish(&mut |a, _| got += a.len());
                eprintln!("BENCH threads {threads} {name}: {:.3} s for 170 s ({got} frames)", t0.elapsed().as_secs_f64());
            }
        }
        for (name, isp, sub, apod) in [("isp+sub15", true, 15u32, 0u32), ("all", true, 15, 2)] {
            let s = PlayerSettings { isp, subsonic_hz: sub, apodizing: apod, adaptive_apodizer: false, declip: false, ..PlayerSettings::default() };
            let plan = SourcePlan::new(&s, rate);
            let t0 = std::time::Instant::now();
            let tally = Arc::new(Mutex::new(SourceTally::default()));
            let st = SourceStages::after_repairs(&plan, &[], &[], &[], &[], tally, pool.current_num_threads());
            let mut got = 0usize;
            let mut first = None;
            st.run_file(&l, &r, Some(pool), &|| true, &mut |a, _| {
                got += a.len();
                first.get_or_insert(t0.elapsed().as_secs_f64());
            });
            eprintln!("BENCH piped {name}: {:.3} s for 170 s ({got} frames), first out {:.3} s", t0.elapsed().as_secs_f64(), first.unwrap_or(0.0));
        }
    }

    /// The live stages on recorded streams, as a station's chain runs them
    /// from its first sample (the 2 Hz high-pass and the repair; no subsonic
    /// filter, no apodizer): the repair's report, the samples it rewrote (and
    /// how many of them were over full scale as decoded), the error against
    /// the same stages without the repair, and a hash. A measurement, not a
    /// test: AURA_ISP_RADIO_FILES="a.aac|b.mp3" `cargo test --profile fast
    /// --bins radio_dump_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn radio_dump_harness() {
        let files = std::env::var("AURA_ISP_RADIO_FILES").unwrap_or_default();
        let label = std::env::var("AURA_ISP_HOT_LABEL").unwrap_or_else(|_| "isp".into());
        fn rms(x: &[f64], y: &[f64]) -> f64 {
            (x.iter().chain(y).map(|v| v * v).sum::<f64>() / (2 * x.len()).max(1) as f64).sqrt()
        }
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = std::path::Path::new(f.trim());
            let a = match crate::audio::converter::decode::decode_file(path) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("HARNESS {}: cannot decode: {e}", path.display());
                    continue;
                }
            };
            let rate = a.sample_rate;
            let run = |isp: bool| {
                let s = PlayerSettings { isp, subsonic_hz: 0, apodizing: 0, adaptive_apodizer: false, declip: false, ..PlayerSettings::default() };
                stream(&SourcePlan::new(&s, rate), &a.samples_l, &a.samples_r, &[4_096])
            };
            let (al, ar, _, _) = run(false);
            let (l, r, rep, t) = run(true);
            let n = l.len();
            let hot: Vec<bool> = a.samples_l.iter().chain(&a.samples_r).map(|v| v.abs() > 1.0).collect();
            let raw_peak = a.samples_l.iter().chain(&a.samples_r).fold(0.0f64, |m, v| m.max(v.abs()));
            let changed: Vec<bool> = l.iter().chain(&r).zip(al.iter().chain(&ar)).map(|(x, y)| x.to_bits() != y.to_bits()).collect();
            let n_changed = changed.iter().filter(|&&c| c).count();
            let n_changed_hot = changed.iter().zip(&hot).filter(|(&c, &h)| c && h).count();
            let n_hot = hot.iter().filter(|&&h| h).count();
            let (el, er): (Vec<f64>, Vec<f64>) = (l.iter().zip(&al).map(|(x, y)| x - y).collect(), r.iter().zip(&ar).map(|(x, y)| x - y).collect());
            eprintln!(
                "HARNESS {} [{label}] {} Hz lossy={:?}: {:.1} s, raw peak {:+.2} dBFS, samples over full scale {} ({:.3} %)",
                path.file_name().unwrap_or_default().to_string_lossy(), rate, a.lossy, n as f64 / rate as f64,
                20.0 * raw_peak.max(1e-300).log10(), n_hot, 100.0 * n_hot as f64 / (2 * n).max(1) as f64
            );
            match &rep {
                Some(rep) => eprintln!(
                    "HARNESS   report: max {:+.2} dBTP, clusters {}, fixed {}, unfixed {}, hot {}, residual {:+.2} dBTP (tally {}/{}/{} hot {})",
                    rep.max_dbtp, rep.clusters, rep.fixed, rep.unfixed, rep.hot, rep.residual_dbtp, t.isp_clusters, t.isp_fixed, t.isp_unfixed, t.isp_hot
                ),
                None => eprintln!("HARNESS   report: none (nothing over)"),
            }
            eprintln!(
                "HARNESS   rewritten {} samples ({:.3} %), {} of them over full scale as decoded; error to the stages without the repair {:.1} dB; hash {:#018x}",
                n_changed, 100.0 * n_changed as f64 / (2 * n).max(1) as f64, n_changed_hot,
                20.0 * (rms(&el, &er).max(1e-300) / rms(&al, &ar)).log10(),
                crate::audio::converter::pipeline::prepare::tests::fnv_bits(&l, &r)
            );
        }
    }

    /// What a rack asks of a stream: its tokens and rows, the static preset
    /// standing in for the adaptive apodizer, nothing above 48 kHz.
    #[test]
    fn the_plan_follows_the_rack() {
        let s = PlayerSettings { isp: true, subsonic_hz: 15, apodizing: 2, adaptive_apodizer: true, declip: true, ..PlayerSettings::default() };
        let p = SourcePlan::new(&s, 44_100);
        assert_eq!(p.tokens(), ["ISP", "SUB15", "Apod-M"]);
        let rows: Vec<(String, u8)> = p.stages().into_iter().map(|(t, st, _)| (t, st)).collect();
        assert_eq!(rows, [("DC".to_string(), CHAIN_DECLINED), ("ISP".into(), CHAIN_RAN), ("SUB".into(), CHAIN_RAN), ("AA".into(), CHAIN_DECLINED)]);
        assert_eq!(SourcePlan::new(&s, 96_000).apodizing, 0, "the static apodizer is for 44.1/48 kHz sources");
        assert_ne!(p.key(), SourcePlan::new(&PlayerSettings { isp: false, ..s.clone() }, 44_100).key());
    }

    /// A stream's subsonic filter holds it back a partition and half its
    /// length at most (`latency_frames`, what a radio's start waits for on
    /// top of its margin): 0.43 / 0.54 / 0.77 s for SUB20 / SUB15 / SUB10 at
    /// 44.1 kHz, where one block as long as the filter held 1.08 / 1.94 /
    /// 2.16 s — fed in pieces of any size, from its first sample on.
    #[test]
    fn the_subsonic_filter_holds_a_stream_back_a_partition_and_half_its_length() {
        for (rate, sub, want_s) in [(44_100u32, 20u32, 0.431), (44_100, 15, 0.544), (44_100, 10, 0.769), (48_000, 15, 0.536), (96_000, 10, 0.761)] {
            let s = PlayerSettings { isp: false, subsonic_hz: sub, apodizing: 0, adaptive_apodizer: false, declip: false, ..PlayerSettings::default() };
            let plan = SourcePlan::new(&s, rate);
            let hold = plan.latency_frames();
            assert_eq!(hold, subsonic_block_len(rate) + design_subsonic_highpass(rate, sub).len() / 2);
            assert!((hold as f64 / rate as f64 - want_s).abs() < 0.001, "SUB{sub} at {rate}: {:.4} s", hold as f64 / rate as f64);
            let n = 3 * rate as usize;
            let (l, r) = programme(n, rate);
            let mut st = SourceStages::new(&plan, Arc::new(Mutex::new(SourceTally::default())));
            let (mut fed, mut out) = (0usize, 0usize);
            for &c in [1usize, 777, 4_096, 9_999, 3].iter().cycle() {
                if fed >= n {
                    break;
                }
                let c = c.min(n - fed);
                st.push(&l[fed..fed + c], &r[fed..fed + c], &mut |a, _| out += a.len());
                fed += c;
                assert!(out + hold >= fed, "SUB{sub} at {rate}: {fed} in, {out} out");
            }
        }
    }
}