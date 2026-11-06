//! One whole-track separation at a time for a track (Anton 30.10, «Да,
//! объединить»): Hybrid-Phase's lists (`netkick`) and Line Up's map
//! (`job`) both start from HTDemucs and DrumSep over the whole track. The
//! first of them to come owns the track's separation and makes both; the
//! other waits for it and then finds its part on the disk — or, when the
//! owner ended without it (stopped, failed), makes its own as before.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Entry {
    /// the owner's progress 0…1 (the map job's scale), whether it ended, and
    /// whether Hybrid-Phase's lists are kept (they come before the map)
    at: Mutex<(f32, bool, bool)>,
    cv: Condvar,
}

/// The tracks being separated, by path.
#[derive(Default, Clone)]
pub struct Owners {
    now: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
}

pub enum Claim {
    /// This caller separates the track; the guard lets it go when dropped.
    Mine(Guard),
    /// Another one does; wait for it.
    Theirs(Waiter),
}

pub struct Guard {
    path: String,
    entry: Arc<Entry>,
    owners: Owners,
}

pub struct Waiter {
    entry: Arc<Entry>,
}

impl Owners {
    /// The track at `path`: this caller's to separate, or another's.
    pub fn claim(&self, path: &str) -> Claim {
        let mut now = self.now.lock().unwrap();
        if let Some(e) = now.get(path) {
            return Claim::Theirs(Waiter { entry: e.clone() });
        }
        let entry = Arc::new(Entry::default());
        now.insert(path.to_string(), entry.clone());
        Claim::Mine(Guard { path: path.to_string(), entry, owners: self.clone() })
    }

    /// Whether someone separates the track at `path` now.
    #[cfg(test)]
    pub fn busy(&self, path: &str) -> bool {
        self.now.lock().unwrap().contains_key(path)
    }
}

impl Guard {
    /// How far the owner is, 0…1 (never backwards).
    pub fn progress(&self, p: f32) {
        let mut at = self.entry.at.lock().unwrap();
        if p > at.0 {
            at.0 = p.min(1.0);
            self.entry.cv.notify_all();
        }
    }

    /// Hybrid-Phase's lists of the track are kept: a waiter for them goes on.
    pub fn lists_kept(&self) {
        self.entry.at.lock().unwrap().2 = true;
        self.entry.cv.notify_all();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut now = self.owners.now.lock().unwrap();
        if now.get(&self.path).is_some_and(|e| Arc::ptr_eq(e, &self.entry)) {
            now.remove(&self.path);
        }
        drop(now);
        self.entry.at.lock().unwrap().1 = true;
        self.entry.cv.notify_all();
    }
}

impl Waiter {
    /// Until the owner ends (true) or `stop` (false); `tick` hears the
    /// owner's progress as it moves.
    pub fn wait(&self, stop: &dyn Fn() -> bool, tick: &mut dyn FnMut(f32)) -> bool {
        self.wait_until(false, stop, tick)
    }

    /// The same, or until the owner has kept Hybrid-Phase's lists (true).
    pub fn wait_lists(&self, stop: &dyn Fn() -> bool, tick: &mut dyn FnMut(f32)) -> bool {
        self.wait_until(true, stop, tick)
    }

    fn wait_until(&self, lists: bool, stop: &dyn Fn() -> bool, tick: &mut dyn FnMut(f32)) -> bool {
        let mut seen = -1f32;
        let mut at = self.entry.at.lock().unwrap();
        loop {
            if at.0 != seen {
                seen = at.0;
                let p = seen;
                drop(at);
                tick(p);
                at = self.entry.at.lock().unwrap();
            }
            if at.1 || (lists && at.2) {
                return true;
            }
            if stop() {
                return false;
            }
            at = self.entry.cv.wait_timeout(at, Duration::from_millis(100)).unwrap().0;
        }
    }
}

/// The app's one table.
pub fn owners() -> &'static Owners {
    static O: std::sync::OnceLock<Owners> = std::sync::OnceLock::new();
    O.get_or_init(Owners::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn the_first_owns_a_track_and_the_second_waits_for_it() {
        let o = Owners::default();
        let Claim::Mine(g) = o.claim("a") else { panic!("first") };
        assert!(o.busy("a") && !o.busy("b"));
        assert!(matches!(o.claim("b"), Claim::Mine(_)), "another track is anyone's");
        let Claim::Theirs(w) = o.claim("a") else { panic!("second") };
        let t = std::thread::spawn(move || {
            let mut heard = Vec::new();
            let ended = w.wait(&|| false, &mut |p| heard.push(p));
            (ended, heard)
        });
        g.progress(0.5);
        g.progress(0.25); // never backwards
        std::thread::sleep(Duration::from_millis(30));
        drop(g);
        let (ended, heard) = t.join().unwrap();
        assert!(ended);
        assert!(heard.windows(2).all(|w| w[0] <= w[1]) && *heard.last().unwrap() == 0.5, "{heard:?}");
        assert!(!o.busy("a"));
        assert!(matches!(o.claim("a"), Claim::Mine(_)), "free again once its owner ended");
    }

    #[test]
    fn the_lists_come_before_the_owner_ends() {
        let o = Owners::default();
        let Claim::Mine(g) = o.claim("a") else { panic!() };
        let (Claim::Theirs(lists), Claim::Theirs(map)) = (o.claim("a"), o.claim("a")) else { panic!() };
        let (hp, lu) = (std::thread::spawn(move || lists.wait_lists(&|| false, &mut |_| {})), std::thread::spawn(move || map.wait(&|| false, &mut |_| {})));
        g.lists_kept();
        assert!(hp.join().unwrap(), "the lists' waiter goes on");
        std::thread::sleep(Duration::from_millis(30));
        assert!(!lu.is_finished(), "the map's waiter waits for the owner's end");
        drop(g);
        assert!(lu.join().unwrap());
    }

    #[test]
    fn a_waiter_leaves_when_it_is_stopped() {
        let o = Owners::default();
        let _g = match o.claim("a") {
            Claim::Mine(g) => g,
            Claim::Theirs(_) => panic!(),
        };
        let Claim::Theirs(w) = o.claim("a") else { panic!() };
        let stop = AtomicBool::new(false);
        let ended = std::thread::scope(|s| {
            let h = s.spawn(|| w.wait(&|| stop.load(Ordering::Acquire), &mut |_| {}));
            stop.store(true, Ordering::Release);
            h.join().unwrap()
        });
        assert!(!ended);
        assert!(o.busy("a"), "the owner still has it");
    }
}
