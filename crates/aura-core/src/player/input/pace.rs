//! The input reader's own clock once the chain plays.
//!
//! The chain takes its source in portions — a quarter of a second at a time,
//! whatever the rate or the filter (its output stages' step). Read from the
//! link only when the source runs short, the link would hand the player's
//! whole portion over at once: the driver calls the player when a block
//! fits, so the player would be called four to a dozen times in a
//! millisecond and then not for a quarter of a second. A player that reads a
//! file does not mind; one whose own source keeps its own clock — a bridge
//! from a system device, through a FIFO of a few blocks — runs dry and over,
//! and plays in jerks.
//!
//! So the reader takes the link evenly, by the clock: so many frames a
//! second, a little at a time, and the driver's calls follow, once a block's
//! time. The source holds the chain's next portion instead of the player:
//! it grows evenly between two portions and the chain takes it in one go.
//!
//! The pace is the stream's rate, trimmed so that over time the reader goes
//! exactly as fast as the chain reads — the device's clock: no drift,
//! nothing cut or repeated. The trim watches what the reader has fed past
//! what the chain has asked for — the source and what its stages still hold
//! together, since the stages hand frames on a block (93 ms) at a time and
//! the source alone moves in those steps — and keeps its low, over
//! `WINDOW_S`, at the margin and the stages' share. It moves slowly: a loop
//! of some eighty seconds, and never faster than `SLEW` a second, so a player
//! that measures the rate it is called at (a bridge) sees it steady to a
//! hundredth of a percent from one second to the next.
//!
//! Nothing is ever taken past the pace, not even when the chain waits (it
//! asked for more than the source holds): the player's own source keeps the
//! system's clock, and all the reader takes past the pace it loses — a
//! bridge's FIFO runs short by just as much, and taken in one go the player
//! is called a dozen times a moment. The render's look-ahead waits for the
//! chain then, and the trim takes the level up slowly. The level is set
//! right where the output is silent anyway: the start (the chain begins as
//! far into what gathered while it was built as its level allows) and a
//! switch (below). A player late with its blocks is caught up (`CREDIT_S`),
//! so its lateness costs no level. While the player does not play (between
//! two of its segments) the trim learns nothing.
//!
//! Another chain on the air (BIT-PERFECT turned on, another rack in the
//! place of the one there) may take its source in other portions and ask for
//! it less far ahead — BIT-PERFECT's goes a step of the render at a time and
//! has no subsonic guard on the output, so the level stands a third of a
//! second higher from then on. Drained, that would be minutes at the trim's
//! edge, and the cap would stop the reader at each portion's top and let it
//! go in a burst. So the level is left to settle (`SETTLE_S`), its low over
//! a window is the new aim — the music goes on from the same frame, its
//! delay as it was — and the cap is that far up too; the trim keeps the
//! clock it found. The aim may be lower than the one before, too, down to
//! a little past where the chain would wait (`FLOOR_S`): what the trim took up to the old one, the
//! player would lose. Nor does the credit pile up against the cap: a reader
//! stopped there goes on at the pace, not in one go.
//!
//! BIT-PERFECT turned on or off opens the device again, and there the
//! difference is put right at once, while the output is silent anyway
//! (`skip_to`, the controller's pre-roll): BIT-PERFECT's chain starts past
//! what the rack's held ahead of the sound, so the delay is its own; the
//! rack's waits on the device's start for what it holds ahead, the source
//! gathering it at the player's pace — the pause a little longer, the player
//! never called faster. Its level settles once it is on the air (`aired`);
//! the session's first chain's, too.

use std::collections::VecDeque;
use std::time::Instant;

/// How far the trim takes the pace from the stream's rate: ±0.5 % — a
/// device whose clock runs that far from the system's is still followed.
pub const TRIM_MAX: f64 = 0.005;
/// How fast the trim moves at most, a second: 100 ppm (0.01 %).
pub const SLEW: f64 = 1e-4;
/// How long the low is watched before the trim's aim moves, seconds:
/// thirty-two of the chain's portions, two of its cycles (below) — over
/// one, the low is the double's in one window and not in the next, and the
/// trim follows that.
pub const WINDOW_S: f64 = 8.0;
/// A chain's level settles on its low over two of its cycles, seconds: a rack's chain takes its
/// source in portions, two of them at once every few seconds (a long
/// filter's tail: 3.5 s for 30M taps ×16 at 48 kHz) — the level's low is
/// there.
pub const SETTLE_WATCH_S: f64 = 8.0;
/// The chain's one go — the most the source's level drops within this,
/// seconds (`burst`).
const BURST_S: f64 = 0.1;
/// The trim's gains: a low off its aim by `e` seconds is put right at
/// `KP·e` a second, and `KI` finds a device clock that runs apart from the
/// system's (the two critically damped: a loop of some eighty seconds —
/// a long chain's low moves by ±5 ms from one window to the next, the
/// portions it takes being of two sizes, and the trim is to keep the
/// device's clock through that to a hundredth of a percent).
const KP: f64 = 0.025;
const KI: f64 = KP * KP / 4.0;
/// The most error the loop takes in, seconds: a level further off its aim
/// (a hiccup) is put right no faster than this one (0.15 %).
const FAR: f64 = 0.06;
/// The session's log hears of the pace every this many windows (32 s;
/// each window in the bench).
const LOG_EVERY: u32 = if cfg!(test) { 1 } else { 4 };
/// The most the reader owes the link, seconds: a player late with its
/// blocks (its thread held up) is caught up — the level kept, not made up
/// later in a burst of its own; and the longest the reader may be away and
/// still owe the time: longer, it was held (paused), and goes on from now.
const CREDIT_S: f64 = 0.25;
const AWAY_S: f64 = 0.5;
/// Another chain on the air: how long its level is left to settle before
/// its low is watched for the aim (a window from then), seconds; and how
/// much higher the cap stands meanwhile.
pub const SETTLE_S: f64 = 1.0;
pub const SETTLE_ROOM_S: f64 = 1.0;
/// The least a settled level keeps past what the chain asks for at its
/// low, seconds: the low moves by up to 17 ms from one window to the next
/// on a long chain (30M taps ×16: two sizes of portion, 10.7 ms apart, and
/// the reader's tick) — at the edge, the chain would wait at each double.
const FLOOR_S: f64 = 0.03;
/// The longest a switch's chain is waited for to go on the air, seconds:
/// past it, its level settles from then.
const REOPEN_MAX_S: f64 = 5.0;

/// Where BIT-PERFECT's chain starts on the input at a switch, source frame:
/// the source then holds just the level the feed keeps past what it asks
/// for — `keep` past `reads` (the most the render holds read ahead of the
/// sound) — by the time the device plays its first frame, `opening` frames
/// of the player's on (the output's fade and the device's opening); never
/// before `after_fade` (heard once the output has faded: nothing twice),
/// nor past the source's `end`.
pub fn skip_to(after_fade: i64, end: i64, opening: i64, reads: i64, keep: i64) -> i64 {
    (end + opening - reads - keep).min(end).max(after_fade)
}

/// Another chain on the air: since when, and when it went on the air (a
/// switch's opens the device again first).
struct Settle {
    changed: Instant,
    aired: Option<Instant>,
}

/// The reader's clock for one session.
pub struct Pace {
    rate: f64,
    /// What the source keeps past what the chain has asked for at least,
    /// frames.
    margin: i64,
    /// The most the source stages hold back, frames.
    hold: i64,
    /// How far above the margin and the stages' share the level is kept,
    /// frames: what the chain on the air left there (it asks for its source
    /// less far ahead than the one the session began with).
    lift: i64,
    /// Another chain on the air since then: its level settling, and its low
    /// watched so far.
    settling: Option<Settle>,
    settle_low: i64,
    last: Option<Instant>,
    /// Frames the reader may take now.
    credit: f64,
    /// The trim the pace runs at, and the one the loop aims at.
    trim: f64,
    aim: f64,
    integral: f64,
    /// The least fed past what was asked for in this window, and when it
    /// began.
    low: i64,
    since: Option<Instant>,
    /// The window before's low: the error is the lower of the two (a
    /// window that missed the chain's deepest go reads high alone).
    last_low: i64,
    /// Windows ended, for the log; times the chain waited for the source on
    /// the air, and whether it does.
    windows: u32,
    waits: u32,
    waiting: bool,
    told: Option<Instant>,
    /// What was ahead of the chain over the last `BURST_S`, and the most it
    /// took in one go so far (`burst`).
    recent: VecDeque<(Instant, i64)>,
    burst: i64,
}

impl Pace {
    /// A clock for a stream at `rate` that keeps `margin` frames in the
    /// source past what the chain asks for, behind source stages that hold
    /// back `hold` frames at most.
    pub fn new(rate: u32, margin: i64, hold: i64) -> Pace {
        Pace {
            rate: rate.max(1) as f64,
            margin: margin.max(0),
            hold: hold.max(0),
            lift: 0,
            settling: None,
            settle_low: i64::MAX,
            last: None,
            credit: 0.0,
            trim: 0.0,
            aim: 0.0,
            integral: 0.0,
            low: i64::MAX,
            since: None,
            last_low: i64::MAX,
            windows: 0,
            waits: 0,
            waiting: false,
            told: None,
            recent: VecDeque::new(),
            burst: 0,
        }
    }

    /// Frames the reader may take from the link now, the source holding
    /// `ahead` past what the chain has asked for (less than nothing: the
    /// chain waits) and its stages `held` more, and at most `cap` ahead.
    /// `playing`: the player plays (the trim learns only then).
    pub fn room(&mut self, now: Instant, ahead: i64, held: i64, cap: i64, playing: bool) -> i64 {
        let dt = self.last.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        self.last = Some(now);
        let step = SLEW * dt.min(1.0);
        self.trim += (self.aim - self.trim).clamp(-step, step);
        let most = CREDIT_S * self.rate;
        let due = if dt > AWAY_S { 0.0 } else { dt * self.rate * (1.0 + self.trim) };
        // Against the cap nothing is owed: a reader stopped there goes on at
        // the pace once the chain takes, not with all the time it stood.
        self.credit = (self.credit + due).min(most).min((cap - ahead).max(0) as f64);
        // The level as it is once the reader has what it is owed: a reader
        // held up (its thread, 20–30 ms now and then) is no level short —
        // the trim, the settling and the chain's one go see past it.
        let owed = self.credit as i64;
        let ahead_owed = ahead + owed;
        let settling = playing && self.settle(now, ahead_owed, held);
        // The chain waits for the source, or the level is far below what
        // the trim keeps: nothing is made up at once. What the reader takes
        // past its pace, the player's own source loses — a bridge's FIFO,
        // filled by the system's clock, runs short by just as much, and in
        // one go it is called a dozen times a moment (a rate of 53 kHz to
        // the bridge). The render's look-ahead waits for the chain instead,
        // and the trim takes the level up at its own slow pace. Where the
        // level is set right — the start, a switch — the output is silent
        // anyway (`skip_to`, the device waiting for the level), so this is
        // a hiccup's, rare.
        if ahead_owed < 0 && playing && !self.reopening() {
            if !self.waiting {
                self.waiting = true;
                self.waits += 1;
                // A line a log window at most: the summary counts them.
                if self.told.is_none_or(|t| now.saturating_duration_since(t).as_secs_f64() >= WINDOW_S * LOG_EVERY as f64) {
                    self.told = Some(now);
                    crate::aelog!(
                        "[INPUT] pace: the chain waits for {:.3} s of the source — the render's look-ahead takes it, the player is not hurried",
                        -ahead_owed as f64 / self.rate
                    );
                }
            }
        } else {
            self.waiting = false;
        }
        if playing && !settling {
            self.watch(now, ahead_owed + held, dt);
            self.watch_burst(now, ahead_owed);
        }
        if !playing {
            self.since = None;
        }
        owed.min(cap - ahead)
    }

    /// The most the source may hold past what the chain has asked for:
    /// `cushion` over the margin and the lift — the chain's portion never
    /// meets it, only a chain that stopped taking; more while another
    /// chain's level settles.
    pub fn cap(&self, cushion: i64) -> i64 {
        let settling = if self.settling.is_some() { (SETTLE_ROOM_S * self.rate) as i64 } else { 0 };
        // A chain that takes two portions at once now and then holds the
        // level higher by one: the cap too.
        self.margin + self.lift + cushion + self.burst + settling
    }

    /// Another chain plays on (BIT-PERFECT turned on, another rack in the
    /// place of the one there): its level settles, then its low is the aim.
    /// The trim goes on at the clock it found. `reopen`: it goes on the air
    /// on a device opened again (`aired`), and settles from then.
    pub fn chain_changed(&mut self, now: Instant, reopen: bool) {
        self.settling = Some(Settle { changed: now, aired: (!reopen).then_some(now) });
        self.settle_low = i64::MAX;
        self.recent.clear();
        self.burst = 0;
        self.lift = 0;
        self.aim = (-KI * self.integral).clamp(-TRIM_MAX, TRIM_MAX);
        self.since = None;
    }

    /// A switch's chain went on the air.
    pub fn aired(&mut self, now: Instant) {
        if let Some(s) = self.settling.as_mut() {
            s.aired.get_or_insert(now);
        }
    }

    /// A switch's chain is not on the air yet.
    fn reopening(&self) -> bool {
        self.settling.as_ref().is_some_and(|s| s.aired.is_none())
    }

    /// While another chain's level settles: true until it has, and then
    /// the lift from its low over a window — below the margin too, down to
    /// `FLOOR_S` past what the chain asks for: the level is the chain's own, and
    /// what the trim took up to the old aim the player would lose (a
    /// bridge's FIFO). The most the chain takes in one go is watched with
    /// the low, so the cap has it from then on.
    fn settle(&mut self, now: Instant, ahead: i64, held: i64) -> bool {
        let level = ahead + held;
        let Some(s) = self.settling.as_mut() else { return false };
        let from = match s.aired {
            Some(t) => t,
            None if now.saturating_duration_since(s.changed).as_secs_f64() < REOPEN_MAX_S => return true,
            None => *s.aired.insert(now),
        };
        let t = now.saturating_duration_since(from).as_secs_f64();
        if t < SETTLE_S {
            return true;
        }
        self.settle_low = self.settle_low.min(level);
        self.watch_burst(now, ahead);
        if t < SETTLE_S + SETTLE_WATCH_S {
            return true;
        }
        self.settling = None;
        let floor = ((FLOOR_S * self.rate) as i64 - self.margin).min(0);
        self.lift = (self.settle_low - self.margin - self.hold).max(floor);
        self.since = None;
        crate::aelog!(
            "[INPUT] pace: another chain plays on; the level's aim {:.3} s ({:+.3} s on what the source keeps)",
            (self.margin + self.hold + self.lift) as f64 / self.rate,
            self.lift as f64 / self.rate
        );
        false
    }

    /// The most the chain took of its source in one go so far (two portions
    /// at once, a long filter's), frames: what the device waits for past the
    /// level after a switch to it (`RadioShared::level_short`). Since its
    /// level settled; another chain's starts again.
    pub fn burst(&self) -> i64 {
        self.burst
    }

    fn watch_burst(&mut self, now: Instant, ahead: i64) {
        while self.recent.front().is_some_and(|r| now.saturating_duration_since(r.0).as_secs_f64() > BURST_S) {
            self.recent.pop_front();
        }
        self.recent.push_back((now, ahead));
        let high = self.recent.iter().map(|r| r.1).max().unwrap_or(ahead);
        self.burst = self.burst.max(high - ahead);
    }

    /// The reader took `frames` from the link.
    pub fn took(&mut self, frames: i64) {
        self.credit = (self.credit - frames as f64).max(0.0);
    }

    /// The pace against the stream's rate, less one (+0.001: 0.1 % faster).
    #[cfg(test)]
    pub fn trim(&self) -> f64 {
        self.trim
    }

    /// The low of what is fed past what was asked for, over a window, and
    /// the aim from it at its end. A window the reader was away for (held:
    /// nothing is read) starts again.
    fn watch(&mut self, now: Instant, fed: i64, away: f64) {
        let Some(since) = self.since.filter(|_| away <= WINDOW_S / 2.0) else {
            self.since = Some(now);
            self.low = fed;
            self.last_low = i64::MAX;
            return;
        };
        self.low = self.low.min(fed);
        let w = now.saturating_duration_since(since).as_secs_f64();
        if w < WINDOW_S {
            return;
        }
        let e = (self.low.min(self.last_low) - self.margin - self.hold - self.lift) as f64 / self.rate;
        self.last_low = self.low;
        // Only while the trim keeps up with its aim and the aim is not at its
        // edge: a trim held back by its slew or its reach (a level far off,
        // nothing made up at once) would wind the integral up past the clock
        // it finds, and the loop would hunt.
        // Nor while the level is far off (a hiccup): the clock is found by
        // the level's drift, not by a level put right — wound up there, the
        // integral would take it past its aim for minutes.
        let near = e.abs() < FAR;
        let e = e.clamp(-FAR, FAR);
        let integral = (self.integral + e * w).clamp(-TRIM_MAX / KI, TRIM_MAX / KI);
        if near && (self.aim - self.trim).abs() <= SLEW * w && (KP * e + KI * integral).abs() < TRIM_MAX {
            self.integral = integral;
        }
        self.aim = (-KP * e - KI * self.integral).clamp(-TRIM_MAX, TRIM_MAX);
        self.windows += 1;
        if self.windows % LOG_EVERY == 0 {
            crate::aelog!(
                "[INPUT] pace {:+.0} ppm against the system clock; the level's low {:.3} s for {:.3} s; the chain waited {} times",
                self.trim * 1e6,
                self.low as f64 / self.rate,
                (self.margin + self.hold + self.lift) as f64 / self.rate,
                self.waits
            );
        }
        self.since = Some(now);
        self.low = fed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole way in virtual time, every half millisecond: the player
    /// called as the driver calls it (a block of 2048 when it fits the link,
    /// whose target is 0.05 s), the reader every 2 ms taking from the link by
    /// `Pace` (on at once while it takes something, as the reader's loop
    /// does), the source stages handing frames on in blocks of 4096 after
    /// holding back half a subsonic filter (0.45 s), and the chain taking
    /// its source in portions (12288, 12288, 8192 frames — what the bench
    /// sees) as its device plays, `speed` times the system clock. The
    /// player's own source is a bridge's: a FIFO of four blocks filled by the
    /// system clock, one taken out at each call (from `settle` on, half full
    /// then).
    struct Sim {
        rate: f64,
        /// When the player was called, seconds (from `settle` on).
        calls: Vec<f64>,
        /// The trim each second (from `settle` on).
        trims: Vec<f64>,
        /// The chain asked for more than the source had (from `settle` on).
        waits: u32,
        /// The bridge's FIFO: blocks of silence played (short), blocks
        /// dropped (full).
        dry: u32,
        over: u32,
    }

    fn simulate(rate: u32, speed: f64, secs: f64, settle: f64) -> Sim {
        simulate_switch(rate, speed, secs, settle, None)
    }

    /// The chain on the air changes at `switch.0` s to one that takes its
    /// source in portions of `switch.2` frames and asks for it `switch.1` s
    /// less far ahead (BIT-PERFECT's after a rack's: a step of the render at
    /// the stream's rate, no subsonic guard on the output) — the reader told
    /// of it, as `Feed` tells it.
    fn simulate_switch(rate: u32, speed: f64, secs: f64, settle: f64, switch: Option<(f64, f64, i64)>) -> Sim {
        simulate_with(rate, speed, secs, settle, switch, false, None, &[12_288, 12_288, 8_192], None)
    }

    /// A call of the player, from `settle` on: the bridge's FIFO, filled
    /// by the system clock, gives a block up — one too many is dropped, one
    /// short is silence.
    fn call(out: &mut Sim, t: f64, settle: f64, blocks_per_s: f64, fifo: &mut f64, fifo_at: &mut f64) {
        if t < settle {
            return;
        }
        out.calls.push(t);
        *fifo += (t - *fifo_at) * blocks_per_s;
        *fifo_at = t;
        if *fifo > 4.0 {
            out.over += 1;
            *fifo -= 1.0;
        }
        if *fifo < 1.0 {
            out.dry += 1;
        } else {
            *fifo -= 1.0;
        }
    }

    /// And `clock`: the driver calls the player by its own clock
    /// (`aura_link::clock`, as it does while the reader is even), not when
    /// the reader makes room; `stand`: the reader stands still from
    /// `stand.0` s for `stand.1` s (its thread held up); `portions`: what
    /// the chain takes of its source, in turn; `first`: the session's start
    /// as `Feed` has it — the chain's level left to settle from going on
    /// the air, the source short of the set level by this many seconds.
    fn simulate_with(
        rate: u32,
        speed: f64,
        secs: f64,
        settle: f64,
        switch: Option<(f64, f64, i64)>,
        clock: bool,
        stand: Option<(f64, f64)>,
        portions: &[i64],
        first: Option<f64>,
    ) -> Sim {
        let r = rate as f64;
        let block = 2_048i64;
        let target = (0.05 * r) as i64;
        let margin = (0.05 * r) as i64;
        let (sub_block, h2) = (4_096i64, (0.45 * r) as i64);
        let hold = h2 + sub_block;
        let cushion = (0.5 * r) as i64;
        let end_of = |fed: i64| if fed > h2 { (fed - h2) / sub_block * sub_block } else { 0 };
        let mut portions = portions.to_vec();
        let mut lead = (0.5 * r) as i64;
        let t0 = Instant::now();
        let mut p = Pace::new(rate, margin, hold);
        // Built and steady: the stages' share and a margin past the first
        // portion, and past the largest (set right at the start).
        let mut asked = portions[0];
        let mut k = 1;
        let mut fed = hold + margin + asked + portions.iter().copied().max().unwrap_or(asked) - asked - (first.unwrap_or(0.0) * r) as i64;
        if first.is_some() {
            p.chain_changed(t0, false);
        }
        // The device a portion short of the chain's next ask: it has just
        // taken the first (from there on the level's low is the stages'
        // share and the margin).
        let device0 = (asked - lead - portions[0]) as f64;
        let mut ring = 0i64;
        let mut fifo = 2.0f64;
        let mut fifo_at = settle;
        let mut out = Sim { rate: r, calls: Vec::new(), trims: Vec::new(), waits: 0, dry: 0, over: 0 };
        let (dt, tick) = (0.0005, 0.002);
        let mut next_read = 0.0;
        let mut next_second = settle;
        let mut t = 0.0;
        let mut switched = false;
        let mut driver = clock.then(|| aura_link::clock::Clock::new(r, block as u64, target as u64, 1 << 21));
        let blocks_per_s = r / block as f64;
        while t < secs {
            if let Some((_, less, portion)) = switch.filter(|s| !switched && t >= s.0) {
                switched = true;
                portions = vec![portion];
                lead -= (less * r) as i64;
                k = 0;
                p.chain_changed(t0 + Duration::from_secs_f64(t), false);
            }
            // The chain asks for its next portion as the device nears what it has.
            let device = device0 + speed * r * t;
            while device >= (asked - lead) as f64 {
                asked += portions[k % portions.len()];
                k += 1;
                if end_of(fed) < asked && t >= settle {
                    out.waits += 1;
                }
            }
            // The driver by its own clock.
            if let Some(c) = driver.as_mut() {
                let now = t0 + Duration::from_secs_f64(t);
                while let aura_link::clock::Next::Call { early } = c.next(now, ring as u64) {
                    c.called(now, ring as u64, early);
                    ring += block;
                    call(&mut out, t, settle, blocks_per_s, &mut fifo, &mut fifo_at);
                }
            }
            // The reader: not while it stands; its next tick after.
            if stand.is_some_and(|(from, len)| t >= from && t < from + len) {
                next_read = t + tick;
            } else if t >= next_read {
                next_read += tick;
                loop {
                    let end = end_of(fed);
                    let now = t0 + Duration::from_secs_f64(t);
                    let cap = p.cap(cushion);
                    let room = p.room(now, end - asked, fed - end, cap, true);
                    let take = room.min(ring);
                    if take > 0 {
                        ring -= take;
                        fed += take;
                        p.took(take);
                    }
                    // By the room rule the driver hears the room at once.
                    let mut called = false;
                    while driver.is_none() && (ring == 0 || ring + block <= target) {
                        ring += block;
                        called = true;
                        call(&mut out, t, settle, blocks_per_s, &mut fifo, &mut fifo_at);
                    }
                    if take <= 0 || !called || room <= take {
                        break;
                    }
                }
            }
            if t >= next_second {
                next_second += 1.0;
                out.trims.push(p.trim());
            }
            t += dt;
        }
        out
    }

    /// The pace of the calls, off the stream's rate, each second: their
    /// slope over the 5 s from there (least squares: a call is late by up to
    /// a tick of the reader, 2 ms, which over a second alone reads as a
    /// tenth of a percent).
    fn pace_by_the_second(s: &Sim) -> Vec<f64> {
        let period = 2_048.0 / s.rate;
        let c = &s.calls;
        let mut out = Vec::new();
        let mut from = c[0];
        while from + 5.0 <= c[c.len() - 1] {
            let w: Vec<(f64, f64)> = c.iter().enumerate().filter(|(_, &x)| x >= from && x < from + 5.0).map(|(i, &x)| (i as f64, x)).collect();
            let n = w.len() as f64;
            let (mi, mx) = (w.iter().map(|q| q.0).sum::<f64>() / n, w.iter().map(|q| q.1).sum::<f64>() / n);
            let b = w.iter().map(|q| (q.0 - mi) * (q.1 - mx)).sum::<f64>() / w.iter().map(|q| (q.0 - mi).powi(2)).sum::<f64>();
            out.push(period / b - 1.0);
            from += 1.0;
        }
        out
    }

    /// Runs `speed` for seven minutes and checks the last two: the trim
    /// within 0.02 % of the device's clock against the system's, every
    /// second; the calls' pace within 0.025 % of it (each call lands on a
    /// tick of the reader, and over 5 s that beat reads as 0.02 %); the
    /// chain never waits.
    fn settles(rate: u32, speed: f64) -> Sim {
        let s = simulate(rate, speed, 420.0, 300.0);
        let want = speed - 1.0;
        let worst_trim = s.trims.iter().map(|t| (t - want).abs()).fold(0.0, f64::max);
        let pace = pace_by_the_second(&s);
        let worst_pace = pace.iter().map(|x| (x - want).abs()).fold(0.0, f64::max);
        let said = format!(
            "{rate} Hz, the device {:+.3} %: trim {:+.4}..{:+.4} %, the calls' pace {:+.4}..{:+.4} %, waits {}, the bridge dry {} over {}",
            want * 100.0,
            s.trims.iter().copied().fold(f64::MAX, f64::min) * 100.0,
            s.trims.iter().copied().fold(f64::MIN, f64::max) * 100.0,
            pace.iter().copied().fold(f64::MAX, f64::min) * 100.0,
            pace.iter().copied().fold(f64::MIN, f64::max) * 100.0,
            s.waits,
            s.dry,
            s.over
        );
        println!("{said}");
        assert!(worst_trim <= 2e-4, "the trim holds the device's clock: {said}");
        assert!(worst_pace <= 2.5e-4, "the calls keep its pace: {said}");
        assert_eq!(s.waits, 0, "{said}");
        s
    }

    /// The device on the system's clock, or 100 ppm apart: the calls keep
    /// its pace to 0.02 %, and a bridge's FIFO of four blocks neither runs
    /// dry nor over in two minutes.
    #[test]
    fn the_pace_holds_the_device_s_clock_smoothly() {
        for (rate, speed) in [(48_000, 1.0), (48_000, 1.0001), (48_000, 0.9999), (44_100, 1.0)] {
            let s = settles(rate, speed);
            assert_eq!((s.dry, s.over), (0, 0), "{rate} Hz at {speed}: the bridge's FIFO");
        }
    }

    /// A device 0.1 % off the system's clock (ten times a poor crystal's
    /// error): the reader follows it, and a bridge on the system's clock has
    /// to drop or repeat blocks at that difference — no more often. (0.3 %
    /// off, the level falls far below its aim before the trim is there, and
    /// with nothing made up at once the trim hunts at its edge.)
    #[test]
    fn the_pace_follows_a_device_far_off_the_system_s_clock() {
        for speed in [1.001, 0.999] {
            let s = settles(48_000, speed);
            let blocks = (speed - 1.0).abs() * 120.0 * 48_000.0 / 2_048.0;
            let events = (s.dry + s.over) as f64;
            assert!(events <= blocks + 3.0, "at {speed}: {events} FIFO events for {blocks:.1} blocks of clock apart");
        }
    }

    /// BIT-PERFECT turned on under a steady stream: the direct chain takes
    /// its source a step of the render at a time (8192 frames, 0.17 s) and
    /// asks for it 0.33 s less far ahead than the rack's did, so the level
    /// stands that much higher from then on — its aim follows, and the
    /// player's calls keep their pace from the switch on: none bunched, none
    /// late, a bridge's FIFO neither dry nor over, the trim on the clock.
    #[test]
    fn a_chain_that_looks_less_far_ahead_keeps_the_calls_even() {
        for speed in [1.0, 1.0001] {
            let s = simulate_switch(48_000, speed, 340.0, 300.0, Some((300.0, 0.33, 8_192)));
            let period = 2_048.0 / s.rate;
            let gaps: Vec<f64> = s.calls.windows(2).map(|w| w[1] - w[0]).collect();
            let bunched = gaps.iter().filter(|&&g| g < 0.5 * period).count();
            let late = gaps.iter().filter(|&&g| g > 1.5 * period).count();
            let want = speed - 1.0;
            let worst_trim = s.trims.iter().map(|t| (t - want).abs()).fold(0.0, f64::max);
            let pace = pace_by_the_second(&s);
            let worst_pace = pace.iter().map(|x| (x - want).abs()).fold(0.0, f64::max);
            let said = format!(
                "the device {:+.3} %: {} calls, {bunched} bunched, {late} late; trim {:+.4}..{:+.4} %, the calls' pace {:+.4}..{:+.4} %, waits {}, the bridge dry {} over {}",
                want * 100.0,
                s.calls.len(),
                s.trims.iter().copied().fold(f64::MAX, f64::min) * 100.0,
                s.trims.iter().copied().fold(f64::MIN, f64::max) * 100.0,
                pace.iter().copied().fold(f64::MAX, f64::min) * 100.0,
                pace.iter().copied().fold(f64::MIN, f64::max) * 100.0,
                s.waits,
                s.dry,
                s.over
            );
            println!("{said}");
            assert_eq!((bunched, late), (0, 0), "the calls one a block's time apart: {said}");
            assert_eq!((s.dry, s.over), (0, 0), "{said}");
            assert_eq!(s.waits, 0, "{said}");
            assert!(worst_trim <= 2e-4, "the trim holds the device's clock: {said}");
            assert!(worst_pace <= 2.5e-4, "the calls keep its pace: {said}");
        }
    }

    /// The driver on its own clock, the reader's pace on the device's: the
    /// two loops settle together — the calls once a period, their pace on
    /// the device's to 0.02 %, a bridge's FIFO untouched — and the reader
    /// standing 150 ms (its thread held up) is the reserve's, not the
    /// player's. By the room rule the player waits as long.
    #[test]
    fn the_driver_s_own_clock_keeps_the_calls_even_through_the_reader_s_stand() {
        let period = 2_048.0 / 48_000.0;
        let gaps = |s: &Sim| -> (usize, usize, f64) {
            let g: Vec<f64> = s.calls.windows(2).map(|w| w[1] - w[0]).collect();
            (g.iter().filter(|&&x| x < 0.5 * period).count(), g.iter().filter(|&&x| x > 1.5 * period).count(), g.iter().copied().fold(0.0, f64::max))
        };
        for speed in [1.0, 1.0001, 0.9999] {
            let s = simulate_with(48_000, speed, 420.0, 300.0, None, true, Some((360.0, 0.15)), &[12_288, 12_288, 8_192], None);
            let (bunched, late, longest) = gaps(&s);
            let want = speed - 1.0;
            let worst_trim = s.trims.iter().map(|t| (t - want).abs()).fold(0.0, f64::max);
            let pace = pace_by_the_second(&s);
            let worst_pace = pace.iter().map(|x| (x - want).abs()).fold(0.0, f64::max);
            let said = format!(
                "the device {:+.3} %: {} calls, {bunched} bunched, {late} late (the longest {:.1} ms); trim {:+.4}..{:+.4} %, the calls' pace {:+.4}..{:+.4} %, the bridge dry {} over {}",
                want * 100.0,
                s.calls.len(),
                longest * 1e3,
                s.trims.iter().copied().fold(f64::MAX, f64::min) * 100.0,
                s.trims.iter().copied().fold(f64::MIN, f64::max) * 100.0,
                pace.iter().copied().fold(f64::MAX, f64::min) * 100.0,
                pace.iter().copied().fold(f64::MIN, f64::max) * 100.0,
                s.dry,
                s.over
            );
            println!("{said}");
            assert_eq!((bunched, late), (0, 0), "{said}");
            assert_eq!((s.dry, s.over), (0, 0), "{said}");
            assert!(worst_trim <= 2e-4, "the trim holds the device's clock: {said}");
            assert!(worst_pace <= 2.5e-4, "the calls keep its pace: {said}");
        }
        let s = simulate_with(48_000, 1.0, 420.0, 300.0, None, false, Some((360.0, 0.15)), &[12_288, 12_288, 8_192], None);
        let (_, late, longest) = gaps(&s);
        println!("by the room rule: {late} late, the longest gap {:.1} ms", longest * 1e3);
        assert!(late >= 1 && longest > 0.14, "by the room rule the stand reaches the player");
    }

    /// A rack whose chain takes two portions at once every few seconds
    /// (30M taps ×16 on the card at 48 kHz: its look-ahead 0.75 ↔ 1.03 s
    /// every 3.5 s), the session started as `Feed` starts it — the level
    /// left to settle from going on the air, the source set right or short
    /// of it by 20 ms (the margin's 50 down to `FLOOR_S`): the aim is the level the chain settled at, the most
    /// it takes in one go is under the cap before the settling is over, so
    /// from then on the chain never waits, the player is called once a
    /// block's time at the device's pace (to 0.05 % over every 10 s) and a
    /// bridge's FIFO is untouched. As it was, the first double past the
    /// settling met the cap (the reader stood, its time lost), the level
    /// below the aim was taken up from the player — the trim's integral
    /// wound up at the edge of its error, the pace 0.1–0.3 % off for
    /// minutes and the chain waiting at each double meanwhile.
    #[test]
    fn a_chain_that_takes_two_portions_at_once_now_and_then_keeps_the_calls_even() {
        let mut portions = vec![12_288i64; 13];
        portions.push(24_576);
        for (speed, clock, short) in [(1.0, false, 0.0), (1.0001, true, 0.0), (1.0, true, 0.02), (0.9999, true, 0.02)] {
            let s = simulate_with(48_000, speed, 180.0, 30.0, None, clock, None, &portions, Some(short));
            let period = 2_048.0 / s.rate;
            let gaps: Vec<f64> = s.calls.windows(2).map(|w| w[1] - w[0]).collect();
            let bunched = gaps.iter().filter(|&&g| g < 0.5 * period).count();
            let late = gaps.iter().filter(|&&g| g > 1.5 * period).count();
            let want = speed - 1.0;
            let pace = pace_by_ten(&s, 30.0);
            let worst = pace.iter().map(|x| (x - want).abs()).fold(0.0, f64::max);
            let worst_trim = s.trims.iter().map(|t| (t - want).abs()).fold(0.0, f64::max);
            let said = format!(
                "the device {:+.3} % ({}), the source {:.0} ms short: {} calls, {bunched} bunched, {late} late; the calls' pace by 10 s {}; trim {:+.4}..{:+.4} %; waits {}, the bridge dry {} over {}",
                want * 100.0,
                if clock { "the driver's own clock" } else { "the room rule" },
                short * 1e3,
                s.calls.len(),
                pace.iter().map(|x| format!("{:+.3}", x * 100.0)).collect::<Vec<_>>().join(" "),
                s.trims.iter().copied().fold(f64::MAX, f64::min) * 100.0,
                s.trims.iter().copied().fold(f64::MIN, f64::max) * 100.0,
                s.waits,
                s.dry,
                s.over
            );
            println!("{said}");
            assert_eq!((bunched, late), (0, 0), "{said}");
            assert_eq!((s.waits, s.dry, s.over), (0, 0, 0), "{said}");
            assert!(worst <= 5e-4, "the calls' pace within 0.05 % by 10 s: {said}");
            assert!(worst_trim <= 2e-4, "the trim holds the device's clock: {said}");
        }
    }

    /// A chain whose level settled low — at its deepest, just what it asks
    /// for: the aim is `FLOOR_S` past that, not at the edge (where it would
    /// wait at each double); one that settled high keeps its own.
    #[test]
    fn a_level_that_settled_low_is_aimed_a_little_above_the_chain_s_edge() {
        const RATE: u32 = 48_000;
        for (low, lift) in [(0i64, (FLOOR_S * RATE as f64) as i64 - 4_800), (9_600, 4_800)] {
            let t0 = Instant::now();
            let mut p = Pace::new(RATE, 4_800, 0);
            p.chain_changed(t0, false);
            let mut t = 0.0;
            while t < SETTLE_S + SETTLE_WATCH_S + 0.1 {
                // A saw-tooth of a portion (0.25 s) down to `low`.
                let ahead = low + (12_000.0 * (t * 4.0 % 1.0)) as i64;
                let room = p.room(t0 + Duration::from_secs_f64(t), ahead, 0, 1 << 30, true);
                p.took(room);
                t += 0.002;
            }
            assert!(p.settling.is_none(), "settled");
            // What the reader is owed at a tick (2 ms) is counted with it.
            assert!((lift..=lift + 200).contains(&p.lift), "settled with its low at {low}: lift {}", p.lift);
        }
    }

    /// How the input goes through BIT-PERFECT turned on or off.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Switching {
        /// BIT-PERFECT's chain from the place heard once the output has
        /// faded, the rack's a lead (0.47 s) past it; the pace told when the
        /// chain is built; the device started by the pre-roll alone.
        Before,
        /// `skip_to`; the rack's from where the output faded; the device
        /// waiting for the chain's level (`RadioShared::level_short`); the
        /// pace told when the chain goes on the air.
        After,
    }

    /// A chain in the model of a switch: it asks for its next portion of
    /// the source once it holds less than `reads` ahead of the sound and has
    /// its last one in (the render's look-ahead, and what the chain's own
    /// stages hold ahead of it).
    struct Reader {
        portions: Vec<i64>,
        k: usize,
        asked: i64,
        reads: i64,
    }

    impl Reader {
        fn new(direct: bool, r: f64, from: i64) -> Reader {
            // BIT-PERFECT: a step of the render (8192) past its 0.5 s; the
            // rack: the convolver's portions past the render's 0.5 s, the
            // subsonic guard's 0.54 s, the filter's look-ahead and a step —
            // 1.2..1.45 s ahead of the sound, as the window of 17.10 saw.
            let (portions, reads_s) = if direct { (vec![8_192], 0.5) } else { (vec![12_288, 12_288, 8_192], 1.2) };
            Reader { portions, k: 0, asked: from, reads: (reads_s * r) as i64 }
        }

        /// What the chain has made of the source, the source ending at `end`.
        fn rendered(&self, end: i64) -> i64 {
            if self.asked <= end || self.k == 0 {
                self.asked
            } else {
                self.asked - self.portions[(self.k - 1) % self.portions.len()]
            }
        }
    }

    /// What a switch did: the player's calls (the bridge's FIFO in `calls`),
    /// the delay from the player to the sound every 0.1 s, the times the
    /// chain waited for the source on the air from the first switch on, the device run
    /// dry, and when each chain went on the air.
    struct Reopen {
        calls: Sim,
        delay: Vec<(f64, f64)>,
        waits: u32,
        underruns: u32,
        aired: Vec<f64>,
    }

    /// The input through BIT-PERFECT turned on at `on` s and off at `off`
    /// s, every half millisecond: the device at `speed` times the system
    /// clock, its opening taking `open_s` (the controller reckons on
    /// `open_guess_s`, the last one's), the driver calling the player by its
    /// own clock, the reader every 2 ms by `Pace`, the source stages handing
    /// frames on in blocks of 4096 after half a subsonic filter (0.45 s). A
    /// switch: the chain built, the output's fade (0.13 s) on the old one,
    /// then the new one reads while the device opens and starts — once the
    /// pre-roll is in (0.3 s), and as `how` has it. The source's reader edge
    /// is the furthest any chain has asked for, as `LiveSource`'s is.
    fn simulate_reopen(speed: f64, open_s: f64, open_guess_s: f64, how: Switching, on: f64, off: f64, secs: f64, direct0: bool) -> Reopen {
        let r = 48_000f64;
        let block = 2_048i64;
        let target = (0.05 * r) as i64;
        let margin = (0.05 * r) as i64;
        let (sub_block, h2) = (4_096i64, (0.45 * r) as i64);
        let hold = h2 + sub_block;
        let keep = margin + hold;
        let cushion = (0.5 * r) as i64;
        let fade = (0.13 * r) as i64;
        let end_of = |fed: i64| if fed > h2 { (fed - h2) / sub_block * sub_block } else { 0 };
        let t0 = Instant::now();
        let at = |t: f64| t0 + Duration::from_secs_f64(t);
        let mut p = Pace::new(r as u32, margin, hold);
        let mut cur = Reader::new(direct0, r, 0);
        cur.asked = cur.reads + cur.portions[0];
        let mut furthest = cur.asked;
        let mut steps = [0i64; 4];
        let mut step_at = 0usize;
        let mut fed = cur.asked + margin + hold;
        let mut produced = fed;
        let mut heard = 0.0f64;
        let mut device_on = true;
        let mut ring = 0i64;
        let mut fifo = 2.0f64;
        let mut fifo_at = 5.0;
        let mut out = Reopen {
            calls: Sim { rate: r, calls: Vec::new(), trims: Vec::new(), waits: 0, dry: 0, over: 0 },
            delay: Vec::new(),
            waits: 0,
            underruns: 0,
            aired: Vec::new(),
        };
        let mut driver = aura_link::clock::Clock::new(r, block as u64, target as u64, 1 << 21);
        let blocks_per_s = r / block as f64;
        let mut events = vec![(on, true), (off, false)];
        // A switch under way: the new chain and its start, when it reads
        // (the fade over) and whether the device has started for it.
        let mut next: Option<(f64, Reader, i64)> = None;
        let mut opening: Option<(f64, i64)> = None;
        let mut waits_from = None;
        let (dt, tick) = (0.0005, 0.002);
        let (mut t, mut next_read, mut next_note) = (0.0, 0.0, 0.0);
        while t < secs {
            let now = at(t);
            let end = end_of(fed);
            if let Some(i) = events.iter().position(|e| t >= e.0) {
                let (_, direct) = events.remove(i);
                waits_from.get_or_insert(p.waits);
                let after_fade = heard as i64 + fade;
                let start = match (how, direct) {
                    (Switching::Before, true) => after_fade,
                    (Switching::Before, false) => after_fade + (0.47 * r) as i64,
                    (Switching::After, true) => skip_to(after_fade, end, ((0.13 + open_guess_s) * r) as i64, (0.5 * r) as i64 + 8_192, keep - (fed - end)),
                    (Switching::After, false) => after_fade,
                };
                if how == Switching::After {
                    steps = [0; 4];
                }
                p.chain_changed(now, how == Switching::After);
                next = Some((t + 0.13, Reader::new(direct, r, start), start));
            }
            if next.as_ref().is_some_and(|n| t >= n.0) {
                let (_, reader, start) = next.take().unwrap();
                cur = reader;
                device_on = false;
                opening = Some((t, start));
            }
            // The chain asks; the source's edge is the furthest asked for.
            let held_at = if device_on { heard } else { opening.map_or(heard, |o| o.1 as f64) };
            while cur.asked <= end && ((cur.asked - cur.reads) as f64) < held_at {
                let portion = cur.portions[cur.k % cur.portions.len()];
                cur.k += 1;
                cur.asked += portion;
                if cur.asked > furthest {
                    steps[step_at % 4] = cur.asked - furthest;
                    step_at += 1;
                    furthest = cur.asked;
                }
            }
            // The device starts once it has opened and the pre-roll is in;
            // after a switch, as `how` has it.
            if let Some((from, start)) = opening {
                let buffered = cur.rendered(end) - start;
                let full = buffered >= cur.reads && cur.asked <= end;
                let need = keep + steps.iter().copied().max().unwrap_or(0) - (buffered - cur.reads).max(0);
                let short = how == Switching::After && (!full || (end - furthest) + (fed - end) < need);
                let late = t - from >= 3.0;
                if t - from >= open_s && ((buffered as f64 >= 0.3 * r && !short) || late) {
                    opening = None;
                    device_on = true;
                    heard = start as f64;
                    out.aired.push(t);
                    if how == Switching::After {
                        p.aired(now);
                    }
                }
            }
            if device_on {
                heard += speed * r * dt;
                let made = cur.rendered(end) as f64;
                if heard > made {
                    heard = made;
                    out.underruns += 1;
                }
            }
            while let aura_link::clock::Next::Call { early } = driver.next(now, ring as u64) {
                driver.called(now, ring as u64, early);
                ring += block;
                produced += block;
                call(&mut out.calls, t, 5.0, blocks_per_s, &mut fifo, &mut fifo_at);
            }
            if t >= next_read {
                next_read += tick;
                let end = end_of(fed);
                let cap = p.cap(cushion);
                let room = p.room(now, end - furthest, fed - end, cap, true);
                let take = room.min(ring);
                if take > 0 {
                    ring -= take;
                    fed += take;
                    p.took(take);
                }
            }
            if t >= next_note {
                next_note += 0.1;
                out.delay.push((t, (produced - heard as i64) as f64 / r));
            }
            t += dt;
        }
        out.waits = p.waits - waits_from.unwrap_or(p.waits);
        out
    }

    /// The mean delay over `from..to` s.
    fn mean_delay(s: &Reopen, from: f64, to: f64) -> f64 {
        let w: Vec<f64> = s.delay.iter().filter(|d| d.0 >= from && d.0 < to).map(|d| d.1).collect();
        w.iter().sum::<f64>() / w.len().max(1) as f64
    }

    /// The pace of the player's calls over each 10 s from `from` on, off
    /// the system clock: the frames called for over the time between the
    /// first call and the last.
    fn pace_by_ten(s: &Sim, from: f64) -> Vec<f64> {
        let period = 2_048.0 / s.rate;
        let mut out = Vec::new();
        let mut a = from;
        while a + 10.0 <= s.calls[s.calls.len() - 1] {
            let w: Vec<f64> = s.calls.iter().copied().filter(|&x| x >= a && x < a + 10.0).collect();
            out.push((w.len() - 1) as f64 * period / (w[w.len() - 1] - w[0]) - 1.0);
            a += 10.0;
        }
        out
    }

    /// The switches as they were and as they are, against each chain's own
    /// delay (that chain from the start): what is said of them, and whether
    /// they meet it — the delay within 0.05 s of the new chain's own
    /// (BIT-PERFECT's) and no more than a portion of the source over it
    /// (the rack's: its stages' phase is not to be seen from outside), the
    /// calls' pace within 0.05 % of the device's over every 10 s, the
    /// bridge's FIFO untouched, the chain never waiting on the air, the device never
    /// dry.
    fn switch_meets(speed: f64, open_s: f64, open_guess_s: f64, how: Switching) -> (bool, String) {
        let (on, off, end) = (200.0, 260.0, 320.0);
        let own_dsp = mean_delay(&simulate_reopen(speed, open_s, open_guess_s, how, f64::MAX, f64::MAX, 200.0, false), 150.0, 200.0);
        let own_bp = mean_delay(&simulate_reopen(speed, open_s, open_guess_s, how, f64::MAX, f64::MAX, 200.0, true), 150.0, 200.0);
        let s = simulate_reopen(speed, open_s, open_guess_s, how, on, off, end, false);
        let before = mean_delay(&s, on - 50.0, on);
        let bp = mean_delay(&s, on + 5.0, off);
        let dsp = mean_delay(&s, off + 5.0, end);
        let pace = pace_by_ten(&s.calls, on - 10.0);
        let want = speed - 1.0;
        let worst = pace.iter().map(|x| (x - want).abs()).fold(0.0, f64::max);
        let said = format!(
            "{how:?}, the device {:+.3} %, opening {:.2} s (reckoned {:.2}): the delay {:.3} s (its own {:.3}) -> BIT-PERFECT {:.3} s (its own {:.3}) -> the rack {:.3} s; the calls' pace by 10 s {}; the bridge dry {} over {}; the chain waited {}; the device dry {}; on the air {:?}",
            want * 100.0,
            open_s,
            open_guess_s,
            before,
            own_dsp,
            bp,
            own_bp,
            dsp,
            pace.iter().map(|x| format!("{:+.3}", x * 100.0)).collect::<Vec<_>>().join(" "),
            s.calls.dry,
            s.calls.over,
            s.waits,
            s.underruns,
            s.aired.iter().map(|a| format!("{a:.2}")).collect::<Vec<_>>()
        );
        let ok = (bp - own_bp).abs() <= 0.05
            && dsp - own_dsp >= -0.05
            && dsp - own_dsp <= 12_288.0 / 48_000.0 + 0.05
            && worst <= 5e-4
            && (s.calls.dry, s.calls.over, s.waits, s.underruns) == (0, 0, 0, 0);
        (ok, said)
    }

    /// BIT-PERFECT turned on and off under the input: BIT-PERFECT's chain
    /// starts past what the rack held ahead of the sound, the rack's device
    /// waits for its level — each chain's own delay after its switch, the
    /// player called at the device's pace throughout, a bridge's FIFO
    /// untouched, nothing made up at once. As it was, BIT-PERFECT kept the
    /// rack's delay and its pace drained it, and the rack took what it held
    /// ahead from the player at once.
    #[test]
    fn a_switch_on_the_input_leaves_the_new_chain_s_own_delay_and_the_player_s_pace() {
        for speed in [1.0, 1.0001] {
            let (ok, said) = switch_meets(speed, 0.15, 0.15, Switching::Before);
            println!("{said}");
            assert!(!ok, "as it was, the switch is off: {said}");
            let (ok, said) = switch_meets(speed, 0.15, 0.15, Switching::After);
            println!("{said}");
            assert!(ok, "{said}");
        }
        // The device opening slower or quicker than the last time: the
        // pace and the FIFO as well; the delay off by the difference at
        // most (slower: kept; quicker: the device waits for the level).
        for (open, guess) in [(0.3, 0.15), (0.1, 0.3)] {
            let (_, said) = switch_meets(1.0, open, guess, Switching::After);
            println!("{said}");
            assert!(said.contains("dry 0 over 0; the chain waited 0; the device dry 0"), "{said}");
        }
    }

    /// The chain waiting: nothing at once — the time's worth, as ever (the
    /// most it took in one go kept); the cap bounds the even pace, and
    /// nothing is owed against it; a reader away (held) owes no more than a
    /// little.
    #[test]
    fn a_waiting_chain_is_given_its_frames_at_the_pace() {
        const RATE: u32 = 48_000;
        let t = Instant::now();
        let mut p = Pace::new(RATE, 2_400, 0);
        assert_eq!(p.room(t, 2_400, 0, 50_000, true), 0, "nothing until the clock moves");
        let t0 = t + Duration::from_millis(10);
        assert_eq!(p.room(t0, -10_000, 0, 50_000, true), 480, "the chain waits: 10 ms at the stream's rate, not what it waits for");
        assert_eq!(p.burst(), 12_400 - 480, "what it took in one go, past what the reader is owed now");
        p.took(480);
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(p.room(t1, 1_000, 0, 50_000, true), 480, "10 ms at the stream's rate");
        p.took(480);
        assert_eq!(p.room(t1, 1_480, 0, 50_000, true), 0, "nothing more until the clock moves");
        let t2 = t1 + Duration::from_millis(10);
        assert_eq!(p.room(t2, 49_900, 0, 50_000, true), 100, "under the cap");
        assert_eq!(p.room(t2, 50_000, 0, 50_000, true), 0, "at the cap: nothing, and nothing owed for it");
        let t3 = t2 + Duration::from_secs(5);
        assert_eq!(p.room(t3, 2_400, 0, 500_000, true), 0, "away for 5 s (held): nothing owed for it");
        let t4 = t3 + Duration::from_millis(200);
        assert_eq!(p.room(t4, 2_400, 0, 500_000, true), 9_600, "late by 0.2 s: all of it owed");
        let t5 = t4 + Duration::from_millis(200);
        assert_eq!(p.room(t5, 2_400, 0, 500_000, true), (CREDIT_S * RATE as f64) as i64, "but no more than the credit");
        assert_eq!(p.trim(), 0.0, "no window ran");
    }
}
