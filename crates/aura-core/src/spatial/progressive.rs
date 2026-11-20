//! Hybrid-Phase's lists in parts: the separation and the drum kit run a
//! segment at a time from the track's start, and the lists are made as far
//! as their sound is finished — the player starts once the first seconds
//! are, the rest comes as it plays.
//!
//! The whole run is the one `job::separate_all` and `job::kit_run` make,
//! only in order and in parts: each source sample is the same segments'
//! weighted sum (two at most, so their order does not matter either), the
//! drum kit's run is pushed as the sources finish (it does not depend on
//! how), and the lists kept on disk are made at the end from the whole of
//! it by the same functions as before — bit for bit today's. The lists made
//! on the way (`Cover`) are those frames' hits of the whole lists: each frame
//! read only once every frame its rule looks at is in.
//!
//! A seek far ahead of the run starts a run of its own there (`region`): a
//! few segments before the point (the overlap the point's samples need),
//! its hits kept from a warm-up of the rule's 3 s on. It covers the player
//! from there; the whole run goes on after it, and is what is kept.

use super::kit::{self, KitRun, Net};
use super::netkick::{self as nk, Hit};
use super::{core, stems};

/// HTDemucs over the track a segment at a time, as `job::separate_all`:
/// the segments from `k0` on, the channels `keep` (`source·2 + channel`).
pub(super) struct SepAcc {
    len: usize,
    seg: usize,
    stride: usize,
    w: Vec<f32>,
    keep: Vec<usize>,
    /// the track sample `out[..][0]` is
    base: usize,
    out: Vec<Vec<f32>>,
    sw: Vec<f32>,
    k: usize,
    segs: usize,
    /// the finished samples (every segment over them added, divided)
    fin_lo: usize,
    fin_hi: usize,
}

impl SepAcc {
    pub(super) fn new(len: usize, k0: usize, keep: Vec<usize>) -> SepAcc {
        let seg = core::SEG;
        let stride = (0.75 * seg as f64) as usize;
        let half = seg / 2;
        let top = (seg - half).max(half) as f32;
        let w: Vec<f32> = (0..seg).map(|i| if i < half { (i + 1) as f32 } else { (seg - i) as f32 } / top).collect();
        let segs = len.div_ceil(stride);
        let base = (k0 * stride).min(len);
        let fin_lo = if k0 == 0 { 0 } else { ((k0 - 1) * stride + seg).min(len) };
        SepAcc {
            len,
            seg,
            stride,
            w,
            out: vec![vec![0f32; len - base]; keep.len()],
            keep,
            base,
            sw: vec![0f32; len - base],
            k: k0.min(segs),
            segs,
            fin_lo,
            fin_hi: fin_lo,
        }
    }

    /// The segment `k0` a run begun for samples from `from` on starts at:
    /// the first whose samples before `from` need no segment before it
    /// (None: from the track's start).
    pub(super) fn k0_for(from: usize) -> Option<usize> {
        let seg = core::SEG;
        let stride = (0.75 * seg as f64) as usize;
        (from >= seg).then(|| (from - seg) / stride + 1)
    }

    pub(super) fn done(&self) -> bool {
        self.k >= self.segs
    }

    pub(super) fn progress(&self) -> f32 {
        if self.segs == 0 { 1.0 } else { self.k as f32 / self.segs as f32 }
    }

    /// The finished samples `[lo, hi)`.
    pub(super) fn finished(&self) -> (usize, usize) {
        (self.fin_lo, self.fin_hi)
    }

    /// Kept channel `i`'s samples from `base()` on.
    pub(super) fn chan(&self, i: usize) -> &[f32] {
        &self.out[i]
    }

    pub(super) fn base(&self) -> usize {
        self.base
    }

    /// The next segment, as `separate_all` runs it.
    pub(super) fn step(&mut self, c: &mut dyn core::Separate, l: &[f32], r: &[f32]) -> Result<(), String> {
        if self.done() {
            return Ok(());
        }
        let (len, seg) = (self.len, self.seg);
        let off = self.k * self.stride;
        let n = seg.min(len - off);
        let start = off as isize - ((seg - n) / 2) as isize;
        let cut = |x: &[f32]| -> Vec<f32> {
            (0..seg as isize)
                .map(|i| {
                    let j = start + i;
                    if j >= 0 && (j as usize) < len { x[j as usize] } else { 0.0 }
                })
                .collect()
        };
        let o = c.separate(&cut(l), &cut(r))?;
        let trim = (seg - n) / 2;
        let at = off - self.base;
        for (dst, &s) in self.out.iter_mut().zip(&self.keep) {
            let src = &o[s * seg + trim..s * seg + trim + n];
            for i in 0..n {
                dst[at + i] += self.w[i] * src[i];
            }
        }
        for i in 0..n {
            self.sw[at + i] += self.w[i];
        }
        self.k += 1;
        let hi = if self.k >= self.segs { len } else { self.k * self.stride };
        let from = self.fin_hi.max(self.fin_lo);
        if hi > from {
            for dst in self.out.iter_mut() {
                for i in from - self.base..hi - self.base {
                    dst[i] /= self.sw[i];
                }
            }
            self.fin_hi = hi;
        }
        Ok(())
    }

    /// The channels, whole (a run from the track's start).
    pub(super) fn into_out(self) -> Vec<Vec<f32>> {
        debug_assert!(self.base == 0 && self.done());
        self.out
    }
}

/// Frames read after a run begun mid-track before its hits count: the
/// rule's 3 s of top level, its rise's reach, with room.
pub(super) const WARM: usize = (3.0 * nk::FPS) as usize + 16;
/// Frames past a frame its hit reads (the rise's top 2 on, the steepest
/// step within 3, the level 2 past it, `other`'s rise 4 past it).
const LOOK: usize = 8;

/// One piece's hits as `netkick::hits_of` finds them, a frame at a time as
/// the frames come: its power and `other`'s in its band from frame `f0` on.
struct HitsRun {
    f0: usize,
    /// hits before it are a warm-up's (not kept)
    keep_from: usize,
    next: usize,
    prev_x: bool,
    last: Option<usize>,
    loud: f64,
    p: Vec<f64>,
    l: Vec<f64>,
    lo: Vec<f64>,
    hits: Vec<Hit>,
}

impl HitsRun {
    fn new(f0: usize, keep_from: usize, loud: f64) -> HitsRun {
        HitsRun { f0, keep_from, next: f0, prev_x: false, last: None, loud, p: Vec::new(), l: Vec::new(), lo: Vec::new(), hits: Vec::new() }
    }

    fn push(&mut self, piece: &[f64], other: &[f64]) {
        self.p.extend_from_slice(piece);
        self.l.extend(piece.iter().map(|&p| nk::db(p)));
        self.lo.extend(other.iter().map(|&p| nk::db(p)));
    }

    /// `rise`'s (rise, top) of level `l` at frame `g` (`n` frames in all).
    fn rise_at(&self, l: &[f64], g: usize, n: usize) -> (f64, f64) {
        let f0 = self.f0;
        let a = l[g.saturating_sub(1).max(f0) - f0..(g + 3).min(n) - f0].iter().copied().fold(f64::MIN, f64::max);
        let b = if g == f0 { l[g - f0] } else { l[g.saturating_sub(6).max(f0) - f0..g - f0].iter().copied().fold(f64::MAX, f64::min) };
        (a - b, a)
    }

    /// The frames whose rule's look is in (all of them once the track's
    /// `total` frames are).
    fn advance(&mut self, mix_db: &[f64], total: usize) {
        let avail = self.f0 + self.p.len();
        let (n, lim) = if avail >= total { (total.min(mix_db.len()), total.min(mix_db.len())) } else { (avail, avail.saturating_sub(LOOK)) };
        let f0 = self.f0;
        let ctx = (3.0 * nk::FPS) as usize;
        for f in self.next..lim {
            let l = &self.l;
            let (r, top) = self.rise_at(l, f, n);
            let top3 = l[(f + 1).saturating_sub(ctx).max(f0) - f0..=f - f0].iter().copied().fold(f64::MIN, f64::max);
            let x = r >= 6.0 && top >= top3 - 15.0 && top - mix_db[f] >= -40.0;
            let edge = x && !self.prev_x;
            self.prev_x = x;
            if !edge || self.last.is_some_and(|j| (f - j) as f64 <= 0.09 * nk::FPS) {
                continue;
            }
            self.last = Some(f);
            let steep = (f..(f + 4).min(n))
                .map(|k| (k, if k == 0 || k == f0 { 0.0 } else { l[k - f0] - l[k - 1 - f0] }))
                .fold((f, f64::MIN), |b, (k, d)| if d > b.1 { (k, d) } else { b })
                .0;
            let lvl = nk::db(self.p[steep - f0..(steep + 3).min(n) - f0].iter().copied().fold(0.0, f64::max)) - self.loud;
            let leak = (steep.saturating_sub(3).max(f0)..(steep + 2).min(n))
                .map(|k| self.rise_at(&self.lo, k, n).0)
                .fold(f64::MIN, f64::max)
                - r;
            let conf = nk::ramp(r, 8.0, 18.0) * nk::ramp(lvl, -40.0, -28.0) * (1.0 - nk::ramp(leak, -6.0, 0.0));
            if conf >= nk::CONF_MIN && f >= self.keep_from {
                self.hits.push(Hit { frame: f as u32, conf: conf as f32 });
            }
        }
        self.next = self.next.max(lim);
    }
}

/// The whole mix's levels the lists read their pieces against: the mix's
/// level each frame, and its loud level whole and in the snare's and the
/// hats' bands (`kicks_of`, `pieces_of`).
pub(super) struct MixLevels {
    mix_db: Vec<f64>,
    loud_k: f64,
    loud: [f64; 2],
}

impl MixLevels {
    pub(super) fn of(l: &[f32], r: &[f32]) -> MixLevels {
        use rayon::prelude::*;
        let m = nk::mono(l, r);
        let frames = nk::frames_of(m.len());
        const PART: usize = 1024;
        let bins = [(0, usize::MAX), nk::snare_band(), nk::hats_band()];
        let parts: Vec<Vec<Vec<f64>>> = super::threads::par(|| {
            (0..frames.div_ceil(PART)).into_par_iter().map(|i| nk::band_powers_at(&m, 0, &bins, i * PART..((i + 1) * PART).min(frames))).collect()
        });
        let mut b = vec![Vec::with_capacity(frames); 3];
        for p in parts {
            for (o, v) in b.iter_mut().zip(p) {
                o.extend(v);
            }
        }
        let mix_db = nk::levels(&b[0]);
        MixLevels {
            loud_k: nk::percentile(&mix_db, 99.5),
            loud: [nk::percentile(&nk::levels(&b[1]), 99.5), nk::percentile(&nk::levels(&b[2]), 99.5)],
            mix_db,
        }
    }
}

/// What a run's lists cover: the hits of `[from, upto)` (list frames).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cover {
    pub from: u32,
    pub upto: u32,
    pub kicks: Vec<u32>,
    /// the snare's and the hats' (time order, once each)
    pub pieces: Vec<u32>,
}

/// A run's lists, made as its sound finishes.
struct ListsRun {
    done_to: usize,
    total: usize,
    kick: HitsRun,
    snare: HitsRun,
    hats: HitsRun,
}

impl ListsRun {
    fn new(f0: usize, keep_from: usize, total: usize, mix: &MixLevels) -> ListsRun {
        ListsRun {
            done_to: f0,
            total,
            kick: HitsRun::new(f0, keep_from, mix.loud_k),
            snare: HitsRun::new(f0, keep_from, mix.loud[0]),
            hats: HitsRun::new(f0, keep_from, mix.loud[1]),
        }
    }

    /// The frames whose samples are in: the drums' and `other`'s mono from
    /// track sample `m0`, the snare's and the hats' (`taps`, from `t0`;
    /// None: the drums' bands), `upto` samples finished in all.
    fn feed(&mut self, mix: &MixLevels, drums: &[f32], other: &[f32], m0: usize, taps: Option<(&[f32], &[f32], usize)>, upto: usize) {
        let mut end = self.done_to;
        while end < self.total && end * nk::HOP + nk::NFFT <= upto {
            end += 1;
        }
        if end > self.done_to {
            let fr = self.done_to..end;
            let (sb, hb) = (nk::snare_band(), nk::hats_band());
            let dr = nk::band_powers_at(drums, m0, &[(0, nk::bin(172.0))], fr.clone()).remove(0);
            let ot = nk::band_powers_at(other, m0, &[(0, nk::bin(344.0)), sb, hb], fr.clone());
            let p = match taps {
                Some((s, h, t0)) => {
                    vec![nk::band_powers_at(s, t0, &[(0, usize::MAX)], fr.clone()).remove(0), nk::band_powers_at(h, t0, &[(0, usize::MAX)], fr).remove(0)]
                }
                None => nk::band_powers_at(drums, m0, &[sb, hb], fr),
            };
            self.kick.push(&dr, &ot[0]);
            self.snare.push(&p[0], &ot[1]);
            self.hats.push(&p[1], &ot[2]);
            self.done_to = end;
        }
        for h in [&mut self.kick, &mut self.snare, &mut self.hats] {
            h.advance(&mix.mix_db, self.total);
        }
    }

    fn cover(&self) -> Cover {
        let upto = self.kick.next.min(self.snare.next).min(self.hats.next);
        let below = |h: &HitsRun| h.hits.iter().map(|h| h.frame).filter(|&j| (j as usize) < upto).collect::<Vec<_>>();
        let mut pieces = below(&self.snare);
        pieces.extend(below(&self.hats));
        pieces.sort_unstable();
        pieces.dedup();
        Cover { from: self.kick.keep_from as u32, upto: upto as u32, kicks: below(&self.kick), pieces }
    }

    fn finished(&self) -> bool {
        self.kick.next >= self.total && self.snare.next >= self.total && self.hats.next >= self.total
    }
}

/// One run: the separation from a segment on, its drums through the kit
/// (when there is one), its lists.
struct Run {
    sep: SepAcc,
    kit: Option<KitRun>,
    lists: ListsRun,
    /// the drums' and `other`'s mono from track sample `m0`
    drums: Vec<f32>,
    other: Vec<f32>,
    m0: usize,
    /// where the drums and `other` are among the kept channels
    at_d: usize,
    at_o: usize,
}

const D: usize = stems::DRUMS * 2;
const O: usize = stems::OTHER * 2;

impl Run {
    /// From the track's start: every source kept, the kit run with its
    /// measures (Line Up's map may take it).
    fn whole(len: usize, mix: &MixLevels, kit_on: bool) -> Run {
        let mut kit = kit_on.then(|| KitRun::new(len, kit::OVERLAP));
        if let Some(k) = kit.as_mut() {
            k.mono_tap = Some(vec![(super::job::SNARE, Vec::new()), (super::job::HATS, Vec::new())]);
        }
        Run {
            sep: SepAcc::new(len, 0, (0..core::SOURCES * 2).collect()),
            kit,
            lists: ListsRun::new(0, 0, nk::frames_of(len), mix),
            drums: Vec::new(),
            other: Vec::new(),
            m0: 0,
            at_d: D,
            at_o: O,
        }
    }

    /// For the player reading list frame `j` on: the drums and `other` only.
    /// None: a run from the start would do (it is near).
    fn region(len: usize, j: usize, mix: &MixLevels, kit_on: bool) -> Option<Run> {
        let s = j.saturating_sub(WARM) * nk::HOP;
        let step = kit::CHUNK / kit::OVERLAP;
        let need = if kit_on { s.saturating_sub(2 * step) } else { s };
        let k0 = SepAcc::k0_for(need)?;
        let sep = SepAcc::new(len, k0, vec![D, D + 1, O, O + 1]);
        let m0 = sep.finished().0;
        let mut kit = kit_on.then(|| KitRun::region(len, kit::OVERLAP, m0));
        let mut t0 = m0;
        if let Some(k) = kit.as_mut() {
            k.mono_tap = Some(vec![(super::job::SNARE, Vec::new()), (super::job::HATS, Vec::new())]);
            t0 = k.tap_from();
        }
        let f0 = m0.max(t0).div_ceil(nk::HOP);
        Some(Run {
            sep,
            kit,
            lists: ListsRun::new(f0, f0 + WARM, nk::frames_of(len), mix),
            drums: Vec::new(),
            other: Vec::new(),
            m0,
            at_d: 0,
            at_o: 2,
        })
    }

    fn step(&mut self, c: &mut dyn core::Separate, net: Option<&mut Net>, l: &[f32], r: &[f32], mix: &MixLevels) -> Result<(), String> {
        self.sep.step(c, l, r)?;
        let (_, hi) = self.sep.finished();
        let base = self.sep.base();
        let have = self.m0 + self.drums.len();
        if hi > have {
            let (dl, dr) = (self.sep.chan(self.at_d), self.sep.chan(self.at_d + 1));
            let (ol, or) = (self.sep.chan(self.at_o), self.sep.chan(self.at_o + 1));
            self.drums.extend((have..hi).map(|i| 0.5 * (dl[i - base] + dr[i - base])));
            self.other.extend((have..hi).map(|i| 0.5 * (ol[i - base] + or[i - base])));
        }
        if let (Some(k), Some(net)) = (self.kit.as_mut(), net) {
            let from = k.in_from();
            if hi > from {
                let (dl, dr) = (self.sep.chan(self.at_d), self.sep.chan(self.at_d + 1));
                k.push(net, &dl[from - base..hi - base], &dr[from - base..hi - base])?;
            }
            if self.sep.done() && !k.is_done() {
                k.run_out(net)?;
            }
        }
        let taps = self.kit.as_ref().and_then(|k| {
            let t = k.mono_tap.as_ref()?;
            Some((&t[0].1[..], &t[1].1[..], k.tap_from()))
        });
        let upto = match taps {
            Some((s, h, t0)) => hi.min(t0 + s.len().min(h.len())),
            None => hi,
        };
        self.lists.feed(mix, &self.drums, &self.other, self.m0, taps, upto);
        Ok(())
    }

    fn span(&self) -> Span {
        Span { from: self.lists.kick.keep_from, upto: self.lists.kick.next.min(self.lists.snare.next).min(self.lists.hats.next), end: self.finished() }
    }

    fn finished(&self) -> bool {
        self.sep.done() && self.kit.as_ref().is_none_or(|k| k.is_done()) && self.lists.finished()
    }
}

/// A run's reach in list frames; `end`: it has reached the track's end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Span {
    pub from: usize,
    pub upto: usize,
    pub end: bool,
}

/// What the runs do next.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Pick {
    Whole,
    Region(usize),
    New(usize),
}

/// The player reads list frame `p` on (None: nothing reads yet). The whole
/// run goes on while it covers `p` or comes to it within `soon` frames; a
/// region run covers a seek past that, while it is less than `lead` frames
/// ahead of `p` and not at the track's end; else a new one begins at `p`.
pub(super) fn pick(whole: Span, regions: &[Span], p: Option<usize>, soon: usize, lead: usize) -> Pick {
    let Some(p) = p else { return Pick::Whole };
    if whole.end || p < whole.upto + soon {
        return Pick::Whole;
    }
    match regions.iter().position(|r| r.from <= p && p < r.upto + soon) {
        Some(i) if regions[i].end || regions[i].upto >= p + lead => Pick::Whole,
        Some(i) => Pick::Region(i),
        None => Pick::New(p),
    }
}

/// A region run's reach soon enough: 10 s of frames; ahead enough: 30 s.
const SOON: usize = (10.0 * nk::FPS) as usize;
const LEAD: usize = (30.0 * nk::FPS) as usize;

/// What the runs tell and ask on the way.
pub(super) trait Io {
    /// The list frame the player reads on, if any.
    fn reading(&self) -> Option<usize>;
    /// Run `id`'s lists so far (0: the whole run).
    fn publish(&mut self, id: usize, cover: Cover);
    /// The whole run's separation and kit, each 0…1.
    fn tick(&mut self, stems: f32, kit: f32);
}

/// The whole run's end: the six sources whole and the kit's run (as
/// `separate_all` and `kit_run` give them).
pub(super) struct Whole {
    pub src: Vec<Vec<f32>>,
    pub run: Option<KitRun>,
}

/// The track `l`, `r` (44.1 kHz) through `sep` and the kit's `net` (None:
/// the drums' bands), in parts: the whole run, and region runs where the
/// player reads past it (`io`).
pub(super) fn run(sep: &mut dyn core::Separate, mut net: Option<&mut Net>, l: &[f32], r: &[f32], io: &mut dyn Io, stop: &dyn Fn() -> bool) -> Result<Whole, String> {
    let len = l.len().min(r.len());
    let (l, r) = (&l[..len], &r[..len]);
    let mix = MixLevels::of(l, r);
    let kit_on = net.is_some();
    let mut whole = Run::whole(len, &mix, kit_on);
    // (id, run) — a run's span stays known when it is let go
    let mut regions: Vec<(usize, Option<Run>, Span)> = Vec::new();
    let mut ids = 1;
    while !whole.finished() {
        if stop() {
            return Err("stopped".into());
        }
        let spans: Vec<Span> = regions.iter().map(|r| r.2).collect();
        let p = io.reading().map(|p| p.min(nk::frames_of(len)));
        let mut pk = pick(whole.span(), &spans, p, SOON, LEAD);
        if let Pick::New(j) = pk {
            pk = match Run::region(len, j, &mix, kit_on) {
                Some(run) => {
                    regions.push((ids, Some(run), Span { from: j, upto: j, end: false }));
                    ids += 1;
                    Pick::Region(regions.len() - 1)
                }
                None => Pick::Whole,
            };
        }
        match pk {
            Pick::Region(i) if regions[i].1.is_some() => {
                let (id, run, span) = &mut regions[i];
                let rr = run.as_mut().unwrap();
                rr.step(sep, net.as_deref_mut(), l, r, &mix)?;
                *span = rr.span();
                io.publish(*id, rr.lists.cover());
                if span.end {
                    *run = None;
                }
            }
            _ => {
                whole.step(sep, net.as_deref_mut(), l, r, &mix)?;
                io.publish(0, whole.lists.cover());
                io.tick(whole.sep.progress(), whole.kit.as_ref().map_or(1.0, |k| k.progress()));
                // a region the whole run has come to goes
                let upto = whole.span().upto;
                for reg in regions.iter_mut() {
                    if reg.2.upto <= upto {
                        reg.1 = None;
                        reg.2.end = true;
                    }
                }
            }
        }
    }
    Ok(Whole { src: whole.sep.into_out(), run: whole.kit })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for HTDemucs: each source a different function of the
    /// segment's samples (and of where in it they are), so a wrong segment,
    /// offset or weight shows.
    struct FakeSep;
    impl core::Separate for FakeSep {
        fn separate(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
            let n = l.len();
            let mut o = vec![0f32; core::SOURCES * 2 * n];
            for s in 0..core::SOURCES {
                for c in 0..2 {
                    let x = if c == 0 { l } else { r };
                    for i in 0..n {
                        let pos = (i as f32 / n as f32 - 0.5) * 0.01;
                        // `other` the segment scrambled: no rise with the drums' (else every hit leaks)
                        o[(s * 2 + c) * n + i] = if s == stems::OTHER {
                            0.1 * x[(i * 7919 + 13) % n] + pos * x[i]
                        } else {
                            x[i] * (0.3 + 0.1 * s as f32) + pos * x[(i + 17 * (s + 1)) % n]
                        };
                    }
                }
            }
            Ok(o)
        }
    }

    /// A stand-in for DrumSep: the pieces from the chunk's samples.
    fn fake_kit(l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
        let n = kit::CHUNK;
        let mut y = vec![0f32; kit::NP * 2 * n];
        for p in 0..kit::NP {
            for c in 0..2 {
                let x = if c == 0 { l } else { r };
                for i in 0..n {
                    y[(p * 2 + c) * n + i] = x[i] * (0.2 + 0.15 * p as f32) + 0.05 * x[(i + 31 * (p + 1)) % n] * (i as f32 / n as f32);
                }
            }
        }
        Ok(y)
    }

    /// A drum-like track: noise bursts (kicks, snares, hats) over a pad.
    fn track(secs: f64) -> (Vec<f32>, Vec<f32>) {
        let n = (secs * nk::SR) as usize;
        let mut seed = 11u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        };
        let mut l = vec![0f32; n];
        let mut r = vec![0f32; n];
        for i in 0..n {
            let t = i as f64 / nk::SR;
            let pad = 0.1 * (2.0 * std::f64::consts::PI * 220.0 * t).sin() as f32;
            let beat = (t * 2.0).fract();
            let kick = if beat < 0.08 { (1.0 - beat / 0.08) as f32 * (2.0 * std::f64::consts::PI * 60.0 * t).sin() as f32 } else { 0.0 };
            let snare = if (t * 2.0 + 0.5).fract() < 0.05 { 0.4 * rnd() } else { 0.0 };
            let hat = if (t * 8.0).fract() < 0.02 { 0.2 * rnd() } else { 0.0 };
            l[i] = pad + 0.8 * kick + snare + hat;
            r[i] = pad + 0.7 * kick + snare + 0.8 * hat;
        }
        (l, r)
    }

    struct Rec {
        reading: Option<usize>,
        covers: Vec<(usize, Cover)>,
    }
    impl Io for Rec {
        fn reading(&self) -> Option<usize> {
            self.reading
        }
        fn publish(&mut self, id: usize, cover: Cover) {
            self.covers.push((id, cover));
        }
        fn tick(&mut self, _: f32, _: f32) {}
    }

    fn frames(h: &[Hit]) -> Vec<u32> {
        h.iter().map(|h| h.frame).collect()
    }

    /// Today's whole path on the same stand-ins: `separate_all`, the kit
    /// pushed whole, the lists by `kicks_of_sources` and `pieces_from`.
    fn whole_path(l: &[f32], r: &[f32], kit_on: bool) -> (Vec<Vec<f32>>, Vec<Vec<f32>>, nk::Kicks, nk::Kicks) {
        let src = super::super::job::separate_all(&mut FakeSep, l, r, &mut |_| {}, &|| false).unwrap();
        let drums = [&src[D][..], &src[D + 1][..]];
        let mut run = kit_on.then(|| {
            let mut run = KitRun::new(l.len(), kit::OVERLAP);
            run.mono_tap = Some(vec![(super::super::job::SNARE, Vec::new()), (super::super::job::HATS, Vec::new())]);
            let mut net = |a: &[f32], b: &[f32]| fake_kit(a, b);
            run.push(&mut net, drums[0], drums[1]).unwrap();
            run.run_out(&mut net).unwrap();
            run
        });
        let taps: Vec<Vec<f32>> = run.as_ref().map_or(Vec::new(), |r| r.mono_tap.as_ref().unwrap().iter().map(|t| t.1.clone()).collect());
        let kicks = nk::kicks_of_sources(l, r, &src);
        let other = nk::mono(&src[O], &src[O + 1]);
        let (pieces, _) = nk::pieces_from([l, r], drums, &other, run.as_mut());
        (src, taps, kicks, pieces)
    }

    fn progressive(l: &[f32], r: &[f32], kit_on: bool, reading: Option<usize>) -> (Whole, Vec<(usize, Cover)>) {
        let mut io = Rec { reading, covers: Vec::new() };
        let mut netf = |a: &[f32], b: &[f32]| fake_kit(a, b);
        let net: Option<&mut Net> = if kit_on { Some(&mut netf) } else { None };
        let w = run(&mut FakeSep, net, l, r, &mut io, &|| false).unwrap();
        (w, io.covers)
    }

    fn bits(v: &[Vec<f32>]) -> Vec<Vec<u32>> {
        v.iter().map(|c| c.iter().map(|x| x.to_bits()).collect()).collect()
    }

    /// In parts or whole: the same sources, the same kit sound, the same
    /// lists; every cover on the way is the whole lists' hits in its frames.
    #[test]
    fn parts_give_the_whole_bit_for_bit() {
        for (secs, kit_on) in [(31.0, true), (19.0, false)] {
            let (l, r) = track(secs);
            let (src, taps, kicks, pieces) = whole_path(&l, &r, kit_on);
            let (w, covers) = progressive(&l, &r, kit_on, None);
            assert_eq!(bits(&w.src), bits(&src));
            if kit_on {
                let t: Vec<Vec<f32>> = w.run.as_ref().unwrap().mono_tap.as_ref().unwrap().iter().map(|t| t.1.clone()).collect();
                assert_eq!(bits(&t), bits(&taps));
            }
            assert!(!kicks.hits.is_empty() && !pieces.hits.is_empty(), "the track has hits");
            assert!(covers.len() >= 3, "covers on the way: {}", covers.len());
            let last = &covers.last().unwrap().1;
            assert_eq!(last.upto as usize, nk::frames_of(l.len()));
            assert_eq!(last.kicks, frames(&kicks.hits));
            assert_eq!(last.pieces, frames(&pieces.hits));
            let mut prev = 0;
            for (id, c) in &covers {
                assert_eq!(*id, 0);
                assert!(c.upto >= prev);
                prev = c.upto;
                let inr = |h: &[Hit]| frames(h).into_iter().filter(|&j| j < c.upto).collect::<Vec<_>>();
                assert_eq!(c.kicks, inr(&kicks.hits));
                assert_eq!(c.pieces, inr(&pieces.hits));
            }
            // the first cover comes after the first segment, not the end
            assert!((covers[0].1.upto as usize) < nk::frames_of(l.len()) / 2);
        }
    }

    /// A seek far ahead: a region run covers it first, its hits the whole
    /// lists' from its warm-up on; the whole run still gives the whole.
    #[test]
    fn a_seek_ahead_runs_a_region_with_the_whole_lists_hits() {
        let (l, r) = track(48.0);
        let (src, _, kicks, pieces) = whole_path(&l, &r, true);
        let p = (36.0 * nk::FPS) as usize;
        let (w, covers) = progressive(&l, &r, true, Some(p));
        assert_eq!(bits(&w.src), bits(&src));
        let reg: Vec<&Cover> = covers.iter().filter(|(id, _)| *id == 1).map(|(_, c)| c).collect();
        assert!(!reg.is_empty(), "a region run");
        // it covered p before the whole run came near it
        let first = covers.iter().position(|(id, c)| *id == 1 && (c.from as usize) <= p && c.upto as usize > p).unwrap();
        assert!(covers[..first].iter().all(|(id, c)| *id != 0 || (c.upto as usize) < p));
        for c in reg {
            assert!(c.from as usize <= p);
            let inr = |h: &[Hit]| frames(h).into_iter().filter(|&j| j >= c.from && j < c.upto).collect::<Vec<_>>();
            assert_eq!(c.kicks, inr(&kicks.hits));
            assert_eq!(c.pieces, inr(&pieces.hits));
        }
        let last = &covers.iter().rev().find(|(id, _)| *id == 0).unwrap().1;
        assert_eq!(last.kicks, frames(&kicks.hits));
        assert_eq!(last.pieces, frames(&pieces.hits));
    }

    /// The rule fed in small parts of any size gives `hits_of`'s hits, each
    /// part's as soon as they are given: every frame read only once its look
    /// is in, the state carried over the parts' edges.
    #[test]
    fn the_rule_in_parts_is_hits_of() {
        let mut seed = 5u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let n = 3000;
        // bursts every ~9 frames over a floor, `other` leaking now and then
        let (mut piece, mut other, mut mix_db) = (vec![0f64; n], vec![0f64; n], vec![0f64; n]);
        for f in 0..n {
            let burst = f % 9 < 2 || rnd() < 0.08;
            piece[f] = if burst { 1e-1 * (0.2 + rnd()) } else { 1e-5 * (0.5 + rnd()) };
            other[f] = if rnd() < 0.1 { piece[f] * rnd() } else { 1e-5 * rnd() };
            mix_db[f] = nk::db(piece[f] + other[f]) + 3.0 * rnd();
        }
        let loud = -12.0;
        let whole = frames(&nk::hits_of(&piece, &other, &mix_db, loud));
        assert!(whole.len() > 100, "hits: {}", whole.len());
        for _ in 0..20 {
            let mut h = HitsRun::new(0, 0, loud);
            let mut at = 0;
            while at < n {
                let k = (1 + (rnd() * 20.0) as usize).min(n - at);
                h.push(&piece[at..at + k], &other[at..at + k]);
                at += k;
                h.advance(&mix_db, n);
                let got = frames(&h.hits);
                let want: Vec<u32> = whole.iter().copied().filter(|&j| (j as usize) < h.next).collect();
                assert_eq!(got, want, "after {at} frames (read to {})", h.next);
            }
            assert_eq!(h.next, n);
        }
    }

    /// A run from a segment on: its finished samples are the whole
    /// separation's, bit for bit.
    #[test]
    fn a_region_separation_finishes_the_whole_ones_samples() {
        let (l, r) = track(40.0);
        let src = super::super::job::separate_all(&mut FakeSep, &l, &r, &mut |_| {}, &|| false).unwrap();
        for from in [core::SEG + 1, 3 * core::SEG + 777] {
            let k0 = SepAcc::k0_for(from).unwrap();
            let mut acc = SepAcc::new(l.len(), k0, vec![D, O + 1]);
            while !acc.done() {
                acc.step(&mut FakeSep, &l, &r).unwrap();
                let (lo, hi) = acc.finished();
                assert!(lo <= from, "from {from}: finished from {lo}");
                for (i, &s) in [D, O + 1].iter().enumerate() {
                    let (got, want) = (&acc.chan(i)[lo - acc.base()..hi - acc.base()], &src[s][lo..hi]);
                    assert!(got.iter().zip(want).all(|(a, b)| a.to_bits() == b.to_bits()), "from {from}: [{lo}, {hi})");
                }
            }
            assert_eq!(acc.finished().1, l.len());
        }
    }

    #[test]
    fn picks() {
        let s = |from, upto, end| Span { from, upto, end };
        let w = s(0, 100, false);
        assert_eq!(pick(w, &[], None, 10, 50), Pick::Whole);
        assert_eq!(pick(w, &[], Some(50), 10, 50), Pick::Whole);
        assert_eq!(pick(w, &[], Some(109), 10, 50), Pick::Whole);
        assert_eq!(pick(w, &[], Some(110), 10, 50), Pick::New(110));
        assert_eq!(pick(w, &[s(200, 220, false)], Some(210), 10, 50), Pick::Region(0));
        assert_eq!(pick(w, &[s(200, 270, false)], Some(210), 10, 50), Pick::Whole);
        assert_eq!(pick(w, &[s(200, 220, true)], Some(210), 10, 50), Pick::Whole);
        assert_eq!(pick(w, &[s(200, 220, false)], Some(240), 10, 50), Pick::New(240));
        assert_eq!(pick(s(0, 100, true), &[], Some(500), 10, 50), Pick::Whole);
    }

    /// A region kit run's taps are the whole run's sound from `tap_from` on.
    #[test]
    fn a_region_kit_run_has_the_whole_runs_sound() {
        let (l, r) = track(20.0);
        let len = l.len();
        let mut net = |a: &[f32], b: &[f32]| fake_kit(a, b);
        let mut whole = KitRun::new(len, kit::OVERLAP);
        whole.mono_tap = Some(vec![(1, Vec::new()), (3, Vec::new())]);
        whole.push(&mut net, &l, &r).unwrap();
        whole.run_out(&mut net).unwrap();
        let from = 7 * 44_100 + 123;
        let mut reg = KitRun::region(len, kit::OVERLAP, from);
        reg.mono_tap = Some(vec![(1, Vec::new()), (3, Vec::new())]);
        let at = reg.in_from();
        assert!(at >= from);
        reg.push(&mut net, &l[at..], &r[at..]).unwrap();
        reg.run_out(&mut net).unwrap();
        let t0 = reg.tap_from();
        assert!(t0 > from && t0 < from + kit::CHUNK * 2);
        for k in 0..2 {
            let (a, b) = (&whole.mono_tap.as_ref().unwrap()[k].1, &reg.mono_tap.as_ref().unwrap()[k].1);
            assert_eq!(b.len(), len - t0);
            assert!(a[t0..].iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits()));
        }
    }
}
