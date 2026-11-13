//! The render behind real time: a clean pause in place of dropouts.
//!
//! A chain slower than real time (1G on the CPU, the cores shared with a
//! preparation) used to leave the output to play whatever was written: holes
//! of silence between bits of music — jerks (Anton 30.10, Hybrid-Phase off at
//! 1G: 4.7 s of dropouts in 12 s). Now the render says it is starved
//! (`OutputShared::starved`): the output fades out over the music on its
//! way and sends silence without moving the reader, as a pause does, so no
//! music is lost; the render fills meanwhile, and the music fades back in at
//! the same frame once there is a cushion and the chain keeps up — or, when
//! it cannot, once the ring is full, so a long clean stretch plays before the
//! next pause.
//!
//! `Starve::decide` is the whole rule, pure, on what the render sees each
//! time round (`Seen`), with its own clock.

/// Engage below this much buffered (s) …
pub const ENGAGE_S: f64 = 0.35;
/// … when the render has not been at its look-ahead for this long (s): a
/// render that keeps up comes back to it every few milliseconds, so a short
/// stall (a GPU hiccup) never pauses the music; one behind real time never
/// gets there. A start or a splice counts as being there.
pub const BEHIND_S: f64 = 1.0;
/// Release with this much buffered (s) …
pub const RELEASE_S: f64 = 1.0;
/// … once the render filled at this many times real time while held …
pub const KEEP_UP: f64 = 1.1;
/// … measured over this long at least (s).
pub const MEASURE_S: f64 = 0.25;

/// What the render sees, each time round.
#[derive(Clone, Copy, Debug)]
pub struct Seen {
    /// A clock in seconds (monotonic).
    pub now_s: f64,
    /// Frames written ahead of the reader.
    pub buffered: u64,
    /// The render's write position (frames); it only grows between splices.
    pub written: u64,
    /// The stream's rate.
    pub rate: f64,
    /// The frames the render fills to while held (the ring's ceiling).
    pub top: u64,
    /// The render is at its look-ahead (or the ring is full) this time.
    pub full: bool,
    /// A chain of its own plays: one on the air, not ended, without a
    /// look-ahead of its own (the input's is that of what comes in).
    pub on_air: bool,
    /// The output is paused or held for a jump: nothing to engage.
    pub out_busy: bool,
    /// A rack change of the track on the air is in flight: a hold waits for
    /// the new chain rather than go back to the old one.
    pub wait_new: bool,
}

/// What `decide` asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Nothing changes.
    Stay,
    /// Hold the music: the render fell behind with this much buffered (s).
    Engage { buffered_s: f64 },
    /// Let it go: held this long (s), filled at this many times real time.
    Release { held_s: f64, fill: f64 },
}

#[derive(Clone, Copy, Debug)]
struct Held {
    /// When the measurement began (the hold, or the splice since).
    t0: f64,
    /// The write position then.
    w0: u64,
    /// When the hold began.
    since: f64,
    /// A rack change in flight keeps it (`Seen::wait_new`); a seek's does
    /// not wait for one.
    waits_new: bool,
}

/// The starved hold's state, the render's own.
#[derive(Debug, Default)]
pub struct Starve {
    held: Option<Held>,
    /// The last time the render was at its look-ahead (or a start/splice).
    last_full: Option<f64>,
}

impl Starve {
    pub fn new() -> Starve {
        Starve::default()
    }

    /// The music is held.
    pub fn held(&self) -> bool {
        self.held.is_some()
    }

    /// A new stream or none (Start, Stop): nothing held, a fresh start.
    pub fn reset(&mut self, now_s: f64) {
        self.held = None;
        self.last_full = Some(now_s);
    }

    /// A splice (a jump, a seek, a swap): what was written before it is not
    /// the new chain's — a hold measures it afresh, and a render that keeps
    /// up has `BEHIND_S` to show it.
    pub fn splice(&mut self, s: &Seen) {
        self.last_full = Some(s.now_s);
        if let Some(h) = self.held.as_mut() {
            h.t0 = s.now_s;
            h.w0 = s.written;
        }
    }

    /// Hold now, whatever is buffered: a rack change's new chain goes on
    /// the air from nothing (the dip jump), and is heard once it has its
    /// cushion and keeps up. Measured from here.
    pub fn engage(&mut self, s: &Seen) {
        self.engage_as(s, true);
    }

    /// Hold now at a seek's splice: the pre-roll goes on the air in
    /// silence, and the new place is heard once it has its cushion and
    /// keeps up — one clean silence. Heard at once, a render behind real
    /// time played the pre-roll and fell silent again (Anton 31.10, 1G: a
    /// fade, a blip, silence, then the music). A rack change being
    /// prepared beside it is not waited for: the seek's chain is the one
    /// to hear.
    pub fn engage_seek(&mut self, s: &Seen) {
        self.engage_as(s, false);
    }

    fn engage_as(&mut self, s: &Seen, waits_new: bool) {
        let since = self.held.map_or(s.now_s, |h| h.since);
        self.held = Some(Held { t0: s.now_s, w0: s.written, since, waits_new });
        self.last_full = Some(s.now_s);
    }

    /// The rule, each time round.
    pub fn decide(&mut self, s: &Seen) -> Step {
        if s.full {
            self.last_full = Some(s.now_s);
        }
        let rate = s.rate.max(1.0);
        match self.held {
            None => {
                let behind = self.last_full.map_or(true, |t| s.now_s - t >= BEHIND_S);
                if s.on_air && !s.out_busy && behind && (s.buffered as f64) < ENGAGE_S * rate {
                    self.held = Some(Held { t0: s.now_s, w0: s.written, since: s.now_s, waits_new: true });
                    return Step::Engage { buffered_s: s.buffered as f64 / rate };
                }
                Step::Stay
            }
            Some(h) => {
                let held_s = s.now_s - h.since;
                // Nothing more to come (the track ended, stopped): what is
                // written plays out.
                if !s.on_air {
                    self.held = None;
                    return Step::Release { held_s, fill: 0.0 };
                }
                if s.wait_new && h.waits_new {
                    return Step::Stay;
                }
                let el = s.now_s - h.t0;
                let fill = s.written.saturating_sub(h.w0) as f64 / rate / el.max(1e-9);
                let need = ((RELEASE_S * rate) as u64).min(s.top);
                let keeps_up = s.buffered >= need && el >= MEASURE_S && fill >= KEEP_UP;
                // It cannot keep real time: the ring full, a long clean
                // stretch plays before the next pause.
                let topped = s.buffered >= s.top || (s.full && s.buffered >= need);
                if keeps_up || (topped && el >= MEASURE_S) {
                    self.held = None;
                    self.last_full = Some(s.now_s);
                    return Step::Release { held_s, fill };
                }
                Step::Stay
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 100_000.0;

    /// A render and an output on a fake clock: the output reads in real
    /// time unless held or paused; the render writes at `speed` × real time
    /// up to its look-ahead (`ahead_s`), up to `top` while held.
    struct Sim {
        st: Starve,
        now: f64,
        buffered: f64,
        written: f64,
        speed: f64,
        ahead_s: f64,
        top_s: f64,
        on_air: bool,
        out_busy: bool,
        wait_new: bool,
        held_by_output: bool,
        dropouts_s: f64,
        heard_s: f64,
    }

    impl Sim {
        fn new(speed: f64, buffered_s: f64) -> Sim {
            let mut st = Starve::new();
            st.reset(0.0);
            Sim {
                st,
                now: 0.0,
                buffered: buffered_s * RATE,
                written: buffered_s * RATE,
                speed,
                ahead_s: 3.0,
                top_s: 10.0,
                on_air: true,
                out_busy: false,
                wait_new: false,
                held_by_output: false,
                dropouts_s: 0.0,
                heard_s: 0.0,
            }
        }

        fn seen(&self, full: bool) -> Seen {
            Seen {
                now_s: self.now,
                buffered: self.buffered as u64,
                written: self.written as u64,
                rate: RATE,
                top: (self.top_s * RATE) as u64,
                full,
                on_air: self.on_air,
                out_busy: self.out_busy,
                wait_new: self.wait_new,
            }
        }

        /// One 10 ms round; the steps `decide` asked for.
        fn tick(&mut self) -> Step {
            let dt = 0.01;
            let cap = if self.st.held() { self.top_s } else { self.ahead_s } * RATE;
            let w = (self.speed * dt * RATE).min((cap - self.buffered).max(0.0));
            self.buffered += w;
            self.written += w;
            let full = self.buffered >= cap - 1.0;
            if !self.held_by_output && !self.out_busy {
                let r = (dt * RATE).min(self.buffered);
                self.dropouts_s += dt - r / RATE;
                self.heard_s += r / RATE;
                self.buffered -= r;
            }
            self.now += dt;
            let step = self.st.decide(&self.seen(full));
            match step {
                Step::Engage { .. } => self.held_by_output = true,
                Step::Release { .. } => self.held_by_output = false,
                Step::Stay => {}
            }
            step
        }

        fn run(&mut self, secs: f64) -> Vec<Step> {
            (0..(secs / 0.01).round() as usize).map(|_| self.tick()).filter(|s| *s != Step::Stay).collect()
        }
    }

    #[test]
    fn a_render_that_keeps_up_never_holds() {
        let mut s = Sim::new(3.0, 1.0);
        assert!(s.run(30.0).is_empty());
        assert_eq!(s.dropouts_s, 0.0);
    }

    #[test]
    fn a_short_stall_of_a_render_that_keeps_up_never_holds() {
        let mut s = Sim::new(3.0, 1.0);
        s.ahead_s = 0.6;
        s.run(5.0);
        // 0.4 s with nothing written: down to 0.2 s buffered.
        s.speed = 0.0;
        s.run(0.4);
        s.speed = 3.0;
        assert!(s.run(5.0).is_empty(), "back at its look-ahead within BEHIND_S");
        assert_eq!(s.dropouts_s, 0.0);
    }

    #[test]
    fn a_render_behind_real_time_holds_cleanly_instead_of_dropping_out() {
        // rtf 0.7, from a full look-ahead: no dropout, ever — the music
        // pauses and comes back in long stretches.
        let mut s = Sim::new(0.7, 3.0);
        let steps = s.run(60.0);
        assert_eq!(s.dropouts_s, 0.0, "no holes in the music");
        let engages = steps.iter().filter(|x| matches!(x, Step::Engage { .. })).count();
        assert!(engages >= 1, "it held: {:?}", steps);
        // Filled to the top each time it could not keep up: stretches of
        // ~10 s / (1 - 0.7) ≈ 33 s, not a stutter.
        assert!(engages <= 3, "few long stretches, not a stutter: {:?}", steps);
        for x in &steps {
            if let Step::Release { fill, .. } = x {
                assert!(*fill < KEEP_UP);
            }
        }
    }

    #[test]
    fn a_hold_lets_go_with_a_cushion_once_the_render_keeps_up_again() {
        let mut s = Sim::new(0.5, 3.0);
        let steps = s.run(8.0);
        assert!(matches!(steps.first(), Some(Step::Engage { .. })), "{:?}", steps);
        // The other work stops: it runs at 2× again.
        s.speed = 2.0;
        let steps = s.run(3.0);
        let Some(Step::Release { held_s, fill }) = steps.first().copied() else { panic!("{:?}", steps) };
        assert!(fill >= KEEP_UP);
        assert!(held_s < 8.0);
        assert!(s.buffered >= RELEASE_S * RATE - 1.0);
        assert_eq!(s.dropouts_s, 0.0);
    }

    #[test]
    fn paused_held_for_a_jump_or_with_nothing_on_the_air_it_never_engages() {
        for f in [|s: &mut Sim| s.out_busy = true, |s: &mut Sim| s.on_air = false] {
            let mut s = Sim::new(0.0, 0.2);
            f(&mut s);
            assert!(s.run(5.0).is_empty());
        }
    }

    #[test]
    fn the_track_ending_while_held_lets_what_is_written_play_out() {
        let mut s = Sim::new(0.5, 0.3);
        s.run(1.5);
        assert!(s.st.held());
        s.on_air = false;
        assert!(matches!(s.run(0.02).first(), Some(Step::Release { .. })));
        assert!(!s.st.held());
    }

    #[test]
    fn a_rack_change_in_flight_keeps_the_hold_until_its_chain_is_there() {
        let mut s = Sim::new(0.5, 3.0);
        s.run(8.0);
        assert!(s.st.held());
        s.wait_new = true;
        s.speed = 3.0;
        assert!(s.run(5.0).is_empty(), "never back onto the old chain");
        // The new chain's dip jump: the ring from its frame, measured afresh.
        s.buffered = 0.0;
        s.wait_new = false;
        let seen = s.seen(false);
        s.st.splice(&seen);
        let steps = s.run(2.0);
        let Some(Step::Release { fill, .. }) = steps.first().copied() else { panic!("{:?}", steps) };
        assert!(fill >= KEEP_UP);
        assert_eq!(s.dropouts_s, 0.0);
    }

    #[test]
    fn engaged_at_a_dip_jump_it_is_heard_once_the_new_chain_has_its_cushion() {
        let mut s = Sim::new(1.5, 0.0);
        let seen = s.seen(false);
        s.st.engage(&seen);
        s.held_by_output = true;
        let steps = s.run(2.0);
        let Some(Step::Release { held_s, fill }) = steps.first().copied() else { panic!("{:?}", steps) };
        // 1 s of music at 1.5×: ~0.67 s of silence, then no dropout.
        assert!((0.6..0.8).contains(&held_s), "{held_s}");
        assert!((fill - 1.5).abs() < 0.1, "{fill}");
        s.run(10.0);
        assert_eq!(s.dropouts_s, 0.0);
    }

    /// A seek's hold: the pre-roll (0.33 s) in silence, heard once the
    /// render has 1 s and keeps up — never the pre-roll, then a hole. A
    /// rack change prepared beside it does not hold it longer.
    #[test]
    fn a_seek_is_held_until_its_place_keeps_up_whatever_is_prepared_beside() {
        for wait_new in [false, true] {
            let mut s = Sim::new(1.3, 0.33);
            s.wait_new = wait_new;
            let seen = s.seen(false);
            s.st.engage_seek(&seen);
            s.held_by_output = true;
            let steps = s.run(2.0);
            let Some(Step::Release { held_s, .. }) = steps.first().copied() else { panic!("{wait_new}: {:?}", steps) };
            assert!(held_s < 0.7, "{wait_new}: {held_s}");
            assert!(s.buffered >= RELEASE_S * RATE - 1.0);
            s.run(10.0);
            assert_eq!(s.dropouts_s, 0.0, "{wait_new}: one silence, no hole after it");
        }
        // A dip's hold still waits for its chain.
        let mut s = Sim::new(3.0, 0.0);
        s.wait_new = true;
        let seen = s.seen(false);
        s.st.engage(&seen);
        s.held_by_output = true;
        assert!(s.run(2.0).is_empty());
    }

    #[test]
    fn a_fast_new_chain_is_heard_after_the_shortest_measure() {
        let mut s = Sim::new(8.0, 0.0);
        let seen = s.seen(false);
        s.st.engage(&seen);
        s.held_by_output = true;
        let Some(Step::Release { held_s, .. }) = s.run(1.0).first().copied() else { panic!() };
        assert!(held_s <= MEASURE_S + 0.02, "{held_s}");
    }
}
