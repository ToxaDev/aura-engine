//! A start or a seek on its way to what the listener waits for, and the
//! work behind the music that gives way to it.
//!
//! Right after a start's first variant, work the listener does not wait
//! for began at once beside it: the full variant (the adaptive apodizer over
//! the whole track, on every core), its output-peak probe, the pair's
//! minimum-phase bank. A start or a seek holds `begin()`'s guard until it is
//! heard; that work asks `wait_quiet` first and goes on `AFTER` its end. What
//! it makes is the same — it is only made a little later — and the wait is
//! never longer than `MAX_WAIT`, so a burst of seeks cannot starve it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long after a start or seek is heard the work behind it stays out.
pub const AFTER: Duration = Duration::from_millis(300);
/// The longest wait for quiet: then the work goes on anyway.
pub const MAX_WAIT: Duration = Duration::from_secs(2);
/// How often a waiter looks again.
const POLL: Duration = Duration::from_millis(5);

/// Starts and seeks going on, and when the last one ended.
pub struct Foreground {
    busy: AtomicUsize,
    /// When the last one ended: ms since `epoch`, + 1 (0: none yet).
    last_end: AtomicU64,
    epoch: Instant,
}

/// Held by a start or a seek until it is heard.
pub struct Guard<'a>(&'a Foreground);

impl Foreground {
    pub fn new() -> Foreground {
        Foreground { busy: AtomicUsize::new(0), last_end: AtomicU64::new(0), epoch: Instant::now() }
    }

    pub fn begin(&self) -> Guard<'_> {
        self.busy.fetch_add(1, Ordering::AcqRel);
        Guard(self)
    }

    /// Nothing going on and nothing ended within `after`; else how long
    /// to wait at least.
    fn quiet_in(&self, after: Duration) -> Option<Duration> {
        if self.busy.load(Ordering::Acquire) > 0 {
            return Some(POLL);
        }
        let end = self.last_end.load(Ordering::Acquire);
        if end == 0 {
            return None;
        }
        let since = self.epoch.elapsed().saturating_sub(Duration::from_millis(end - 1));
        (since < after).then(|| after - since)
    }

    /// Wait until no start or seek goes on and `after` has passed since the
    /// last one, `cancel` is up, or `max` has passed. How long it waited.
    pub fn wait_quiet_for(&self, cancel: &AtomicBool, after: Duration, max: Duration) -> Duration {
        let t0 = Instant::now();
        while let Some(d) = self.quiet_in(after) {
            let left = max.saturating_sub(t0.elapsed());
            if left.is_zero() || cancel.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(d.min(POLL).min(left));
        }
        t0.elapsed()
    }
}

impl Default for Foreground {
    fn default() -> Self {
        Foreground::new()
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let f = self.0;
        f.last_end.store(f.epoch.elapsed().as_millis() as u64 + 1, Ordering::Release);
        f.busy.fetch_sub(1, Ordering::AcqRel);
    }
}

fn player() -> &'static Foreground {
    static F: OnceLock<Foreground> = OnceLock::new();
    F.get_or_init(Foreground::new)
}

/// A start or a seek of the player begins: the guard ends it.
pub fn begin() -> Guard<'static> {
    player().begin()
}

/// The work behind the music waits for quiet (`AFTER` the last start or
/// seek is heard, at most `MAX_WAIT`). How long it waited.
pub fn wait_quiet(cancel: &AtomicBool) -> Duration {
    player().wait_quiet_for(cancel, AFTER, MAX_WAIT)
}

/// The work behind the music gives way mid-run as well: a seek right after
/// a start came while the full variant's apodizer and output-peak probe
/// had the cores (they go on at the start's pre-roll end + `AFTER`), and
/// waited for them. Work that carries the mark (`give_way_here`) stops
/// between its pieces while a start or a seek goes on — the pieces are the
/// same, only later. The mark is the thread's own, so what a start or a
/// seek runs itself never waits for itself; a thread passes it on to the
/// threads it starts (`GiveWay::current`). All the waits of one mark come
/// to `MAX_WAIT` at most, so a burst of seeks cannot starve the work.
#[derive(Clone)]
pub struct GiveWay(std::sync::Arc<Spent>);

struct Spent {
    fg: &'static Foreground,
    budget_ms: u64,
    /// The time some piece of this work waited, ms.
    ms: AtomicU64,
    /// When a waiting piece last counted it: ms since `fg.epoch`, + 1.
    stamp: AtomicU64,
}

/// The mark on this thread until it is dropped (the one before it then).
pub struct Mark(Option<GiveWay>);

thread_local! {
    static WAY: std::cell::RefCell<Option<GiveWay>> = const { std::cell::RefCell::new(None) };
}

impl GiveWay {
    fn on(fg: &'static Foreground, budget: Duration) -> GiveWay {
        GiveWay(std::sync::Arc::new(Spent {
            fg,
            budget_ms: budget.as_millis() as u64,
            ms: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }))
    }

    /// Mark this thread with it.
    pub fn mark(self) -> Mark {
        Mark(WAY.with(|w| w.borrow_mut().replace(self)))
    }

    /// This thread's mark, to pass on.
    pub fn current() -> Option<GiveWay> {
        WAY.try_with(|w| w.borrow().clone()).ok().flatten()
    }

    /// While a start or a seek goes on, wait — until it is heard, `cancel`
    /// goes up, or the mark's time is spent. How long it waited.
    pub fn pause(&self, cancel: &AtomicBool) -> Duration {
        let s = &*self.0;
        if s.fg.busy.load(Ordering::Acquire) == 0 {
            return Duration::ZERO;
        }
        let t0 = Instant::now();
        let poll = POLL.as_millis() as u64;
        while s.fg.busy.load(Ordering::Acquire) > 0
            && !cancel.load(Ordering::Acquire)
            && s.ms.load(Ordering::Acquire) < s.budget_ms
        {
            std::thread::sleep(POLL);
            // Wall time, however many pieces wait side by side: each counts
            // what passed since any of them last did (a step at most).
            let now = s.fg.epoch.elapsed().as_millis() as u64 + 1;
            let last = s.stamp.swap(now, Ordering::AcqRel);
            let d = if last == 0 { poll } else { now.saturating_sub(last).min(2 * poll) };
            s.ms.fetch_add(d, Ordering::AcqRel);
        }
        t0.elapsed()
    }

    /// The time this mark's work waited so far.
    #[cfg(test)]
    fn waited(&self) -> Duration {
        Duration::from_millis(self.0.ms.load(Ordering::Acquire))
    }
}

impl Drop for Spent {
    fn drop(&mut self) {
        let ms = *self.ms.get_mut();
        if ms > 0 {
            crate::aelog!("[PLAYER] the work behind the music gave way {} ms to a start or a seek", ms);
        }
    }
}

impl Drop for Mark {
    fn drop(&mut self) {
        let before = self.0.take();
        let _ = WAY.try_with(|w| *w.borrow_mut() = before);
    }
}

/// From here until the mark is dropped, the work on this thread gives way
/// to the player's starts and seeks (`pause_here`).
pub fn give_way_here() -> Mark {
    GiveWay::on(player(), MAX_WAIT).mark()
}

/// A piece of work is through: with this thread's mark, wait while a start
/// or a seek goes on (`GiveWay::pause`); without one, go on at once.
pub fn pause_here(cancel: &AtomicBool) -> Duration {
    // The common case — nothing going on — without the thread-local.
    if player().busy.load(Ordering::Acquire) == 0 {
        return Duration::ZERO;
    }
    GiveWay::current().map_or(Duration::ZERO, |w| w.pause(cancel))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn nothing_going_on_is_quiet_at_once() {
        let f = Foreground::new();
        let no = AtomicBool::new(false);
        assert!(f.wait_quiet_for(&no, AFTER, MAX_WAIT) < 50 * MS);
    }

    #[test]
    fn the_work_waits_while_a_start_goes_on_and_a_while_after_it() {
        let f: &'static Foreground = Box::leak(Box::new(Foreground::new()));
        let no = AtomicBool::new(false);
        let g = f.begin();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(150 * MS);
            drop(g);
            Instant::now()
        });
        let t0 = Instant::now();
        let waited = f.wait_quiet_for(&no, 200 * MS, Duration::from_secs(5));
        let ended = ender.join().unwrap();
        assert!(waited >= 300 * MS, "waited only {:?}", waited);
        // Went on after the start's end + `after`, not much later.
        assert!(Instant::now().duration_since(ended) >= 195 * MS);
        assert!(t0.elapsed() < 2_000 * MS, "waited {:?}", t0.elapsed());
    }

    #[test]
    fn two_starts_overlapping_keep_it_waiting_until_the_last_ends() {
        let f = Foreground::new();
        let no = AtomicBool::new(false);
        let a = f.begin();
        let b = f.begin();
        drop(a);
        assert!(f.quiet_in(Duration::ZERO).is_some(), "one start still goes on");
        drop(b);
        assert!(f.quiet_in(Duration::ZERO).is_none());
        assert!(f.quiet_in(Duration::from_secs(60)).is_some(), "within `after` of the end");
        assert!(f.wait_quiet_for(&no, Duration::ZERO, MAX_WAIT) < 50 * MS);
    }

    #[test]
    fn a_cancel_ends_the_wait() {
        let f = Foreground::new();
        let _g = f.begin();
        let cancel = AtomicBool::new(true);
        assert!(f.wait_quiet_for(&cancel, AFTER, Duration::from_secs(5)) < 50 * MS);
    }

    fn leaked() -> &'static Foreground {
        Box::leak(Box::new(Foreground::new()))
    }

    #[test]
    fn work_that_gives_way_goes_on_at_once_with_nothing_going_on() {
        let f = leaked();
        let w = GiveWay::on(f, MAX_WAIT);
        let no = AtomicBool::new(false);
        assert_eq!(w.pause(&no), Duration::ZERO);
        // A start that has been heard is nothing going on: no `AFTER` here.
        drop(f.begin());
        assert_eq!(w.pause(&no), Duration::ZERO);
        assert_eq!(w.waited(), Duration::ZERO);
    }

    #[test]
    fn work_that_gives_way_waits_while_a_seek_goes_on_and_no_longer() {
        let f = leaked();
        let w = GiveWay::on(f, Duration::from_secs(5));
        let no = AtomicBool::new(false);
        let g = f.begin();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(120 * MS);
            drop(g);
            Instant::now()
        });
        let waited = w.pause(&no);
        let ended = ender.join().unwrap();
        assert!(waited >= 110 * MS, "waited only {:?}", waited);
        assert!(Instant::now().duration_since(ended) < 100 * MS, "went on {:?} after the seek", Instant::now().duration_since(ended));
        assert!(w.waited() >= 100 * MS && w.waited() < 400 * MS, "counted {:?}", w.waited());
    }

    #[test]
    fn the_waits_of_one_mark_share_its_time_however_many_pieces_wait() {
        let f = leaked();
        let _g = f.begin();
        let w = GiveWay::on(f, 150 * MS);
        let no: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let t0 = Instant::now();
        let pieces: Vec<_> = (0..8)
            .map(|_| {
                let w = w.clone();
                std::thread::spawn(move || w.pause(no))
            })
            .collect();
        for p in pieces {
            let waited = p.join().unwrap();
            assert!(waited >= 100 * MS, "a piece waited {:?}", waited);
        }
        // Eight side by side spend the mark's time once, not eight times.
        assert!(t0.elapsed() < 700 * MS, "took {:?}", t0.elapsed());
        // Spent: what comes next goes on at once.
        assert!(w.pause(no) < 20 * MS);
    }

    #[test]
    fn a_cancel_ends_a_pause() {
        let f = leaked();
        let _g = f.begin();
        let w = GiveWay::on(f, Duration::from_secs(5));
        assert!(w.pause(&AtomicBool::new(true)) < 50 * MS);
    }

    #[test]
    fn the_mark_is_the_threads_own_and_is_passed_on_by_hand() {
        assert!(GiveWay::current().is_none());
        {
            let _m = give_way_here();
            let w = GiveWay::current().expect("marked");
            assert!(std::thread::spawn(|| GiveWay::current().is_none()).join().unwrap(), "not another thread's");
            assert!(std::thread::spawn(move || w.mark().0.is_none()).join().unwrap());
            {
                let _inner = GiveWay::on(leaked(), MAX_WAIT).mark();
                assert!(GiveWay::current().is_some());
            }
            assert!(GiveWay::current().is_some(), "the outer mark is back");
        }
        assert!(GiveWay::current().is_none(), "gone with its mark");
    }

    #[test]
    fn the_wait_is_bounded_however_long_a_start_goes_on() {
        let f = Foreground::new();
        let _g = f.begin();
        let no = AtomicBool::new(false);
        let waited = f.wait_quiet_for(&no, AFTER, 120 * MS);
        assert!(waited >= 120 * MS && waited < 1_000 * MS, "waited {:?}", waited);
    }
}
