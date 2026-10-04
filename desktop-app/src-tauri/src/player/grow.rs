//! A file's source while its source stages run: what they have made so far,
//! read by absolute frame index the way a convolver reads a decoded track.
//!
//! Instant start plays a track before the source stages have been over the
//! whole of it (`source_stages`): a producer thread pushes their output here
//! many times faster than it plays, and a reader that reaches past it waits
//! for the frames (`ensure`). The track's length is known from the start, so
//! a reader past the end gets silence at once, as from a file — and when the
//! whole track has been pushed, the frames become an ordinary `SourceBuf`
//! without a copy (`complete`).

use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use super::convolver::SourceBuf;

pub struct GrowSource {
    rate: u32,
    total: usize,
    st: Mutex<Grow>,
    cv: Condvar,
    /// The whole track, once every frame is in.
    whole: OnceLock<Arc<SourceBuf>>,
}

struct Grow {
    l: Vec<f64>,
    r: Vec<f64>,
    /// No more frames are coming: the whole track is in, or the producer
    /// stopped short of it.
    done: bool,
}

/// `ch`'s frames from `from` into `out`, zero outside `x`. True when any is
/// not zero (the convolver skips the transform of a silent block).
fn copy_out(x: &[f64], from: i64, out: &mut [f64]) -> bool {
    let mut any = false;
    for (i, o) in out.iter_mut().enumerate() {
        let k = from + i as i64;
        let v = if k >= 0 && (k as usize) < x.len() { x[k as usize] } else { 0.0 };
        any |= v != 0.0;
        *o = v;
    }
    any
}

impl GrowSource {
    /// An empty source for a track of `total` frames at `rate`.
    pub fn new(rate: u32, total: usize) -> GrowSource {
        GrowSource {
            rate,
            total,
            st: Mutex::new(Grow { l: Vec::with_capacity(total), r: Vec::with_capacity(total), done: false }),
            cv: Condvar::new(),
            whole: OnceLock::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Grow> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The track's length, frames.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Frames in so far.
    #[cfg(test)]
    pub fn ready(&self) -> usize {
        if self.whole.get().is_some() {
            return self.total;
        }
        let st = self.lock();
        // Handed over since the look above.
        if self.whole.get().is_some() {
            return self.total;
        }
        st.l.len()
    }

    /// No more frames are coming.
    pub fn is_done(&self) -> bool {
        self.whole.get().is_some() || self.lock().done
    }

    /// The producer: the next frames of the track, in order. Frames past the
    /// track's length are dropped.
    pub fn push(&self, l: &[f64], r: &[f64]) {
        {
            let mut st = self.lock();
            if st.done {
                return;
            }
            let room = self.total - st.l.len();
            let n = l.len().min(r.len()).min(room);
            st.l.extend_from_slice(&l[..n]);
            st.r.extend_from_slice(&r[..n]);
        }
        self.cv.notify_all();
    }

    /// The producer is through: every reader waiting is let go. When the
    /// whole track is in, it becomes the `SourceBuf` `complete` hands out.
    pub fn finish(&self) {
        {
            let mut st = self.lock();
            st.done = true;
            if st.l.len() == self.total && self.whole.get().is_none() {
                let l = std::mem::take(&mut st.l);
                let r = std::mem::take(&mut st.r);
                let _ = self.whole.set(Arc::new(SourceBuf { l, r, rate: self.rate }));
            }
        }
        self.cv.notify_all();
    }

    /// The whole track, once it is all in.
    pub fn complete(&self) -> Option<Arc<SourceBuf>> {
        self.whole.get().cloned()
    }

    /// Wait until no more frames are coming (`is_done`), looking at `cancel`
    /// every 50 ms: false when it went up first.
    pub fn wait_done(&self, cancel: &std::sync::atomic::AtomicBool) -> bool {
        use std::sync::atomic::Ordering;
        if self.whole.get().is_some() {
            return true;
        }
        let mut st = self.lock();
        while !st.done {
            if cancel.load(Ordering::Acquire) {
                return false;
            }
            st = self
                .cv
                .wait_timeout(st, std::time::Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Make every frame below `upto` readable: waits for the producer (a
    /// track's frames come many times faster than they play). Past the end,
    /// or once the producer has stopped, there is nothing to wait for.
    pub fn ensure(&self, upto: i64) {
        if self.whole.get().is_some() {
            return;
        }
        let upto = upto.clamp(0, self.total as i64) as usize;
        let mut st = self.lock();
        while st.l.len() < upto && !st.done {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Channel `ch`'s frames from `from` into `out`; frames not in yet, and
    /// those outside the track, read as zero. True when any is not zero.
    pub fn read(&self, ch: usize, from: i64, out: &mut [f64]) -> bool {
        if let Some(w) = self.whole.get() {
            return copy_out(if ch == 0 { &w.l } else { &w.r }, from, out);
        }
        let st = self.lock();
        // The whole track may have been handed over since the look above.
        if let Some(w) = self.whole.get() {
            drop(st);
            return copy_out(if ch == 0 { &w.l } else { &w.r }, from, out);
        }
        copy_out(if ch == 0 { &st.l } else { &st.r }, from, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ramp(n: usize, k: f64) -> Vec<f64> {
        (0..n).map(|i| (i as f64 + 1.0) * k).collect()
    }

    #[test]
    fn frames_read_back_by_absolute_index_and_zero_outside() {
        let g = GrowSource::new(44_100, 10);
        g.push(&ramp(4, 1.0), &ramp(4, -1.0));
        let mut out = [9.0; 6];
        assert!(g.read(0, -2, &mut out));
        assert_eq!(out, [0.0, 0.0, 1.0, 2.0, 3.0, 4.0]);
        assert!(g.read(1, 3, &mut out));
        assert_eq!(out, [-4.0, 0.0, 0.0, 0.0, 0.0, 0.0], "frames not in yet read as zero");
        assert!(!g.read(0, 20, &mut out), "past the end: silence");
        assert_eq!(g.ready(), 4);
        // Past the track's length nothing more goes in.
        g.push(&ramp(9, 1.0), &ramp(9, 1.0));
        assert_eq!(g.ready(), 10);
    }

    #[test]
    fn a_reader_waits_for_the_frames_the_producer_pushes() {
        let g = Arc::new(GrowSource::new(44_100, 50_000));
        let w = g.clone();
        let producer = std::thread::spawn(move || {
            let (l, r) = (ramp(50_000, 1e-5), ramp(50_000, -1e-5));
            let mut at = 0;
            while at < l.len() {
                let e = (at + 3_001).min(l.len());
                w.push(&l[at..e], &r[at..e]);
                at = e;
                std::thread::sleep(Duration::from_millis(1));
            }
            w.finish();
        });
        g.ensure(40_000);
        assert!(g.ready() >= 40_000);
        let mut out = vec![0.0; 1_000];
        g.read(0, 39_000, &mut out);
        assert_eq!(out[999], 40_000.0 * 1e-5);
        producer.join().unwrap();
        assert!(g.is_done());
        let whole = g.complete().expect("the whole track is in");
        assert_eq!(whole.len(), 50_000);
        assert_eq!(whole.r[49_999], -50_000.0 * 1e-5);
        // Read on after the hand-over, from the whole track.
        g.ensure(i64::MAX);
        assert!(g.read(1, 49_999, &mut out[..2]));
        assert_eq!(&out[..2], &[-50_000.0 * 1e-5, 0.0]);
    }

    #[test]
    fn a_producer_that_stops_short_lets_its_readers_go() {
        let g = Arc::new(GrowSource::new(44_100, 1_000));
        let w = g.clone();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w.push(&ramp(100, 1.0), &ramp(100, 1.0));
            w.finish();
        });
        g.ensure(1_000);
        producer.join().unwrap();
        assert_eq!(g.ready(), 100);
        assert!(g.complete().is_none(), "not the whole track");
        let mut out = [1.0; 3];
        assert!(!g.read(0, 500, &mut out), "what never came reads as silence");
    }
}
