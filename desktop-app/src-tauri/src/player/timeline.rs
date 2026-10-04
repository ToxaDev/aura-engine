//! The output timeline: rendered audio waiting for the device.
//!
//! A ring of stereo `f64` frames addressed by absolute frame number. The
//! render thread writes ahead of the output thread, which reads behind it.
//!
//! What makes it more than an SPSC queue is `rewrite_from`: the writer may
//! throw away frames the reader has not reached yet and write different ones
//! in their place. That is how a settings change or a seek is heard a fraction
//! of a second after it is made instead of after the whole look-ahead has
//! drained — the renderer starts the new chain a little ahead of the read
//! position, crossfades into it, and everything after that is the new sound.
//!
//! Safety of the rewrite rests on one number, [`GUARD_FRAMES_MS`]: the reader
//! never takes more than one device period per call, and the writer never
//! rewrites closer than the guard to the read position. The frames the reader
//! is copying and the frames the writer is replacing therefore never overlap.
//!
//! Frames are pre-volume and pre-dither. The output thread applies the volume
//! and the dither, so the volume answers at once rather than 3–4 s later.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Minimum distance, in milliseconds of output, between the read position and
/// the first frame a rewrite may touch. Comfortably more than any exclusive-
/// mode device period (3–20 ms) plus the time the writer spends copying.
pub const GUARD_FRAMES_MS: u64 = 250;

// analytics: splice log — records each rewrite_from call with source-context
// populated by render.rs via record_splice.
/// A single splice event recorded when `rewrite_from` is called.
/// render.rs populates all fields immediately after calling `rewrite_from`.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // the splice log is read by the tests
pub struct SpliceEntry {
    /// The output-timeline frame at which the splice begins.
    pub splice_frame: u64,
    /// Source index in the OLD chain at `splice_frame`.
    pub from_src_index: u64,
    /// Source index in the NEW chain at `splice_frame`.
    pub to_src_index: u64,
    /// Track id of the old chain (0 if unknown).
    pub track_from: u64,
    /// Track id of the new chain (0 if unknown).
    pub track_to: u64,
}

/// A running splice log maintained inside the Timeline.
/// analytics: the counter and last entry can be read by live.rs at any time.
#[derive(Default)]
pub struct SpliceLog {
    /// Monotonically increasing count of splice events recorded so far.
    pub counter: u64,
    /// The most recent splice entry, or `None` before the first splice.
    pub last: Option<SpliceEntry>,
}

/// The next timeline's number (see `Timeline::id`).
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Timeline {
    /// This timeline's own number, never given to another.
    id: u64,
    /// Interleaved L, R. Length = 2 × `cap`.
    buf: Box<[UnsafeCell<f64>]>,
    /// Capacity in frames. A power of two, so the index is a mask.
    cap: u64,
    mask: u64,
    /// Output sample rate this timeline was created for.
    rate: u32,
    /// Next frame the reader will consume.
    read: AtomicU64,
    /// One past the last valid frame.
    write: AtomicU64,
    /// Frames the output thread had to fill with silence.
    underruns: AtomicU64,
    // analytics: splice log — written by render.rs, read by analytics
    splice_log: Mutex<SpliceLog>,
    /// Incremented atomically whenever the splice_log is updated so readers
    /// can check cheaply whether anything new has arrived.
    // analytics: splice counter — fast change-detection for live.rs
    pub splice_gen: AtomicU32,
}

// One writer (render thread) and one reader (output thread). The frames each
// touches are disjoint by construction; see the module comment.
unsafe impl Sync for Timeline {}
unsafe impl Send for Timeline {}

impl Timeline {
    /// A timeline holding at least `seconds` of audio at `rate`.
    pub fn new(rate: u32, seconds: f64) -> Self {
        let want = ((rate as f64) * seconds).ceil().max(1024.0) as u64;
        let cap = want.next_power_of_two();
        let buf: Vec<UnsafeCell<f64>> = (0..cap * 2).map(|_| UnsafeCell::new(0.0)).collect();
        Timeline {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            buf: buf.into_boxed_slice(),
            cap,
            mask: cap - 1,
            rate,
            read: AtomicU64::new(0),
            write: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            // analytics: splice log initialised empty
            splice_log: Mutex::new(SpliceLog::default()),
            splice_gen: AtomicU32::new(0),
        }
    }

    /// What tells this timeline from any other, the ones before it included:
    /// a new stream's timeline may take the memory of the one it replaces,
    /// so its address does not (the scenes' spans name their ring by this).
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn capacity_frames(&self) -> u64 {
        self.cap
    }

    /// Guard distance in frames at this timeline's rate.
    pub fn guard_frames(&self) -> u64 {
        self.rate as u64 * GUARD_FRAMES_MS / 1000
    }

    pub fn read_pos(&self) -> u64 {
        self.read.load(Ordering::Acquire)
    }

    pub fn write_pos(&self) -> u64 {
        self.write.load(Ordering::Acquire)
    }

    /// Frames rendered and not yet played.
    pub fn buffered_frames(&self) -> u64 {
        self.write_pos().saturating_sub(self.read_pos())
    }

    /// Frames the writer may append without overrunning the reader.
    pub fn free_frames(&self) -> u64 {
        self.cap - self.buffered_frames()
    }

    pub fn underrun_frames(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    #[inline]
    fn slot(&self, frame: u64, ch: usize) -> *mut f64 {
        let i = ((frame & self.mask) as usize) * 2 + ch;
        self.buf[i].get()
    }

    // ── Writer side (render thread only) ────────────────────────────────

    /// Append frames at the write position. Returns how many were written —
    /// fewer than asked when the ring is full.
    pub fn append(&self, l: &[f64], r: &[f64]) -> usize {
        debug_assert_eq!(l.len(), r.len());
        let w = self.write.load(Ordering::Relaxed);
        let free = self.free_frames() as usize;
        let n = l.len().min(free);
        for i in 0..n {
            let f = w + i as u64;
            unsafe {
                *self.slot(f, 0) = l[i];
                *self.slot(f, 1) = r[i];
            }
        }
        self.write.store(w + n as u64, Ordering::Release);
        n
    }

    /// The earliest frame a rewrite may start at right now.
    pub fn earliest_rewrite(&self) -> u64 {
        self.read_pos() + self.guard_frames()
    }

    /// Copy frames the writer wrote earlier (and the reader has not reached)
    /// — the old side of a crossfade. `start..start+len` must lie inside
    /// `earliest_rewrite()..write_pos()`; frames outside it read as silence.
    pub fn peek(&self, start: u64, out_l: &mut [f64], out_r: &mut [f64]) {
        let w = self.write_pos();
        for i in 0..out_l.len() {
            let f = start + i as u64;
            if f < w && f + self.cap > w {
                unsafe {
                    out_l[i] = *self.slot(f, 0);
                    out_r[i] = *self.slot(f, 1);
                }
            } else {
                out_l[i] = 0.0;
                out_r[i] = 0.0;
            }
        }
    }

    /// Discard every frame from `start` on, so the next `append` writes at
    /// `start`. Refused (returns the earliest allowed frame) when `start` is
    /// too close to the reader; the caller then picks a later start.
    pub fn rewrite_from(&self, start: u64) -> Result<(), u64> {
        let earliest = self.earliest_rewrite();
        if start < earliest {
            return Err(earliest);
        }
        let w = self.write.load(Ordering::Relaxed);
        // Rewriting beyond the current end is just appending after a gap of
        // silence; not something the renderer asks for.
        let start = start.min(w);
        self.write.store(start, Ordering::Release);
        Ok(())
    }

    // analytics: record_splice — called by render.rs after every rewrite_from
    /// Record a splice event in the analytics splice log.
    ///
    /// Called by `render.rs` immediately after `rewrite_from` succeeds.
    /// The log retains only the most recent entry plus a monotonic counter;
    /// `live.rs` polls `splice_gen` cheaply and reads the full entry only
    /// on change.
    ///
    /// # Parameters
    /// - `splice_frame`   — same frame passed to `rewrite_from`.
    /// - `from_src_index` — source index in the old chain at `splice_frame`.
    /// - `to_src_index`   — source index in the new chain at `splice_frame`.
    /// - `track_from`     — track id of the old chain (0 if unknown).
    /// - `track_to`       — track id of the new chain (0 if unknown).
    // analytics: splice log entry — matches SPEC.md ERRATA E10 and NEXT.md §3
    pub fn record_splice(
        &self,
        splice_frame:   u64,
        from_src_index: u64,
        to_src_index:   u64,
        track_from:     u64,
        track_to:       u64,
    ) {
        let entry = SpliceEntry {
            splice_frame,
            from_src_index,
            to_src_index,
            track_from,
            track_to,
        };
        if let Ok(mut log) = self.splice_log.lock() {
            log.counter += 1;
            log.last = Some(entry);
        }
        // Increment generation AFTER the mutex write so readers always see a
        // consistent entry when they recheck after observing the gen change.
        self.splice_gen.fetch_add(1, Ordering::Release);
    }

    /// Read the current splice log.  Returns a snapshot; the caller must
    /// tolerate the data being slightly stale (written on the render thread,
    /// read on the analytics thread).
    // analytics: splice log reader — for live.rs and tests
    pub fn read_splice_log(&self) -> Option<(u64, SpliceEntry)> {
        let log = self.splice_log.lock().ok()?;
        let entry = log.last.clone()?;
        Some((log.counter, entry))
    }

    /// Teleport the read position to `frame` without copying any audio.
    /// Only safe to call from the output thread while the output is held
    /// (hold=true) — the render thread does not read `read`, so the write is
    /// race-free in that context. Used by the jump protocol so a track switch
    /// or seek skips the old audio that the render thread wrote before the jump
    /// landed.
    pub fn jump_to(&self, frame: u64) {
        self.read.store(frame, Ordering::Release);
    }

    // ── Reader side (output thread only) ────────────────────────────────

    /// Copy up to `max_frames` frames into `out` (interleaved L, R) and
    /// advance the read position. Returns the number of frames copied; the
    /// caller fills the rest of its period with silence and reports it via
    /// [`note_underrun`](Self::note_underrun).
    pub fn read_into(&self, out: &mut [f64], max_frames: usize) -> usize {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        let avail = w.saturating_sub(r) as usize;
        let n = avail.min(max_frames).min(out.len() / 2);
        for i in 0..n {
            let f = r + i as u64;
            unsafe {
                out[2 * i] = *self.slot(f, 0);
                out[2 * i + 1] = *self.slot(f, 1);
            }
        }
        self.read.store(r + n as u64, Ordering::Release);
        n
    }

    pub fn note_underrun(&self, frames: usize) {
        self.underruns.fetch_add(frames as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_in_order() {
        let t = Timeline::new(48_000, 1.0);
        let l: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        let r: Vec<f64> = (0..1000).map(|i| -(i as f64)).collect();
        assert_eq!(t.append(&l, &r), 1000);
        let mut out = vec![0.0; 2 * 600];
        assert_eq!(t.read_into(&mut out, 600), 600);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2 * 599], 599.0);
        assert_eq!(out[2 * 599 + 1], -599.0);
        assert_eq!(t.read_pos(), 600);
        assert_eq!(t.buffered_frames(), 400);
    }

    #[test]
    fn rewrite_respects_guard_and_replaces_future() {
        let t = Timeline::new(48_000, 2.0);
        let n = 48_000;
        let ones = vec![1.0; n];
        t.append(&ones, &ones);
        let guard = t.guard_frames();
        assert!(t.rewrite_from(guard - 1).is_err());
        t.rewrite_from(guard + 100).unwrap();
        assert_eq!(t.write_pos(), guard + 100);
        let twos = vec![2.0; 1000];
        t.append(&twos, &twos);
        let mut out = vec![0.0; 2 * (guard as usize + 1100)];
        let got = t.read_into(&mut out, guard as usize + 1100);
        assert_eq!(got, guard as usize + 1100);
        assert_eq!(out[2 * (guard as usize + 99)], 1.0);
        assert_eq!(out[2 * (guard as usize + 100)], 2.0);
    }

    /// The scenes tell a new stream by its timeline's number: one dropped and
    /// a new one made in its place (often at the same address) differ.
    #[test]
    fn a_new_timeline_is_never_named_as_the_one_before() {
        let a = std::sync::Arc::new(Timeline::new(48_000, 1.0));
        let (a_id, a_at) = (a.id(), std::sync::Arc::as_ptr(&a) as usize);
        drop(a);
        let b = std::sync::Arc::new(Timeline::new(48_000, 1.0));
        assert_ne!(b.id(), a_id, "the same number for another timeline (address {} then {})", a_at, std::sync::Arc::as_ptr(&b) as usize);
        let c = Timeline::new(44_100, 1.0);
        assert!(c.id() > b.id());
    }

    #[test]
    fn append_stops_when_full() {
        let t = Timeline::new(1024, 1.0);
        let cap = t.capacity_frames() as usize;
        let v = vec![0.5; cap + 10];
        assert_eq!(t.append(&v, &v), cap);
        assert_eq!(t.free_frames(), 0);
    }

    // ── Splice log ──────────────────────────────────────────────────────────

    #[test]
    fn splice_log_starts_empty() {
        let t = Timeline::new(44_100, 1.0);
        assert!(t.read_splice_log().is_none());
        assert_eq!(t.splice_gen.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn splice_log_records_entry() {
        let t = Timeline::new(44_100, 1.0);
        t.record_splice(1000, 5, 10, 1, 2);
        let entry = t.read_splice_log().expect("should have entry");
        assert_eq!(entry.0, 1); // counter = 1
        let e = &entry.1;
        assert_eq!(e.splice_frame,    1000);
        assert_eq!(e.from_src_index,  5);
        assert_eq!(e.to_src_index,    10);
        assert_eq!(e.track_from,      1);
        assert_eq!(e.track_to,        2);
    }

    #[test]
    fn splice_log_counter_increments() {
        let t = Timeline::new(44_100, 1.0);
        t.record_splice(100, 0, 0, 1, 1);
        t.record_splice(200, 0, 0, 1, 1);
        t.record_splice(300, 0, 0, 1, 1);
        let (counter, _) = t.read_splice_log().unwrap();
        assert_eq!(counter, 3);
        assert_eq!(t.splice_gen.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn splice_log_keeps_last_entry() {
        let t = Timeline::new(44_100, 1.0);
        t.record_splice(100, 10, 20, 1, 2); // first splice
        t.record_splice(500, 30, 40, 2, 3); // second splice
        let (_, e) = t.read_splice_log().unwrap();
        // Must reflect the second (most recent) splice
        assert_eq!(e.splice_frame,   500);
        assert_eq!(e.from_src_index, 30);
        assert_eq!(e.to_src_index,   40);
        assert_eq!(e.track_from,     2);
        assert_eq!(e.track_to,       3);
    }
}
