//! The client's own clock: while the server's reader takes the ring at an
//! even pace (`Client::even`), the client calls its player once a period by
//! the system clock, trimmed so that over time it puts in what the reader
//! takes — not whenever the reader happens to make room. The player is then
//! called as evenly as the clock keeps time, whatever the server's own
//! hiccups: the ring holds a reserve for them (`RESERVE_S`), filled only
//! while the reader stands and taken back by it after, so a player whose
//! own source keeps the system's clock (a bridge from a system device,
//! through a FIFO of a few blocks) never hears them.
//!
//! The trim watches the ring's fill just before each call against an aim —
//! what the room rule (a block when it fits under the server's target) left
//! there, and `FLOOR_S` at least — and moves slowly, a loop of some twenty
//! seconds and never faster than `SLEW` a second, so a player that measures
//! the rate it is called at sees it steady.
//!
//! The ring emptied before a call is due: the reader wants more than the
//! clock gives (it makes up a shortfall, a player catches up) — the call
//! comes at once, as by the room rule, and the clock goes on from it. The
//! reserve full: the reader stands longer than it holds — the client waits
//! for room as by the room rule, and the clock goes on from the next call.
//!
//! No system calls here: the time is the caller's, so a model can run it.

use std::time::{Duration, Instant};

/// The reserve the ring keeps for the reader's hiccups, seconds: no more
/// than the reader catches up after one (Aura Player's `CREDIT_S`), or what
/// is left over would be the trim's to take back.
pub const RESERVE_S: f64 = 0.2;
/// The least the ring holds just before a call, seconds: the reader's tick
/// and the clock's own lateness.
pub const FLOOR_S: f64 = 0.01;
/// How far the trim takes the clock from the stream's rate, and how fast it
/// moves at most, a second.
pub const TRIM_MAX: f64 = 0.006;
pub const SLEW: f64 = 1e-4;
/// The longest sleep before a call: an emptied ring is heard that soon.
pub const NAP: Duration = Duration::from_millis(1);
/// How long the fill is averaged before the trim's aim moves, seconds.
const WINDOW_S: f64 = 1.0;
/// The trim's gains (critically damped: a loop of some twenty seconds), and
/// the most error it takes in, seconds.
const KP: f64 = 0.1;
const KI: f64 = KP * KP / 4.0;
const FAR: f64 = 0.05;
/// A call this far behind its time (the machine slept, the thread was
/// stopped) goes on from now.
const BEHIND: Duration = Duration::from_millis(500);

/// What the client does next.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Next {
    /// Call the player now: its block is due, or (`early`) the ring ran
    /// empty before it was.
    Call { early: bool },
    /// Not yet: look again after this.
    Sleep(Duration),
    /// The reserve is full: wait for the reader to take.
    Room,
}

/// The clock for one run of the player in one format.
pub struct Clock {
    /// The player's period at the stream's rate, seconds.
    period: f64,
    /// Samples a second (the rate times the channels).
    per_s: f64,
    /// The fill just before a call that the trim keeps, and the most the
    /// ring may hold then (the aim and the reserve), samples.
    aim: u64,
    most: u64,
    due: Option<Instant>,
    trim: f64,
    want: f64,
    integral: f64,
    last: Option<Instant>,
    /// The fill before the calls of this window, and when it began.
    sum: f64,
    n: u32,
    since: Option<Instant>,
}

impl Clock {
    /// A clock for blocks of `block` samples at `per_s` samples a second,
    /// under a server's `target`, in a ring of `ring` samples.
    pub fn new(per_s: f64, block: u64, target: u64, ring: u64) -> Clock {
        let per_s = per_s.max(1.0);
        let aim = target.saturating_sub(block).max((FLOOR_S * per_s) as u64);
        let most = (aim + (RESERVE_S * per_s) as u64).min(ring.saturating_sub(block)).max(aim);
        Clock {
            period: block as f64 / per_s,
            per_s,
            aim,
            most,
            due: None,
            trim: 0.0,
            want: 0.0,
            integral: 0.0,
            last: None,
            sum: 0.0,
            n: 0,
            since: None,
        }
    }

    /// The fill just before a call the clock keeps, samples.
    pub fn aim(&self) -> u64 {
        self.aim
    }

    /// The most the ring holds before a call (the aim and the reserve).
    pub fn most(&self) -> u64 {
        self.most
    }

    /// The clock against the stream's rate, less one (+0.001: 0.1 % fast).
    pub fn trim(&self) -> f64 {
        self.trim
    }

    pub fn ppm(&self) -> i32 {
        (self.trim * 1e6).round() as i32
    }

    /// What to do at `now`, the ring holding `used` samples.
    pub fn next(&mut self, now: Instant, used: u64) -> Next {
        let at = *self.due.get_or_insert(now);
        if now < at && used > 0 {
            return Next::Sleep((at - now).min(NAP));
        }
        if used > self.most {
            return Next::Room;
        }
        Next::Call { early: now < at }
    }

    /// The player was called at `now` (`early`: the ring ran empty first),
    /// the ring holding `used` samples just before: the next call's time,
    /// and the trim learns from a call that is not late (an early one says
    /// the ring ran empty: the clock is slow, or the reader makes up).
    pub fn called(&mut self, now: Instant, used: u64, early: bool) {
        let at = self.due.unwrap_or(now);
        let late = now.saturating_duration_since(at);
        self.learn(now, used, late.as_secs_f64() < self.period / 2.0);
        let p = Duration::from_secs_f64(self.period / (1.0 + self.trim));
        self.due = Some(if early || late > BEHIND { now + p } else { at + p });
    }

    /// The player was not called by the clock for a while (the client
    /// waited for room, the server held it or let it go): the next call is
    /// on time whenever it comes, and the window starts again.
    pub fn stood(&mut self) {
        self.due = None;
        self.since = None;
    }

    fn learn(&mut self, now: Instant, used: u64, counts: bool) {
        let dt = self.last.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64()).min(1.0);
        self.last = Some(now);
        let step = SLEW * dt;
        self.trim += (self.want - self.trim).clamp(-step, step);
        if !counts {
            return;
        }
        let since = *self.since.get_or_insert(now);
        self.sum += used as f64;
        self.n += 1;
        let w = now.saturating_duration_since(since).as_secs_f64();
        if w < WINDOW_S {
            return;
        }
        // Fuller than the aim: the reader takes less than the clock gives.
        let e = (self.sum / self.n as f64 - self.aim as f64) / self.per_s;
        // Only on an error the loop takes in whole, and while the trim
        // keeps up with its aim: a reserve filled while the trim was on its
        // way, or a trim held back by its slew, would wind the integral up
        // past the clock it finds.
        let whole = e.abs() < FAR;
        let e = e.clamp(-FAR, FAR);
        if whole && (self.want - self.trim).abs() <= SLEW * w {
            self.integral = (self.integral + e * w).clamp(-TRIM_MAX / KI, TRIM_MAX / KI);
        }
        self.want = (-KP * e - KI * self.integral).clamp(-TRIM_MAX, TRIM_MAX);
        self.since = Some(now);
        self.sum = 0.0;
        self.n = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client by this clock and a reader that takes the ring evenly at
    /// `speed` times the stream's rate, by ticks of 2 ms, in virtual time —
    /// standing still for `stand` (from, for) once. Gives the calls' times
    /// and the trim at the end.
    fn run(speed: f64, secs: f64, stand: Option<(f64, f64)>) -> (Vec<f64>, f64, u64) {
        let (rate, ch, block) = (48_000.0, 2u64, 4_096u64);
        let target = (0.05 * rate) as u64 * ch;
        let t0 = Instant::now();
        let mut c = Clock::new(rate * ch as f64, block, target, 1 << 21);
        let (mut used, mut owed, mut most) = (0u64, 0.0f64, 0u64);
        let mut calls = Vec::new();
        let (dt, tick) = (0.0002, 0.002);
        let (mut t, mut next_read) = (0.0, 0.0);
        while t < secs {
            let now = t0 + Duration::from_secs_f64(t);
            loop {
                match c.next(now, used) {
                    Next::Call { early } => {
                        c.called(now, used, early);
                        used += block;
                        most = most.max(used);
                        calls.push(t);
                    }
                    _ => break,
                }
            }
            if t >= next_read {
                next_read += tick;
                let standing = stand.is_some_and(|(from, len)| t >= from && t < from + len);
                owed += tick * speed * rate * ch as f64;
                if !standing {
                    let take = (owed as u64 / ch * ch).min(used);
                    used -= take;
                    owed -= take as f64;
                }
            }
            t += dt;
        }
        (calls, c.trim(), most)
    }

    /// A reader on the stream's rate, 0.1 % off it either way, or 0.3 %
    /// slow: the clock finds it to 0.01 %, and calls the player once a
    /// period — not one call bunched with another, not one late. (Far
    /// faster takes longer: the ring runs no emptier than empty.)
    #[test]
    fn the_clock_finds_the_reader_s_pace_and_calls_evenly() {
        let period = 4_096.0 / 96_000.0;
        for speed in [1.0, 1.001, 0.999, 0.997] {
            let (calls, trim, _) = run(speed, 240.0, None);
            assert!((trim - (speed - 1.0)).abs() < 1e-4, "at {speed}: the trim {trim} ({} calls)", calls.len());
            let gaps: Vec<f64> = calls.windows(2).filter(|w| w[0] > 60.0).map(|w| w[1] - w[0]).collect();
            let (lo, hi) = gaps.iter().fold((f64::MAX, 0.0f64), |(a, b), &g| (a.min(g), b.max(g)));
            assert!(lo > 0.9 * period / speed && hi < 1.1 * period / speed, "at {speed}: gaps {lo}..{hi} for {period}");
        }
    }

    /// The reader stands 150 ms: the calls go on by the clock, the reserve
    /// takes what the player gives meanwhile, and the reader takes it back —
    /// no call late; standing longer than the reserve holds, the client
    /// waits for room.
    #[test]
    fn the_reader_standing_is_not_heard_by_the_player() {
        let period = 4_096.0 / 96_000.0;
        let (calls, _, most) = run(1.0, 40.0, Some((30.0, 0.15)));
        let worst = calls.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(worst < 1.1 * period, "the longest gap {worst} for a period of {period}");
        assert!(most as f64 > 0.14 * 96_000.0, "the reserve took the stand: {most}");
        let (calls, _, _) = run(1.0, 40.0, Some((30.0, 0.4)));
        let worst = calls.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(worst > 0.2, "past the reserve the player waits: {worst}");
    }

    /// The ring empty before the call is due (the reader makes up a
    /// shortfall): the call comes at once; the aim is the room rule's fill,
    /// and some milliseconds at least.
    #[test]
    fn an_emptied_ring_is_filled_at_once() {
        let t0 = Instant::now();
        let mut c = Clock::new(96_000.0, 4_096, 9_600, 1 << 21);
        assert_eq!(c.aim(), 9_600 - 4_096);
        assert_eq!(c.most(), 9_600 - 4_096 + 19_200);
        assert_eq!(Clock::new(96_000.0, 16_384, 9_600, 1 << 21).aim(), 960, "a block over the target: the floor");
        assert_eq!(c.next(t0, 0), Next::Call { early: false });
        c.called(t0, 0, false);
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(c.next(t1, 3_000), Next::Sleep(NAP));
        assert_eq!(c.next(t1, 0), Next::Call { early: true });
        c.called(t1, 0, true);
        let t2 = t1 + Duration::from_millis(20);
        assert_eq!(c.next(t2, 3_000), Next::Sleep(NAP), "the clock goes on from the early call");
        let t3 = t1 + Duration::from_secs_f64(4_096.0 / 96_000.0);
        assert_eq!(c.next(t3, 3_000), Next::Call { early: false });
        assert_eq!(c.next(t3, c.most() + 1), Next::Room);
    }
}
