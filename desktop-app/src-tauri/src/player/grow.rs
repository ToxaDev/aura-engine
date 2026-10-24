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
//!
//! Readers take no lock: a start's first block reads hundreds of blocks of
//! it side by side (`PolyStream::prime`, a filter's look-ahead), and behind
//! one mutex those reads and the producer's pushes went one at a time
//! (30M ×4: 300 ms where the reads themselves take 15). The buffers are the
//! whole track's from the start; the producer writes the frames past the
//! published count and then publishes them, and a reader copies only frames
//! below it — frames no one writes again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use super::convolver::SourceBuf;

pub struct GrowSource {
    rate: u32,
    total: usize,
    /// The buffers' frames, `total` each: `bufs`' heap memory, which stays
    /// where it is until the source is dropped (`whole` takes the vectors,
    /// not their frames).
    l: *mut f64,
    r: *mut f64,
    /// Frames in so far: every frame below it is written and final.
    len: AtomicUsize,
    st: Mutex<Grow>,
    cv: Condvar,
    /// The whole track, once every frame is in.
    whole: OnceLock<Arc<SourceBuf>>,
}

// The frames behind `l` and `r` are written by the producer under `st`, past
// `len` only, and read below `len` only (Acquire after its Release).
unsafe impl Send for GrowSource {}
unsafe impl Sync for GrowSource {}

struct Grow {
    /// The track's buffers until the whole track is in (then `whole` has
    /// them).
    bufs: Option<(Vec<f64>, Vec<f64>)>,
    /// No more frames are coming: the whole track is in, or the producer
    /// stopped short of it.
    done: bool,
}

/// `ch`'s frames from `from` into `out`, zero outside `x`. True when any is
/// not zero (the convolver skips the transform of a silent block).
fn copy_out(x: &[f64], from: i64, out: &mut [f64]) -> bool {
    let n = out.len() as i64;
    // `out[a..b]` is `x[from + a .. from + b]`; the rest is outside `x`.
    // Saturating: a `from` far out on either side is all zeros, not an overflow.
    let a = from.saturating_neg().clamp(0, n) as usize;
    let b = (x.len() as i64).saturating_sub(from).clamp(a as i64, n) as usize;
    out[..a].fill(0.0);
    if b > a {
        let s = (from + a as i64) as usize;
        out[a..b].copy_from_slice(&x[s..s + (b - a)]);
    }
    out[b..].fill(0.0);
    out[a..b].iter().any(|v| *v != 0.0)
}

impl GrowSource {
    /// An empty source for a track of `total` frames at `rate`.
    pub fn new(rate: u32, total: usize) -> GrowSource {
        // Zeroed, so untouched pages are not made until frames come.
        let (mut l, mut r) = (vec![0.0f64; total], vec![0.0f64; total]);
        GrowSource {
            rate,
            total,
            l: l.as_mut_ptr(),
            r: r.as_mut_ptr(),
            len: AtomicUsize::new(0),
            st: Mutex::new(Grow { bufs: Some((l, r)), done: false }),
            cv: Condvar::new(),
            whole: OnceLock::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Grow> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Channel `ch`'s frames that are in: final, never written again.
    fn frames_in(&self, ch: usize) -> &[f64] {
        let n = self.len.load(Ordering::Acquire);
        // SAFETY: the frames below `len` were written before it was
        // published (Release in `push`) and are not written again; the
        // buffer holds `total` ≥ `len` frames and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(if ch == 0 { self.l } else { self.r }, n) }
    }

    /// The track's length, frames.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Frames in so far.
    pub fn ready(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// No more frames are coming.
    pub fn is_done(&self) -> bool {
        self.whole.get().is_some() || self.lock().done
    }

    /// The producer: the next frames of the track, in order. Frames past the
    /// track's length are dropped.
    pub fn push(&self, l: &[f64], r: &[f64]) {
        {
            let st = self.lock();
            if st.done {
                return;
            }
            let at = self.len.load(Ordering::Relaxed);
            let n = l.len().min(r.len()).min(self.total - at);
            // SAFETY: frames `at .. at + n` are inside the buffers and past
            // `len`: no reader looks at them yet, and pushes are one at a
            // time (under `st`).
            unsafe {
                std::ptr::copy_nonoverlapping(l.as_ptr(), self.l.add(at), n);
                std::ptr::copy_nonoverlapping(r.as_ptr(), self.r.add(at), n);
            }
            self.len.store(at + n, Ordering::Release);
            drop(st);
        }
        self.cv.notify_all();
    }

    /// The producer is through: every reader waiting is let go. When the
    /// whole track is in, it becomes the `SourceBuf` `complete` hands out.
    pub fn finish(&self) {
        {
            let mut st = self.lock();
            st.done = true;
            if self.len.load(Ordering::Acquire) == self.total && self.whole.get().is_none() {
                if let Some((l, r)) = st.bufs.take() {
                    let _ = self.whole.set(Arc::new(SourceBuf { l, r, rate: self.rate }));
                }
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
        let upto = upto.clamp(0, self.total as i64) as usize;
        if self.len.load(Ordering::Acquire) >= upto {
            return;
        }
        let mut st = self.lock();
        while self.len.load(Ordering::Acquire) < upto && !st.done {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Channel `ch`'s frames from `from` into `out`; frames not in yet, and
    /// those outside the track, read as zero. True when any is not zero.
    pub fn read(&self, ch: usize, from: i64, out: &mut [f64]) -> bool {
        copy_out(self.frames_in(ch), from, out)
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

    /// The copy a read makes is the frame-by-frame one it replaced: zeros
    /// outside the frames, and "any" over what it copied.
    #[test]
    fn a_read_copies_what_the_frame_by_frame_copy_did() {
        fn old(x: &[f64], from: i64, out: &mut [f64]) -> bool {
            let mut any = false;
            for (i, o) in out.iter_mut().enumerate() {
                let k = from + i as i64;
                let v = if k >= 0 && (k as usize) < x.len() { x[k as usize] } else { 0.0 };
                any |= v != 0.0;
                *o = v;
            }
            any
        }
        let mut x: Vec<f64> = ramp(37, 0.5);
        x[3] = -0.0;
        x[10] = 0.0;
        for len in [0usize, 1, 5, 37] {
            for from in [-50i64, -37, -5, -1, 0, 1, 3, 4, 10, 11, 30, 36, 37, 40] {
                for n in [0usize, 1, 2, 7, 37, 100] {
                    let (mut a, mut b) = (vec![9.0; n], vec![7.0; n]);
                    let any_a = copy_out(&x[..len], from, &mut a);
                    let any_b = old(&x[..len], from, &mut b);
                    assert_eq!(any_a, any_b, "len {len} from {from} n {n}");
                    assert!(a.iter().zip(&b).all(|(p, q)| p.to_bits() == q.to_bits()), "len {len} from {from} n {n}");
                }
            }
        }
        // A block of silence (and of -0.0) is silent.
        let z = vec![0.0, -0.0, 0.0];
        assert!(!copy_out(&z, 0, &mut [1.0; 3]));
        // A place far out on either side reads zeros, no overflow.
        for from in [i64::MIN, i64::MIN + 1, -(1i64 << 62), 1i64 << 62, i64::MAX] {
            let mut a = vec![9.0; 5];
            assert!(!copy_out(&x, from, &mut a), "from {from}");
            assert!(a.iter().all(|v| v.to_bits() == 0.0f64.to_bits()), "from {from}");
        }
    }

    /// What a start pays for a source (two buffers of 4 min at 44.1k and at
    /// 192k), what the producer then pays to write every frame, and the
    /// ways of getting the memory beside it: zeroed through this binary's
    /// allocator, zeroed and plain from the system's.
    ///     cargo test --profile fast --bins grow_new_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn grow_new_cost() {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::time::Instant;
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let line = |what: &str, mut t: Vec<f64>| {
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            format!("{what} {:.3} ({:.3}..{:.3})", t[t.len() / 2], t[0], t[t.len() - 1])
        };
        let piece = vec![0.25f64; 1 << 16];
        for (rate, secs) in [(44_100u32, 240usize), (192_000, 240)] {
            let n = secs * rate as usize;
            let lay = Layout::array::<f64>(n).unwrap();
            let mut t: [Vec<f64>; 6] = Default::default();
            for _ in 0..7 {
                let t0 = Instant::now();
                let g = GrowSource::new(rate, n);
                t[0].push(ms(t0));
                let t0 = Instant::now();
                let mut at = 0;
                while at < n {
                    g.push(&piece, &piece);
                    at += piece.len();
                }
                g.finish();
                t[1].push(ms(t0));
                assert!(g.complete().is_some());
                let t0 = Instant::now();
                drop(g);
                t[2].push(ms(t0));
                let t0 = Instant::now();
                let v = (vec![0.0f64; n], vec![0.0f64; n]);
                t[3].push(ms(t0));
                drop(v);
                // SAFETY: allocated and freed with the same layout, never read.
                unsafe {
                    let t0 = Instant::now();
                    let (a, b) = (System.alloc_zeroed(lay), System.alloc_zeroed(lay));
                    t[4].push(ms(t0));
                    System.dealloc(a, lay);
                    System.dealloc(b, lay);
                    let t0 = Instant::now();
                    let (a, b) = (System.alloc(lay), System.alloc(lay));
                    t[5].push(ms(t0));
                    System.dealloc(a, lay);
                    System.dealloc(b, lay);
                }
            }
            let [a, b, c, d, e, f] = t;
            println!(
                "[grow] {rate} {secs} s, ms: {} | {} | {} | {} | {} | {}",
                line("new", a),
                line("fill", b),
                line("drop", c),
                line("vec0x2", d),
                line("sys-zeroed", e),
                line("sys-plain", f)
            );
        }
    }

    /// Readers side by side with the producer, as a start's first block
    /// reads its look-ahead: what a reader finds below what it waited for
    /// is what was pushed, frame for frame; above it, a pushed frame or zero.
    #[test]
    fn readers_side_by_side_with_the_producer_read_what_it_pushed() {
        let n = 400_000;
        let g = Arc::new(GrowSource::new(44_100, n));
        let (l, r) = (Arc::new(ramp(n, 1e-6)), Arc::new(ramp(n, -1e-6)));
        let producer = {
            let (g, l, r) = (g.clone(), l.clone(), r.clone());
            std::thread::spawn(move || {
                let mut at = 0;
                let mut k = 1usize;
                while at < n {
                    // Pieces of uneven sizes.
                    k = (k * 7919 + 13) % 6_000 + 1;
                    let e = (at + k).min(n);
                    g.push(&l[at..e], &r[at..e]);
                    at = e;
                }
                g.finish();
            })
        };
        let readers: Vec<_> = (0..8u64)
            .map(|seed| {
                let (g, l, r) = (g.clone(), l.clone(), r.clone());
                std::thread::spawn(move || {
                    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    let mut out = vec![0.0; 4_096];
                    let mut upto = 0i64;
                    while upto < n as i64 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        upto = (upto + (x % 9_000) as i64).min(n as i64);
                        g.ensure(upto);
                        // A window ending past the frames waited for.
                        let from = upto - 3_000;
                        for ch in 0..2 {
                            let src = if ch == 0 { &l } else { &r };
                            g.read(ch, from, &mut out);
                            for (i, v) in out.iter().enumerate() {
                                let k = from + i as i64;
                                let want = if k >= 0 && (k as usize) < n { src[k as usize] } else { 0.0 };
                                if k < upto {
                                    assert_eq!(v.to_bits(), want.to_bits(), "frame {k} below {upto}");
                                } else {
                                    assert!(*v == 0.0 || v.to_bits() == want.to_bits(), "frame {k}: {v} is neither in nor zero");
                                }
                            }
                        }
                    }
                })
            })
            .collect();
        producer.join().unwrap();
        for h in readers {
            h.join().unwrap();
        }
        let whole = g.complete().expect("the whole track is in");
        assert!(whole.l.iter().zip(l.iter()).all(|(a, b)| a.to_bits() == b.to_bits()));
        assert!(whole.r.iter().zip(r.iter()).all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}
