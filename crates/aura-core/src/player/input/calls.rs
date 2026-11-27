//! What the driver tells of its calls of the player (`aura_link::CallStats`)
//! and what this reader sees of itself, for the session's log — the stretch
//! between the player and the link that the stream's own figures do not
//! show. Each late call of the player is a line, with why it was late: the
//! driver waited for room (this reader stood), the player's own call before
//! took long, or the driver's thread was late (the system); and so is each
//! time this reader was held up. Every 30 s, how the calls went. No more
//! than a few lines a window: the rest are counted.

use std::time::{Duration, Instant};

use aura_link::{CallStats, LateCall, Server};

/// How often the calls are summed up, and how many lines one by one each
/// time at most.
const EVERY: Duration = Duration::from_secs(30);
const LINES: u32 = 10;
/// This reader going round later than it meant to by this much is held up.
pub const HELD_UP: Duration = Duration::from_millis(20);
/// A player that has played this long with no account of its calls has an
/// older driver.
const NO_ACCOUNT: Duration = Duration::from_secs(5);

pub struct CallWatch {
    /// The client whose account this is, and its late calls read so far.
    gen: Option<u32>,
    seen: u64,
    /// The account at the window's start, and when it began.
    last: CallStats,
    at: Instant,
    left: u32,
    untold: u64,
    late: Vec<LateCall>,
    /// When this reader last went round, and how long it meant to wait.
    tick: Option<(Instant, Duration)>,
    /// Since when the player plays (its account should be moving), and
    /// whether it was said that the driver keeps none.
    playing: Option<Instant>,
    said_old: bool,
}

impl Default for CallWatch {
    fn default() -> Self {
        CallWatch::new()
    }
}

impl CallWatch {
    pub fn new() -> CallWatch {
        CallWatch {
            gen: None,
            seen: 0,
            last: CallStats::default(),
            at: Instant::now(),
            left: LINES,
            untold: 0,
            late: Vec::new(),
            tick: None,
            playing: None,
            said_old: false,
        }
    }

    /// Looks at the account of client `gen`, whose stream comes at
    /// `samples_per_s` (its rate times its channels; 0: not known yet), the
    /// player `running` or not.
    pub fn look(&mut self, server: &Server, gen: u32, running: bool, samples_per_s: f64, now: Instant) {
        if self.gen != Some(gen) {
            *self = CallWatch { gen: Some(gen), tick: self.tick, ..CallWatch::new() };
            self.at = now;
        }
        let st = server.call_stats();
        if st.late != self.seen {
            self.late.clear();
            let lost = server.late_calls(&mut self.seen, &mut self.late);
            self.untold += lost;
            for i in 0..self.late.len() {
                let c = self.late[i];
                self.say(|| late_line(&c, st.period_us, samples_per_s));
            }
        }
        self.playing = if running { Some(self.playing.unwrap_or(now)) } else { None };
        let played = self.playing.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        if st.calls == 0 && st.period_us == 0 && !self.said_old && played >= NO_ACCOUNT {
            self.said_old = true;
            crate::aelog!("[INPUT] the driver keeps no account of its calls: an older one (install it again from the settings)");
        }
        let window = now.saturating_duration_since(self.at);
        if window >= EVERY {
            if st.calls > self.last.calls {
                crate::aelog!("{}", summary(&self.last, &st, window, self.untold));
            }
            self.last = st;
            self.at = now;
            self.left = LINES;
            self.untold = 0;
        }
    }

    /// This reader goes round at `now`, meaning to wait `meant` before the
    /// next time (None: it does not read now).
    pub fn round(&mut self, now: Instant, meant: Option<Duration>) {
        if let Some((t, was)) = self.tick {
            let gap = now.saturating_duration_since(t);
            if gap > was + HELD_UP {
                let over = gap - was;
                self.say(|| format!("[INPUT] this reader was held up {:.0} ms", over.as_secs_f64() * 1e3));
            }
        }
        self.tick = meant.map(|m| (now, m));
    }

    fn say(&mut self, line: impl FnOnce() -> String) {
        if self.left > 0 {
            self.left -= 1;
            crate::aelog!("{}", line());
        } else {
            self.untold += 1;
        }
    }
}

/// A late call of the player, and why.
pub fn late_line(c: &LateCall, period_us: u32, samples_per_s: f64) -> String {
    let ms = |us: u32| us as f64 / 1e3;
    let gap_over = c.gap_us.saturating_sub(period_us);
    let why = if c.waited_us >= gap_over / 2 && c.waited_us > 0 {
        format!("the driver waited {:.1} ms for room in the link — this reader stood", ms(c.waited_us))
    } else if c.before_us > period_us / 2 {
        format!("its own call before took {:.1} ms — the player", ms(c.before_us))
    } else {
        "the driver's thread was late — the system".to_string()
    };
    let fill = if samples_per_s > 0.0 { format!("; the link held {:.1} ms", c.fill as f64 / samples_per_s * 1e3) } else { String::new() };
    format!("[INPUT] the player was called {:.1} ms after its call before (its period {:.1} ms): {}{}", ms(c.gap_us), ms(period_us), why, fill)
}

/// How the calls went from `a` to `b`, over `window`.
pub fn summary(a: &CallStats, b: &CallStats, window: Duration, untold: u64) -> String {
    let untold = if untold > 0 { format!(" ({untold} more not told one by one)") } else { String::new() };
    let clock = if b.clocked { format!("by the driver's own clock, {:+} ppm", b.clock_ppm) } else { "as room is made in the link".to_string() };
    format!(
        "[INPUT] the player's calls: {} in {:.0} s, {} late{}; the driver waited for room {:.0} ms in all; the player's longest call {:.1} ms; called {}",
        b.calls - a.calls,
        window.as_secs_f64(),
        b.late.saturating_sub(a.late),
        untold,
        b.waited_us.saturating_sub(a.waited_us) as f64 / 1e3,
        b.longest_us as f64 / 1e3,
        clock
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each cause as the log says it.
    #[test]
    fn a_late_call_says_why() {
        let p = 42_667;
        let stood = LateCall { gap_us: 180_000, waited_us: 150_000, before_us: 30, fill: 0 };
        assert_eq!(
            late_line(&stood, p, 96_000.0),
            "[INPUT] the player was called 180.0 ms after its call before (its period 42.7 ms): the driver waited 150.0 ms for room in the link — this reader stood; the link held 0.0 ms"
        );
        let slow = LateCall { gap_us: 90_000, waited_us: 0, before_us: 60_000, fill: 960 };
        assert!(late_line(&slow, p, 96_000.0).contains("its own call before took 60.0 ms — the player; the link held 10.0 ms"));
        let system = LateCall { gap_us: 90_000, waited_us: 2_000, before_us: 100, fill: 960 };
        assert!(late_line(&system, p, 0.0).ends_with("the driver's thread was late — the system"));
    }

    /// The watch reads each late call once, tells the first few and counts
    /// the rest; a new client's account from its start; a player playing
    /// with no account is an older driver (said once), one only connected
    /// is not.
    #[test]
    fn the_watch_tells_each_late_call_once() {
        use aura_link::{Client, HeapRegion, LocalSignals};
        let ring = 1 << 12;
        let region = HeapRegion::new(aura_link::region_bytes(ring));
        let sig = LocalSignals::default();
        let server = Server::create(Box::new(region.clone()), Box::new(sig.clone()), ring, 1);
        let client = Client::open(Box::new(region.clone()), Box::new(sig.clone()), 2, |_| true).unwrap();
        let t0 = Instant::now();
        let mut w = CallWatch::new();
        w.look(&server, client.gen(), false, 96_000.0, t0);
        w.look(&server, client.gen(), false, 96_000.0, t0 + Duration::from_secs(9));
        assert!(!w.said_old, "connected, not playing: no account due yet");
        w.look(&server, client.gen(), true, 96_000.0, t0 + Duration::from_secs(10));
        w.look(&server, client.gen(), true, 96_000.0, t0 + Duration::from_secs(16));
        assert!(w.said_old, "playing 6 s with no account");
        let late = LateCall { gap_us: 90_000, waited_us: 80_000, before_us: 10, fill: 0 };
        for _ in 0..(LINES + 3) {
            client.count_call(80_000, 10, Some(late));
        }
        w.look(&server, client.gen(), true, 96_000.0, t0 + Duration::from_secs(17));
        assert_eq!((w.seen, w.left, w.untold), ((LINES + 3) as u64, 0, 3));
        w.look(&server, client.gen(), true, 96_000.0, t0 + Duration::from_secs(18));
        assert_eq!(w.untold, 3, "each once");
        // Another player takes the link (the one before gone): its account afresh.
        std::mem::forget(client);
        let other = Client::open(Box::new(region), Box::new(sig), 3, |_| false).unwrap();
        other.set_period(Duration::from_micros(42_667));
        w.look(&server, other.gen(), true, 96_000.0, t0 + Duration::from_secs(19));
        assert_eq!((w.seen, w.left, w.untold, w.said_old), (0, LINES, 0, false), "another client: afresh");
    }

    #[test]
    fn the_calls_are_summed_up() {
        let a = CallStats { calls: 100, late: 1, waited_us: 1_000, ..Default::default() };
        let b = CallStats { period_us: 42_667, calls: 803, late: 3, waited_us: 151_000, longest_us: 1_240, clocked: true, clock_ppm: -95 };
        assert_eq!(
            summary(&a, &b, Duration::from_secs(30), 4),
            "[INPUT] the player's calls: 703 in 30 s, 2 late (4 more not told one by one); the driver waited for room 150 ms in all; the player's longest call 1.2 ms; called by the driver's own clock, -95 ppm"
        );
        let by_room = CallStats { calls: 200, clocked: false, ..b };
        assert!(summary(&a, &by_room, Duration::from_secs(30), 0).ends_with("called as room is made in the link"));
    }
}
