//! A stream taken apart into its six sources as it comes: HTDemucs over the
//! mix segment by segment, on the same grid as a file's (`job::separate_all`):
//! a 7.8 s segment every three quarters of one, weighted by a triangle, from
//! the session's first sample (or the place the analysis started afresh). A
//! segment goes as soon as its last sample is in; until the next one comes
//! its last quarter is its own alone (weighed by itself), and the next one
//! makes it whole.
//!
//! Behind (a busy card): a segment whose part of the stream would be heard
//! before it could be ready is skipped — the newest one ahead of what is heard
//! goes instead, and the frames skipped keep the stand-in from the mix. The
//! sound never waits for any of this: the separation only reads the shadow.
//!
//! The sources are f32: the network's own boundary (§4.1), read by the
//! analysis that draws the picture; nothing goes back to what is heard.

use super::super::core::{SEG, SOURCES};
use super::super::STRIDE;

/// The samples kept are dropped from the front in pieces of at least this many.
const TRIM: usize = 1 << 18;

pub struct Sep {
    origin: usize,
    /// The next segment not decided yet (separated or skipped).
    next: usize,
    /// `acc[source·2 + channel][0]` is session sample `base`.
    base: usize,
    acc: Vec<Vec<f32>>,
    sw: Vec<f32>,
    w: Vec<f32>,
    /// The runs of samples some segment has, in order.
    runs: Vec<(usize, usize)>,
    pub done: u64,
    pub skipped: u64,
}

impl Sep {
    /// The grid from session sample `origin`.
    pub fn new(origin: usize) -> Sep {
        let half = SEG / 2;
        let top = (SEG - half).max(half) as f32;
        let w = (0..SEG).map(|i| if i < half { (i + 1) as f32 } else { (SEG - i) as f32 } / top).collect();
        Sep { origin, next: 0, base: origin, acc: vec![Vec::new(); SOURCES * 2], sw: Vec::new(), w, runs: Vec::new(), done: 0, skipped: 0 }
    }

    /// Segment `k`'s first sample.
    pub fn start(&self, k: usize) -> usize {
        self.origin + k * STRIDE
    }

    /// Samples below this are whole: no segment still to come adds to them.
    pub fn final_end(&self) -> usize {
        self.start(self.next)
    }

    /// The first sample still held.
    pub fn held_from(&self) -> usize {
        self.base
    }

    /// Where the run of samples the last segment is in begins.
    pub fn run_start(&self) -> usize {
        self.runs.last().map_or(self.origin, |r| r.0)
    }

    /// Samples below this have some segment (the last one's end).
    pub fn prov_end(&self) -> usize {
        self.runs.last().map_or(self.origin, |r| r.1)
    }

    /// Whether every sample of [a, b) has a segment.
    pub fn covered(&self, a: usize, b: usize) -> bool {
        self.runs.iter().any(|r| r.0 <= a && b <= r.1)
    }

    /// The segment to take apart now, with the mix in up to `mix_end`, the
    /// place heard at `heard` and the work taking about `budget` samples'
    /// time: the first whole one whose own part (up to where the next one
    /// starts) is still ahead of what will be heard by then; those before it
    /// are skipped. None: none is whole, or none would be in time.
    pub fn pick(&mut self, mix_end: usize, heard: usize, budget: usize) -> Option<usize> {
        if mix_end < self.origin + SEG {
            return None;
        }
        let kmax = (mix_end - self.origin - SEG) / STRIDE;
        if self.next > kmax {
            return None;
        }
        let soon = heard + budget;
        let k = (self.next..=kmax).find(|&k| self.start(k) + STRIDE > soon).unwrap_or(kmax);
        if self.start(k) + SEG <= soon {
            self.skipped += (kmax + 1 - self.next) as u64;
            self.next = kmax + 1;
            return None;
        }
        self.skipped += (k - self.next) as u64;
        self.next = k;
        Some(k)
    }

    /// Segment `k`'s sources (`[source][channel][SEG]` flattened) in.
    pub fn add(&mut self, k: usize, out: &[f32]) {
        let a = self.start(k);
        debug_assert!(a >= self.base && out.len() == SOURCES * 2 * SEG);
        let need = a + SEG - self.base;
        if self.sw.len() < need {
            self.sw.resize(need, 0.0);
            for v in self.acc.iter_mut() {
                v.resize(need, 0.0);
            }
        }
        let o = a - self.base;
        for (sc, dst) in self.acc.iter_mut().enumerate() {
            let src = &out[sc * SEG..(sc + 1) * SEG];
            for i in 0..SEG {
                dst[o + i] += self.w[i] * src[i];
            }
        }
        for i in 0..SEG {
            self.sw[o + i] += self.w[i];
        }
        match self.runs.last_mut() {
            Some(r) if r.1 >= a => r.1 = r.1.max(a + SEG),
            _ => self.runs.push((a, a + SEG)),
        }
        self.done += 1;
        self.next = k + 1;
    }

    /// Source·channel `sc` from session sample `from`: the segments' sum
    /// over their weights; zero where none has it.
    pub fn read(&self, sc: usize, from: isize, out: &mut [f32]) {
        let (v, sw) = (&self.acc[sc], &self.sw);
        for (i, o) in out.iter_mut().enumerate() {
            let k = from + i as isize - self.base as isize;
            *o = if k >= 0 && (k as usize) < sw.len() && sw[k as usize] > 0.0 { v[k as usize] / sw[k as usize] } else { 0.0 };
        }
    }

    /// Forget the samples before `keep_from` (never past the next segment's start).
    pub fn trim(&mut self, keep_from: usize) {
        let keep_from = keep_from.min(self.start(self.next));
        if keep_from > self.base + TRIM {
            let n = (keep_from - self.base).min(self.sw.len());
            self.sw.drain(..n);
            for v in self.acc.iter_mut() {
                v.drain(..n);
            }
            self.base += n;
            self.runs.retain(|r| r.1 > keep_from);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::super::core::Separate;
    use super::super::super::job::separate_all;
    use super::*;

    /// A stand-in network whose sources depend on where a segment begins
    /// (a turn within the segment): the stream's segments must be cut where
    /// a file's are for the two to agree.
    pub struct Fake {
        pub calls: usize,
    }

    impl Separate for Fake {
        fn separate(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
            self.calls += 1;
            let mut out = vec![0f32; SOURCES * 2 * SEG];
            for s in 0..SOURCES {
                for (ch, x) in [l, r].into_iter().enumerate() {
                    let o = &mut out[(s * 2 + ch) * SEG..(s * 2 + ch + 1) * SEG];
                    for i in 0..SEG {
                        o[i] = x[i] * (s + 1) as f32 * 0.1 + 0.05 * x[(i + 37 * (s + 1)) % SEG];
                    }
                }
            }
            Ok(out)
        }
    }

    pub fn signal(n: usize) -> (Vec<f32>, Vec<f32>) {
        let l = (0..n).map(|i| ((i as f32 * 0.0123).sin() * 0.4 + ((i * 7919) % 1013) as f32 / 1013.0 * 0.1) as f32).collect();
        let r = (0..n).map(|i| ((i as f32 * 0.0071).cos() * 0.3) as f32).collect();
        (l, r)
    }

    /// A stream taken apart as it comes, in blocks of any length, is the
    /// same as the whole file taken apart (`separate_all`) wherever no
    /// segment still to come adds to it.
    #[test]
    fn a_stream_taken_apart_as_it_comes_is_the_file_taken_apart() {
        let n = SEG * 4 + 12_345;
        let (l, r) = signal(n);
        let want = separate_all(&mut Fake { calls: 0 }, &l, &r, &mut |_| {}, &|| false).unwrap();
        let mut sep = Sep::new(0);
        let mut net = Fake { calls: 0 };
        let mut end = 0;
        let mut step = 1usize;
        while end < n {
            end = (end + step * 104_729 % 300_000 + 1).min(n);
            step += 1;
            while let Some(k) = sep.pick(end, 0, 0) {
                let a = sep.start(k);
                let out = net.separate(&l[a..a + SEG], &r[a..a + SEG]).unwrap();
                sep.add(k, &out);
            }
        }
        let fin = sep.final_end();
        assert!(fin >= SEG * 3, "{fin}");
        assert_eq!(sep.skipped, 0);
        let mut got = vec![0f32; fin];
        for sc in 0..SOURCES * 2 {
            sep.read(sc, 0, &mut got);
            let err = got.iter().zip(&want[sc][..fin]).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(err < 1e-6, "source·channel {sc}: {err}");
        }
        assert!(sep.covered(0, sep.prov_end()));
    }

    /// The same with the network itself (the pack in `AURA_SPATIAL_PACK_DIR`,
    /// a track in `AURA_SPATIAL_FILE`; its first 30 s): the stream's sources
    /// are the file's wherever no segment still to come adds to them.
    ///   cargo test --profile fast stream_through_the_network -- --ignored --nocapture
    #[test]
    #[ignore]
    fn a_stream_through_the_network_is_the_file_through_it() {
        let (Some(pack), Some(file)) = (std::env::var_os("AURA_SPATIAL_PACK_DIR"), std::env::var_os("AURA_SPATIAL_FILE")) else { return };
        let (mut l, mut r) = super::super::super::decode::decode_44k(std::path::Path::new(&file), &|| false).expect("decode");
        let n = l.len().min(44_100 * 30);
        l.truncate(n);
        r.truncate(n);
        let mut net = super::super::super::core::Core::open(std::path::Path::new(&pack)).expect("the network");
        let want = separate_all(&mut net, &l, &r, &mut |_| {}, &|| false).unwrap();
        let mut sep = Sep::new(0);
        let mut end = 0;
        while end < n {
            end = (end + 44_100 / 4).min(n);
            while let Some(k) = sep.pick(end, 0, 0) {
                let a = sep.start(k);
                let out = Separate::separate(&mut net, &l[a..a + SEG], &r[a..a + SEG]).unwrap();
                sep.add(k, &out);
            }
        }
        let fin = sep.final_end();
        let mut got = vec![0f32; fin];
        let mut worst = 0f32;
        for sc in 0..SOURCES * 2 {
            sep.read(sc, 0, &mut got);
            worst = got.iter().zip(&want[sc][..fin]).fold(worst, |m, (a, b)| m.max((a - b).abs()));
        }
        eprintln!("gpu {}: {:.1} s compared, worst difference {worst:e}", net.gpu, fin as f64 / 44_100.0);
        assert!(worst < 1e-3, "{worst}");
    }

    /// Behind: the segments whose part would be heard before they are ready
    /// are skipped, the newest one ahead goes; the frames skipped have
    /// nothing (the mix stands in for them).
    #[test]
    fn behind_the_heard_place_old_segments_are_skipped() {
        let mut sep = Sep::new(0);
        let net_out = vec![0.5f32; SOURCES * 2 * SEG];
        // five segments in, the listener already at the fourth's own part
        let mix_end = sep.start(4) + SEG;
        let heard = sep.start(3) + STRIDE / 2;
        let k = sep.pick(mix_end, heard, 44_100).unwrap();
        assert_eq!(k, 3, "the first whose own part is still ahead");
        assert_eq!(sep.skipped, 3);
        sep.add(k, &net_out);
        assert!(!sep.covered(sep.start(1), sep.start(2)), "skipped frames have no sources");
        assert!(sep.covered(sep.start(3), sep.start(3) + SEG));
        // the next one, in time
        assert_eq!(sep.pick(mix_end, heard, 44_100), Some(4));
        sep.add(4, &net_out);
        // far behind: nothing in time — none goes, all are passed
        let mut late = Sep::new(0);
        assert_eq!(late.pick(late.start(2) + SEG, late.start(2) + SEG, 0), None);
        assert_eq!((late.skipped, late.final_end()), (3, late.start(3)));
    }
}
