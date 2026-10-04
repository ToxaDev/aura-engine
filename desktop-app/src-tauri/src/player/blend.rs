//! Hybrid-Phase and continuous alpha on a stream that can start anywhere.
//!
//! The converter's streaming blender (`segmented::StreamingBlender`) plans
//! from the first sample of the file: every boundary is snapped in order and
//! the switch state is carried from one to the next. A player that starts at
//! minute three cannot replay that history, so this blender begins a short
//! distance before the start — two snap windows plus two half-fades and a
//! margin — and settles the boundaries before that point by counting them
//! (each one flips the branch). Everything a listener hears from the start on
//! is then planned exactly as the converter plans it: the same boundary scan,
//! the same `snap_boundary` on the mid difference, the same `switch_fade`,
//! the same stereo link. The one thing it cannot reproduce is a fade clipped
//! by a previous fade that ended before the lead-in, which needs two switches
//! within a millisecond of each other right at the start point.
//!
//! Continuous alpha has no plan at all — each sample's weight is the smoothed
//! envelope at that sample — so it is exact from any start.

use std::collections::VecDeque;

use crate::audio::hybrid_phase::{
    catmull_env_at, smooth_analysis_envelope, snap_boundary, switch_fade, switch_params, SWITCH_THRESHOLD,
};

use crate::audio::hpss_native::OnsetStream;

use super::convolver::Input;
use super::stages::Stage;

/// The onset envelope of one source variant at one output rate.
pub struct Envelope {
    pub analysis: Vec<f64>,
    pub analysis_sr: f64,
}

/// Switch boundaries for the whole track, at output rate — the scan step of
/// the converter's plan (`segmented::scan_boundaries`, retriggerable hold).
pub struct Plan {
    pub boundaries: Vec<usize>,
    pub use_min_0: bool,
}

pub fn scan_plan(env: &Envelope, output_sr: f64, total: usize) -> Plan {
    let (_sw, _fade, cooldown) = switch_params(output_sr);
    let frames_to_output = output_sr / env.analysis_sr.max(1.0);
    let thr = SWITCH_THRESHOLD;
    let at = |i: usize| catmull_env_at(&env.analysis, frames_to_output, i);
    let mut boundaries = Vec::new();
    let mut min_active = total > 0 && at(0) >= thr;
    let use_min_0 = min_active;
    let mut min_end = 0usize;
    for i in 1..total {
        let m = at(i) >= thr;
        if m {
            if !min_active {
                min_active = true;
                boundaries.push(i);
            }
            min_end = i + cooldown;
        } else if min_active && i >= min_end {
            min_active = false;
            boundaries.push(i);
        }
    }
    Plan { boundaries, use_min_0 }
}

/// How far before `start` the two branches have to begin.
pub fn lead_in(output_sr: f64) -> u64 {
    let (sw, fade, _) = switch_params(output_sr);
    (2 * (sw + fade / 2) + 1024) as u64
}

/// Source frames a live Hybrid-Phase or alpha-HP chain at `output_sr` reads
/// before its first sound, its branches begun at index 0: the plan known to
/// the end of the first pull and a snap window and half a fade past it
/// (`HybridStage::pull`), the envelope frames that takes (`PlanFeed::advance`)
/// and the source the last of them needs (`OnsetStream::need_for_frame`) —
/// about 0.3 s, whatever the filter's own look-ahead. Alpha-HP reads its
/// envelope no further.
pub fn live_plan_need(src_rate: u32, output_sr: f64) -> usize {
    let (sw, fade, _) = switch_params(output_sr);
    let upto = PULL + sw + fade / 2 + 1;
    let onset = OnsetStream::new(src_rate, 0);
    let f2o = output_sr / onset.sidecar_sr().max(1.0);
    let frames = ((upto - 1) as f64 / f2o) as usize + 3;
    onset.need_for_frame(frames - 1)
}

/// Analysis frames a chain started mid-stream reads before its start: the
/// onset gate's 3 s of history and the harmonic median's reach, with room.
const FEED_WARM_S: f64 = 3.5;

/// The plan of a source still arriving: `scan_plan`, a sample at a time, on
/// the onset envelope `OnsetStream` makes as the source comes in — read from
/// the source by absolute index, as the convolvers read it, so a live
/// stream's starved silence and fades are in it where the convolvers hear
/// them. From the source's first sample it finds `scan_plan`'s boundaries
/// exactly (on a file, told its length, to the last one); started later, it
/// begins on the track's frame grid a warm-up early. It also keeps the
/// smoothed envelope alpha-HP blends by.
pub struct PlanFeed {
    onset: OnsetStream,
    input: Input,
    f2o: f64,
    cooldown: usize,
    /// Output samples (none past it), when the source is a file.
    total: Option<usize>,
    /// First output index scanned, and the next one to scan.
    start: usize,
    scanned: usize,
    min_active: bool,
    min_end: usize,
    use_min_0: bool,
    boundaries: Vec<usize>,
    smoothed: Vec<f64>,
    smooth_prev: f64,
    release: f64,
}

impl PlanFeed {
    /// A plan for `input` (at `src_rate`) at output rate `output_sr`, scanned
    /// from output index `start` on.
    pub fn new(input: Input, src_rate: u32, output_sr: f64, start: usize) -> PlanFeed {
        let (_, _, cooldown) = switch_params(output_sr);
        let (_, hop) = crate::audio::hpss_native::stft_params(src_rate);
        let probe = OnsetStream::new(src_rate, 0);
        let sr = probe.sidecar_sr();
        let f2o = output_sr / sr.max(1.0);
        let first = (start as f64 / f2o) as usize;
        let warm = (FEED_WARM_S * src_rate as f64 / hop as f64) as usize;
        let f0 = if start == 0 { 0 } else { first.saturating_sub(warm) };
        let src_len = match &input {
            Input::File(s) => Some(s.len()),
            Input::Live(_) => None,
            Input::Grow(g) => Some(g.total()),
        };
        let l = (output_sr / src_rate as f64).round().max(1.0) as usize;
        PlanFeed {
            onset: if f0 == 0 { probe } else { OnsetStream::new(src_rate, f0) },
            input,
            f2o,
            cooldown,
            total: src_len.map(|n| n * l),
            start,
            scanned: start,
            min_active: false,
            min_end: 0,
            use_min_0: false,
            boundaries: Vec::new(),
            smoothed: Vec::new(),
            smooth_prev: 0.0,
            release: (-1.0_f64 / (sr.max(1.0) * 0.030)).exp(),
        }
    }

    /// Envelope frames `[0, frames)` made (or the source's end reached).
    fn ensure_frames(&mut self, frames: usize) {
        if self.onset.is_finished() || self.onset.envelope().len() >= frames {
            return;
        }
        let need = self.onset.need_for_frame(frames.saturating_sub(1));
        match &self.input {
            Input::File(s) => {
                let len = s.len();
                let mut read = |at: usize, a: &mut [f64], b: &mut [f64]| {
                    a.copy_from_slice(&s.l[at..at + a.len()]);
                    b.copy_from_slice(&s.r[at..at + b.len()]);
                };
                if need >= len {
                    self.onset.finish(len, &mut read);
                } else {
                    self.onset.advance(need, &mut read);
                }
            }
            Input::Live(live) => {
                live.ensure(need as i64);
                let mut read = |at: usize, a: &mut [f64], b: &mut [f64]| {
                    live.read(0, at as i64, a);
                    live.read(1, at as i64, b);
                };
                self.onset.advance(need, &mut read);
            }
            Input::Grow(g) => {
                // A file still growing: its frames come as the convolvers
                // read them, and its end is the file's.
                let len = g.total();
                g.ensure(need.min(len) as i64);
                let mut read = |at: usize, a: &mut [f64], b: &mut [f64]| {
                    g.read(0, at as i64, a);
                    g.read(1, at as i64, b);
                };
                if need >= len {
                    self.onset.finish(len, &mut read);
                } else {
                    self.onset.advance(need, &mut read);
                }
            }
        }
    }

    /// Scan the plan up to output index `upto` (not past the end of a file),
    /// pulling the envelope it needs: a live source waits for its frames as
    /// a convolver does. Returns how far the plan is known.
    pub fn advance(&mut self, upto: usize) -> usize {
        let upto = self.total.map_or(upto, |t| upto.min(t));
        while self.scanned < upto {
            let i1 = (self.scanned as f64 / self.f2o) as usize;
            if !self.onset.is_finished() && self.onset.envelope().len() < i1 + 3 {
                self.ensure_frames(((upto - 1) as f64 / self.f2o) as usize + 3);
                if !self.onset.is_finished() && self.onset.envelope().len() < i1 + 3 {
                    break;
                }
            }
            let finished = self.onset.is_finished();
            let env = self.onset.envelope();
            let at = |i: usize| catmull_env_at(env, self.f2o, i) >= SWITCH_THRESHOLD;
            while self.scanned < upto {
                let i = self.scanned;
                if !finished && (i as f64 / self.f2o) as usize + 2 >= env.len() {
                    break;
                }
                if i == self.start {
                    self.min_active = self.total != Some(0) && at(i);
                    self.use_min_0 = self.min_active;
                } else if at(i) {
                    if !self.min_active {
                        self.min_active = true;
                        self.boundaries.push(i);
                    }
                    self.min_end = i + self.cooldown;
                } else if self.min_active && i >= self.min_end {
                    self.min_active = false;
                    self.boundaries.push(i);
                }
                self.scanned += 1;
            }
        }
        self.scanned
    }

    /// Output indices below this are planned.
    pub fn known(&self) -> usize {
        self.scanned
    }

    /// Whether minimum phase was on at the first output index scanned.
    pub fn use_min_0(&self) -> bool {
        self.use_min_0
    }

    pub fn boundaries(&self) -> &[usize] {
        &self.boundaries
    }

    pub fn frames_to_output(&self) -> f64 {
        self.f2o
    }

    /// The smoothed envelope (`smooth_analysis_envelope`) far enough for
    /// output samples below `upto`.
    pub fn smooth_upto(&mut self, upto: usize) -> &[f64] {
        let need = ((upto.max(1) - 1) as f64 / self.f2o) as usize + 3;
        self.ensure_frames(need);
        let env = self.onset.envelope();
        while self.smoothed.len() < env.len() {
            let raw = env[self.smoothed.len()];
            let v = if raw >= self.smooth_prev { raw } else { self.smooth_prev * self.release };
            let v = v.clamp(0.0, 1.0);
            self.smoothed.push(v);
            self.smooth_prev = v;
        }
        &self.smoothed
    }
}

pub struct HybridStage {
    lin: Box<dyn Stage>,
    min: Box<dyn Stage>,
    total: usize,
    sw: usize,
    hf: usize,
    pending: VecDeque<usize>,
    xfades: VecDeque<(usize, usize, bool)>,
    plan_pos: usize,
    plan_min: bool,
    cur_min: bool,
    emitted: usize,
    base: usize,
    lin_l: Vec<f64>,
    lin_r: Vec<f64>,
    min_l: Vec<f64>,
    min_r: Vec<f64>,
    out: VecDeque<(f64, f64)>,
    /// Output index of the next sample `read` returns.
    pos: u64,
    chunk_l: Vec<f64>,
    chunk_r: Vec<f64>,
    chunk_ml: Vec<f64>,
    chunk_mr: Vec<f64>,
    /// A plan that grows with its source (`live`), its boundaries taken so
    /// far, and how far it is known; None: the whole plan was given.
    feed: Option<PlanFeed>,
    next_b: usize,
    plan_known: usize,
}

const PULL: usize = 16384;

impl HybridStage {
    /// `lin` and `min` must both be positioned at `start − lead_in(sr)` (or 0).
    pub fn new(lin: Box<dyn Stage>, min: Box<dyn Stage>, plan: &Plan, output_sr: f64, start: u64) -> HybridStage {
        let (sw, fade, _) = switch_params(output_sr);
        let hf = fade / 2;
        let s0 = lin.position() as usize;
        debug_assert_eq!(s0 as u64, min.position());
        let total = lin.total() as usize;
        // Boundaries whose snap window is fully inside the rendered lead-in
        // or later are planned for real; earlier ones only flip the state.
        let first_real = s0 + sw;
        let mut state = plan.use_min_0;
        let mut pending = VecDeque::new();
        for &b in &plan.boundaries {
            if b < first_real && s0 > 0 {
                state = !state;
            } else {
                pending.push_back(b);
            }
        }
        HybridStage {
            lin,
            min,
            total,
            sw,
            hf,
            pending,
            xfades: VecDeque::new(),
            plan_pos: s0,
            plan_min: state,
            cur_min: state,
            emitted: s0,
            base: s0,
            lin_l: Vec::new(),
            lin_r: Vec::new(),
            min_l: Vec::new(),
            min_r: Vec::new(),
            out: VecDeque::new(),
            pos: start,
            chunk_l: vec![0.0; PULL],
            chunk_r: vec![0.0; PULL],
            chunk_ml: vec![0.0; PULL],
            chunk_mr: vec![0.0; PULL],
            feed: None,
            next_b: 0,
            plan_known: usize::MAX,
        }
    }

    /// `new` on a plan that grows with its source: `feed` scanned from no
    /// later than the branches' start. A boundary is snapped once known,
    /// nothing is heard past what the plan knows less a snap window and a
    /// half fade, and the branches wait for the plan as they wait for their
    /// source. From the source's first sample (or with the feed scanned from
    /// it) the output is `new`'s with `scan_plan`, bit for bit.
    pub fn live(lin: Box<dyn Stage>, min: Box<dyn Stage>, mut feed: PlanFeed, output_sr: f64, start: u64) -> HybridStage {
        let (sw, _, _) = switch_params(output_sr);
        let s0 = lin.position() as usize;
        feed.advance(s0 + sw);
        let plan = Plan { boundaries: feed.boundaries().to_vec(), use_min_0: feed.use_min_0() };
        let mut st = HybridStage::new(lin, min, &plan, output_sr, start);
        st.next_b = plan.boundaries.len();
        st.plan_known = feed.known();
        st.feed = Some(feed);
        st
    }

    /// Take what the growing plan has found up to `upto`.
    fn feed_plan(&mut self, upto: usize) {
        if let Some(feed) = self.feed.as_mut() {
            self.plan_known = feed.advance(upto);
            let b = feed.boundaries();
            self.pending.extend(b[self.next_b..].iter().copied());
            self.next_b = b.len();
        }
    }

    fn filled(&self) -> usize {
        self.base + self.lin_l.len()
    }

    fn snap_ready(&mut self, at_end: bool) {
        loop {
            let b = match self.pending.front() {
                Some(&b) => b,
                None => break,
            };
            if !(at_end || b + self.sw < self.filled()) {
                break;
            }
            self.pending.pop_front();
            let sb = {
                let (lin_l, lin_r, min_l, min_r, base) =
                    (&self.lin_l, &self.lin_r, &self.min_l, &self.min_r, self.base);
                let n = lin_l.len();
                let diff = move |j: usize| {
                    if j < base || j - base >= n {
                        return 0.0;
                    }
                    let k = j - base;
                    0.5 * ((lin_l[k] - min_l[k]) + (lin_r[k] - min_r[k]))
                };
                snap_boundary(b, self.total, self.sw, &diff)
            };
            let fade_start = sb.saturating_sub(self.hf).max(self.plan_pos);
            let fade_end = (sb + self.hf).min(self.total).max(fade_start);
            if fade_end > fade_start {
                self.xfades.push_back((fade_start, fade_end, self.plan_min));
            }
            self.plan_pos = fade_end;
            self.plan_min = !self.plan_min;
        }
    }

    fn emit_upto(&mut self, frontier: usize) {
        let mut i = self.emitted;
        while i < frontier {
            match self.xfades.front().copied() {
                Some((fs, _fe, _fm)) if i < fs => {
                    let e = fs.min(frontier);
                    self.copy_flat(i, e);
                    i = e;
                }
                Some((fs, fe, from_min)) => {
                    let e = fe.min(frontier);
                    let fade_len = fe - fs;
                    for j in i..e {
                        let t = (j - fs) as f64 / fade_len.max(1) as f64;
                        let blend = switch_fade(t);
                        let bi = j - self.base;
                        let (fl, fr, tl, tr) = if from_min {
                            (self.min_l[bi], self.min_r[bi], self.lin_l[bi], self.lin_r[bi])
                        } else {
                            (self.lin_l[bi], self.lin_r[bi], self.min_l[bi], self.min_r[bi])
                        };
                        self.push_out(j, fl * (1.0 - blend) + tl * blend, fr * (1.0 - blend) + tr * blend);
                    }
                    i = e;
                    if e == fe {
                        self.xfades.pop_front();
                        self.cur_min = !self.cur_min;
                    }
                }
                None => {
                    self.copy_flat(i, frontier);
                    i = frontier;
                }
            }
        }
        self.emitted = frontier.max(self.emitted);
        if self.emitted > self.base {
            let d = self.emitted - self.base;
            self.lin_l.drain(..d);
            self.lin_r.drain(..d);
            self.min_l.drain(..d);
            self.min_r.drain(..d);
            self.base = self.emitted;
        }
    }

    fn copy_flat(&mut self, from: usize, to: usize) {
        for j in from..to {
            let k = j - self.base;
            let (l, r) = if self.cur_min {
                (self.min_l[k], self.min_r[k])
            } else {
                (self.lin_l[k], self.lin_r[k])
            };
            self.push_out(j, l, r);
        }
    }

    #[inline]
    fn push_out(&mut self, j: usize, l: f64, r: f64) {
        // The lead-in is planned but not heard.
        if j as u64 >= self.pos + self.out.len() as u64 {
            self.out.push_back((l, r));
        }
    }

    fn pull(&mut self) {
        let n = PULL.min(self.total - self.filled());
        self.lin.read(&mut self.chunk_l[..n], &mut self.chunk_r[..n]);
        self.min.read(&mut self.chunk_ml[..n], &mut self.chunk_mr[..n]);
        self.lin_l.extend_from_slice(&self.chunk_l[..n]);
        self.lin_r.extend_from_slice(&self.chunk_r[..n]);
        self.min_l.extend_from_slice(&self.chunk_ml[..n]);
        self.min_r.extend_from_slice(&self.chunk_mr[..n]);
        let at_end = self.filled() >= self.total;
        let want = (self.filled() + self.sw + self.hf + 1).min(self.total);
        self.feed_plan(want);
        self.snap_ready(at_end);
        let frontier = if at_end {
            self.total
        } else {
            let f = match self.pending.front() {
                Some(&b) => self.filled().min(b.saturating_sub(self.sw + self.hf)),
                None => self.filled(),
            };
            // No boundary still unknown can reach back before this.
            f.min(self.plan_known.saturating_sub(self.sw + self.hf))
        };
        if frontier > self.emitted {
            self.emit_upto(frontier);
        }
    }
}

impl Stage for HybridStage {
    fn is_gpu_failed(&self) -> bool {
        self.lin.is_gpu_failed() || self.min.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let mut done = 0;
        let mut inside = 0;
        while done < n {
            if let Some((l, r)) = self.out.pop_front() {
                out_l[done] = l;
                out_r[done] = r;
                done += 1;
                inside += 1;
                self.pos += 1;
                continue;
            }
            if self.pos >= self.total as u64 || self.filled() >= self.total && self.emitted >= self.total {
                for i in done..n {
                    out_l[i] = 0.0;
                    out_r[i] = 0.0;
                }
                break;
            }
            self.pull();
        }
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.total as u64
    }
}

/// Continuous alpha: `y = α·min + (1 − α)·lin`, α the smoothed envelope.
pub struct AlphaStage {
    lin: Box<dyn Stage>,
    min: Box<dyn Stage>,
    smoothed: Vec<f64>,
    frames_to_output: f64,
    ml: Vec<f64>,
    mr: Vec<f64>,
    /// The envelope growing with its source (`live`); None: it was given whole.
    feed: Option<PlanFeed>,
}

impl AlphaStage {
    /// Both branches positioned at the same start.
    pub fn new(lin: Box<dyn Stage>, min: Box<dyn Stage>, env: &Envelope, output_sr: f64) -> AlphaStage {
        AlphaStage {
            lin,
            min,
            smoothed: smooth_analysis_envelope(&env.analysis, env.analysis_sr),
            frames_to_output: output_sr / env.analysis_sr.max(1.0),
            ml: Vec::new(),
            mr: Vec::new(),
            feed: None,
        }
    }

    /// `new` on an envelope that grows with its source: each read waits for
    /// the frames its weights need, as the branches wait for their source.
    pub fn live(lin: Box<dyn Stage>, min: Box<dyn Stage>, feed: PlanFeed) -> AlphaStage {
        AlphaStage {
            lin,
            min,
            smoothed: Vec::new(),
            frames_to_output: feed.frames_to_output(),
            ml: Vec::new(),
            mr: Vec::new(),
            feed: Some(feed),
        }
    }
}

impl Stage for AlphaStage {
    fn is_gpu_failed(&self) -> bool {
        self.lin.is_gpu_failed() || self.min.is_gpu_failed()
    }
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let base = self.lin.position() as usize;
        if self.ml.len() < n {
            self.ml.resize(n, 0.0);
            self.mr.resize(n, 0.0);
        }
        let inside = self.lin.read(out_l, out_r);
        self.min.read(&mut self.ml[..n], &mut self.mr[..n]);
        let smoothed: &[f64] = match self.feed.as_mut() {
            Some(f) => f.smooth_upto(base + n),
            None => &self.smoothed,
        };
        for k in 0..n {
            let alpha = catmull_env_at(smoothed, self.frames_to_output, base + k) as f64;
            out_l[k] = alpha * self.ml[k] + (1.0 - alpha) * out_l[k];
            out_r[k] = alpha * self.mr[k] + (1.0 - alpha) * out_r[k];
        }
        inside
    }
    fn position(&self) -> u64 {
        self.lin.position()
    }
    fn total(&self) -> u64 {
        self.lin.total()
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::player::convolver::{Alignment, Bank, PolyStream, SourceBuf};
    use std::f64::consts::PI;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Tone bursts with sharp attacks at random moments over a quiet bed.
    fn music(n: usize, rate: u32) -> (Vec<f64>, Vec<f64>) {
        let mut s = 0x1234_5678_9abc_def1u64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut l: Vec<f64> = (0..n).map(|_| 0.01 * (rnd() - 0.5)).collect();
        let mut r = l.clone();
        let mut at = 0usize;
        while at < n {
            at += ((0.08 + 0.35 * rnd()) * rate as f64) as usize;
            let f = 100.0 + 3_000.0 * rnd() * rnd();
            let amp = 0.1 + 0.4 * rnd();
            for k in 0..(0.3 * rate as f64) as usize {
                if at + k >= n {
                    break;
                }
                let t = k as f64 / rate as f64;
                let v = amp * (-t * 12.0).exp() * (2.0 * PI * f * t).sin() + amp * 0.4 * (-t * 80.0).exp() * (rnd() - 0.5);
                l[at + k] += v;
                r[at + k] += 0.6 * v;
            }
        }
        (l, r)
    }

    /// What a live Hybrid-Phase or alpha-HP chain reads before its first sound
    /// (`live_plan_need`: 0.31 s at 44.1 kHz ×8, 0.42 s at 48 kHz ×2, whose
    /// first pull spans more of the source) is what its plan asks the stream
    /// for: with that much of the stream in, the plan of the first pull is
    /// made without a wait; a frame less, and the stream starves.
    #[test]
    fn a_live_plan_reads_what_live_plan_need_says() {
        use crate::player::radio::live::LiveSource;
        for (src, out) in [(44_100u32, 352_800.0f64), (48_000, 96_000.0), (96_000, 384_000.0)] {
            let need = live_plan_need(src, out);
            let s = need as f64 / src as f64;
            assert!(s > 0.2 && s < 0.5, "{src} Hz: {s:.3} s");
            let (sw, fade, _) = switch_params(out);
            for (have, starves) in [(need, 0u32), (need - 1, 1)] {
                let live = Arc::new(LiveSource::new(src, usize::MAX / 4, 0.2, 1e6));
                let (l, r) = music(have, src);
                live.push(&l, &r);
                let mut feed = PlanFeed::new(Input::Live(live.clone()), src, out, 0);
                feed.advance(PULL + sw + fade / 2 + 1);
                assert_eq!(live.stats().starves, starves, "{src} Hz, {have} frames in");
            }
        }
    }

    /// A short linear-phase low-pass for ×`l` and its minimum-phase twin, as banks.
    fn banks(l: usize, out_rate: u32) -> (Arc<Bank>, Arc<Bank>) {
        let taps = 2_047usize;
        let m = (taps / 2) as f64;
        let fc = 0.45 / l as f64;
        let lin: Vec<f64> = (0..taps)
            .map(|k| {
                let x = k as f64 - m;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
                sinc * (0.5 - 0.5 * (2.0 * PI * k as f64 / (taps - 1) as f64).cos())
            })
            .collect();
        let min = crate::audio::dsp_core::to_minimum_phase(&lin);
        (
            Arc::new(Bank::from_coeffs("lin", &lin, l, Alignment::Linear, out_rate)),
            Arc::new(Bank::from_coeffs("min", &min, l, Alignment::BandWeighted, out_rate)),
        )
    }

    /// The whole-track envelope through the sidecar, as the player gets it.
    fn envelope_of(src: &SourceBuf, tag: &str) -> Envelope {
        let dir = std::env::temp_dir().join(format!("aura-blend-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.flac");
        crate::audio::hpss_native::generate_and_save(&path, &src.l, &src.r, src.rate, &AtomicBool::new(false)).unwrap();
        let (analysis, analysis_sr) = crate::audio::hybrid_phase::load_analysis_envelope(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        Envelope { analysis, analysis_sr }
    }

    fn read(st: &mut dyn Stage, n: usize, chunks: &[usize]) -> (Vec<f64>, Vec<f64>) {
        let (mut l, mut r) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut k = 0;
        while l.len() < n {
            let c = chunks[k % chunks.len()].min(n - l.len());
            let (mut a, mut b) = (vec![0.0; c], vec![0.0; c]);
            st.read(&mut a, &mut b);
            l.extend(a);
            r.extend(b);
            k += 1;
        }
        (l, r)
    }

    fn same_bits(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    fn setup(tag: &str) -> (Arc<SourceBuf>, Arc<Bank>, Arc<Bank>, Envelope, u32, f64) {
        let rate = 44_100u32;
        let (l, r) = music(12 * rate as usize, rate);
        let src = Arc::new(SourceBuf { l, r, rate });
        let out = 2 * rate;
        let (lin, min) = banks(2, out);
        let env = envelope_of(&src, tag);
        (src, lin, min, env, rate, out as f64)
    }

    /// Hybrid-Phase on a plan that grows with its source plays what it plays
    /// on the whole track's plan: the feed finds `scan_plan`'s boundaries,
    /// and the output is the same to the bit — from the first sample and
    /// from a start in the middle, read in any sizes.
    #[test]
    fn hybrid_phase_on_a_growing_plan_is_the_whole_plans_bit_for_bit() {
        let (src, lin, min, env, rate, out) = setup("hp");
        let total = src.len() * 2;
        let plan = scan_plan(&env, out, total);
        assert!(plan.boundaries.len() > 20, "the plan switches: {}", plan.boundaries.len());
        let mut f = PlanFeed::new(Input::File(src.clone()), rate, out, 0);
        assert_eq!(f.advance(total), total);
        assert_eq!(f.boundaries(), &plan.boundaries[..]);
        assert_eq!(f.use_min_0(), plan.use_min_0);
        for at in [0u64, 3 * out as u64 + 777] {
            let s0 = at.saturating_sub(lead_in(out));
            let mut whole = HybridStage::new(
                Box::new(PolyStream::new(lin.clone(), src.clone(), s0)),
                Box::new(PolyStream::new(min.clone(), src.clone(), s0)),
                &plan,
                out,
                at,
            );
            let mut live = HybridStage::live(
                Box::new(PolyStream::with_input(lin.clone(), Input::File(src.clone()), s0)),
                Box::new(PolyStream::with_input(min.clone(), Input::File(src.clone()), s0)),
                PlanFeed::new(Input::File(src.clone()), rate, out, 0),
                out,
                at,
            );
            let n = total - at as usize + 5_000;
            let (wl, wr) = read(&mut whole, n, &[16_384]);
            let (ll, lr) = read(&mut live, n, &[1_000, 37, 65_536]);
            assert!(same_bits(&wl, &ll) && same_bits(&wr, &lr), "start {at}: the outputs differ");
        }
    }

    /// The radio's way: the branches and the plan read a live source that
    /// grows; Hybrid-Phase plays what it plays on the same audio as a file
    /// with the whole track's plan — bit for bit up to the last seconds,
    /// where the file's envelope sees its end.
    #[test]
    fn hybrid_phase_on_a_live_source_is_the_files() {
        let (src, lin, min, env, rate, out) = setup("live");
        let total = src.len() * 2;
        let plan = scan_plan(&env, out, total);
        let live = Arc::new(crate::player::radio::live::LiveSource::new(rate, 1 << 30, 1.0, 1e9));
        for k in (0..src.len()).step_by(4_096) {
            let e = (k + 4_096).min(src.len());
            live.push(&src.l[k..e], &src.r[k..e]);
        }
        live.close();
        let input = Input::Live(live);
        // A live stream's branches play head and tail (convolver.rs): the
        // file's through head and tail too, so the blend is compared bit for
        // bit (whole blocks differ from head and tail by f64 rounding).
        let mut whole = HybridStage::new(
            Box::new(PolyStream::with_head(lin.clone(), Input::File(src.clone()), 0)),
            Box::new(PolyStream::with_head(min.clone(), Input::File(src.clone()), 0)),
            &plan,
            out,
            0,
        );
        let mut on_air = HybridStage::live(
            Box::new(PolyStream::with_input(lin.clone(), input.clone(), 0)),
            Box::new(PolyStream::with_input(min.clone(), input.clone(), 0)),
            PlanFeed::new(input, rate, out, 0),
            out,
            0,
        );
        let n = total - 2 * out as usize;
        let (wl, wr) = read(&mut whole, n, &[16_384]);
        let (ll, lr) = read(&mut on_air, n, &[8_192, 3]);
        let first = wl.iter().zip(&ll).chain(wr.iter().zip(&lr)).position(|(a, b)| a.to_bits() != b.to_bits());
        assert!(first.is_none(), "the live chain differs from sample {first:?}");
    }
    /// Instant start's way: the branches and the plan read a file whose
    /// source stages are still running (`GrowSource`, pushed in pieces by a
    /// producer while it plays); Hybrid-Phase plays what it plays on the
    /// whole track's plan, to the bit and to the file's end.
    #[test]
    fn hybrid_phase_on_a_growing_file_is_the_whole_plans_bit_for_bit() {
        let (src, lin, min, env, rate, out) = setup("grow");
        let total = src.len() * 2;
        let plan = scan_plan(&env, out, total);
        let grow = Arc::new(crate::player::grow::GrowSource::new(rate, src.len()));
        let (w, s2) = (grow.clone(), src.clone());
        let producer = std::thread::spawn(move || {
            let mut at = 0;
            while at < s2.len() {
                let e = (at + 5_000).min(s2.len());
                w.push(&s2.l[at..e], &s2.r[at..e]);
                at = e;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            w.finish();
        });
        let input = Input::Grow(grow);
        let mut whole = HybridStage::new(
            Box::new(PolyStream::new(lin.clone(), src.clone(), 0)),
            Box::new(PolyStream::new(min.clone(), src.clone(), 0)),
            &plan,
            out,
            0,
        );
        let mut growing = HybridStage::live(
            Box::new(PolyStream::with_input(lin.clone(), input.clone(), 0)),
            Box::new(PolyStream::with_input(min.clone(), input.clone(), 0)),
            PlanFeed::new(input, rate, out, 0),
            out,
            0,
        );
        let n = total + 5_000;
        let (wl, wr) = read(&mut whole, n, &[16_384]);
        let (gl, gr) = read(&mut growing, n, &[8_192, 3, 50_000]);
        producer.join().unwrap();
        let first = wl.iter().zip(&gl).chain(wr.iter().zip(&gr)).position(|(a, b)| a.to_bits() != b.to_bits());
        assert!(first.is_none(), "the growing file's chain differs from sample {first:?}");
    }

    /// Alpha-HP on an envelope that grows with its source blends exactly as
    /// on the whole envelope.
    #[test]
    fn alpha_hp_on_a_growing_envelope_is_the_whole_envelopes_bit_for_bit() {
        let (src, lin, min, env, rate, out) = setup("ahp");
        let total = src.len() * 2;
        for at in [0u64, 5 * out as u64 + 11] {
            let mut whole = AlphaStage::new(
                Box::new(PolyStream::new(lin.clone(), src.clone(), at)),
                Box::new(PolyStream::new(min.clone(), src.clone(), at)),
                &env,
                out,
            );
            let mut live = AlphaStage::live(
                Box::new(PolyStream::with_input(lin.clone(), Input::File(src.clone()), at)),
                Box::new(PolyStream::with_input(min.clone(), Input::File(src.clone()), at)),
                PlanFeed::new(Input::File(src.clone()), rate, out, at as usize),
            );
            let n = total - at as usize + 3_000;
            let (wl, wr) = read(&mut whole, n, &[16_384]);
            let (ll, lr) = read(&mut live, n, &[777, 65_536, 5]);
            if at == 0 {
                assert!(same_bits(&wl, &ll) && same_bits(&wr, &lr), "start {at}: the outputs differ");
            } else {
                // Started later, the envelope warms up before the start: the
                // blend agrees within the sidecar's rounding of the gate.
                let worst = wl.iter().zip(&ll).chain(wr.iter().zip(&lr)).fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
                assert!(worst < 1e-4, "start {at}: largest difference {worst}");
            }
        }
    }
}