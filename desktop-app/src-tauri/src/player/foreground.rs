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
//!
//! Heard is not safe yet: a 1G start's quick chain on the CPU went on the
//! air with nothing buffered, and the full variant beside it (and the next
//! track's preparation) took the cores and the memory from it — ~22 s of
//! underruns. So the work that gives way mid-run also waits while what is
//! buffered on the air runs short (`on_air`), its own `SHORT_MAX_WAIT` at
//! most, so it is always made.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long after a start or seek is heard the work behind it stays out.
pub const AFTER: Duration = Duration::from_millis(300);
/// The longest wait for quiet: then the work goes on anyway.
pub const MAX_WAIT: Duration = Duration::from_secs(2);
/// The longest the work of one mark waits for the music on the air to have
/// its buffer back: then it goes on anyway.
pub const SHORT_MAX_WAIT: Duration = Duration::from_secs(20);
/// The music on the air runs short below this part of its look-ahead.
pub const SHORT_FRACTION: f64 = 0.6;
/// A word on the buffer older than this is no word (the render thread said
/// nothing more: it has stopped).
const ON_AIR_STALE_MS: u64 = 1_000;
/// How often a waiter looks again.
const POLL: Duration = Duration::from_millis(5);

/// Starts and seeks going on, and when the last one ended.
pub struct Foreground {
    busy: AtomicUsize,
    /// When the last one ended: ms since `epoch`, + 1 (0: none yet).
    last_end: AtomicU64,
    /// When the render thread last said the music on the air runs short:
    /// ms since `epoch`, + 1 (0: it is not short).
    short_at: AtomicU64,
    epoch: Instant,
}

/// Held by a start or a seek until it is heard.
pub struct Guard<'a>(&'a Foreground);

impl Foreground {
    pub fn new() -> Foreground {
        Foreground {
            busy: AtomicUsize::new(0),
            last_end: AtomicU64::new(0),
            short_at: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// The music on the air runs short now, or not.
    fn set_short(&self, short: bool) {
        self.short_at.store(if short { self.now_ms() } else { 0 }, Ordering::Release);
    }

    /// The music on the air runs short, by a fresh word.
    fn short(&self) -> bool {
        let at = self.short_at.load(Ordering::Acquire);
        at != 0 && self.now_ms().saturating_sub(at) < ON_AIR_STALE_MS
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

/// `buffered` frames of a look-ahead of `ahead` frames run short.
pub fn runs_short(buffered: u64, ahead: f64) -> bool {
    (buffered as f64) < SHORT_FRACTION * ahead
}

/// The render thread's word on the music on the air, each time round:
/// what is buffered and the look-ahead it keeps (frames), or None — nothing
/// on the air, its track ended, or a chain that keeps a look-ahead of its
/// own (the input). Nothing on the air never runs short: the next track's
/// preparation at the end of a track, a paused or held output (its buffer
/// stays full) go on at once. The test build's render threads say nothing:
/// the tests' own marks would wait for them.
pub fn on_air(buffered: Option<(u64, f64)>) {
    if cfg!(test) {
        return;
    }
    player().set_short(buffered.is_some_and(|(b, ahead)| runs_short(b, ahead)));
}

/// The work behind the music gives way mid-run as well: a seek right after
/// a start came while the full variant's apodizer and output-peak probe
/// had the cores (they go on at the start's pre-roll end + `AFTER`), and
/// waited for them. Work that carries the mark (`give_way_here`) stops
/// between its pieces while a start or a seek goes on — the pieces are the
/// same, only later. The mark is the thread's own, so what a start or a
/// seek runs itself never waits for itself; a thread passes it on to the
/// threads it starts (`GiveWay::current`). All the waits of one mark come
/// to `MAX_WAIT` at most, so a burst of seeks cannot starve the work. It
/// waits as well while the music on the air runs short (`on_air`), up to
/// `SHORT_MAX_WAIT` of its own.
#[derive(Clone)]
pub struct GiveWay(std::sync::Arc<Spent>);

struct Spent {
    fg: &'static Foreground,
    budget_ms: u64,
    short_budget_ms: u64,
    /// The time some piece of this work waited for a start or a seek, ms.
    ms: AtomicU64,
    /// The time some piece of it waited for the buffer on the air, ms.
    short_ms: AtomicU64,
    /// When a waiting piece last counted it: ms since `fg.epoch`, + 1.
    stamp: AtomicU64,
}

/// What a piece of marked work waits for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Wait {
    /// A start or a seek goes on.
    Busy,
    /// The music on the air runs short.
    Short,
}

/// The mark on this thread until it is dropped (the one before it then).
pub struct Mark(Option<GiveWay>);

thread_local! {
    static WAY: std::cell::RefCell<Option<GiveWay>> = const { std::cell::RefCell::new(None) };
}

impl GiveWay {
    fn on(fg: &'static Foreground, budget: Duration) -> GiveWay {
        GiveWay::with(fg, budget, SHORT_MAX_WAIT)
    }

    fn with(fg: &'static Foreground, budget: Duration, short_budget: Duration) -> GiveWay {
        GiveWay(std::sync::Arc::new(Spent {
            fg,
            budget_ms: budget.as_millis() as u64,
            short_budget_ms: short_budget.as_millis() as u64,
            ms: AtomicU64::new(0),
            short_ms: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }))
    }

    /// What a piece waits for now, with time left for it; None: go on.
    fn wait(&self) -> Option<Wait> {
        let s = &*self.0;
        if s.fg.busy.load(Ordering::Acquire) > 0 && s.ms.load(Ordering::Acquire) < s.budget_ms {
            return Some(Wait::Busy);
        }
        (s.fg.short() && s.short_ms.load(Ordering::Acquire) < s.short_budget_ms).then_some(Wait::Short)
    }

    /// Mark this thread with it.
    pub fn mark(self) -> Mark {
        Mark(WAY.with(|w| w.borrow_mut().replace(self)))
    }

    /// This thread's mark, to pass on.
    pub fn current() -> Option<GiveWay> {
        WAY.try_with(|w| w.borrow().clone()).ok().flatten()
    }

    /// While a start or a seek goes on, or the music on the air runs short,
    /// wait — until it is heard or has its buffer back, `cancel` goes up, or
    /// the mark's time for it is spent. How long it waited.
    pub fn pause(&self, cancel: &AtomicBool) -> Duration {
        let s = &*self.0;
        let Some(mut w) = self.wait() else { return Duration::ZERO };
        let t0 = Instant::now();
        let mut mine = s.fg.now_ms();
        while !cancel.load(Ordering::Acquire) {
            std::thread::sleep(POLL);
            // Wall time, however many pieces wait side by side: each counts
            // what passed since it began waiting or any of them last counted,
            // whichever came later (a sleep that ran long counts in full).
            let now = s.fg.now_ms();
            let last = s.stamp.swap(now, Ordering::AcqRel);
            let d = now.saturating_sub(last.max(mine));
            mine = now;
            match w {
                Wait::Busy => s.ms.fetch_add(d, Ordering::AcqRel),
                Wait::Short => s.short_ms.fetch_add(d, Ordering::AcqRel),
            };
            match self.wait() {
                Some(next) => w = next,
                None => break,
            }
        }
        t0.elapsed()
    }

    /// The time this mark's work waited so far for starts and seeks.
    #[cfg(test)]
    fn waited(&self) -> Duration {
        Duration::from_millis(self.0.ms.load(Ordering::Acquire))
    }

    /// The time this mark's work waited so far for the buffer on the air.
    #[cfg(test)]
    fn waited_short(&self) -> Duration {
        Duration::from_millis(self.0.short_ms.load(Ordering::Acquire))
    }
}

impl Drop for Spent {
    fn drop(&mut self) {
        let (ms, short) = (*self.ms.get_mut(), *self.short_ms.get_mut());
        if short > 0 {
            crate::aelog!(
                "[PLAYER] the work behind the music gave way {} ms to a start or a seek, {} ms to a short buffer on the air",
                ms,
                short
            );
        } else if ms > 0 {
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
/// to the player's starts and seeks, and to its music running short
/// (`pause_here`).
pub fn give_way_here() -> Mark {
    GiveWay::on(player(), MAX_WAIT).mark()
}

/// A piece of work is through: with this thread's mark, wait while a start
/// or a seek goes on or the music on the air runs short (`GiveWay::pause`);
/// without one, go on at once.
pub fn pause_here(cancel: &AtomicBool) -> Duration {
    // The common case — nothing going on — without the thread-local.
    let f = player();
    if f.busy.load(Ordering::Acquire) == 0 && !f.short() {
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
    fn work_that_gives_way_waits_while_the_music_runs_short_and_no_longer() {
        let f = leaked();
        let w = GiveWay::on(f, MAX_WAIT);
        let no = AtomicBool::new(false);
        f.set_short(true);
        let refill = std::thread::spawn(move || {
            std::thread::sleep(120 * MS);
            f.set_short(false);
            Instant::now()
        });
        let waited = w.pause(&no);
        let refilled = refill.join().unwrap();
        assert!(waited >= 110 * MS, "waited only {:?}", waited);
        assert!(Instant::now().duration_since(refilled) < 100 * MS, "went on {:?} after the refill", Instant::now().duration_since(refilled));
        assert!(w.waited_short() >= 100 * MS && w.waited_short() < 400 * MS, "counted {:?}", w.waited_short());
        // Counted on its own: the time for starts and seeks is untouched.
        assert_eq!(w.waited(), Duration::ZERO);
        assert_eq!(w.pause(&no), Duration::ZERO, "the buffer is back");
    }

    #[test]
    fn the_wait_for_the_buffer_has_its_own_time_and_then_goes_on() {
        let f = leaked();
        f.set_short(true);
        let w = GiveWay::with(f, MAX_WAIT, 100 * MS);
        let no = AtomicBool::new(false);
        let t0 = Instant::now();
        let waited = w.pause(&no);
        assert!(waited >= 90 * MS && waited < 600 * MS, "waited {:?}", waited);
        // Spent: still short, it goes on at once.
        assert!(w.pause(&no) < 20 * MS);
        assert!(t0.elapsed() < 800 * MS);
        // A start still has the mark's own time for it.
        let g = f.begin();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(60 * MS);
            drop(g);
        });
        assert!(w.pause(&no) >= 50 * MS, "a start after the buffer's time is spent is still waited for");
        ender.join().unwrap();
        f.set_short(false);
    }

    #[test]
    fn a_stale_word_on_the_buffer_is_no_word() {
        let mut f = Foreground::new();
        f.epoch = Instant::now().checked_sub(Duration::from_secs(5)).expect("a clock 5 s old");
        f.set_short(true);
        assert!(f.short());
        // The render thread said it at the epoch and nothing since (stopped).
        f.short_at.store(1, Ordering::Release);
        assert!(!f.short());
        let w = GiveWay::on(Box::leak(Box::new(f)), MAX_WAIT);
        assert_eq!(w.pause(&AtomicBool::new(false)), Duration::ZERO);
    }

    #[test]
    fn short_is_below_its_part_of_the_look_ahead() {
        assert!(runs_short(0, 1_000.0));
        assert!(runs_short(599, 1_000.0));
        assert!(!runs_short(600, 1_000.0));
        assert!(!runs_short(1_000, 1_000.0));
        // No look-ahead at all: nothing runs short.
        assert!(!runs_short(0, 0.0));
    }

    #[test]
    fn a_cancel_ends_a_wait_for_the_buffer() {
        let f = leaked();
        f.set_short(true);
        let w = GiveWay::on(f, MAX_WAIT);
        assert!(w.pause(&AtomicBool::new(true)) < 50 * MS);
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
