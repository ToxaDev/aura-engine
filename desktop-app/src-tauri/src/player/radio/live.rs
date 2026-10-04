//! A live source: stream audio that arrives while it plays.
//!
//! The convolver reads its source a block at a time, by absolute frame index
//! (`convolver::Input`). A file has every frame from the start; a stream
//! grows as the network delivers it. `LiveSource` holds what has arrived —
//! the source stages' output as they computed it, in f64: nothing is rounded
//! on the way from the decoder to the filter — keeps a bounded history
//! behind the reader (enough to prime another chain at the current
//! position), and decides what happens when the reader gets ahead of the
//! network:
//!
//! * the reader waits up to `STARVE_WAIT` for the frames it needs: the render
//!   thread works more than a second ahead of the device, so a short wait is
//!   never heard;
//! * past that the stream is *starved*: the frames no block has read yet fade
//!   out, and the reader gets silence, appended to the source — the
//!   convolver's block grid and history stay whole, and the device never runs
//!   dry;
//! * what arrives meanwhile is held until `resume_frames` have gathered, then
//!   goes in after the silence with a fade-in. The stream goes on later by the
//!   length of the gap, as any radio player's does.
//!
//! A connection that breaks mid-stream (`mark_gap`) fades out the same way,
//! and the next one fades in. Everything here happens before the chain: the
//! fades and the silence are part of the source, so the volume, the test
//! mute and the guard after the chain treat them as any other audio.
//!
//! Beside what the source stages handed on, the source keeps the stream as
//! it came before them — the shadow, what BIT-PERFECT plays (`push_raw`,
//! `read_raw`): frame for frame at the same index, the silence, the fades
//! and the cuts made to one made to the other, so either can take over from
//! the place being heard. The shadow keeps a shorter history.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long a reader waits for frames before the stream counts as starved.
pub const STARVE_WAIT: Duration = Duration::from_millis(400);

/// The fade on either side of a gap.
const FADE_MS: f64 = 10.0;

/// The most a starved stream gathers before it goes on.
pub const MAX_RESUME_S: f64 = 30.0;

/// History is dropped in pieces of at least this many frames (a drain moves
/// everything after it).
const TRIM_CHUNK: usize = 1 << 20;

/// The shadow's history behind the reader, seconds: BIT-PERFECT takes over
/// from the place being heard, which the rack's chain has read ahead of by
/// what its slow gain holds (the stream in hand, up to the most a starved
/// stream gathers) and the device's buffer; a live stream's instruments read
/// from a little before it. Kept whatever the chain keeps itself: on a short
/// filter that is little, while a station's burst on connecting puts the
/// reader half a minute ahead of the place heard (TASK-34).
pub const RAW_KEEP_S: f64 = 40.0;

/// The stream's buffers grow by an eighth of what they hold, and at least
/// this many frames, rather than doubling: the history alone is 15M frames
/// (241 MB in f64), and a paused stream gathers up to ten minutes ahead.
const GROW_MIN: usize = 1 << 20;

/// Room for `n` more frames in `v`, an eighth more at a time.
fn room(v: &mut Vec<f64>, n: usize) {
    if v.len() + n > v.capacity() {
        v.reserve_exact(n.max(v.len() / 8).max(GROW_MIN));
    }
}

/// What the source has seen so far.
#[derive(Clone, Debug, Default)]
pub struct LiveStats {
    /// Frames of stream audio appended.
    pub real_frames: u64,
    /// Frames of silence a starved reader was given.
    pub silent_frames: u64,
    /// Times the reader got ahead of the network and was given silence.
    pub starves: u32,
    /// Breaks in the stream itself (a connection lost mid-audio).
    pub gaps: u32,
    /// The longest wait for frames that then came in time, ms.
    pub max_wait_ms: u64,
    /// Frames dropped because the buffer ahead of the reader was full.
    pub dropped_frames: u64,
    /// Frames cut where the stream broke anyway — the oldest a starved
    /// stream gathered beyond what it waits for, the start of a new
    /// connection's burst after a gap — so the delay stays where it was.
    pub cut_frames: u64,
    /// Frames handed on with no shadow frame come for them (`push_raw`):
    /// the shadow holds silence there. None on the decoder's way.
    pub raw_missing: u64,
}

struct State {
    /// Absolute index of `l[0]`.
    base: i64,
    l: Vec<f64>,
    r: Vec<f64>,
    /// The reader's edge: every frame below it has been read by some block.
    want: i64,
    /// Starved: the reader is being given silence until `held_*` is long
    /// enough to go on.
    starved: bool,
    held_l: Vec<f64>,
    held_r: Vec<f64>,
    /// Frames held after a starve before playback goes on (grows).
    resume: usize,
    /// The next frames pushed fade in (after a gap or a starve).
    fade_in_next: bool,
    /// Frames are being dropped: the buffer ahead is full (a long pause).
    dropping: bool,
    /// Where the frames after the last gap begin.
    last_gap: Option<i64>,
    closed: bool,
    stats: LiveStats,
    /// The shadow: the stream as it came before the source stages, frame
    /// for frame with `l`/`r` — `raw_l[0]` is frame `raw_base`, at or past
    /// `base` (a shorter history), and it ends where they end.
    raw_base: i64,
    raw_l: Vec<f64>,
    raw_r: Vec<f64>,
    /// Shadow frames in ahead of their stages' (the stages hold the stream
    /// back): they go in with them.
    raw_wait_l: VecDeque<f64>,
    raw_wait_r: VecDeque<f64>,
    /// Shadow frames held with `held_l`/`held_r` after a starve.
    raw_held_l: Vec<f64>,
    raw_held_r: Vec<f64>,
}

impl State {
    fn end(&self) -> i64 {
        self.base + self.l.len() as i64
    }

    /// Fade out the frames from `from` (absolute) to the end: the last
    /// `fade` of them, or all when fewer are there.
    fn fade_tail(&mut self, from: i64, fade: usize) {
        let end = self.end();
        let start = from.max(end - fade as i64).max(self.base);
        let n = (end - start).max(0) as usize;
        if n == 0 {
            return;
        }
        let at = (start - self.base) as usize;
        for i in 0..n {
            let g = ramp(n - 1 - i, n);
            self.l[at + i] *= g;
            self.r[at + i] *= g;
            // The shadow's same frame, when it still holds it.
            let k = start + i as i64 - self.raw_base;
            if k >= 0 {
                self.raw_l[k as usize] *= g;
                self.raw_r[k as usize] *= g;
            }
        }
    }

    fn fill_silence(&mut self, upto: i64) {
        let n = (upto - self.end()).max(0) as usize;
        if n > 0 {
            room(&mut self.l, n);
            room(&mut self.r, n);
            self.l.resize(self.l.len() + n, 0.0);
            self.r.resize(self.r.len() + n, 0.0);
            room(&mut self.raw_l, n);
            room(&mut self.raw_r, n);
            self.raw_l.resize(self.raw_l.len() + n, 0.0);
            self.raw_r.resize(self.raw_r.len() + n, 0.0);
            self.stats.silent_frames += n as u64;
        }
    }

    /// The shadow's frames for `n` frames the stages hand on: the oldest
    /// waiting, silence for any that never came (`LiveStats::raw_missing`).
    fn take_raw(&mut self, n: usize) -> (Vec<f64>, Vec<f64>) {
        let k = n.min(self.raw_wait_l.len());
        let mut a: Vec<f64> = self.raw_wait_l.drain(..k).collect();
        let mut b: Vec<f64> = self.raw_wait_r.drain(..k).collect();
        if k < n {
            a.resize(n, 0.0);
            b.resize(n, 0.0);
            self.stats.raw_missing += (n - k) as u64;
        }
        (a, b)
    }

    fn append_raw(&mut self, a: &[f64], b: &[f64]) {
        room(&mut self.raw_l, a.len());
        room(&mut self.raw_r, b.len());
        self.raw_l.extend_from_slice(a);
        self.raw_r.extend_from_slice(b);
    }
}

/// Channel frames `src` holds from its `lo`-th on into `out`; frames it does
/// not hold read as zero. True when any frame is non-zero.
fn copy_out(src: &[f64], lo: i64, out: &mut [f64]) -> bool {
    let mut any = false;
    for (i, o) in out.iter_mut().enumerate() {
        let k = lo + i as i64;
        let v = if k >= 0 && (k as usize) < src.len() { src[k as usize] } else { 0.0 };
        any |= v != 0.0;
        *o = v;
    }
    any
}

/// Raised-cosine gain of step `i` of an `n`-step fade-in (0 → 1).
fn ramp(i: usize, n: usize) -> f64 {
    let x = (i as f64 + 0.5) / n.max(1) as f64;
    0.5 - 0.5 * (std::f64::consts::PI * x).cos()
}

/// Stream audio at the source rate, readable by absolute frame index while
/// it grows.
pub struct LiveSource {
    rate: u32,
    /// Frames kept behind the reader's edge; the shadow keeps `raw_keep`
    /// (`RAW_KEEP_S`, whatever the chain keeps).
    keep: usize,
    raw_keep: usize,
    /// Frames held after the first starve before playback goes on; each
    /// starve after it holds half as much again, up to `max_resume`.
    max_resume: usize,
    /// At most this many frames wait ahead of the reader (a paused stream).
    max_ahead: usize,
    fade: usize,
    st: Mutex<State>,
    cv: Condvar,
}

impl LiveSource {
    /// `keep_frames` of history behind the reader; after a starve, playback
    /// goes on once `resume_s` seconds have gathered; at most `max_ahead_s`
    /// seconds wait ahead of the reader.
    pub fn new(rate: u32, keep_frames: usize, resume_s: f64, max_ahead_s: f64) -> LiveSource {
        let fade = ((FADE_MS / 1000.0) * rate as f64).round().max(1.0) as usize;
        LiveSource {
            rate,
            keep: keep_frames,
            raw_keep: (RAW_KEEP_S * rate as f64) as usize,
            max_resume: ((MAX_RESUME_S * rate as f64) as usize).max(((resume_s * rate as f64) as usize).max(fade)),
            max_ahead: (max_ahead_s * rate as f64) as usize,
            fade,
            st: Mutex::new(State {
                base: 0,
                l: Vec::new(),
                r: Vec::new(),
                want: 0,
                starved: false,
                held_l: Vec::new(),
                held_r: Vec::new(),
                resume: ((resume_s * rate as f64) as usize).max(fade),
                fade_in_next: false,
                dropping: false,
                last_gap: None,
                closed: false,
                stats: LiveStats::default(),
                raw_base: 0,
                raw_l: Vec::new(),
                raw_r: Vec::new(),
                raw_wait_l: VecDeque::new(),
                raw_wait_r: VecDeque::new(),
                raw_held_l: Vec::new(),
                raw_held_r: Vec::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The end of the source so far (stream audio and silence), frames.
    pub fn end(&self) -> i64 {
        self.lock().end()
    }

    /// Frames ready ahead of the reader's edge: the stream's buffer. The edge
    /// moves a block at a time, so this saw-tooths by a block.
    pub fn ahead(&self) -> i64 {
        let st = self.lock();
        st.end() - st.want
    }

    pub fn starved(&self) -> bool {
        self.lock().starved
    }

    /// Seconds gathered after a starve before the stream goes on.
    pub fn resume_s(&self) -> f64 {
        self.lock().resume as f64 / self.rate as f64
    }

    pub fn stats(&self) -> LiveStats {
        self.lock().stats.clone()
    }

    /// The stream is over: readers stop waiting and read silence from here.
    pub fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// The stream as it came before the source stages (the writer: the
    /// decoder thread, ahead of the stages' frames for the same place):
    /// the shadow, which goes in frame for frame with what they hand on
    /// (`push`).
    pub fn push_raw(&self, l: &[f64], r: &[f64]) {
        debug_assert_eq!(l.len(), r.len());
        let mut st = self.lock();
        if st.closed {
            return;
        }
        st.raw_wait_l.extend(l.iter().copied());
        st.raw_wait_r.extend(r.iter().copied());
    }

    /// Append frames as the source stages handed them on (the writer: the
    /// decoder thread), the shadow's for them beside them.
    pub fn push(&self, l: &[f64], r: &[f64]) {
        debug_assert_eq!(l.len(), r.len());
        let mut st = self.lock();
        if st.closed || l.is_empty() {
            return;
        }
        let (mut rl, mut rr) = st.take_raw(l.len());
        if st.starved {
            st.held_l.extend_from_slice(l);
            st.held_r.extend_from_slice(r);
            st.raw_held_l.append(&mut rl);
            st.raw_held_r.append(&mut rr);
            if st.held_l.len() >= st.resume {
                let mut hl = std::mem::take(&mut st.held_l);
                let mut hr = std::mem::take(&mut st.held_r);
                let mut sl = std::mem::take(&mut st.raw_held_l);
                let mut sr = std::mem::take(&mut st.raw_held_r);
                // What came in beyond the wait (a new connection's burst):
                // the oldest of it goes, so the stream goes on as far behind
                // the live edge as it waited for.
                let extra = hl.len() - st.resume;
                if extra > 0 {
                    hl.drain(..extra);
                    hr.drain(..extra);
                    sl.drain(..extra);
                    sr.drain(..extra);
                    st.stats.cut_frames += extra as u64;
                }
                let n = self.fade.min(hl.len());
                for i in 0..n {
                    let g = ramp(i, n);
                    hl[i] *= g;
                    hr[i] *= g;
                    sl[i] *= g;
                    sr[i] *= g;
                }
                st.stats.real_frames += hl.len() as u64;
                room(&mut st.l, hl.len());
                room(&mut st.r, hr.len());
                st.l.extend_from_slice(&hl);
                st.r.extend_from_slice(&hr);
                st.append_raw(&sl, &sr);
                st.starved = false;
                st.fade_in_next = false;
                crate::aelog!(
                    "[RADIO] resumed after a starve: {:.2} s held, {:.2} s of silence so far",
                    hl.len() as f64 / self.rate as f64,
                    st.stats.silent_frames as f64 / self.rate as f64
                );
                drop(st);
                self.cv.notify_all();
            }
            return;
        }
        if st.end() - st.want > self.max_ahead as i64 {
            // Full (a long pause): the frames held fade out at their end, and
            // what comes after the frames dropped fades in — no jump from one
            // moment of the stream to another.
            if !st.dropping {
                st.dropping = true;
                let from = st.want;
                st.fade_tail(from, self.fade);
                st.stats.gaps += 1;
            }
            st.stats.dropped_frames += l.len() as u64;
            return;
        }
        if st.dropping {
            st.dropping = false;
            st.fade_in_next = true;
        }
        let at = st.l.len();
        let raw_at = st.raw_l.len();
        room(&mut st.l, l.len());
        room(&mut st.r, r.len());
        st.l.extend_from_slice(l);
        st.r.extend_from_slice(r);
        st.append_raw(&rl, &rr);
        if st.fade_in_next {
            let n = self.fade.min(l.len());
            for i in 0..n {
                let g = ramp(i, n);
                st.l[at + i] *= g;
                st.r[at + i] *= g;
                st.raw_l[raw_at + i] *= g;
                st.raw_r[raw_at + i] *= g;
            }
            st.fade_in_next = false;
        }
        st.stats.real_frames += l.len() as u64;
        self.trim(&mut st);
        drop(st);
        self.cv.notify_all();
    }

    /// The stream broke mid-audio (a connection lost; another follows): the
    /// frames no reader has had yet fade out, and the next ones fade in.
    pub fn mark_gap(&self) {
        let mut st = self.lock();
        let from = st.want;
        st.fade_tail(from, self.fade);
        st.fade_in_next = true;
        st.stats.gaps += 1;
        st.last_gap = Some(st.end());
    }

    /// After the last gap the new connection brought more than the stream
    /// keeps: its oldest `n` frames (the start of its burst) are cut, when
    /// no block has read them yet, and what follows fades in where they
    /// began. Returns where and how many were cut.
    pub fn cut_after_gap(&self, n: usize) -> Option<(i64, usize)> {
        let mut st = self.lock();
        let g = st.last_gap?;
        if st.want > g || st.starved {
            return None;
        }
        let n = n.min(((st.end() - g).max(0) as usize).saturating_sub(self.fade));
        if n == 0 {
            return None;
        }
        let at = (g - st.base) as usize;
        st.l.drain(at..at + n);
        st.r.drain(at..at + n);
        // The shadow holds every frame from the reader on (g is past it).
        let raw_at = (g - st.raw_base) as usize;
        st.raw_l.drain(raw_at..raw_at + n);
        st.raw_r.drain(raw_at..raw_at + n);
        let m = self.fade.min(st.l.len() - at);
        for i in 0..m {
            let g = ramp(i, m);
            st.l[at + i] *= g;
            st.r[at + i] *= g;
            st.raw_l[raw_at + i] *= g;
            st.raw_r[raw_at + i] *= g;
        }
        st.stats.real_frames -= n as u64;
        st.stats.cut_frames += n as u64;
        st.last_gap = None;
        Some((g, n))
    }

    /// Make every frame below `upto` readable (the reader: a convolver about
    /// to transform a block). Waits for the network up to `STARVE_WAIT`;
    /// past that the stream starves and the reader is given silence.
    pub fn ensure(&self, upto: i64) {
        let t0 = Instant::now();
        let deadline = t0 + STARVE_WAIT;
        let mut st = self.lock();
        let mut waited = false;
        loop {
            if st.end() >= upto || st.closed {
                break;
            }
            if st.starved {
                st.fill_silence(upto);
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                let from = st.want;
                st.fade_tail(from, self.fade);
                st.starved = true;
                // A stream that starved before gathers more before it goes
                // on: the network has shown it needs more in hand.
                if st.stats.starves > 0 {
                    st.resume = (st.resume + st.resume / 2).min(self.max_resume);
                }
                st.stats.starves += 1;
                let short = upto - st.end();
                st.fill_silence(upto);
                crate::aelog!(
                    "[RADIO] starved: the reader needs {} frames the network has not delivered ({:.2} s); silence until {:.1} s have gathered",
                    short,
                    short as f64 / self.rate as f64,
                    st.resume as f64 / self.rate as f64
                );
                break;
            }
            waited = true;
            st = self.cv.wait_timeout(st, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        if waited && !st.starved {
            let ms = t0.elapsed().as_millis() as u64;
            st.stats.max_wait_ms = st.stats.max_wait_ms.max(ms);
        }
        if upto > st.want {
            st.want = upto;
        }
        self.trim(&mut st);
    }

    /// Channel `ch`'s frames from `from` into `out`; frames not held (before
    /// the history or past the end) read as zero. True when any frame is
    /// non-zero.
    pub fn read(&self, ch: usize, from: i64, out: &mut [f64]) -> bool {
        let st = self.lock();
        copy_out(if ch == 0 { &st.l } else { &st.r }, from - st.base, out)
    }

    /// `read` on the shadow: the stream as it came before the source
    /// stages, frame for frame with what they handed on.
    pub fn read_raw(&self, ch: usize, from: i64, out: &mut [f64]) -> bool {
        let st = self.lock();
        copy_out(if ch == 0 { &st.raw_l } else { &st.raw_r }, from - st.raw_base, out)
    }

    /// The first frame the shadow still holds.
    pub fn raw_start(&self) -> i64 {
        self.lock().raw_base
    }

    fn trim(&self, st: &mut State) {
        let keep_from = st.want - self.keep as i64;
        let excess = keep_from - st.base;
        if excess > TRIM_CHUNK as i64 {
            let n = (excess as usize).min(st.l.len());
            st.l.drain(..n);
            st.r.drain(..n);
            st.base += n as i64;
        }
        let raw_from = st.want - self.raw_keep as i64;
        let excess = raw_from - st.raw_base;
        if excess > TRIM_CHUNK.min(self.raw_keep.max(1)) as i64 {
            let n = (excess as usize).min(st.raw_l.len());
            st.raw_l.drain(..n);
            st.raw_r.drain(..n);
            st.raw_base += n as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_up(n: usize) -> Vec<f64> {
        (0..n).map(|i| 1.0 + i as f64).collect()
    }

    #[test]
    fn frames_read_back_by_absolute_index_and_zero_outside() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 600.0);
        s.push(&ramp_up(100), &ramp_up(100));
        let mut out = vec![9.0; 10];
        assert!(s.read(0, 95, &mut out));
        assert_eq!(&out[..5], &[96.0, 97.0, 98.0, 99.0, 100.0]);
        assert!(out[5..].iter().all(|&v| v == 0.0));
        assert!(!s.read(1, -10, &mut out));
    }

    /// What the source stages hand on is read back to the bit — values no
    /// f32 holds (a filter's output) as much as any other.
    #[test]
    fn frames_are_kept_to_the_bit() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 600.0);
        let l: Vec<f64> = (0..5000).map(|i| (i as f64 * 0.37).sin() / 3.0 + 1e-12 * i as f64).collect();
        let r: Vec<f64> = l.iter().map(|v| -v * std::f64::consts::FRAC_1_SQRT_2).collect();
        assert!(l.iter().any(|&v| v as f32 as f64 != v), "values an f32 does not hold");
        for (a, b) in l.chunks(777).zip(r.chunks(777)) {
            s.push(a, b);
        }
        let (mut gl, mut gr) = (vec![0.0; 5000], vec![0.0; 5000]);
        s.read(0, 0, &mut gl);
        s.read(1, 0, &mut gr);
        assert!(gl.iter().zip(&l).chain(gr.iter().zip(&r)).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    /// The buffers grow an eighth at a time (at least GROW_MIN frames), not
    /// to twice what they hold.
    #[test]
    fn the_buffers_grow_by_an_eighth_not_twice() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 1e9);
        let chunk = vec![0.5; 4_608];
        let mut grew = 0;
        let mut cap = 0;
        while s.end() < 3 * GROW_MIN as i64 {
            s.push(&chunk, &chunk);
            let st = s.lock();
            let (len, c) = (st.l.len(), st.l.capacity());
            assert_eq!(c, st.r.capacity());
            assert!(c <= len + (len / 8).max(GROW_MIN) + chunk.len(), "{} frames held in room for {}", len, c);
            if c != cap {
                grew += 1;
                cap = c;
            }
        }
        assert!(grew >= 3, "grew in steps: {}", grew);
    }

    #[test]
    fn a_reader_waits_for_frames_that_come_in_time() {
        let s = std::sync::Arc::new(LiveSource::new(1000, 1 << 30, 0.5, 600.0));
        let w = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            w.push(&ramp_up(200), &ramp_up(200));
        });
        s.ensure(150);
        t.join().unwrap();
        let st = s.stats();
        assert_eq!(st.starves, 0);
        assert_eq!(st.silent_frames, 0);
        assert!(st.max_wait_ms >= 40, "waited {} ms", st.max_wait_ms);
    }

    #[test]
    fn a_starved_reader_gets_silence_and_the_stream_goes_on_after_it_faded() {
        let rate = 1000;
        let s = LiveSource::new(rate, 1 << 30, 0.5, 600.0);
        s.push(&vec![1.0; 300], &vec![1.0; 300]);
        s.ensure(200); // the reader's edge is at 200
        s.ensure(400); // 100 frames short: waits STARVE_WAIT, then starves
        assert!(s.starved());
        let st = s.stats();
        assert_eq!(st.starves, 1);
        assert_eq!(st.silent_frames, 100);
        // The unread tail (200..300) faded out over 10 ms = 10 frames.
        let mut out = vec![0.0; 300];
        s.read(0, 0, &mut out);
        assert!(out[..290].iter().all(|&v| v == 1.0));
        assert!(out[290] < 1.0 && out[299] < 0.1);
        // A second short request gets silence at once.
        let t0 = Instant::now();
        s.ensure(500);
        assert!(t0.elapsed() < Duration::from_millis(100));
        assert_eq!(s.stats().silent_frames, 200);
        // What arrives is held until 0.5 s have gathered, then goes in after
        // the silence with a fade-in — the newest 0.5 s of it: the oldest
        // 0.1 s beyond the wait is cut.
        s.push(&vec![0.5; 300], &vec![0.5; 300]);
        assert!(s.starved());
        assert_eq!(s.end(), 500);
        s.push(&vec![0.5; 300], &vec![0.5; 300]);
        assert!(!s.starved());
        assert_eq!((s.end(), s.stats().cut_frames), (1000, 100));
        let mut tail = vec![0.0; 500];
        s.read(0, 500, &mut tail);
        assert!(tail[0] < 0.1);
        assert!(tail[10..].iter().all(|&v| v == 0.5));
    }

    #[test]
    fn each_starve_after_the_first_gathers_half_as_much_again() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 600.0);
        assert_eq!(s.resume_s(), 0.5);
        s.push(&vec![1.0; 100], &vec![1.0; 100]);
        s.ensure(200); // starved: goes on after 0.5 s
        assert_eq!(s.resume_s(), 0.5);
        s.push(&vec![1.0; 500], &vec![1.0; 500]);
        assert!(!s.starved());
        let end = s.end();
        s.ensure(end + 100); // starved again: 0.75 s now
        assert_eq!(s.stats().starves, 2);
        assert_eq!(s.resume_s(), 0.75);
        s.push(&vec![1.0; 500], &vec![1.0; 500]);
        assert!(s.starved(), "0.5 s is not enough any more");
        s.push(&vec![1.0; 250], &vec![1.0; 250]);
        assert!(!s.starved());
        // Never past the most.
        let t = LiveSource::new(1000, 1 << 30, 20.0, 600.0);
        for _ in 0..4 {
            let end = t.end();
            t.ensure(end + 10);
            t.push(&vec![1.0; 40_000], &vec![1.0; 40_000]);
        }
        assert_eq!(t.resume_s(), MAX_RESUME_S);
    }

    #[test]
    fn a_gap_fades_out_what_was_not_read_and_fades_in_what_follows() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 600.0);
        s.push(&vec![1.0; 100], &vec![1.0; 100]);
        s.ensure(95); // read up to 95: only 95..100 is left to fade
        s.mark_gap();
        s.push(&vec![1.0; 100], &vec![1.0; 100]);
        let mut out = vec![0.0; 200];
        s.read(0, 0, &mut out);
        assert!(out[..95].iter().all(|&v| v == 1.0), "read frames are never changed");
        assert!(out[99] < 0.2);
        assert!(out[100] < 0.2 && out[110] == 1.0);
        assert_eq!(s.stats().gaps, 1);
    }

    #[test]
    fn a_bursts_oldest_frames_are_cut_after_a_gap_and_the_rest_fades_in() {
        let s = LiveSource::new(1000, 1 << 30, 0.5, 600.0);
        s.push(&vec![1.0; 100], &vec![1.0; 100]);
        s.ensure(50);
        s.mark_gap();
        // The new connection: 300 frames numbered from 1.
        let burst: Vec<f64> = (1..=300).map(|i| i as f64).collect();
        s.push(&burst, &burst);
        assert_eq!(s.cut_after_gap(120), Some((100, 120)));
        assert_eq!((s.end(), s.stats().cut_frames), (280, 120));
        let mut out = vec![0.0; 180];
        s.read(0, 100, &mut out);
        // From 121 on, faded in again over 10 frames.
        assert!(out[0] < 121.0 * 0.1 && out[10] == 131.0 && out[179] == 300.0, "{:?}", &out[..12]);
        // Once: and never what a block has read.
        assert_eq!(s.cut_after_gap(10), None);
        s.mark_gap();
        s.push(&burst, &burst);
        s.ensure(400);
        assert_eq!(s.cut_after_gap(10), None, "the reader is past the gap");
    }

    #[test]
    fn history_is_kept_behind_the_reader_and_dropped_beyond() {
        let keep = 10;
        let s = LiveSource::new(1000, keep, 0.5, 1e9);
        let n = TRIM_CHUNK + 5000;
        s.push(&vec![1.0; n], &vec![1.0; n]);
        s.ensure(n as i64);
        let mut out = vec![0.0; keep];
        assert!(s.read(0, n as i64 - keep as i64, &mut out));
        assert!(out.iter().all(|&v| v == 1.0));
        let mut old = vec![0.0; 10];
        assert!(!s.read(0, 0, &mut old), "frames far behind the reader are gone");
    }

    #[test]
    fn frames_dropped_on_a_long_pause_leave_a_fade_not_a_jump() {
        // At most 0.5 s (500 frames) ahead of the reader.
        let s = LiveSource::new(1000, 1 << 30, 0.5, 0.5);
        s.push(&vec![1.0; 600], &vec![1.0; 600]);
        s.push(&vec![1.0; 100], &vec![1.0; 100]); // full: dropped
        assert_eq!(s.stats().dropped_frames, 100);
        s.ensure(200); // the reader goes on: room again
        s.push(&vec![1.0; 100], &vec![1.0; 100]);
        let mut out = vec![0.0; 700];
        s.read(0, 0, &mut out);
        assert!(out[..590].iter().all(|&v| v == 1.0));
        assert!(out[599] < 0.1, "the frames held fade out where the drop began");
        assert!(out[600] < 0.1 && out[610] == 1.0, "what follows fades in");
        assert_eq!(s.stats().gaps, 1);
    }

    #[test]
    fn a_closed_source_lets_its_reader_go() {
        let s = std::sync::Arc::new(LiveSource::new(1000, 1 << 30, 0.5, 600.0));
        let c = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            c.close();
        });
        let t0 = Instant::now();
        s.ensure(1000);
        t.join().unwrap();
        assert!(t0.elapsed() < STARVE_WAIT);
        assert_eq!(s.stats().starves, 0);
    }

    /// The test stream's frame `t`: no two alike, so a shadow a frame off
    /// would show.
    fn frame(t: u64) -> (f64, f64) {
        let l = (t as f64 * 0.618_034).sin() * 0.5;
        (l, -0.75 * l)
    }

    /// A source fed as the decoder feeds it: each piece to the shadow at
    /// once, and through stages that hold `hold` frames back — the identity,
    /// late — so the shadow must be, frame for frame and to the bit, what
    /// the stages handed on, whatever befalls the stream.
    struct Fed {
        s: LiveSource,
        held: VecDeque<(f64, f64)>,
        hold: usize,
        at: u64,
    }

    impl Fed {
        fn new(s: LiveSource, hold: usize) -> Fed {
            Fed { s, held: VecDeque::new(), hold, at: 0 }
        }

        /// `n` more frames of the stream, in pieces of many sizes.
        fn more(&mut self, n: usize) {
            let (mut left, mut piece) = (n, 1 + (self.at % 997) as usize);
            while left > 0 {
                let k = piece.min(left);
                let (l, r): (Vec<f64>, Vec<f64>) = (self.at..self.at + k as u64).map(frame).unzip();
                self.at += k as u64;
                self.s.push_raw(&l, &r);
                self.held.extend(l.into_iter().zip(r));
                if self.held.len() > self.hold {
                    let (a, b): (Vec<f64>, Vec<f64>) = self.held.drain(..self.held.len() - self.hold).unzip();
                    self.s.push(&a, &b);
                }
                left -= k;
                piece = if piece > 20_000 { 13 } else { piece * 3 + 7 };
            }
        }

        /// A connection broke: what the stages hold goes in first (the
        /// decoder finishes them), then the gap.
        fn gap(&mut self) {
            let (a, b): (Vec<f64>, Vec<f64>) = self.held.drain(..).unzip();
            self.s.push(&a, &b);
            self.s.mark_gap();
        }

        fn same(&self, what: &str) {
            let st = self.s.lock();
            assert_eq!(st.raw_base + st.raw_l.len() as i64, st.end(), "{what}: the shadow ends where the stream does");
            assert!(st.raw_base >= st.base, "{what}");
            let off = (st.raw_base - st.base) as usize;
            let same = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits());
            assert!(same(&st.raw_l, &st.l[off..]) && same(&st.raw_r, &st.r[off..]), "{what}: the shadow differs");
            assert!(same(&st.raw_held_l, &st.held_l) && same(&st.raw_held_r, &st.held_r), "{what}: what a starve holds differs");
            assert_eq!(st.raw_wait_l.len(), self.held.len(), "{what}: the shadow waits for what the stages hold");
            assert_eq!(st.stats.raw_missing, 0, "{what}");
        }
    }

    /// The shadow beside the stream: read back as it came, and frame for
    /// frame what the stages handed on through a gap and the cut after it, a
    /// starve, a full buffer dropping and reading on, a long run trimming
    /// the histories — with stages that hold nothing back, and 777 frames.
    #[test]
    fn the_shadow_is_what_the_stages_handed_on_frame_for_frame_whatever_befalls_the_stream() {
        for hold in [0usize, 777] {
            // 1 kHz; the stream's history kept whole, the shadow's 40 s;
            // at most 30 s ahead of the reader.
            let mut f = Fed::new(LiveSource::new(1000, 1 << 30, 0.5, 30.0), hold);
            f.more(5_000);
            f.s.ensure(3_000);
            let mut out = vec![0.0; 100];
            f.s.read_raw(1, 1_000, &mut out);
            assert!(out.iter().enumerate().all(|(i, v)| v.to_bits() == frame(1_000 + i as u64).1.to_bits()), "read back as it came");
            f.same("pushed and read");
            f.gap();
            f.more(2_000);
            assert!(f.s.cut_after_gap(300).is_some());
            f.same("a gap, and a cut after it");
            let end = f.s.end();
            f.s.ensure(end + 100);
            assert!(f.s.starved());
            f.more(300);
            f.same("starved, holding");
            f.more(900 + hold);
            assert!(!f.s.starved());
            f.same("resumed after a starve");
            // Full is looked at before each piece: past 30 s, the next go.
            f.more(35_000);
            f.more(5_000);
            assert!(f.s.stats().dropped_frames > 0);
            let want = f.s.end() - 20_000;
            f.s.ensure(want);
            f.more(3_000);
            f.same("dropped while full, then read on");
            for _ in 0..300 {
                f.more(4_000);
                let end = f.s.end();
                f.s.ensure(end);
            }
            f.same("a long run");
            assert_eq!(f.s.raw_start(), f.s.lock().raw_base);
            let st = f.s.lock();
            assert!(st.base == 0 && st.raw_base > 0, "the stream's history kept whole, the shadow's dropped: {}", st.raw_base);
            assert!(st.want - st.raw_base >= 40_000, "the shadow keeps its 40 s: {}", st.want - st.raw_base);
        }
    }

    /// On a short filter the chain keeps little behind the reader, but the
    /// shadow keeps its 40 s all the same: after a station's burst the reader
    /// runs half a minute ahead of the place heard, and BIT-PERFECT and the
    /// stream's instruments read from there (TASK-34: they could not, and the
    /// instruments' work started afresh at every ask).
    #[test]
    fn the_shadow_keeps_its_40_s_on_a_short_filter() {
        let s = LiveSource::new(1000, 50, 0.5, 600.0);
        let x = vec![0.25f64; 1000];
        for _ in 0..90 {
            s.push_raw(&x, &x);
            s.push(&x, &x);
            let end = s.end();
            s.ensure(end);
        }
        let st = s.lock();
        assert!(st.want >= 89_000, "read on: {}", st.want);
        assert!(st.want - st.base < 50 + TRIM_CHUNK as i64 + 1, "the chain keeps its little: {}", st.want - st.base);
        assert!(st.want - st.raw_base >= 40_000, "the shadow keeps its 40 s: {}", st.want - st.raw_base);
        let mut out = vec![0f64; 100];
        drop(st);
        assert!(s.read_raw(0, s.end() - 38_000, &mut out) && out.iter().all(|v| *v == 0.25), "38 s behind the reader");
    }
}
