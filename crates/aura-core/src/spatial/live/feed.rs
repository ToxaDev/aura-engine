//! The stream's mix for the instruments: the shadow — the stream as it was
//! decoded, before the rack's source stages (`LiveSource::read_raw`) — at
//! 44.1 kHz, the rate the separation network was trained at, in f64 (the
//! manifesto's §4.1: nothing of ours in f32; the network's own boundary is
//! the only place the sound becomes f32, and nothing goes back from there to
//! what is heard).
//!
//! The session's own timeline: its sample `s` at 44.1 kHz is the shadow's
//! frame `s · rate / 44 100`, from the frame the feed started at on.
//!
//! The shadow's frames ahead of the place being heard may still change after
//! they were read: a connection that breaks fades its last 10 ms, and the
//! burst a new one brings after a gap is cut (`LiveSource::cut_after_gap`:
//! what follows moves earlier). Each block read is kept by a checksum; when
//! the shadow says it was edited (its counters moved), the blocks are read
//! again and the feed starts afresh where the first one differs.

use rubato::{FftFixedIn, Resampler};

use crate::player::radio::live::LiveSource;

/// The network's rate (the session's timeline).
pub const SR: usize = super::super::core::SR as usize;
/// Shadow frames per checksum.
const BLOCK: usize = 4096;
/// The checksums kept: the shadow's frames this far back may still be read
/// again (the place being heard is up to ~8 s behind the stream's edge, the
/// history behind it is the shadow's 40 s).
const SUMS_S: f64 = 30.0;
/// The mix is dropped from its front in pieces of at least this many samples.
const TRIM: usize = 1 << 18;
/// What the feed reads in one go, shadow frames.
const READ: usize = 1 << 15;

/// What the feed reads: the live source's shadow, or a test's signal.
pub trait Shadow {
    fn rate(&self) -> u32;
    /// The end of what is there, frames.
    fn end(&self) -> i64;
    /// The first frame still held.
    fn first(&self) -> i64;
    /// Channel `ch` from frame `from`; frames not held read as zero.
    fn read(&self, ch: usize, from: i64, out: &mut [f64]);
    /// A number that moves whenever frames already there may have changed
    /// (a gap's fade, a cut, a starve's fade).
    fn edits(&self) -> u64;
}

impl Shadow for LiveSource {
    fn rate(&self) -> u32 {
        LiveSource::rate(self)
    }
    fn end(&self) -> i64 {
        LiveSource::end(self)
    }
    fn first(&self) -> i64 {
        self.raw_start()
    }
    fn read(&self, ch: usize, from: i64, out: &mut [f64]) {
        self.read_raw(ch, from, out);
    }
    fn edits(&self) -> u64 {
        let s = self.stats();
        s.gaps as u64 + s.starves as u64 + s.cut_frames
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A block's checksum: its samples' bits, both channels.
fn checksum(l: &[f64], r: &[f64]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for (a, b) in l.iter().zip(r) {
        for v in [a.to_bits(), b.to_bits()] {
            h = (h ^ v).wrapping_mul(0x0100_0000_01b3).rotate_left(23);
        }
    }
    h
}

/// The mix at 44.1 kHz on the session's timeline, and where it is read from.
pub struct Feed {
    rate: u32,
    /// Shadow frames and session samples in one whole step of both
    /// (48 kHz: 160 frames are 147 samples).
    q: i64,
    p: i64,
    /// The next shadow frame to read.
    next: i64,
    rs: Option<FftFixedIn<f64>>,
    pend: [Vec<f64>; 2],
    /// Output samples still to drop: the resampler's delay after a start.
    drop: usize,
    /// The mix: `mix[ch][0]` is session sample `base`.
    base: usize,
    mix: [Vec<f64>; 2],
    edits: u64,
    /// (first shadow frame, checksum) of the blocks read lately.
    sums: std::collections::VecDeque<(i64, u64)>,
    /// A block not whole yet: its first frame and its samples so far.
    part: (i64, [Vec<f64>; 2]),
    /// Starts after the first (an edit found, or the shadow ran out from under the feed).
    pub restarts: u32,
}

/// What a pull brought.
pub struct Pulled {
    /// The feed started afresh from this session sample: what was made from
    /// the mix past it no longer holds.
    pub restart: Option<usize>,
}

impl Feed {
    /// A feed from shadow frame `from` (taken down to a whole step), its
    /// edits counted from `edits`.
    pub fn new(rate: u32, from: i64, edits: u64) -> Feed {
        let g = gcd(rate as usize, SR) as i64;
        let (q, p) = (rate as i64 / g, SR as i64 / g);
        let from = from.max(0) / q * q;
        let mut f = Feed {
            rate,
            q,
            p,
            next: from,
            rs: None,
            pend: [Vec::new(), Vec::new()],
            drop: 0,
            base: (from / q * p) as usize,
            mix: [Vec::new(), Vec::new()],
            edits,
            sums: Default::default(),
            part: (from, [Vec::new(), Vec::new()]),
            restarts: 0,
        };
        f.start_resampler();
        f
    }

    fn start_resampler(&mut self) {
        self.pend = [Vec::new(), Vec::new()];
        if self.rate as usize == SR {
            self.rs = None;
            self.drop = 0;
        } else {
            let rs = FftFixedIn::<f64>::new(self.rate as usize, SR, 8192, 2, 2).expect("a resampler to 44.1 kHz");
            self.drop = rs.output_delay();
            self.rs = Some(rs);
        }
    }

    /// The session sample of shadow frame `frame` (fractional).
    pub fn session_of(&self, frame: f64) -> f64 {
        frame * SR as f64 / self.rate as f64
    }

    /// The mix's first and end samples held.
    pub fn start(&self) -> usize {
        self.base
    }
    pub fn end(&self) -> usize {
        self.base + self.mix[0].len()
    }

    /// Session samples [from, from + n) of channel `ch` as f32 for the
    /// analysis and the network (their boundary: §4.1); zero where not held.
    pub fn get_f32(&self, ch: usize, from: isize, out: &mut [f32]) {
        let m = &self.mix[ch];
        for (i, o) in out.iter_mut().enumerate() {
            let k = from + i as isize - self.base as isize;
            *o = if k >= 0 && (k as usize) < m.len() { m[k as usize] as f32 } else { 0.0 };
        }
    }

    /// The mix's samples [from, to) of both channels (f64), cut to what is held.
    pub fn slice(&self, from: usize, to: usize) -> (&[f64], &[f64]) {
        let a = from.max(self.base).min(self.end()) - self.base;
        let b = to.max(self.base).min(self.end()) - self.base;
        (&self.mix[0][a..b.max(a)], &self.mix[1][a..b.max(a)])
    }

    /// Forget the mix before session sample `keep_from`.
    pub fn trim(&mut self, keep_from: usize) {
        if keep_from > self.base + TRIM {
            let n = (keep_from - self.base).min(self.mix[0].len());
            self.mix[0].drain(..n);
            self.mix[1].drain(..n);
            self.base += n;
        }
    }

    /// Start afresh from shadow frame `frame` (down to a whole step): the
    /// mix past its session sample is dropped.
    fn restart_at(&mut self, frame: i64) -> usize {
        let frame = frame.max(0) / self.q * self.q;
        let s = (frame / self.q * self.p) as usize;
        if s < self.base || s > self.end() {
            self.mix[0].clear();
            self.mix[1].clear();
            self.base = s;
        } else {
            self.mix[0].truncate(s - self.base);
            self.mix[1].truncate(s - self.base);
        }
        self.next = frame;
        self.sums.retain(|(f, _)| *f + (BLOCK as i64) <= frame);
        self.part = (frame, [Vec::new(), Vec::new()]);
        self.start_resampler();
        self.restarts += 1;
        s
    }

    /// The first block read that the shadow no longer holds the same.
    fn changed(&self, sh: &dyn Shadow) -> Option<i64> {
        let (mut l, mut r) = (vec![0f64; BLOCK], vec![0f64; BLOCK]);
        for &(f, sum) in &self.sums {
            if f < sh.first() {
                continue;
            }
            sh.read(0, f, &mut l);
            sh.read(1, f, &mut r);
            if checksum(&l, &r) != sum {
                return Some(f);
            }
        }
        let (f, part) = &self.part;
        let n = part[0].len();
        if n > 0 && *f >= sh.first() {
            sh.read(0, *f, &mut l[..n]);
            sh.read(1, *f, &mut r[..n]);
            if l[..n] != part[0][..] || r[..n] != part[1][..] {
                return Some(*f);
            }
        }
        None
    }

    /// Read what the shadow has brought since, into the mix.
    pub fn pull(&mut self, sh: &dyn Shadow) -> Pulled {
        let mut restart = None;
        let edits = sh.edits();
        if edits != self.edits {
            self.edits = edits;
            if let Some(f) = self.changed(sh) {
                let s = self.restart_at(f);
                crate::aelog!("[SPATIAL] live: the stream changed under the feed at {:.2} s — the analysis starts afresh there", s as f64 / SR as f64);
                restart = Some(s);
            }
        }
        if self.next < sh.first() {
            // the shadow's history moved on past the feed (it was not read for long)
            let s = self.restart_at(sh.first() + self.q - 1);
            restart = Some(restart.map_or(s, |r: usize| r.min(s)));
        }
        let end = sh.end();
        let (mut l, mut r) = (vec![0f64; READ], vec![0f64; READ]);
        while self.next < end {
            let n = ((end - self.next) as usize).min(READ);
            sh.read(0, self.next, &mut l[..n]);
            sh.read(1, self.next, &mut r[..n]);
            self.keep_sums(&l[..n], &r[..n]);
            self.next += n as i64;
            self.resample(&l[..n], &r[..n]);
        }
        Pulled { restart }
    }

    fn keep_sums(&mut self, l: &[f64], r: &[f64]) {
        let mut i = 0;
        while i < l.len() {
            let take = (BLOCK - self.part.1[0].len()).min(l.len() - i);
            self.part.1[0].extend_from_slice(&l[i..i + take]);
            self.part.1[1].extend_from_slice(&r[i..i + take]);
            i += take;
            if self.part.1[0].len() == BLOCK {
                let f = self.part.0;
                let sum = checksum(&self.part.1[0], &self.part.1[1]);
                self.sums.push_back((f, sum));
                self.part = (f + BLOCK as i64, [Vec::with_capacity(BLOCK), Vec::with_capacity(BLOCK)]);
            }
        }
        let keep = (SUMS_S * self.rate as f64 / BLOCK as f64) as usize + 1;
        while self.sums.len() > keep {
            self.sums.pop_front();
        }
    }

    fn resample(&mut self, l: &[f64], r: &[f64]) {
        let Some(rs) = self.rs.as_mut() else {
            self.mix[0].extend_from_slice(l);
            self.mix[1].extend_from_slice(r);
            return;
        };
        self.pend[0].extend_from_slice(l);
        self.pend[1].extend_from_slice(r);
        let mut used = 0;
        while self.pend[0].len() - used >= rs.input_frames_next() {
            let n = rs.input_frames_next();
            let out = rs.process(&[&self.pend[0][used..used + n], &self.pend[1][used..used + n]], None).expect("the resampler");
            used += n;
            let d = self.drop.min(out[0].len());
            self.drop -= d;
            self.mix[0].extend_from_slice(&out[0][d..]);
            self.mix[1].extend_from_slice(&out[1][d..]);
        }
        self.pend[0].drain(..used);
        self.pend[1].drain(..used);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// A shadow of the tests: a signal that grows, and can be edited.
    pub struct TestShadow {
        pub rate: u32,
        pub l: Mutex<Vec<f64>>,
        pub r: Mutex<Vec<f64>>,
        pub edits: AtomicU64,
        pub first: i64,
    }

    impl TestShadow {
        pub fn new(rate: u32) -> TestShadow {
            TestShadow { rate, l: Mutex::new(Vec::new()), r: Mutex::new(Vec::new()), edits: AtomicU64::new(0), first: 0 }
        }
        pub fn push(&self, l: &[f64], r: &[f64]) {
            self.l.lock().unwrap().extend_from_slice(l);
            self.r.lock().unwrap().extend_from_slice(r);
        }
    }

    impl Shadow for TestShadow {
        fn rate(&self) -> u32 {
            self.rate
        }
        fn end(&self) -> i64 {
            self.l.lock().unwrap().len() as i64
        }
        fn first(&self) -> i64 {
            self.first
        }
        fn read(&self, ch: usize, from: i64, out: &mut [f64]) {
            let v = if ch == 0 { self.l.lock().unwrap() } else { self.r.lock().unwrap() };
            for (i, o) in out.iter_mut().enumerate() {
                let k = from + i as i64;
                *o = if k >= 0 && (k as usize) < v.len() { v[k as usize] } else { 0.0 };
            }
        }
        fn edits(&self) -> u64 {
            self.edits.load(Ordering::Relaxed)
        }
    }

    fn tone(rate: u32, n: usize, f: f64, from: usize) -> Vec<f64> {
        (from..from + n).map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin() * 0.5).collect()
    }

    /// A 48 kHz stream fed in blocks of any length comes out at 44.1 kHz on
    /// the session's timeline: a tone keeps its frequency and its phase (the
    /// resampler's delay is off the front), a click lands at its frame's
    /// sample.
    #[test]
    fn a_48k_stream_is_the_same_sound_at_44k_on_the_sessions_timeline() {
        let sh = TestShadow::new(48_000);
        let n = 48_000 * 3;
        let x = tone(48_000, n, 1000.0, 0);
        let mut f = Feed::new(48_000, 0, 0);
        let mut at = 0;
        let mut k = 1usize;
        while at < n {
            let m = (k * 7919 % 20_000 + 1).min(n - at);
            sh.push(&x[at..at + m], &x[at..at + m]);
            f.pull(&sh);
            at += m;
            k += 1;
        }
        assert_eq!(f.start(), 0);
        // 3 s at 48 kHz → about 3 s at 44.1 kHz (the resampler keeps its last chunk back)
        assert!(f.end() > 44_100 * 2 && f.end() <= 44_100 * 3, "{}", f.end());
        let want = tone(44_100, f.end(), 1000.0, 0);
        let (l, _) = f.slice(0, f.end());
        let err = l[4410..f.end() - 4410].iter().zip(&want[4410..f.end() - 4410]).fold(0f64, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-3, "the tone at its own phase: {err}");
        assert_eq!(f.session_of(48_000.0), 44_100.0);
    }

    /// The shadow edited under the feed — the burst after a gap cut, what
    /// followed moved earlier — is found, and the feed starts afresh at the
    /// first block that differs; before it, nothing changes.
    #[test]
    fn a_cut_in_the_shadow_starts_the_feed_afresh_where_it_differs() {
        let sh = TestShadow::new(44_100);
        let a = tone(44_100, 44_100 * 4, 300.0, 0);
        sh.push(&a, &a);
        let mut f = Feed::new(44_100, 0, 0);
        assert!(f.pull(&sh).restart.is_none());
        assert_eq!(f.end(), 44_100 * 4);
        // 0.5 s cut out at 3 s: the rest moves earlier
        let at = 44_100 * 3;
        sh.l.lock().unwrap().drain(at..at + 22_050);
        sh.r.lock().unwrap().drain(at..at + 22_050);
        sh.edits.fetch_add(1, Ordering::Relaxed);
        let p = f.pull(&sh);
        let s = p.restart.expect("found");
        assert!(s <= at && s + BLOCK > at, "afresh from the block of the cut: {s}");
        // what the feed holds now is the shadow, frame for frame
        let (l, _) = f.slice(0, f.end());
        let v = sh.l.lock().unwrap().clone();
        assert_eq!(f.end(), v.len());
        assert!(l.iter().zip(v.iter()).all(|(a, b)| a == b));
        // no edit: nothing is read again
        assert!(f.pull(&sh).restart.is_none());
    }
}
