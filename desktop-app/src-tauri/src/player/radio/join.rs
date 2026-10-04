//! One stream across connections: a new connection after a break starts
//! with what its server keeps for new listeners (Radio Paradise ~10 s, FIP
//! ~4 s), most of it already received. Played as it comes, that would be
//! heard twice.
//!
//! `Joiner` stands between the decoder and the source stages (decode.rs),
//! so it sees the frames as decoded. It keeps the
//! last `TAIL_S` of the stream as decoded; after a break it holds the new
//! connection's first frames and looks for them, sample for sample, in that
//! tail — past the first `WARMUP` frames, which a decoder that starts
//! mid-stream may not make the same (an MP3 frame's reservoir, the overlap
//! of the first AAC frame). Found: what repeats is cut off and the rest goes
//! on from the very next frame, without a seam. A burst longer than the tail
//! starts before it, and a decoder may take longer than `WARMUP` to settle:
//! then the last frames received are looked for among the new ones instead
//! (as `adts::Trail` does with frames' bytes). Not found — the break lost
//! audio, or the new start is too quiet to place — the two sides meet as
//! any gap does: the frames not heard yet fade out, the new ones fade in.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much of the stream is kept to place a new connection in (a server's
/// burst is mostly under it; a longer one is placed by the tail's end).
pub const TAIL_S: f64 = 30.0;
/// A new connection not placed this long after its first frames came (a
/// burst brings a server's frames at once; without one they come as
/// played), or with more frames than the tail keeps and this much again, is
/// taken as new: audio was lost (a gap). The same for a stream placed by its
/// frames' bytes (decode.rs).
pub const PLACE_WAIT: Duration = Duration::from_secs(3);
pub const PLACE_MORE_S: f64 = 10.0;
/// A new decoder's first frames are not compared.
pub const WARMUP: usize = 8192;
/// The frames compared, both channels.
pub const PROBE: usize = 4096;
/// Equal: the same decoder on the same bytes gives the same numbers; this is
/// for the rounding of nothing at all.
const EPS: f64 = 1e-5;
/// A probe quieter than this (−60 dBFS) would match too easily.
const QUIET: f64 = 1e-3;
/// New audio gathered without finding a probe loud enough: given up, faded.
const GIVE_UP_S: f64 = 5.0;

/// What became of a new connection's start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Joined {
    /// It repeated this many frames already received: cut off, no seam.
    Seamless { repeated: usize },
    /// Not in the tail (or too quiet to tell): a gap with fades.
    Gap { quiet: bool },
    /// It followed a break known to have lost audio (`known_gap`): a gap,
    /// not looked for.
    Known,
}

enum State {
    Flowing,
    /// After a break: the new connection's frames, held until placed.
    Placing,
    /// Placed: this many more frames repeat what was received.
    Skipping(usize),
    /// After a known break: the next frames go on at once, after a gap.
    Gap,
}

/// How far placing a new connection has got.
struct Look {
    /// When its first frames came.
    since: Option<Instant>,
    /// Its next window to look for in the tail.
    probe: usize,
    /// Its first loud window is not in the tail: where the tail's last loud
    /// window starts, looked for among its frames from the second index on.
    last: Option<(usize, usize)>,
}

impl Look {
    fn new() -> Look {
        Look { since: None, probe: WARMUP, last: None }
    }
}

pub struct Joiner {
    rate: u32,
    cap: usize,
    tail_l: VecDeque<f64>,
    tail_r: VecDeque<f64>,
    pend_l: Vec<f64>,
    pend_r: Vec<f64>,
    state: State,
    look: Look,
}

impl Joiner {
    pub fn new(rate: u32) -> Joiner {
        let cap = (TAIL_S * rate as f64) as usize;
        Joiner {
            rate,
            cap,
            tail_l: VecDeque::with_capacity(cap),
            tail_r: VecDeque::with_capacity(cap),
            pend_l: Vec::new(),
            pend_r: Vec::new(),
            state: State::Flowing,
            look: Look::new(),
        }
    }

    /// The connection broke (or ended): what comes next is placed first.
    pub fn broken(&mut self) {
        if matches!(self.state, State::Flowing) && !self.tail_l.is_empty() {
            self.state = State::Placing;
        }
        // A break before the last one was placed: its frames held so far are
        // from a connection that is gone; the next one is placed afresh.
        self.pend_l.clear();
        self.pend_r.clear();
        self.look = Look::new();
    }

    /// The next connection follows a break known to have lost audio (HLS:
    /// segments gone before they were fetched, a discontinuity, a server
    /// that started again): it is not looked for in the tail — there is
    /// nothing of it there — and its first frames go on at once, after a gap.
    pub fn known_gap(&mut self) {
        self.pend_l.clear();
        self.pend_r.clear();
        self.look = Look::new();
        if !self.tail_l.is_empty() {
            self.state = State::Gap;
        }
    }

    /// Waiting to place a new connection.
    #[cfg(test)]
    pub fn placing(&self) -> bool {
        matches!(self.state, State::Placing | State::Skipping(_))
    }

    /// Decoded frames in; `out` gets the frames the stream goes on with (and
    /// `gap` true once, before the first frame after a gap). Returns what
    /// became of a new connection's start, when it was decided now.
    pub fn push(&mut self, l: &[f64], r: &[f64], out: impl FnMut(&[f64], &[f64], bool)) -> Option<Joined> {
        self.push_at(l, r, Instant::now(), out)
    }

    /// `push` at the time `now`: a new connection not placed within
    /// PLACE_WAIT of its first frames is given up as new.
    fn push_at(&mut self, l: &[f64], r: &[f64], now: Instant, mut out: impl FnMut(&[f64], &[f64], bool)) -> Option<Joined> {
        match self.state {
            State::Flowing => {
                self.keep(l, r);
                out(l, r, false);
                None
            }
            State::Gap => {
                self.state = State::Flowing;
                self.keep(l, r);
                out(l, r, true);
                Some(Joined::Known)
            }
            State::Skipping(left) => {
                let n = left.min(l.len());
                if n == l.len() {
                    self.state = State::Skipping(left - n);
                    return None;
                }
                self.state = State::Flowing;
                self.keep(&l[n..], &r[n..]);
                out(&l[n..], &r[n..], false);
                None
            }
            State::Placing => {
                self.pend_l.extend_from_slice(l);
                self.pend_r.extend_from_slice(r);
                let found = self.place(now)?;
                self.look = Look::new();
                let (pl, pr) = (std::mem::take(&mut self.pend_l), std::mem::take(&mut self.pend_r));
                match found {
                    Joined::Seamless { repeated } if repeated >= pl.len() => {
                        self.state = State::Skipping(repeated - pl.len());
                    }
                    Joined::Seamless { repeated } => {
                        self.state = State::Flowing;
                        self.keep(&pl[repeated..], &pr[repeated..]);
                        out(&pl[repeated..], &pr[repeated..], false);
                    }
                    Joined::Gap { .. } | Joined::Known => {
                        self.state = State::Flowing;
                        self.keep(&pl, &pr);
                        out(&pl, &pr, true);
                    }
                }
                Some(found)
            }
        }
    }

    /// The newest `cap` frames are kept: what falls out goes first, so the
    /// tail never needs room beyond it.
    fn keep(&mut self, l: &[f64], r: &[f64]) {
        let (l, r) = (&l[l.len().saturating_sub(self.cap)..], &r[r.len().saturating_sub(self.cap)..]);
        let over = (self.tail_l.len() + l.len()).saturating_sub(self.cap);
        if over > 0 {
            self.tail_l.drain(..over);
            self.tail_r.drain(..over);
        }
        self.tail_l.extend(l.iter().copied());
        self.tail_r.extend(r.iter().copied());
    }

    /// Where the held frames start in the tail, when that can be told yet.
    fn place(&mut self, now: Instant) -> Option<Joined> {
        let since = *self.look.since.get_or_insert(now);
        let (tl, tr) = (&*self.tail_l.make_contiguous(), &*self.tail_r.make_contiguous());
        let (nl, nr) = (&self.pend_l[..], &self.pend_r[..]);
        let n = nl.len();
        if self.look.last.is_none() {
            // The new frames' first window loud enough to place, past the
            // decoder's warm-up.
            let a = loop {
                let a = self.look.probe;
                if a + PROBE > n {
                    return (n as f64 >= GIVE_UP_S * self.rate as f64).then_some(Joined::Gap { quiet: true });
                }
                if Window::new(&nl[a..a + PROBE], &nr[a..a + PROBE]).level() >= QUIET {
                    break a;
                }
                self.look.probe += PROBE;
            };
            // The newest place it matches (the server's burst is the newest
            // audio it has; an older match would be music repeating itself).
            let w = Window::new(&nl[a..a + PROBE], &nr[a..a + PROBE]);
            if let Some(at) = tl.len().checked_sub(PROBE).and_then(|e| (0..=e).rev().find(|&p| w.at(tl, tr, p))) {
                // new[a] is tail[at]: new[0] lies (tail.len − at + a) frames
                // before the tail's end, and that much of it is received.
                return Some(Joined::Seamless { repeated: tl.len() - at + a });
            }
            // Not there: a burst longer than the tail starts before it, or
            // the decoder had not settled yet. The last loud window received
            // is looked for among the new frames instead.
            let Some(t) = last_loud(tl, tr) else { return Some(Joined::Gap { quiet: false }) };
            self.look.last = Some((t, WARMUP));
        }
        let (t, mut q) = self.look.last.expect("set above");
        let w = Window::new(&tl[t..t + PROBE], &tr[t..t + PROBE]);
        while q + PROBE <= n {
            if w.at(nl, nr, q) {
                // new[q] is tail[t]: received up to the tail's end, as above.
                return Some(Joined::Seamless { repeated: tl.len() - t + q });
            }
            q += 1;
        }
        self.look.last = Some((t, q));
        let more = (PLACE_MORE_S * self.rate as f64) as usize;
        (now.saturating_duration_since(since) > PLACE_WAIT || n > tl.len() + more).then_some(Joined::Gap { quiet: false })
    }
}

/// PROBE frames to be found among others, sample for sample — tried at
/// their loudest sample first, which tells most places apart at once.
struct Window<'a> {
    l: &'a [f64],
    r: &'a [f64],
    peak: usize,
}

impl<'a> Window<'a> {
    fn new(l: &'a [f64], r: &'a [f64]) -> Window<'a> {
        let level = |i: usize| l[i].abs().max(r[i].abs());
        let peak = (0..l.len()).max_by(|&a, &b| level(a).total_cmp(&level(b))).unwrap_or(0);
        Window { l, r, peak }
    }

    /// Its loudest sample, either channel.
    fn level(&self) -> f64 {
        self.l[self.peak].abs().max(self.r[self.peak].abs())
    }

    /// Whether `l`, `r` hold it from `p` on.
    fn at(&self, l: &[f64], r: &[f64], p: usize) -> bool {
        let (k, n) = (self.peak, self.l.len());
        (l[p + k] - self.l[k]).abs() <= EPS
            && (r[p + k] - self.r[k]).abs() <= EPS
            && l[p..p + n].iter().zip(self.l).all(|(x, y)| (x - y).abs() <= EPS)
            && r[p..p + n].iter().zip(self.r).all(|(x, y)| (x - y).abs() <= EPS)
    }
}

/// Where the newest window of PROBE frames loud enough to place by starts,
/// a window at a time back from the end.
fn last_loud(l: &[f64], r: &[f64]) -> Option<usize> {
    let mut s = l.len().checked_sub(PROBE)?;
    loop {
        if Window::new(&l[s..s + PROBE], &r[s..s + PROBE]).level() >= QUIET {
            return Some(s);
        }
        s = s.checked_sub(PROBE)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test signal no stretch of which repeats: a chirp with noise.
    fn signal(n: usize, seed: u32) -> (Vec<f64>, Vec<f64>) {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        let mut noise = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f64 / u32::MAX as f64 - 0.5) * 0.2
        };
        let l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.001 * (1.0 + i as f64 * 1e-5)).sin() * 0.5 + noise()).collect();
        let r: Vec<f64> = (0..n).map(|i| (i as f64 * 0.0013).cos() * 0.4 + noise()).collect();
        (l, r)
    }

    /// Run frames through a joiner in blocks; what it gives out, and the
    /// gaps it marked.
    fn feed(j: &mut Joiner, l: &[f64], r: &[f64], block: usize, got: &mut (Vec<f64>, Vec<f64>, usize)) -> Vec<Joined> {
        feed_at(j, l, r, block, got, |_| Instant::now())
    }

    /// The same, at the time `clock` tells for the frames fed so far.
    fn feed_at(
        j: &mut Joiner,
        l: &[f64],
        r: &[f64],
        block: usize,
        got: &mut (Vec<f64>, Vec<f64>, usize),
        clock: impl Fn(usize) -> Instant,
    ) -> Vec<Joined> {
        let mut decided = vec![];
        for (k, (cl, cr)) in l.chunks(block).zip(r.chunks(block)).enumerate() {
            if let Some(d) = j.push_at(cl, cr, clock(k * block), |a, b, gap| {
                got.0.extend_from_slice(a);
                got.1.extend_from_slice(b);
                if gap {
                    got.2 += 1;
                }
            }) {
                decided.push(d);
            }
        }
        decided
    }

    #[test]
    fn a_new_connection_that_repeats_the_stream_goes_on_without_a_seam() {
        let rate = 44_100;
        let (l, r) = signal(rate as usize * 40, 7);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        // The first connection: 25 s, then it breaks.
        let cut = rate as usize * 25;
        feed(&mut j, &l[..cut], &r[..cut], 4096, &mut got);
        j.broken();
        assert!(j.placing());
        // The new one starts 10 s back (its server's burst); its decoder's
        // first frames differ from the old one's.
        let from = cut - rate as usize * 10;
        let mut nl = l[from..].to_vec();
        let mut nr = r[from..].to_vec();
        for i in 0..2000 {
            nl[i] += 0.3;
            nr[i] -= 0.3;
        }
        let d = feed(&mut j, &nl, &nr, 4608, &mut got);
        assert_eq!(d, vec![Joined::Seamless { repeated: cut - from }]);
        assert!(!j.placing());
        assert_eq!(got.2, 0, "no gap");
        assert_eq!(got.0.len(), l.len());
        assert!(got.0 == l && got.1 == r, "the stream as if it had never broken");
    }

    /// The tail keeps TAIL_S in the room it was made with, whatever sizes
    /// the frames come in.
    #[test]
    fn the_tail_never_needs_room_beyond_what_it_keeps() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 70, 13);
        let mut j = Joiner::new(rate);
        let room = (j.tail_l.capacity(), j.tail_r.capacity());
        let mut got = (vec![], vec![], 0);
        for block in [1_152, 7, 300_000, 4_096] {
            feed(&mut j, &l, &r, block, &mut got);
            assert_eq!((j.tail_l.capacity(), j.tail_r.capacity()), room, "block {}", block);
            assert_eq!(j.tail_l.len(), j.cap);
        }
        assert!(j.tail_l.iter().eq(l[l.len() - j.cap..].iter()), "the newest frames, as they came");
    }

    #[test]
    fn a_new_connection_after_lost_audio_meets_the_old_with_a_gap() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 40, 11);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 20;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        // It comes back 3 s later than the old one ended, its frames as they
        // are played: nothing to cut. Neither its first frames are in the
        // tail nor the last received among its own; it is looked for until
        // PLACE_WAIT has gone by, then meets the old one as a gap.
        let from = cut + rate as usize * 3;
        let t0 = Instant::now();
        let pace = |k: usize| t0 + Duration::from_secs_f64(k as f64 / rate as f64);
        let early = rate as usize * 5 / 2;
        let d = feed_at(&mut j, &l[from..from + early], &r[from..from + early], 1000, &mut got, &pace);
        assert!(d.is_empty() && got.0.len() == cut, "still looked for 2.5 s on");
        let d = feed_at(&mut j, &l[from + early..], &r[from + early..], 1000, &mut got, |k| pace(early + k));
        assert_eq!(d, vec![Joined::Gap { quiet: false }]);
        assert_eq!(got.2, 1, "one gap, marked before the new frames");
        assert_eq!(got.0.len(), cut + (l.len() - from));
    }

    /// New audio after a loss that comes all at once: more frames than the
    /// tail keeps and PLACE_MORE_S again, the last received nowhere among
    /// them — a gap, without waiting PLACE_WAIT out.
    #[test]
    fn lost_audio_in_a_long_burst_is_a_gap_once_its_frames_tell() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 70, 23);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 20;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        let from = cut + rate as usize;
        let t0 = Instant::now();
        let d = feed_at(&mut j, &l[from..], &r[from..], 4096, &mut got, |_| t0);
        assert_eq!(d, vec![Joined::Gap { quiet: false }]);
        assert_eq!(got.2, 1);
        assert_eq!(got.0.len(), cut + (l.len() - from));
    }

    /// A server's burst longer than the tail starts before it: its first
    /// frames are nowhere in the tail, but the last frames received are among
    /// its own — placed by them, the stream goes on without a seam.
    #[test]
    fn a_burst_longer_than_the_tail_is_placed_by_the_last_frames_received() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 60, 17);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 40;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        // 35 s of burst, 5 s more than the tail keeps; the new decoder's
        // first frames differ from the old one's.
        let from = cut - rate as usize * 35;
        let (mut nl, mut nr) = (l[from..].to_vec(), r[from..].to_vec());
        for i in 0..2000 {
            nl[i] += 0.3;
            nr[i] -= 0.3;
        }
        let t0 = Instant::now();
        let d = feed_at(&mut j, &nl, &nr, 4608, &mut got, |_| t0);
        assert_eq!(d, vec![Joined::Seamless { repeated: cut - from }]);
        assert_eq!(got.2, 0, "no gap");
        assert!(got.0 == l && got.1 == r, "the stream as if it had never broken");
    }

    /// A new decoder that takes longer than WARMUP to give the old one's
    /// numbers: its first loud window is not in the tail, but the last
    /// frames received are among its frames — placed by them.
    #[test]
    fn a_decoder_slow_to_settle_is_placed_by_the_last_frames_received() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 30, 19);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 20;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        let from = cut - rate as usize * 5;
        let (mut nl, mut nr) = (l[from..].to_vec(), r[from..].to_vec());
        for i in 0..WARMUP + 2 * PROBE {
            nl[i] += 1e-3;
            nr[i] -= 1e-3;
        }
        let t0 = Instant::now();
        let d = feed_at(&mut j, &nl, &nr, 1152, &mut got, |_| t0);
        assert_eq!(d, vec![Joined::Seamless { repeated: cut - from }]);
        assert_eq!(got.2, 0, "no gap");
        assert!(got.0 == l && got.1 == r, "the stream as if it had never broken");
    }

    #[test]
    fn silence_is_not_placed_and_a_quiet_start_is_given_up_with_a_gap() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 10, 3);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        feed(&mut j, &l, &r, 1000, &mut got);
        j.broken();
        let quiet = vec![0.0f64; (GIVE_UP_S * rate as f64) as usize + 1000];
        let d = feed(&mut j, &quiet, &quiet, 1000, &mut got);
        assert_eq!(d, vec![Joined::Gap { quiet: true }]);
        assert_eq!(got.2, 1);
    }

    /// A break known to have lost audio (HLS): the next connection is not
    /// looked for — its first frames go on at once, after one gap; before
    /// anything was received there is nothing to meet.
    #[test]
    fn a_known_gap_goes_on_at_once() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 20, 9);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 10;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        j.known_gap();
        let from = cut + rate as usize * 2;
        let d = feed(&mut j, &l[from..from + 1000], &r[from..from + 1000], 1000, &mut got);
        assert_eq!(d, vec![Joined::Known]);
        assert_eq!(got.2, 1, "one gap, before the new frames");
        assert_eq!(got.0.len(), cut + 1000, "nothing held back");
        let mut fresh = Joiner::new(rate);
        fresh.known_gap();
        let mut none = (vec![], vec![], 0);
        let d = feed(&mut fresh, &l[..1000], &r[..1000], 1000, &mut none);
        assert!(d.is_empty() && none.2 == 0 && none.0.len() == 1000);
    }

    #[test]
    fn a_short_burst_that_ends_before_the_probe_is_a_gap_and_a_break_while_placing_starts_over() {
        let rate = 8_000;
        let (l, r) = signal(rate as usize * 30, 5);
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0);
        let cut = rate as usize * 20;
        feed(&mut j, &l[..cut], &r[..cut], 1000, &mut got);
        j.broken();
        // A connection that gives a few frames and breaks again.
        feed(&mut j, &l[cut - 500..cut], &r[cut - 500..cut], 500, &mut got);
        j.broken();
        assert!(j.placing());
        // The next one repeats 2 s: placed, as if the one between had not been.
        let from = cut - rate as usize * 2;
        let d = feed(&mut j, &l[from..], &r[from..], 1000, &mut got);
        assert_eq!(d, vec![Joined::Seamless { repeated: cut - from }]);
        assert!(got.0 == l && got.1 == r);
    }
}
