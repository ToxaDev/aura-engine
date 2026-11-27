//! What the input's reader (`link`) hands its session: the player's
//! interleaved samples as left and right, through the rack's source stages,
//! into the session's live source — the way the radio's decoder feeds it
//! (`radio::decode`), less the joiner and the splicer: the stream keeps the
//! device's clock, so nothing is placed, cut or played twice.
//!
//! A segment is one run of the player in one format. Its end (the player
//! stopped) finishes the stages and fades the source out where it ends; the
//! next segment at the same rate fades in after it — after the silence the
//! chain reads meanwhile, if the player stopped longer than the source held.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use super::pace::Pace;
use crate::player::radio::decode::{finish_stages, split, through_stages};
use crate::player::radio::live::LiveSource;
use crate::player::radio::RadioShared;
use crate::player::source_stages::SourceStages;

/// The margin: what the source keeps past what the chain has asked for,
/// seconds (`AURA_INPUT_AHEAD_S`) — before the chain is built, beyond what
/// it waits for; then the least it holds just after the chain took a
/// portion (`pace`). The reader takes from the link only up to that: the
/// player waits for room, and so plays at the rate the device takes the
/// music. Read as fast as the player sends, it would run ahead of the music
/// by whole tracks; and whatever the source holds is that much later to the
/// ear.
pub const AHEAD_S: f64 = 0.05;
/// What the source may hold beyond the margin once the chain plays, seconds:
/// the chain's portion (a quarter of a second) with room to spare. The even
/// pace keeps the source below it on its own; the cap only stops a reader
/// whose chain stopped taking from running on.
pub const CUSHION_S: f64 = 0.5;
/// What the source may gather beyond what the chain waits for while it is
/// planned and built, seconds: the reader goes on at the player's pace
/// meanwhile (a rack's banks take seconds), and the chain begins as far
/// into it as its level allows (`radio_prepare`). Past it the reader stands.
pub const BUILD_ROOM_S: f64 = 5.0;

/// The session's feeding end.
pub struct Feed {
    shared: Arc<RadioShared>,
    live: Arc<LiveSource>,
    rate: u32,
    channels: usize,
    /// The rack's source stages: made at a segment's first samples.
    stages: Option<SourceStages>,
    /// A segment is being fed (its end not seen yet).
    open: bool,
    ahead_s: f64,
    /// The reader's clock.
    pace: Pace,
    /// The chain on the air as the session tells it — output rate, delay
    /// (bits), route: another in its place goes on from the same frame
    /// (BIT-PERFECT turned on, another rack), and the pace is told.
    chain: Option<(u32, u64, String)>,
    /// Chains gone on the air after a switch, as last seen; the first chain
    /// seen, and whether it has gone on the air.
    aired: u32,
    first_air: Option<bool>,
    /// The chain is built, and the driver may call the player by its own
    /// clock (`AURA_INPUT_DRIVER_CLOCK`).
    built: bool,
    clock: bool,
    l: Vec<f64>,
    r: Vec<f64>,
}

impl Feed {
    /// Feeds `shared` (an input session, its live source made) with a
    /// segment of `channels` that has begun.
    pub fn new(shared: Arc<RadioShared>, channels: u32) -> Result<Feed, String> {
        let live = shared.live.get().cloned().ok_or("the input's session has no source")?;
        let rate = live.rate();
        let ahead_s = std::env::var("AURA_INPUT_AHEAD_S").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(AHEAD_S).max(0.0);
        let hold = shared.source_plan(rate).latency_frames() as i64;
        let margin = (ahead_s * rate as f64) as i64;
        shared.set_keep(margin + hold);
        let aired = shared.aired_count();
        Ok(Feed {
            shared,
            live,
            rate,
            channels: channels.max(1) as usize,
            stages: None,
            open: true,
            ahead_s,
            pace: Pace::new(rate, margin, hold),
            chain: None,
            aired,
            first_air: None,
            built: false,
            clock: std::env::var("AURA_INPUT_DRIVER_CLOCK").map_or(true, |v| v.trim() != "0"),
            l: Vec::new(),
            r: Vec::new(),
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The session ended (Stop, another source, a failure): this feed goes.
    pub fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    /// The most the source holds ahead of the chain's reader, frames, and
    /// AHEAD_S over it: until the chain is built, what it waits for (its
    /// look-ahead and the margin; the margin alone until it is planned);
    /// then the cap over the even pace (`CUSHION_S`, over what the chain on
    /// the air leaves in the source: `Pace::cap`) — what the chain holds
    /// read ahead of the sound is its own (`radio::chain`).
    pub fn ceiling(&self) -> i64 {
        let (need_s, built) = {
            let i = self.shared.info.lock().unwrap();
            (i.need_s, i.t_built_ms.is_some())
        };
        let rate = self.rate as f64;
        if built {
            return self.pace.cap((CUSHION_S * rate) as i64);
        }
        ((need_s.max(self.shared.margin_s) + self.ahead_s) * rate) as i64
    }

    /// The samples the link holds, not read yet (the reader's look).
    pub fn note_link(&self, samples: u64) {
        self.shared.link_frames.store(samples / self.channels as u64, Ordering::Relaxed);
    }

    /// What the input's delay to the sound is reported as to the player
    /// (the driver adds its own buffer), frames: as last measured, or the
    /// source's ceiling before it is.
    pub fn delay_frames(&self) -> i64 {
        match self.shared.input_delay_s() {
            Some(s) => (s * self.rate as f64) as i64,
            None => self.ceiling(),
        }
    }

    /// Frames the reader may take from the link now, by what is ahead of
    /// the chain's reader, or of what it waits for (`ahead_of_asked`): at
    /// the even pace (`pace`) always — until the chain is built at the
    /// stream's own rate, up to `BUILD_ROOM_S` past the ceiling; never what
    /// the chain waits for at once. The pace learns by what the source and
    /// its stages hold together (the stages hand frames on a block at a
    /// time), and only while the player plays; and hears of each chain on
    /// the air — the first one's level settles from then, as another's.
    pub fn room(&mut self) -> i64 {
        let now = Instant::now();
        let ahead = self.live.ahead_of_asked();
        let (changed, first_aired) = {
            let i = self.shared.info.lock().unwrap();
            self.built = i.t_built_ms.is_some();
            if !self.built {
                drop(i);
                let cap = self.ceiling() + (BUILD_ROOM_S * self.rate as f64) as i64;
                return self.pace.room(now, ahead, 0, cap, false);
            }
            let same = self.chain.as_ref().is_some_and(|(r, d, route)| *r == i.out_rate && *d == i.delay_s.to_bits() && *route == i.route);
            let first = self.chain.is_none();
            if !same {
                self.chain = Some((i.out_rate, i.delay_s.to_bits(), i.route.clone()));
            }
            if first {
                self.first_air = Some(false);
            }
            let first_aired = self.first_air == Some(false) && i.t_on_air_ms.is_some();
            (!same, first_aired)
        };
        if changed {
            // The first chain, too: its level settles once it is on the air.
            self.pace.chain_changed(now, self.first_air == Some(false) || self.shared.reopening());
        }
        if first_aired {
            self.first_air = Some(true);
            self.pace.aired(now);
        }
        let aired = self.shared.aired_count();
        if aired != self.aired {
            self.aired = aired;
            self.pace.aired(now);
        }
        let cap = self.ceiling();
        let held = self.shared.fed_frames.load(Ordering::Relaxed) as i64 - self.live.stats().real_frames as i64;
        let room = self.pace.room(now, ahead, held.max(0), cap, self.open);
        // A rack's one go, for the device's wait when BIT-PERFECT is
        // turned off again (BIT-PERFECT's own is a step of the render).
        if self.chain.as_ref().is_some_and(|c| c.2 != "direct") {
            self.shared.note_burst(self.pace.burst());
        }
        room
    }

    /// The reader takes the link at its even pace (from the start, the chain
    /// built or not): the driver may call the player by its own clock
    /// (`aura_link::clock`) — unless `AURA_INPUT_DRIVER_CLOCK=0`.
    pub fn paced(&self) -> bool {
        self.clock
    }

    /// The reader took `frames` from the link.
    pub fn took(&mut self, frames: usize) {
        self.pace.took(frames as i64);
    }

    /// The player's samples, interleaved.
    pub fn push(&mut self, x: &[f64]) {
        let Feed { shared, live, rate, channels, stages, open, l, r, .. } = self;
        split(x, *channels, l, r);
        if l.is_empty() {
            return;
        }
        if !*open {
            *open = true;
            shared.info.lock().unwrap().connected = true;
        }
        {
            let mut i = shared.info.lock().unwrap();
            if i.t_first_pcm_ms.is_none() {
                i.t_first_pcm_ms = Some(shared.t0.elapsed().as_millis() as u64);
            }
        }
        let st = stages.get_or_insert_with(|| SourceStages::new(&shared.source_plan(*rate), shared.source_tally.clone()));
        // The source as it is, for the adaptive headroom's look.
        shared.first_look_push(l, r, *rate);
        shared.fed_frames.fetch_add(l.len() as u64, Ordering::Relaxed);
        through_stages(live, st, l, r);
    }

    /// A new segment at this rate (`channels` its own): the one before is
    /// over, if its end was not seen.
    pub fn segment(&mut self, channels: u32) {
        self.end();
        self.channels = channels.max(1) as usize;
    }

    /// The player stopped: what the stages hold goes in, and the source
    /// fades out where it ends — the chain reads silence after it, until
    /// the player plays again (it fades in then).
    pub fn end(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        if let Some(st) = self.stages.take() {
            finish_stages(st, &self.live);
        }
        self.live.mark_gap();
        self.shared.info.lock().unwrap().connected = false;
    }

    /// A segment is being fed.
    pub fn open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::radio::{Session, INPUT_MARGIN_S};
    use crate::player::settings::PlayerSettings;

    /// A rack without source stages: what is pushed is in the source at once.
    fn plain() -> PlayerSettings {
        PlayerSettings { isp: false, subsonic_hz: 0, apodizing: 0, adaptive_apodizer: false, declip: false, adaptive_headroom: false, ..Default::default() }
    }

    /// The reader keeps the player's pace from the start: until the chain is
    /// built the source gathers at it (not all the chain waits for at once:
    /// the player would be called a dozen times a moment); then under a cap
    /// of the chain's portion with room to spare and AHEAD_S — not seconds
    /// more (it held need + 1.5 s, all of it delay to the sound). A chain
    /// that waits is given what it waits for no faster: a little at a time.
    #[test]
    #[cfg_attr(not(windows), ignore = "timing under a loaded runner; the input is Windows-only")]
    fn the_source_is_kept_thin_once_the_chain_is_built() {
        let rate = 44_100.0;
        let s = Session::start_input(&plain(), 44_100, 2, "test").unwrap();
        let mut f = Feed::new(s.shared.clone(), 2).unwrap();
        assert_eq!(f.ceiling(), ((INPUT_MARGIN_S + AHEAD_S) * rate) as i64, "before the plan: the margin");
        s.mark(|i, _| i.need_s = 0.74);
        assert_eq!(f.ceiling(), ((0.74 + AHEAD_S) * rate) as i64, "planned: what the chain waits for");
        assert!(f.room() < (0.01 * rate) as i64, "planned, not built: at the player's pace, not all of it at once");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = f.room();
        assert!(later > 0 && later <= (0.05 * rate) as i64, "20 ms on: the time's worth, {later} frames");
        s.mark(|i, ms| i.t_built_ms = Some(ms));
        assert_eq!(f.ceiling(), ((CUSHION_S + AHEAD_S) * rate) as i64, "built: the cap and AHEAD_S");
        assert!(f.ceiling() < (0.6 * rate) as i64);
        let live = s.shared.live.get().unwrap().clone();
        let ask = std::thread::spawn(move || live.ensure(4_410));
        while f.live.ahead_of_asked() >= 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let margin = (AHEAD_S * rate) as i64;
        let waits = f.room();
        let since = std::time::Instant::now();
        assert!(waits < 4_410, "the chain waits: given its 0.1 s at the player's pace, not at once ({waits} frames)");
        f.push(&vec![0.0; 2 * 4_410]);
        f.took(4_410);
        ask.join().unwrap();
        f.push(&vec![0.0; 2 * margin as usize]);
        f.took(margin as usize);
        // The time's worth since the room before (a loaded machine may take
        // a while over the pushes), not what the chain asked for.
        let worth = ((since.elapsed().as_secs_f64() + 0.002) * rate) as i64;
        let first = f.room();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = f.room();
        assert!(first <= worth.max((0.01 * rate) as i64), "fed: a little at a time, {first} frames ({worth} the time's worth)");
        assert!(later > first && later <= (0.05 * rate) as i64, "20 ms on: the time's worth, {later} frames");
        s.stop();
    }

    /// BIT-PERFECT turned on (the session's chain another: its output rate,
    /// delay and route): the pace hears of it — the cap stands higher while
    /// the new chain's level settles; the same chain told again is not news.
    #[test]
    fn another_chain_on_the_air_is_told_to_the_pace() {
        let rate = 44_100.0;
        let s = Session::start_input(&plain(), 44_100, 2, "test").unwrap();
        let mut f = Feed::new(s.shared.clone(), 2).unwrap();
        s.mark(|i, ms| {
            i.t_built_ms = Some(ms);
            i.out_rate = 352_800;
            i.route = "gpu".into();
            i.delay_s = 0.05;
        });
        f.room();
        let cap = f.ceiling();
        let settling = (super::super::pace::SETTLE_ROOM_S * rate) as i64;
        assert_eq!(cap, ((CUSHION_S + AHEAD_S) * rate) as i64 + settling, "the first chain's level settles too");
        f.room();
        assert_eq!(f.ceiling(), cap, "the same chain");
        s.mark(|i, _| {
            i.out_rate = 44_100;
            i.route = "direct".into();
            i.delay_s = 0.0;
        });
        f.room();
        assert_eq!(f.ceiling(), cap, "another: its level settles under the higher cap");
        s.stop();
    }

    /// BIT-PERFECT turned on or off: until the new chain is on the air, a
    /// chain waiting for the source is given it at the pace, not at once
    /// (the device waits for its level: `level_short`); on the air neither
    /// (the render's look-ahead waits). The device's wait: the render short of its
    /// look-ahead, or the source short of the level and the chain's portion
    /// by the time the render steps on.
    #[test]
    #[cfg_attr(not(windows), ignore = "timing under a loaded runner; the input is Windows-only")]
    fn a_switch_s_chain_is_waited_for_by_the_device_not_the_player() {
        let rate = 44_100.0;
        let s = Session::start_input(&plain(), 44_100, 2, "test").unwrap();
        let mut f = Feed::new(s.shared.clone(), 2).unwrap();
        let margin = (AHEAD_S * rate) as i64;
        assert_eq!(s.shared.keep(), margin, "no stages: the margin is the level");
        s.mark(|i, ms| {
            i.t_built_ms = Some(ms);
            i.route = "cpu".into();
        });
        f.room();
        f.push(&vec![0.0; 2 * 8_820]);
        let r = f.room();
        f.took(r.max(0) as usize);
        let live = s.shared.live.get().unwrap().clone();
        live.ensure(4_410);
        assert_eq!(live.ask_step(), 4_410, "the chain's portion");
        s.shared.reopen_begins();
        assert!(s.shared.reopening());
        assert_eq!(live.ask_step(), 0, "the chain before's portions forgotten");
        s.mark(|i, _| i.route = "direct".into());
        let r = f.room();
        f.took(r.max(0) as usize);
        let l2 = live.clone();
        let ask = std::thread::spawn(move || l2.ensure(8_820 + 2_205));
        while live.ahead_of_asked() >= 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(f.room() < (0.01 * rate) as i64, "not on the air yet: the pace only");
        assert!(s.shared.level_short(1_000, 2_000.0, 44_100), "the render short of its look-ahead");
        assert!(s.shared.level_short(2_000, 2_000.0, 44_100), "the source short: the chain waits");
        s.shared.reaired();
        assert!(!s.shared.reopening());
        assert!(f.room() < (0.01 * rate) as i64, "on the air too: the pace only, nothing at once");
        f.push(&vec![0.0; 2 * (2_205 + 2 * margin as usize + 2_205)]);
        ask.join().unwrap();
        assert_eq!(live.ask_step(), 2_205 + 4_410, "past the furthest asked before");
        // 6615 past the edge: short of the level (2205) and the portion
        // (6615), less what the render's 1000 frames of slack let the device
        // play before the chain asks again; 4000 of slack cover it.
        assert!(s.shared.level_short(3_000, 2_000.0, 44_100));
        assert!(!s.shared.level_short(6_000, 2_000.0, 44_100));
        s.stop();
    }

    /// The status's delay is the link, the stages and the source past what
    /// is heard; what the driver reports to the player follows it.
    #[test]
    fn the_delay_is_what_lies_between_the_link_and_the_sound() {
        let s = Session::start_input(&plain(), 44_100, 2, "test").unwrap();
        let mut f = Feed::new(s.shared.clone(), 2).unwrap();
        assert_eq!(f.delay_frames(), f.ceiling(), "nothing measured: the ceiling");
        assert!(s.shared.input_delay(None).is_null(), "nothing heard");
        f.push(&vec![0.0; 2 * 44_100]);
        f.note_link(2 * 4_410);
        let v = s.shared.input_delay(Some(22_050));
        let total = v["totalS"].as_f64().unwrap();
        assert!((total - 0.6).abs() < 1e-3, "0.1 s in the link and 0.5 s of the source not heard: {v}");
        assert_eq!(v["stagesS"].as_f64(), Some(0.0));
        assert_eq!((v["subS"].as_f64(), v["withoutSubS"].as_f64()), (Some(0.0), v["totalS"].as_f64()), "no SUB: nothing of its");
        assert!((v["sourceS"].as_f64().unwrap() - 1.0).abs() < 1e-3, "nothing read: the whole second ahead");
        assert_eq!(v["renderS"].as_f64(), Some(crate::player::radio::INPUT_RENDER_S));
        assert_eq!(f.delay_frames(), 26_460);
        s.stop();
        let station = RadioShared::new("http://station", &plain());
        station.live_for(44_100).unwrap();
        assert!(station.input_delay(Some(0)).is_null(), "a station's delay is not the input's");
        assert!(station.render_ahead_s().is_none(), "nor its render's look-ahead");
    }

    /// SUB15's share of the delay on a line of its own: its stage and its
    /// guard on the output, half a second each — and the delay without it.
    #[test]
    fn the_subsonic_filter_s_share_is_said_apart() {
        let s = Session::start_input(&PlayerSettings { subsonic_hz: 15, ..plain() }, 44_100, 2, "test").unwrap();
        s.mark(|i, _| i.out_rate = 88_200);
        let mut f = Feed::new(s.shared.clone(), 2).unwrap();
        f.push(&vec![0.0; 2 * 88_200]);
        let v = s.shared.input_delay(Some(0));
        let (total, sub) = (v["totalS"].as_f64().unwrap(), v["subS"].as_f64().unwrap());
        assert!(sub > 1.08 && sub < 1.095, "SUB15: 0.54 s in the stage and 0.54 s in the guard, {sub:.4} s");
        assert!(v["stagesS"].as_f64().unwrap() > 0.4, "the stage holds its share: {v}");
        assert!((v["withoutSubS"].as_f64().unwrap() - (total - sub)).abs() < 1e-12);
        s.stop();
    }
}
