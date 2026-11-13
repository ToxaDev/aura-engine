//! The EQ window's analyzer tap after the listener's EQ: what the device is
//! about to play (after the fade, the EQ and its clip; before the volume
//! and the dither), mono (L+R)/2, kept by timeline frame so the analyzer
//! reads it at the same frame it reads the render's timeline (pre-EQ).
//!
//! The ring is made on the reader's side (the protocol thread, on the
//! first ask) and handed to the output thread as an `Arc` clone through a
//! change count and a tried lock, as the EQ's design is: the output thread
//! never allocates nor frees one (a ring let go waits in `retired` until the
//! output thread has dropped its clone). It writes only while a lease the
//! reader renews on every ask runs (`LEASE_S`): the EQ window closed or its
//! spectrum off, a period pays one atomic load.

use std::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The output thread keeps writing this long after the reader's last ask.
pub const LEASE_S: f64 = 2.0;
/// The ring holds about this much (rounded up to a power of two).
const RING_S: f64 = 2.0;

/// Mono f32 frames by timeline frame (`frame & mask`), one run at a time:
/// a jump, a new timeline or a gap starts a new run (`seq` bumped, the old
/// one unreadable).
pub struct PostRing {
    pub rate: u32,
    mask: u64,
    buf: Box<[AtomicU32]>,
    /// The timeline the run is of (its address), 0 none yet.
    timeline: AtomicUsize,
    /// The run's first frame, and the frame after its last.
    from: AtomicU64,
    end: AtomicU64,
    /// Bumped on every new run, before its first sample.
    seq: AtomicU64,
}

impl PostRing {
    pub fn new(rate: u32, frames: usize) -> PostRing {
        let cap = frames.next_power_of_two().max(1024);
        PostRing {
            rate,
            mask: cap as u64 - 1,
            buf: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            timeline: AtomicUsize::new(0),
            from: AtomicU64::new(0),
            end: AtomicU64::new(0),
            seq: AtomicU64::new(0),
        }
    }

    pub fn cap(&self) -> u64 {
        self.mask + 1
    }

    /// Frames a reader keeps away from the writer's head: more than a
    /// period, which the writer may be halfway through.
    fn margin(&self) -> u64 {
        self.cap() / 8
    }

    /// The output thread: frames `from..` of `timeline` (`l`, `r` as long).
    pub fn write(&self, timeline: usize, from: u64, l: &[f64], r: &[f64]) {
        if l.is_empty() {
            return;
        }
        let end = self.end.load(Ordering::Relaxed);
        if timeline != self.timeline.load(Ordering::Relaxed) || from != end {
            self.seq.fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Release);
            self.timeline.store(timeline, Ordering::Relaxed);
            self.from.store(from, Ordering::Relaxed);
        }
        for (i, (a, b)) in l.iter().zip(r).enumerate() {
            let m = ((a + b) * 0.5) as f32;
            self.buf[((from + i as u64) & self.mask) as usize].store(m.to_bits(), Ordering::Relaxed);
        }
        self.end.store(from + l.len() as u64, Ordering::Release);
    }

    /// The frames the ring can give of `timeline` now: `(first, after last)`.
    pub fn span(&self, timeline: usize) -> Option<(u64, u64)> {
        let end = self.end.load(Ordering::Acquire);
        if self.timeline.load(Ordering::Relaxed) != timeline || end == 0 {
            return None;
        }
        let lo = self.from.load(Ordering::Relaxed).max((end + self.margin()).saturating_sub(self.cap()));
        (lo < end).then_some((lo, end))
    }

    /// Frames `start..start + out.len()` of `timeline` into `out`: false if
    /// the ring does not hold all of them whole (another run, overwritten).
    pub fn read(&self, timeline: usize, start: u64, out: &mut [f64]) -> bool {
        let seq = self.seq.load(Ordering::Acquire);
        let Some((lo, end)) = self.span(timeline) else { return false };
        let n = out.len() as u64;
        if start < lo || start + n > end || n > self.cap() {
            return false;
        }
        for (i, o) in out.iter_mut().enumerate() {
            *o = f32::from_bits(self.buf[((start + i as u64) & self.mask) as usize].load(Ordering::Relaxed)) as f64;
        }
        fence(Ordering::Acquire);
        let end2 = self.end.load(Ordering::Relaxed);
        self.seq.load(Ordering::Relaxed) == seq && start + self.cap() >= end2 + self.margin()
    }
}

/// The tap's place in `OutputShared`.
#[derive(Default)]
pub struct PostTap {
    ring: Mutex<Option<Arc<PostRing>>>,
    /// Bumped when `ring` is replaced.
    pub gen: AtomicU64,
    /// Frames the output thread still writes for (the reader's lease).
    lease: AtomicI64,
    /// Rings let go, until the output thread has dropped its clone.
    retired: Mutex<Vec<Arc<PostRing>>>,
}

impl PostTap {
    /// The reader: the ring for `rate` (made if there is none for it), the
    /// lease renewed.
    pub fn ring_for(&self, rate: u32) -> Arc<PostRing> {
        self.lease.store((LEASE_S * rate as f64) as i64, Ordering::Relaxed);
        self.retired.lock().unwrap().retain(|r| Arc::strong_count(r) > 1);
        let mut g = self.ring.lock().unwrap();
        if let Some(r) = g.as_ref().filter(|r| r.rate == rate) {
            return r.clone();
        }
        let r = Arc::new(PostRing::new(rate, (RING_S * rate as f64) as usize));
        if let Some(old) = g.replace(r.clone()) {
            self.retired.lock().unwrap().push(old);
        }
        self.gen.fetch_add(1, Ordering::Release);
        r
    }

    /// The output thread: whether to write `frames` more (the lease spent
    /// by them).
    pub fn leased(&self, frames: usize) -> bool {
        if self.lease.load(Ordering::Relaxed) <= 0 {
            return false;
        }
        self.lease.fetch_sub(frames as i64, Ordering::Relaxed);
        true
    }

    /// The output thread: the current ring if the lock is free (`None`:
    /// busy, try again next period).
    pub fn try_ring(&self) -> Option<Option<Arc<PostRing>>> {
        self.ring.try_lock().ok().map(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(from: u64, n: usize) -> Vec<f64> {
        (0..n).map(|i| ((from + i as u64) % 1000) as f64 / 1000.0).collect()
    }

    #[test]
    fn frames_written_read_back_by_timeline_frame() {
        let r = PostRing::new(48_000, 4096);
        let tl = 0x1000;
        let mut at = 500;
        for _ in 0..20 {
            let x = ramp(at, 300);
            r.write(tl, at, &x, &x);
            at += 300;
        }
        let (lo, end) = r.span(tl).unwrap();
        assert_eq!(end, at);
        assert!(end - lo <= r.cap() - r.margin());
        let mut out = vec![0.0; 1024];
        assert!(r.read(tl, end - 1024, &mut out));
        let want = ramp(end - 1024, 1024);
        assert!(out.iter().zip(&want).all(|(a, b)| (a - b).abs() < 1e-6));
        // Too old (overwritten), not yet written, another timeline.
        assert!(!r.read(tl, lo - 1, &mut out));
        assert!(!r.read(tl, end - 1000, &mut out));
        assert!(!r.read(0x2000, end - 1024, &mut out));
    }

    #[test]
    fn a_jump_or_a_new_timeline_starts_a_new_run() {
        let r = PostRing::new(48_000, 4096);
        let x = vec![0.5; 1000];
        r.write(1, 0, &x, &x);
        r.write(1, 5000, &x, &x);
        assert_eq!(r.span(1), Some((5000, 6000)));
        let mut out = vec![0.0; 100];
        assert!(!r.read(1, 900, &mut out));
        assert!(r.read(1, 5900, &mut out));
        r.write(2, 6000, &x, &x);
        assert_eq!(r.span(1), None);
        assert_eq!(r.span(2), Some((6000, 7000)));
    }

    #[test]
    fn the_lease_runs_out_and_a_new_rate_gets_a_new_ring() {
        let t = PostTap::default();
        assert!(!t.leased(100));
        let a = t.ring_for(48_000);
        let gen = t.gen.load(Ordering::Relaxed);
        assert!(Arc::ptr_eq(&a, &t.ring_for(48_000)));
        assert_eq!(t.gen.load(Ordering::Relaxed), gen);
        let mut n = 0;
        while t.leased(4800) {
            n += 1;
        }
        assert_eq!(n, 20, "2 s of 48k in 100 ms periods");
        let held = t.try_ring().unwrap().unwrap();
        let b = t.ring_for(96_000);
        assert_eq!(b.rate, 96_000);
        assert!(t.gen.load(Ordering::Relaxed) > gen);
        // The old ring waits for the output thread's clone to go.
        assert_eq!(t.retired.lock().unwrap().len(), 1);
        drop((a, held));
        t.ring_for(96_000);
        assert!(t.retired.lock().unwrap().is_empty());
    }
}
