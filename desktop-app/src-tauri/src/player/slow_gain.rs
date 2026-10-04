//! The level of a chain whose whole output has not been measured: a slow
//! gain that looks a few seconds ahead.
//!
//! A file's chain takes its level from one number, the true peak of its
//! whole render (`chain::output_peak`): the overs are held at the ceiling by
//! the output limiter when they are rare and within 6 dB, and otherwise the
//! whole track comes down to the ceiling. A stream has no whole render, and
//! neither has a file under Instant start before its full variant is ready.
//! This stage stands in: it reads the chain ahead by up to `WINDOW_S`
//! (no further than the source has come, `horizon`), measures the 4× true
//! peak of every millisecond there, and decides the way the file path does —
//! on what it has seen of the last `HISTORY_S` and the window ahead: overs
//! within 6 dB on under 5 % of the time are left to the limiter (gain 1.0),
//! more comes down. Down, it glides (smoothstep) so it is at the level a
//! peak needs when the peak arrives; up again, it climbs 6 dB in 20 s, never
//! above what is coming. It goes no lower than −6 dB: deeper is the
//! limiter's. On material at one level it settles on the file's own scalar
//! after the loudest place; before it, the level is a little higher, and the
//! limiter holds what is over.
//!
//! A stream says where its songs begin (`with_song_starts`): there what was
//! learned of the last song goes, and the gain may climb back in a second
//! instead of twenty — a quiet song after a loud one is not played down.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::stages::Stage;

/// How far ahead it looks, seconds.
pub const WINDOW_S: f64 = 5.0;
/// How much behind counts in the local/global decision, seconds.
const HISTORY_S: f64 = 10.0;
/// The decision's step, seconds (one true-peak maximum per step).
const STEP_S: f64 = 0.001;
/// Up again: this many dB in `RISE_S` seconds.
const RISE_DB: f64 = 6.0;
const RISE_S: f64 = 20.0;
/// After a song start: `RISE_DB` in this many seconds, for up to
/// `SONG_FAST_S` seconds.
const SONG_RISE_S: f64 = 1.0;
const SONG_FAST_S: f64 = 3.0;
/// The lowest it goes, dB.
const FLOOR_DB: f64 = -6.0;
/// Overs the limiter holds by itself: within this many dB…
const LOCAL_DB: f64 = 6.0;
/// …on no more than this share of the time.
const LOCAL_SHARE: f64 = 0.05;
/// The share is of at least this much seen, seconds.
const MIN_SEEN_S: f64 = 2.0;
/// Back to local only well inside both (no flapping at the edge).
const LOCAL_DB_BACK: f64 = 5.0;
const LOCAL_SHARE_BACK: f64 = 0.04;

pub struct SlowGainStage {
    up: Box<dyn Stage>,
    total: u64,
    target: f64,
    step: usize,
    window: usize,
    history: usize,
    min_seen: usize,
    rise: f64,
    /// The climb after a song start, and the step it lasts until.
    rise_song: f64,
    song_steps: u64,
    fast_until: u64,
    floor: f64,
    coeffs: [[f64; 8]; 3],
    /// Frames read from `up`, not handed on yet: from `pos`.
    fl: VecDeque<f64>,
    fr: VecDeque<f64>,
    pos: u64,
    /// The three frames before `pos`, oldest first (the 4× view reads them).
    back_l: [f64; 3],
    back_r: [f64; 3],
    /// Frames of the queue whose true peak is in `peaks`.
    measured: usize,
    /// True peak (4×, both channels) of each step from `peak0` on: the
    /// history behind `pos`, then what is measured ahead of it.
    peaks: VecDeque<f64>,
    peak0: u64,
    global: bool,
    /// The gain at `pos`, and where it goes by the end of the step `pos`
    /// is in.
    g: f64,
    g_to: f64,
    /// The step `g_to` was decided for.
    decided: Option<u64>,
    /// The furthest output index `up` gives without waiting; None: no limit
    /// but the read-ahead's share of each read.
    horizon: Option<Box<dyn Fn() -> u64 + Send>>,
    /// The first song start at an output index or after it.
    songs: Option<Box<dyn Fn(u64) -> Option<u64> + Send>>,
    /// Frames read ahead and not handed on yet, for whoever measures what is
    /// in hand (a stream's buffer).
    queued: Option<Arc<AtomicU64>>,
    live: Arc<AtomicU64>,
    tmp_l: Vec<f64>,
    tmp_r: Vec<f64>,
}

impl SlowGainStage {
    /// The stage over `up` (at output rate `rate`), holding the output to
    /// `target` (linear) the way the file path's scalar would.
    pub fn new(up: Box<dyn Stage>, target: f64, rate: u32) -> SlowGainStage {
        let rate = rate.max(1) as f64;
        let step = ((STEP_S * rate).round() as usize).max(1);
        let steps_per_s = rate / step as f64;
        let pos = up.position();
        let total = up.total();
        SlowGainStage {
            up,
            total,
            target,
            step,
            window: ((WINDOW_S * steps_per_s) as usize).max(1),
            history: (HISTORY_S * steps_per_s) as usize,
            min_seen: (MIN_SEEN_S * steps_per_s) as usize,
            rise: 10f64.powf(RISE_DB / 20.0 / (RISE_S * steps_per_s)),
            rise_song: 10f64.powf(RISE_DB / 20.0 / (SONG_RISE_S * steps_per_s)),
            song_steps: (SONG_FAST_S * steps_per_s) as u64,
            fast_until: 0,
            floor: 10f64.powf(FLOOR_DB / 20.0),
            coeffs: crate::audio::converter::dsp::true_peak::lanczos4_poly_coeffs(),
            fl: VecDeque::new(),
            fr: VecDeque::new(),
            pos,
            back_l: [0.0; 3],
            back_r: [0.0; 3],
            measured: 0,
            peaks: VecDeque::new(),
            peak0: pos / step as u64,
            global: false,
            g: 1.0,
            g_to: 1.0,
            decided: None,
            horizon: None,
            songs: None,
            queued: None,
            live: Arc::new(AtomicU64::new(1.0f64.to_bits())),
            tmp_l: Vec::new(),
            tmp_r: Vec::new(),
        }
    }

    /// Read no further ahead than `h()` (an output index): what the source
    /// has without waiting for it (a live stream's network).
    pub fn with_horizon(mut self, h: impl Fn() -> u64 + Send + 'static) -> SlowGainStage {
        self.horizon = Some(Box::new(h));
        self
    }

    /// The gain it plays at now, as f64 bits: for the glide of the chain
    /// that replaces it.
    pub fn live_gain(&self) -> Arc<AtomicU64> {
        self.live.clone()
    }

    /// Keep `meter` at the frames read ahead and not handed on yet.
    pub fn with_queue_meter(mut self, meter: Arc<AtomicU64>) -> SlowGainStage {
        self.queued = Some(meter);
        self
    }

    /// Where songs begin: `f(i)` is the first song start at output index
    /// `i` or after it (a stream's titles, each tied to its frame).
    pub fn with_song_starts(mut self, f: impl Fn(u64) -> Option<u64> + Send + 'static) -> SlowGainStage {
        self.songs = Some(Box::new(f));
        self
    }

    /// A song begins here: what was learned of the last one goes — the
    /// decision's history behind — and for a few seconds the gain may climb
    /// back fast. It does not jump, and the call stands until what is ahead
    /// says otherwise: a song start told a little early still has the last
    /// song's tail to hold, and the window ahead still brings the gain down
    /// in time for what is coming.
    fn song_starts(&mut self) {
        self.decided = None;
        let s = self.pos / self.step as u64;
        while self.peak0 < s && !self.peaks.is_empty() {
            self.peaks.pop_front();
            self.peak0 += 1;
        }
        self.fast_until = s + self.song_steps;
    }

    /// The 4× true peak of queue frame `j` (needs j+4 in the queue).
    fn peak_at(&self, j: usize) -> f64 {
        let at = |ch: &VecDeque<f64>, back: &[f64; 3], k: i64| -> f64 {
            if k < 0 {
                back[(3 + k) as usize]
            } else {
                ch[k as usize]
            }
        };
        let mut m = 0.0f64;
        for (ch, back) in [(&self.fl, &self.back_l), (&self.fr, &self.back_r)] {
            m = m.max(ch[j].abs());
            for c in &self.coeffs {
                let mut v = 0.0;
                for (k, w) in c.iter().enumerate() {
                    v += at(ch, back, j as i64 + k as i64 - 3) * w;
                }
                m = m.max(v.abs());
            }
        }
        m
    }

    /// Measure the queue's frames that have their 4 frames after them in.
    fn measure(&mut self) {
        while self.measured + 4 < self.fl.len() {
            let p = self.peak_at(self.measured);
            let s = (self.pos + self.measured as u64) / self.step as u64;
            let i = (s - self.peak0) as usize;
            if i == self.peaks.len() {
                self.peaks.push_back(p);
            } else {
                self.peaks[i] = self.peaks[i].max(p);
            }
            self.measured += 1;
        }
    }

    /// Pull `n` frames from `up` onto the queue.
    fn pull(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.tmp_l.resize(n, 0.0);
        self.tmp_r.resize(n, 0.0);
        self.up.read(&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        self.fl.extend(self.tmp_l[..n].iter().copied());
        self.fr.extend(self.tmp_r[..n].iter().copied());
        self.measure();
    }

    /// The gain step `s` heads for: what the window ahead needs, and the
    /// slow climb.
    fn decide(&mut self, s: u64) {
        let now = (s - self.peak0) as usize;
        let hi = (now + self.window).min(self.peaks.len());
        let lo = now.saturating_sub(self.history).min(hi);
        // Local or global, on the history and the window ahead.
        let (mut max, mut over, mut seen) = (0.0f64, 0usize, 0usize);
        for &p in self.peaks.range(lo..hi) {
            max = max.max(p);
            over += (p > self.target) as usize;
            seen += 1;
        }
        let over_db = 20.0 * (max / self.target).max(1e-12).log10();
        // A share of a few blocks seen at a start says nothing yet.
        let share = over as f64 / seen.max(self.min_seen) as f64;
        self.global = if self.global {
            !(over_db < LOCAL_DB_BACK && share < LOCAL_SHARE_BACK)
        } else {
            over_db > LOCAL_DB || share > LOCAL_SHARE
        };
        // Down: arrive at each peak's gain as it arrives (smoothstep).
        let mut down = 1.0f64;
        if self.global {
            for (d, &p) in self.peaks.range(now.min(hi)..hi).enumerate() {
                if p <= self.target {
                    continue;
                }
                let need = (self.target / p).max(self.floor);
                let u = d as f64 / self.window as f64;
                let w = u * u * (3.0 - 2.0 * u);
                down = down.min(need + (1.0 - need) * w);
            }
        }
        let g = self.g_to;
        self.g = g;
        let rise = if s < self.fast_until { self.rise_song } else { self.rise };
        self.g_to = if down < g { down } else { (g * rise).min(down) }.clamp(self.floor, 1.0);
        self.decided = Some(s);
        // What is behind the history goes.
        let keep = s.saturating_sub(self.history as u64);
        while self.peak0 < keep && !self.peaks.is_empty() {
            self.peaks.pop_front();
            self.peak0 += 1;
        }
    }
}

impl Stage for SlowGainStage {
    fn is_gpu_failed(&self) -> bool {
        self.up.is_gpu_failed()
    }

    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len().min(out_r.len());
        // What this read needs (and the 4× view's 4 frames after it), then
        // the window ahead: what stays queued after the read grows by at most
        // half a read each time, so a start does not wait for the whole
        // window, until it holds the window.
        let need = (n + 4).saturating_sub(self.fl.len());
        let want_ahead = self.window * self.step;
        let keep = want_ahead.min(self.fl.len() + n / 2);
        let mut more = (n + keep).saturating_sub(self.fl.len() + need);
        if let Some(h) = &self.horizon {
            let end = self.pos + self.fl.len() as u64 + need as u64;
            more = more.min(h().saturating_sub(end) as usize);
        }
        self.pull(need + more);
        let step = self.step as u64;
        // A song start in this read (one told after its frame was played
        // is let go).
        let song = self.songs.as_ref().and_then(|f| f(self.pos)).filter(|&b| b < self.pos + n as u64);
        for i in 0..n {
            let at = self.pos;
            if song == Some(at) {
                self.song_starts();
            }
            let s = at / step;
            if self.decided != Some(s) {
                self.decide(s);
            }
            let u = (at % step) as f64 / step as f64;
            let g = self.g + (self.g_to - self.g) * u;
            let (l, r) = (self.fl.pop_front().unwrap_or(0.0), self.fr.pop_front().unwrap_or(0.0));
            self.back_l = [self.back_l[1], self.back_l[2], l];
            self.back_r = [self.back_r[1], self.back_r[2], r];
            self.measured = self.measured.saturating_sub(1);
            out_l[i] = l * g;
            out_r[i] = r * g;
            self.pos += 1;
        }
        self.live.store(self.g.to_bits(), Ordering::Relaxed);
        if let Some(q) = &self.queued {
            q.store(self.fl.len() as u64, Ordering::Relaxed);
        }
        (n as u64).min(self.total.saturating_sub(self.pos - n as u64)) as usize
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signal as a stage.
    struct Sig {
        l: Vec<f64>,
        pos: u64,
        pulled: Arc<AtomicU64>,
    }

    impl Stage for Sig {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for i in 0..out_l.len() {
                let p = self.pos as usize + i;
                let v = self.l.get(p).copied().unwrap_or(0.0);
                inside += (p < self.l.len()) as usize;
                out_l[i] = v;
                out_r[i] = v;
            }
            self.pos += out_l.len() as u64;
            self.pulled.store(self.pos, Ordering::Relaxed);
            inside
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.l.len() as u64
        }
    }

    const RATE: u32 = 8_000;

    /// A tone at `peak`, with `bursts` (start s, length s, peak) on top.
    fn tone(secs: f64, peak: f64, bursts: &[(f64, f64, f64)]) -> Vec<f64> {
        let n = (secs * RATE as f64) as usize;
        let mut x: Vec<f64> = (0..n).map(|i| peak * (2.0 * std::f64::consts::PI * 210.0 * i as f64 / RATE as f64).sin()).collect();
        for &(at, len, p) in bursts {
            for i in (at * RATE as f64) as usize..((at + len) * RATE as f64) as usize {
                if i < n {
                    x[i] = p * (2.0 * std::f64::consts::PI * 210.0 * i as f64 / RATE as f64).sin();
                }
            }
        }
        x
    }

    /// The gain it played at, per output frame (the output over the input).
    fn gains(x: &[f64], horizon_s: Option<f64>) -> (Vec<f64>, u64) {
        let (g, _, ahead) = run(x, horizon_s, &[]);
        (g, ahead)
    }

    /// The gain per frame, the output, and the most it read ahead; songs
    /// begin at `songs` (seconds).
    fn run(x: &[f64], horizon_s: Option<f64>, songs: &[f64]) -> (Vec<f64>, Vec<f64>, u64) {
        let pulled = Arc::new(AtomicU64::new(0));
        let played = Arc::new(AtomicU64::new(0));
        let sig = Sig { l: x.to_vec(), pos: 0, pulled: pulled.clone() };
        let mut st = SlowGainStage::new(Box::new(sig), 10f64.powf(-0.5 / 20.0), RATE);
        if let Some(h) = horizon_s {
            let p = played.clone();
            let ahead = (h * RATE as f64) as u64;
            // The source has `h` seconds more than was played (a stream's margin).
            st = st.with_horizon(move || p.load(Ordering::Relaxed) + ahead);
        }
        if !songs.is_empty() {
            let at: Vec<u64> = songs.iter().map(|s| (s * RATE as f64) as u64).collect();
            st = st.with_song_starts(move |i| at.iter().copied().find(|&b| b >= i));
        }
        let mut out = vec![0.0; x.len()];
        let mut r = vec![0.0; x.len()];
        let mut most_ahead = 0u64;
        let mut at = 0;
        while at < x.len() {
            let e = (at + 512).min(x.len());
            st.read(&mut out[at..e], &mut r[at..e]);
            most_ahead = most_ahead.max(pulled.load(Ordering::Relaxed) - e as u64);
            played.store(e as u64, Ordering::Relaxed);
            at = e;
        }
        let g = x.iter().zip(&out).map(|(a, b)| if a.abs() > 1e-3 { b / a } else { f64::NAN }).collect();
        (g, out, most_ahead)
    }

    fn db(g: f64) -> f64 {
        20.0 * g.log10()
    }

    fn range_db(g: &[f64]) -> (f64, f64) {
        g.iter().filter(|v| v.is_finite()).fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(db(v)), hi.max(db(v))))
    }

    #[test]
    fn material_under_the_ceiling_plays_at_one() {
        let (g, _) = gains(&tone(20.0, 0.8, &[]), None);
        let (lo, hi) = range_db(&g);
        assert!(hi - lo < 0.01 && lo > -0.01, "range {lo:.4}..{hi:.4} dB");
    }

    #[test]
    fn dense_overs_on_one_level_settle_on_the_files_scalar() {
        // Loud all through on one level, 2.5 dB over the ceiling's
        // -0.5 dBTP: the file's scalar to within 0.05 dB, and no breathing.
        let x = tone(40.0, 10f64.powf(2.0 / 20.0), &[]);
        let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let scalar = 10f64.powf(-0.5 / 20.0) / peak;
        let (g, _) = gains(&x, None);
        let after = &g[(1.0 * RATE as f64) as usize..(39.0 * RATE as f64) as usize];
        let (lo, hi) = range_db(after);
        assert!((lo - db(scalar)).abs() < 0.05 && (hi - db(scalar)).abs() < 0.05,
            "after the loudest place {lo:.3}..{hi:.3} dB, the file's scalar {:.3} dB", db(scalar));
    }

    #[test]
    fn rare_overs_within_reach_are_left_to_the_limiter() {
        // Three short overs 4 dB above the ceiling in 30 s.
        let o = 10f64.powf(3.5 / 20.0);
        let x = tone(30.0, 0.5, &[(5.0, 0.2, o), (14.0, 0.3, o), (25.0, 0.1, o)]);
        let (g, _) = gains(&x, None);
        let (lo, hi) = range_db(&g);
        assert!(lo > -0.001 && hi < 0.001, "range {lo:.4}..{hi:.4} dB");
    }

    #[test]
    fn a_quiet_song_after_a_loud_one_is_not_played_down() {
        // 20 s of dense overs 3.5 dB over the ceiling (the gain settles at
        // about -3.5 dB), then 20 s under it.
        let mut x = tone(20.0, 10f64.powf(3.0 / 20.0), &[]);
        x.extend(tone(20.0, 0.5, &[]));
        let at = |s: f64| (s * RATE as f64) as usize;
        let (g, _, _) = run(&x, None, &[20.0]);
        let held = range_db(&g[at(19.0)..at(19.5)]).1;
        assert!(held < -3.0, "the loud song is held: {held:.2} dB");
        let (lo, hi) = range_db(&g[at(21.0)..]);
        assert!(lo > -0.01 && hi < 0.001, "a second after the song start {lo:.3}..{hi:.3} dB");
        // Not told where it begins: the slow climb, still well down.
        let (g0, _, _) = run(&x, None, &[]);
        let slow = range_db(&g0[at(21.0)..at(21.1)]).1;
        assert!(slow < -2.5, "without the start {slow:.2} dB");
        // Told a second early: the loud song's last second is still held
        // to the ceiling, and the quiet one is not played down.
        let (g1, out, _) = run(&x, None, &[19.0]);
        let peak = out[at(18.0)..at(20.0)].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= 10f64.powf(-0.5 / 20.0) * 1.005, "the tail peaks at {:.3} dB", db(peak));
        assert!(range_db(&g1[at(21.0)..]).0 > -0.01);
    }

    #[test]
    fn a_title_said_again_with_its_album_does_not_start_the_gain_afresh() {
        // A stream's titles as the radio places them: a quiet song, a loud
        // one from 10 s (its ICY title), and Radio Paradise's list saying the
        // loud one again with its album at 30 s, where it turns quiet. The
        // song starts the gain is told are the stream's own: 10 s only.
        use crate::player::radio::RadioShared;
        use crate::player::settings::PlayerSettings;
        let sh = RadioShared::new("http://x/stream", &PlayerSettings::default());
        let live = sh.live_for(RATE).unwrap();
        let secs = |s: usize| vec![0.0; s * RATE as usize];
        live.push(&secs(1), &secs(1));
        sh.set_title("A - Quiet", "icy");
        live.push(&secs(9), &secs(9));
        sh.set_title("B - Loud", "icy");
        live.push(&secs(20), &secs(20));
        sh.rp_now_playing("B - Loud", Some("Album · 2001".into()), 100.0);
        let mut starts = Vec::new();
        while let Some(b) = sh.songs.first_from(starts.last().map_or(0, |&b: &i64| b + 1)) {
            starts.push(b);
        }
        assert_eq!(starts, vec![10 * RATE as i64], "one song start, the loud song's");

        let mut x = tone(10.0, 0.5, &[]);
        x.extend(tone(20.0, 10f64.powf(3.0 / 20.0), &[]));
        x.extend(tone(10.0, 0.5, &[]));
        let at = |s: f64| (s * RATE as f64) as usize;
        let told: Vec<f64> = starts.iter().map(|&b| b as f64 / RATE as f64).collect();
        let (g, _, _) = run(&x, None, &told);
        // The loud song's quiet end climbs back slowly, as within a song.
        let slow = range_db(&g[at(31.0)..at(31.1)]).1;
        assert!(slow < -2.5, "a second into the quiet end {slow:.2} dB");
        // What a second start at 30 s did: the gain back up within a second.
        let (g2, _, _) = run(&x, None, &[10.0, 30.0]);
        assert!(range_db(&g2[at(31.0)..]).0 > -0.01, "with a start there the climb is fast");
    }

    #[test]
    fn it_comes_to_look_the_whole_window_ahead() {
        // Reads of 512 frames: the window (5 s) is filled half a read at a
        // time, and then kept.
        let (_, _, ahead) = run(&tone(30.0, 0.5, &[]), None, &[]);
        let w = (WINDOW_S * RATE as f64) as u64;
        assert!(ahead >= w && ahead <= w + 600, "read {ahead} frames ahead of what it handed on, the window is {w}");
        // A loud place well ahead is met by a glide that begins seconds
        // before it, not at the last moment.
        let x = tone(20.0, 0.5, &[(10.0, 4.0, 10f64.powf(9.0 / 20.0))]);
        let (g, _, _) = run(&x, None, &[]);
        let at = |s: f64| (s * RATE as f64) as usize;
        let before = range_db(&g[at(7.0)..at(7.1)]).0;
        assert!(before < -0.5 && before > -9.0, "3 s before the loud place {before:.2} dB");
    }

    /// A stereo signal as a stage.
    struct Stereo {
        l: Vec<f64>,
        r: Vec<f64>,
        pos: u64,
    }

    impl Stage for Stereo {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            for i in 0..out_l.len() {
                let p = self.pos as usize + i;
                out_l[i] = self.l.get(p).copied().unwrap_or(0.0);
                out_r[i] = self.r.get(p).copied().unwrap_or(0.0);
            }
            self.pos += out_l.len() as u64;
            out_l.len()
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.l.len() as u64
        }
    }

    /// The slow gain on recorded streams (`AURA_SG_FILES`, `;` between
    /// them; `AURA_SG_CEILING` dBTP, default -0.5): with the whole window
    /// ahead, and seeing one read ahead as it used to. Per run: the share
    /// of the time under gain, how deep, and the share of milliseconds left
    /// over the ceiling for the limiter, how far.
    /// `cargo test --profile fast --bins slow_gain_on_recorded_streams -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn slow_gain_on_recorded_streams() {
        let Ok(files) = std::env::var("AURA_SG_FILES") else { return };
        let ceiling: f64 = std::env::var("AURA_SG_CEILING").ok().and_then(|v| v.parse().ok()).unwrap_or(-0.5);
        let target = 10f64.powf(ceiling / 20.0);
        let co = crate::audio::converter::dsp::true_peak::lanczos4_poly_coeffs();
        for f in files.split(';').filter(|f| !f.is_empty()) {
            let a = crate::audio::converter::decode::decode_file(std::path::Path::new(f)).expect("decode");
            let (rate, n) = (a.sample_rate, a.samples_l.len());
            // The render reads 8192 frames at ×2: 4096 of the source.
            let read = 4096usize;
            for (name, one_read) in [("one read ahead (before)", true), ("the whole window (after)", false)] {
                let src = Stereo { l: a.samples_l.clone(), r: a.samples_r.clone(), pos: 0 };
                let mut st = SlowGainStage::new(Box::new(src), target, rate);
                let handed = Arc::new(AtomicU64::new(0));
                if one_read {
                    let h = handed.clone();
                    st = st.with_horizon(move || h.load(Ordering::Relaxed) + (read + read / 2) as u64 + 4);
                }
                let (mut ol, mut or) = (vec![0.0; n], vec![0.0; n]);
                let mut at = 0;
                while at < n {
                    let e = (at + read).min(n);
                    st.read(&mut ol[at..e], &mut or[at..e]);
                    handed.store(e as u64, Ordering::Relaxed);
                    at = e;
                }
                let (mut under, mut depth_sum, mut deepest) = (0usize, 0.0f64, 0.0f64);
                for i in 0..n {
                    let x = a.samples_l[i].abs().max(a.samples_r[i].abs());
                    let y = ol[i].abs().max(or[i].abs());
                    if x > 1e-4 && y < x * 10f64.powf(-0.01 / 20.0) {
                        let d = 20.0 * (y / x).log10();
                        under += 1;
                        depth_sum += d;
                        deepest = deepest.min(d);
                    }
                }
                // What is left over the ceiling: 4× true peak of each millisecond.
                let step = (rate as usize / 1000).max(1);
                let (mut over, mut over_db, mut steps) = (0usize, 0.0f64, 0usize);
                for s0 in (3..n.saturating_sub(5)).step_by(step) {
                    let mut m = 0.0f64;
                    for j in s0..(s0 + step).min(n - 5) {
                        for ch in [&ol, &or] {
                            m = m.max(ch[j].abs());
                            for c in &co {
                                let v: f64 = (0..8).map(|k| ch[j + k - 3] * c[k]).sum();
                                m = m.max(v.abs());
                            }
                        }
                    }
                    steps += 1;
                    if m > target {
                        over += 1;
                        over_db = over_db.max(20.0 * (m / target).log10());
                    }
                }
                eprintln!(
                    "SG {} | {} | {:.1} min at {} Hz, ceiling {:.1} dBTP: under gain {:.2}% of the time (mean {:.2} dB, deepest {:.2} dB); over the ceiling {:.3}% of ms (up to {:.2} dB)",
                    f.rsplit(['\\', '/']).next().unwrap_or(f),
                    name,
                    n as f64 / rate as f64 / 60.0,
                    rate,
                    ceiling,
                    100.0 * under as f64 / n as f64,
                    if under > 0 { depth_sum / under as f64 } else { 0.0 },
                    deepest,
                    100.0 * over as f64 / steps.max(1) as f64,
                    over_db
                );
            }
        }
    }

    #[test]
    fn a_horizon_short_of_the_window_keeps_it_from_reading_further() {
        let x = tone(20.0, 1.0, &[(10.0, 1.0, 1.4)]);
        let (g, ahead) = gains(&x, Some(0.5));
        assert!(ahead <= (0.5 * RATE as f64) as u64 + 600, "read {ahead} frames ahead of what it handed on");
        assert!(g.iter().filter(|v| v.is_finite()).all(|&v| v > 0.4 && v <= 1.0));
    }

    /// A stereo signal as a stage.
    struct Pair {
        l: Vec<f64>,
        r: Vec<f64>,
        pos: u64,
    }

    impl Stage for Pair {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for i in 0..out_l.len().min(out_r.len()) {
                let p = self.pos as usize;
                let (a, b) = if p < self.l.len() {
                    inside += 1;
                    (self.l[p], self.r[p])
                } else {
                    (0.0, 0.0)
                };
                out_l[i] = a;
                out_r[i] = b;
                self.pos += 1;
            }
            inside
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn total(&self) -> u64 {
            self.l.len() as u64
        }
    }

    /// Instant start's first variant on real tracks, at the source's rate
    /// (the filter left out: it changes the rate, not the level): the track's
    /// head and repair as the player's default rack runs them (with
    /// AURA_ISP_HOT_OFF=1 the repair as 1.5.0 made it), the slow gain over it
    /// and the always-acting limiter behind that — the gain it plays at,
    /// what the limiter takes, and the limiter's error against the
    /// slow-gained signal, a stretch at a time; then the same with the full
    /// variant's one scalar from the first sample instead of the slow gain.
    /// A measurement, not a test: AURA_ISP_HOT_FILES="a.mp3" `cargo test
    /// --profile fast --bins first_variant_harness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn first_variant_harness() {
        use super::super::stages::LimiterStage;
        use crate::audio::converter::dsp::true_peak::measure_true_peak;
        use crate::audio::converter::pipeline::prepare::tests::band_error_db;
        let files = std::env::var("AURA_ISP_HOT_FILES").unwrap_or_default();
        let off = std::env::var("AURA_ISP_HOT_OFF").is_ok_and(|v| v == "1");
        let target_db = -0.5f64;
        let target = 10f64.powf(target_db / 20.0);
        fn rms(x: &[f64], y: &[f64]) -> f64 {
            (x.iter().chain(y).map(|v| v * v).sum::<f64>() / (2 * x.len()).max(1) as f64).sqrt()
        }
        fn through(mut st: Box<dyn Stage>, n: usize) -> (Vec<f64>, Vec<f64>) {
            let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
            let mut at = 0;
            while at < n {
                let e = (at + 4_096).min(n);
                st.read(&mut l[at..e], &mut r[at..e]);
                at = e;
            }
            (l, r)
        }
        for f in files.split('|').filter(|f| !f.trim().is_empty()) {
            let path = std::path::Path::new(f.trim());
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let eng = crate::player::settings::PlayerSettings::default().to_engine();
            let a = match crate::audio::converter::decode::decode_file(path) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("HARNESS {}: {e}", path.display());
                    continue;
                }
            };
            let rate = a.sample_rate;
            let (mut x_l, mut x_r) = (a.samples_l, a.samples_r);
            let head = crate::audio::converter::pipeline::prepare::source_head(&mut x_l, &mut x_r, rate, a.lossy, &eng, &cancel).expect("the head");
            let (hot_l, hot_r): (&[(usize, usize)], &[(usize, usize)]) = if off { (&[], &[]) } else { (&head.hot_spans_l, &head.hot_spans_r) };
            crate::audio::converter::dsp::lab::isp::run(&mut x_l, &mut x_r, &head.declip_spans_l, &head.declip_spans_r, hot_l, hot_r, &cancel);
            let n = x_l.len();
            // What the full variant's rule costs on the whole track, as
            // `prepare_full_variant` runs it (in pieces, side by side).
            let t0 = std::time::Instant::now();
            let tp_par = crate::audio::converter::dsp::true_peak::measure_true_peak_parallel(&x_l, &x_r);
            let local_par = crate::audio::converter::dsp::lab::isp::output_limit_is_local_parallel(&x_l, &x_r, target, rate);
            eprintln!("HARNESS   the full variant's rule on the whole track, in pieces: {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            let tp = measure_true_peak(&x_l, &x_r);
            let over_db = 20.0 * tp.log10() - target_db;
            let local = crate::audio::converter::dsp::lab::isp::output_limit_is_local(&x_l, &x_r, target, rate);
            assert_eq!((tp_par.to_bits(), local_par), (tp.to_bits(), local));
            let g_full = if over_db <= 0.0 || (local && over_db <= 6.0) { 1.0 } else { target / tp };
            eprintln!(
                "HARNESS {} [{}]: {} Hz, {:.1} s; true peak after the repair {:+.2} dBTP, ceiling {target_db:+.1}: the full variant plays at {:+.2} dB ({})",
                path.file_name().unwrap_or_default().to_string_lossy(), if off { "1.5.0" } else { "T43" }, rate, n as f64 / rate as f64,
                20.0 * tp.log10(), 20.0 * g_full.log10(), if g_full < 1.0 { "the whole track down" } else { "the limiter holds the overs" }
            );
            for slow in [true, false] {
                let (y_l, y_r) = if slow {
                    let sg = SlowGainStage::new(Box::new(Pair { l: x_l.clone(), r: x_r.clone(), pos: 0 }), target, rate);
                    through(Box::new(sg), n)
                } else {
                    (x_l.iter().map(|v| v * g_full).collect(), x_r.iter().map(|v| v * g_full).collect())
                };
                let lim = LimiterStage::new(Box::new(Pair { l: y_l.clone(), r: y_r.clone(), pos: 0 }), target, rate, 0);
                let (z_l, z_r) = through(Box::new(lim), n);
                eprintln!("HARNESS   {}", if slow { "first variant as it is: the slow gain, the limiter behind it" } else { "the full variant's scalar from the first sample, the limiter behind it" });
                let secs = |t: f64| ((t * rate as f64) as usize).min(n);
                let bands = [(5_000.0, 1e9)];
                for (t0, t1) in [(0.0, 0.5), (0.5, 1.0), (1.0, 2.0), (2.0, 3.0), (3.0, 5.0), (5.0, 10.0), (10.0, 30.0), (30.0, 1e9)] {
                    let (a, b) = (secs(t0), secs(t1));
                    if b <= a + 64 {
                        continue;
                    }
                    let gain: Vec<f64> = (a..b)
                        .filter(|&i| x_l[i].abs().max(x_r[i].abs()) > 1e-3)
                        .map(|i| 20.0 * (y_l[i].abs().max(y_r[i].abs()) / x_l[i].abs().max(x_r[i].abs())).log10())
                        .collect();
                    let (g_min, g_max) = gain.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &g| (lo.min(g), hi.max(g)));
                    let g_mean = gain.iter().sum::<f64>() / gain.len().max(1) as f64;
                    let (mut reduced, mut deepest) = (0usize, 0.0f64);
                    for i in a..b {
                        let y = y_l[i].abs().max(y_r[i].abs());
                        if y > 1e-9 {
                            let g = z_l[i].abs().max(z_r[i].abs()) / y;
                            if g < 0.999_999 {
                                reduced += 1;
                                deepest = deepest.min(20.0 * g.max(1e-12).log10());
                            }
                        }
                    }
                    let (el, er): (Vec<f64>, Vec<f64>) = ((a..b).map(|i| z_l[i] - y_l[i]).collect(), (a..b).map(|i| z_r[i] - y_r[i]).collect());
                    let err_db = 20.0 * (rms(&el, &er).max(1e-300) / rms(&y_l[a..b], &y_r[a..b]).max(1e-300)).log10();
                    let hf = band_error_db((&el, &er), (&y_l[a..b], &y_r[a..b]), rate, &bands)[0];
                    eprintln!(
                        "HARNESS   {t0:>5.1}-{:<5.1} s: gain {g_mean:+.2} dB (from {g_max:+.2} to {g_min:+.2}); limiter on {:.2} % of samples, deepest {deepest:+.2} dB; its error to the signal it gets {err_db:.1} dB, above 5 kHz {hf:.1} dB",
                        (b as f64 / rate as f64), 100.0 * reduced as f64 / (b - a) as f64
                    );
                }
            }
        }
    }
}
