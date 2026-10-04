//! A stream's delay kept in a corridor while the server's clock and the
//! DAC's drift apart.
//!
//! The server sends at its rate and the device plays at its own: a few ppm
//! apart (Radio Paradise against the Realtek here, +2 ppm) to over a hundred
//! (FIP, +120 ppm). Over hours the stream in hand grows — the delay to the
//! broadcast creeps — or runs out, and the stream starves. No resampler is
//! put in to follow it (the FIR stays the only one the sound goes through):
//! the level — the stream received less what the device has played — is
//! measured against the one it settled at (`DriftPolicy`), and when it
//! leaves the corridor a few milliseconds are cut out of the stream, or
//! played twice, at a quiet place, a song's start when one is near — at
//! most one splice in `SPLICE_EVERY_S`, until the level is back inside
//! (`Splicer`). Every other sample goes through as it came.

use std::collections::VecDeque;

/// The corridor around the level the stream settled at, seconds.
pub const BAND_S: f64 = 1.0;
/// After the stream goes on the air (or after a gap, a starve, a pause):
/// how long before the level is measured — a server may send ahead of real
/// time for minutes after a connect (Radio Paradise's did, 1.2 s over 7) —
/// and how long the target is taken over.
const WARM_S: f64 = 300.0;
const TARGET_S: f64 = 600.0;
/// The level the corridor is kept on is its mean over this many seconds:
/// the network brings the stream in chunks, and wanders by a second or so
/// over minutes, while the clocks' drift takes hours to make one.
const MEAN_S: f64 = 600.0;
/// At most one splice in this many seconds.
pub const SPLICE_EVERY_S: f64 = 30.0;
/// The most a splice is asked for: the place decides how much of it is cut.
const ASK_MAX_S: f64 = 0.2;
/// The drift is the level's slope over this long (sampled every
/// `TREND_STEP_S`), told once there is `TREND_MIN_S` of it.
const TREND_S: f64 = 3600.0;
const TREND_STEP_S: f64 = 10.0;
const TREND_MIN_S: f64 = 1800.0;

enum Stage {
    /// Not measured before this time.
    Warm { until: f64 },
    /// The target is the mean of the level until this time.
    Measure { until: f64, sum: f64, n: u64 },
    Keep,
}

/// Where the level is held: the target it settled at, the corridor, when a
/// splice is asked for.
pub struct DriftPolicy {
    band: f64,
    stage: Stage,
    target: Option<f64>,
    /// The level as it would be without the splices, over the last MEAN_S
    /// (with its running sum): a splice counts at once, by its length,
    /// not as the mean comes to see it.
    mean: VecDeque<(f64, f64)>,
    mean_sum: f64,
    last_splice: f64,
    /// A splice asked for and not made yet.
    asked: bool,
    /// What the splices have changed the level by (cut: −).
    spliced_s: f64,
    trend: VecDeque<(f64, f64)>,
}

impl DriftPolicy {
    pub fn new(band_s: f64) -> DriftPolicy {
        DriftPolicy {
            band: band_s.max(0.001),
            stage: Stage::Warm { until: f64::INFINITY },
            target: None,
            mean: VecDeque::new(),
            mean_sum: 0.0,
            last_splice: f64::NEG_INFINITY,
            asked: false,
            spliced_s: 0.0,
            trend: VecDeque::new(),
        }
    }

    pub fn band_s(&self) -> f64 {
        self.band
    }

    /// The target is measured afresh from session time `t`: the stream went
    /// on the air, or a gap, a starve or a pause left it at a level of its
    /// own. The last target stays known until then.
    pub fn restart(&mut self, t: f64) {
        self.stage = Stage::Warm { until: t + WARM_S };
        self.asked = false;
        self.mean.clear();
        self.mean_sum = 0.0;
        self.trend.clear();
    }

    /// The target level, seconds: the one measured last.
    pub fn target(&self) -> Option<f64> {
        self.target
    }

    /// The level's mean over the last `MEAN_S`, seconds, every splice made
    /// counted in full.
    pub fn level(&self) -> Option<f64> {
        if self.mean.is_empty() {
            return None;
        }
        Some(self.mean_sum / self.mean.len() as f64 + self.spliced_s)
    }

    /// The level above the target (seconds), while one is kept.
    pub fn deviation(&self) -> Option<f64> {
        match self.stage {
            Stage::Keep => Some(self.level()? - self.target?),
            _ => None,
        }
    }

    /// The server's clock against the DAC's, ppm (+: the server is faster),
    /// from the level's slope less the splices, once there is half an hour
    /// of it.
    pub fn drift_ppm(&self) -> Option<f64> {
        let n = self.trend.len();
        let (first, last) = (self.trend.front()?.0, self.trend.back()?.0);
        if last - first < TREND_MIN_S {
            return None;
        }
        let (mx, my) = self.trend.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
        let (mx, my) = (mx / n as f64, my / n as f64);
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for &(x, y) in &self.trend {
            sxy += (x - mx) * (y - my);
            sxx += (x - mx) * (x - mx);
        }
        (sxx > 0.0).then(|| sxy / sxx * 1e6)
    }

    /// A sample of the level (seconds) at session time `t` while the stream
    /// plays. Returns the splice to ask for now — seconds to cut out (> 0)
    /// or to play twice (< 0) — or 0.
    pub fn sample(&mut self, t: f64, level: f64) -> f64 {
        if let Stage::Warm { until } = self.stage {
            if until.is_infinite() {
                self.stage = Stage::Warm { until: t + WARM_S };
            }
        }
        // As it would be without the splices.
        let bare = level - self.spliced_s;
        self.mean.push_back((t, bare));
        self.mean_sum += bare;
        while self.mean.front().is_some_and(|&(t0, _)| t0 < t - MEAN_S) {
            let (_, v) = self.mean.pop_front().expect("checked above");
            self.mean_sum -= v;
        }
        if self.trend.back().is_none_or(|&(t0, _)| t - t0 >= TREND_STEP_S) {
            self.trend.push_back((t, bare));
            while self.trend.front().is_some_and(|&(t0, _)| t0 < t - TREND_S) {
                self.trend.pop_front();
            }
        }
        match &mut self.stage {
            Stage::Warm { until } => {
                if t >= *until {
                    self.stage = Stage::Measure { until: t + TARGET_S, sum: 0.0, n: 0 };
                }
                return 0.0;
            }
            Stage::Measure { until, sum, n } => {
                *sum += level;
                *n += 1;
                if t >= *until {
                    self.target = Some(*sum / *n as f64);
                    self.stage = Stage::Keep;
                }
                return 0.0;
            }
            Stage::Keep => {}
        }
        if self.asked || t - self.last_splice < SPLICE_EVERY_S {
            return 0.0;
        }
        let Some(dev) = self.deviation() else { return 0.0 };
        if dev.abs() <= self.band {
            return 0.0;
        }
        self.asked = true;
        (dev.abs() - self.band + 0.05).clamp(0.01, ASK_MAX_S).copysign(dev)
    }

    /// A splice was made at `t`: `secs` cut out (> 0) or played twice (< 0).
    /// It counts in the level at once; the next one no sooner than
    /// SPLICE_EVERY_S.
    pub fn spliced(&mut self, t: f64, secs: f64) {
        self.asked = false;
        self.last_splice = t;
        self.spliced_s -= secs;
    }
}

/// The analysis window, the crossfade, how much of each side is compared.
const WIN_S: f64 = 0.010;
const XF_S: f64 = 0.010;
const CMP_S: f64 = 0.020;
/// While a splice waits, this much of the stream is held back to look at.
const HOLD_S: f64 = 0.3;
/// Frames kept after they went on: what a repeat plays again comes from them.
const HIST_S: f64 = 0.25;
/// A place is quiet against the loudest window of the last LOUD_S seconds:
/// QUIET_DB under it, RELAX_DB more for each RELAX_S a splice has waited, up
/// to QUIET_MAX_DB; near a song's start SONG_DB. At SILENT_DBFS or below a
/// place is silence, whatever came before.
const LOUD_S: f64 = 5.0;
const QUIET_DB: f64 = -40.0;
const RELAX_DB: f64 = 6.0;
const RELAX_S: f64 = 60.0;
const QUIET_MAX_DB: f64 = -16.0;
const SONG_DB: f64 = -30.0;
const SILENT_DBFS: f64 = -60.0;
/// A quiet place is at least this long; a splice there cuts or repeats
/// between MUSIC_MIN_S and MUSIC_MAX_S (the sides lined up by their
/// likeness), in silence up to SILENCE_MAX_S.
const RUN_MIN_S: f64 = 0.03;
const MUSIC_MIN_S: f64 = 0.01;
const MUSIC_MAX_S: f64 = 0.04;
const SILENCE_MAX_S: f64 = 0.2;

/// Where a splice may be looked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// Anywhere quiet enough.
    Anywhere,
    /// A song begins within a couple of seconds: a less quiet place will do.
    SongNear,
    /// A song is foretold to begin within a couple of minutes: wait for it.
    Wait,
}

/// A splice made: frames cut out (> 0) or played twice (< 0), the place's
/// level (dBFS), how far under the loudest of the last seconds it was (dB),
/// how alike the two sides were (correlation; NaN in silence), and whether
/// a song began near.
#[derive(Clone, Debug)]
pub struct Splice {
    pub frames: i64,
    pub level_db: f64,
    pub under_db: f64,
    pub corr: f64,
    pub at_song: bool,
}

/// Raised-cosine gain of step `i` of an `n`-step fade-in (0 → 1).
fn ramp(i: usize, n: usize) -> f64 {
    let x = (i as f64 + 0.5) / n.max(1) as f64;
    0.5 - 0.5 * (std::f64::consts::PI * x).cos()
}

/// The stream between the joiner and the source stages: frames go through
/// as they came, but for the splice asked for, made at a quiet place.
pub struct Splicer {
    rate: f64,
    win: usize,
    xf: usize,
    cmp: usize,
    hold: usize,
    hist: usize,
    l: Vec<f64>,
    r: Vec<f64>,
    /// Frames at the front already handed on (kept for a repeat).
    sent: usize,
    /// The window being summed as frames come: both channels' squares.
    acc: (f64, f64, usize),
    /// Each window's level (dBFS) over the last LOUD_S.
    loud: VecDeque<f64>,
    loud_len: usize,
    /// The splice asked for, frames (> 0 cut out, < 0 played twice), and
    /// how many frames went by while it waited for a quiet place.
    want: i64,
    waited: usize,
}

impl Splicer {
    pub fn new(rate: u32) -> Splicer {
        let rate = rate.max(1) as f64;
        let n = |s: f64| ((s * rate).round() as usize).max(1);
        Splicer {
            rate,
            win: n(WIN_S),
            xf: n(XF_S),
            cmp: n(CMP_S),
            hold: n(HOLD_S),
            hist: n(HIST_S),
            l: Vec::new(),
            r: Vec::new(),
            sent: 0,
            acc: (0.0, 0.0, 0),
            loud: VecDeque::new(),
            loud_len: (LOUD_S / WIN_S) as usize,
            want: 0,
            waited: 0,
        }
    }

    /// Frames taken in and not handed on yet.
    pub fn held(&self) -> usize {
        self.l.len() - self.sent
    }

    /// The splice asked for, frames: > 0 cut out, < 0 played twice, 0 none.
    pub fn ask(&mut self, frames: i64) {
        if frames != self.want {
            self.want = frames;
            self.waited = 0;
        }
    }

    /// Take frames and hand on what is ready. A splice made on the way is
    /// returned (the ask is then done).
    pub fn push(&mut self, l: &[f64], r: &[f64], place: Place, out: &mut dyn FnMut(&[f64], &[f64])) -> Option<Splice> {
        let n = l.len().min(r.len());
        for i in 0..n {
            self.measure(l[i], r[i]);
        }
        self.l.extend_from_slice(&l[..n]);
        self.r.extend_from_slice(&r[..n]);
        let mut done = None;
        let looking = self.want != 0 && place != Place::Wait;
        if looking {
            if place == Place::Anywhere {
                self.waited += n;
            }
            done = self.look(place == Place::SongNear);
        }
        let keep = if self.want != 0 && place != Place::Wait { self.hold } else { 0 };
        let upto = self.l.len().saturating_sub(keep).max(self.sent);
        if upto > self.sent {
            out(&self.l[self.sent..upto], &self.r[self.sent..upto]);
            self.sent = upto;
        }
        // What a repeat cannot need any more goes.
        if self.sent > 4 * self.hist.max(self.win) {
            let drop = self.sent - self.hist;
            self.l.drain(..drop);
            self.r.drain(..drop);
            self.sent -= drop;
        }
        done
    }

    /// Everything held goes on as it is (the stream breaks after it); what
    /// was kept for a repeat is not the same stream any more.
    pub fn flush(&mut self, out: &mut dyn FnMut(&[f64], &[f64])) {
        if self.l.len() > self.sent {
            out(&self.l[self.sent..], &self.r[self.sent..]);
        }
        self.l.clear();
        self.r.clear();
        self.sent = 0;
    }

    fn measure(&mut self, a: f64, b: f64) {
        self.acc.0 += a * a;
        self.acc.1 += b * b;
        self.acc.2 += 1;
        if self.acc.2 == self.win {
            self.loud.push_back(db_of(self.acc.0.max(self.acc.1) / self.win as f64));
            if self.loud.len() > self.loud_len {
                self.loud.pop_front();
            }
            self.acc = (0.0, 0.0, 0);
        }
    }

    /// The level of the window at `k`, dBFS (the louder channel).
    fn window_db(&self, k: usize) -> f64 {
        let (mut a, mut b) = (0.0, 0.0);
        for i in k..k + self.win {
            a += self.l[i] * self.l[i];
            b += self.r[i] * self.r[i];
        }
        db_of(a.max(b) / self.win as f64)
    }

    /// Look for a quiet place in what is not handed on yet and splice there.
    fn look(&mut self, song_near: bool) -> Option<Splice> {
        let loudest = self.loud.iter().fold(f64::NEG_INFINITY, |m, &v| m.max(v));
        let relax = (self.waited as f64 / self.rate / RELAX_S).floor() * RELAX_DB;
        let under = if song_near { SONG_DB } else { (QUIET_DB + relax).min(QUIET_MAX_DB) };
        let quiet = (loudest + under).max(SILENT_DBFS);
        let len = self.l.len();
        let mut k = self.sent;
        // A run of quiet windows: where it begins, its loudest window.
        let mut run: Option<(usize, f64)> = None;
        while k + self.win <= len {
            let db = self.window_db(k);
            if db <= quiet {
                run = Some(match run {
                    Some((rs, mx)) => (rs, mx.max(db)),
                    None => (k, db),
                });
            } else if let Some((rs, mx)) = run.take() {
                if let Some(s) = self.place(rs, k, mx, loudest, song_near) {
                    return Some(s);
                }
            }
            k += self.win;
        }
        // A run still going: taken when it is long enough already, or when
        // its start is about to be handed on.
        let (rs, mx) = run?;
        let enough = self.want.unsigned_abs() as usize + self.xf + self.cmp;
        if k - rs >= enough || rs + self.hold < len + self.win {
            return self.place(rs, k, mx, loudest, song_near);
        }
        None
    }

    /// Splice in the quiet run [rs, re) when it is long enough.
    fn place(&mut self, rs: usize, re: usize, max_db: f64, loudest: f64, song_near: bool) -> Option<Splice> {
        let run = re - rs;
        if (run as f64) < RUN_MIN_S * self.rate {
            return None;
        }
        let silent = max_db <= SILENT_DBFS;
        let cap = ((if silent { SILENCE_MAX_S } else { MUSIC_MAX_S }) * self.rate) as usize;
        let d_hi = (self.want.unsigned_abs() as usize).min(cap).min(run - self.xf);
        let d_min = (MUSIC_MIN_S * self.rate) as usize;
        if d_hi < d_min {
            return None;
        }
        let cut = self.want > 0;
        // The crossfade begins at `a`; the stream goes on from `b`: a cut
        // at the run's start, a repeat at its end (what is played again
        // lies in the run).
        let a = if cut { rs } else { re - self.xf };
        let b = |d: usize| if cut { a + d } else { a - d };
        let (d, corr) = if silent {
            (d_hi, f64::NAN)
        } else {
            let lo = (d_hi / 2).max(d_min);
            (lo..=d_hi)
                .map(|d| (d, self.likeness(a, b(d))))
                .fold((d_hi, f64::NEG_INFINITY), |best, c| if c.1 > best.1 { c } else { best })
        };
        self.splice_at(a, b(d));
        let frames = if cut { d as i64 } else { -(d as i64) };
        self.want = 0;
        self.waited = 0;
        Some(Splice { frames, level_db: max_db, under_db: max_db - loudest, corr, at_song: song_near })
    }

    /// How alike the stream is at `a` and at `b` (normalized correlation
    /// over CMP_S, both channels).
    fn likeness(&self, a: usize, b: usize) -> f64 {
        let n = self.cmp.min(self.l.len() - a.max(b));
        let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
        for ch in [&self.l, &self.r] {
            for k in 0..n {
                let (x, y) = (ch[a + k], ch[b + k]);
                xy += x * y;
                xx += x * x;
                yy += y * y;
            }
        }
        if xx <= 0.0 || yy <= 0.0 {
            0.0
        } else {
            xy / (xx * yy).sqrt()
        }
    }

    /// From `a` on, the stream goes on from `b` instead: a crossfade of XF_S
    /// from the one into the other.
    fn splice_at(&mut self, a: usize, b: usize) {
        let xf = self.xf;
        for ch in [&mut self.l, &mut self.r] {
            let mut tail: Vec<f64> = (0..xf)
                .map(|k| {
                    let w = ramp(k, xf);
                    ch[a + k] * (1.0 - w) + ch[b + k] * w
                })
                .collect();
            tail.extend_from_slice(&ch[b + xf..]);
            ch.truncate(a);
            ch.extend(tail);
        }
    }
}

fn db_of(mean_square: f64) -> f64 {
    if mean_square > 0.0 {
        10.0 * mean_square.log10()
    } else {
        -200.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_the_level_the_stream_settled_at_and_the_corridor_holds_it() {
        let mut p = DriftPolicy::new(1.0);
        let mut t = 0.0;
        // Warm, then measured at 6 s.
        while t < WARM_S + TARGET_S + 1.0 {
            assert_eq!(p.sample(t, 6.0), 0.0);
            t += 0.25;
        }
        assert_eq!(p.target(), Some(6.0));
        // Inside the corridor, a whole mean long: nothing.
        let end = t + MEAN_S;
        while t < end {
            assert_eq!(p.sample(t, 6.9), 0.0);
            t += 0.25;
        }
        // Out of it (the server faster): one ask, until it is made.
        let mut asked = Vec::new();
        let end = t + MEAN_S;
        while t < end {
            let a = p.sample(t, 7.2);
            if a != 0.0 {
                asked.push((t, a));
            }
            t += 0.25;
        }
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].1 > 0.0 && asked[0].1 <= ASK_MAX_S);
        // Made (40 ms): it counts at once — the level is 0.04 lower from
        // here on — and the next ask comes no sooner than SPLICE_EVERY_S.
        p.spliced(t, 0.04);
        let t0 = t;
        let mut next = None;
        while t < t0 + 120.0 {
            if p.sample(t, 7.16) != 0.0 && next.is_none() {
                next = Some(t);
            }
            t += 0.25;
        }
        assert!(next.is_some_and(|n| n >= t0 + SPLICE_EVERY_S), "{next:?}");
        // Below it (the server slower): once the mean has seen it, repeats
        // are asked for (each made as it is asked).
        p.spliced(t, 0.04);
        let mut rep = 0.0;
        let end = t + 2.0 * MEAN_S;
        while t < end {
            let a = p.sample(t, 4.8);
            if a != 0.0 {
                rep = a;
                p.spliced(t, a.signum() * 0.04);
            }
            t += 0.25;
        }
        assert!(rep < 0.0, "{rep}");
    }

    #[test]
    fn a_drifting_stream_stays_in_its_corridor_through_the_networks_wander() {
        // Five hours, a look every 250 ms: a server 120 ppm faster than the
        // DAC; its CDN 1.2 s ahead of real time over the first 7 minutes; the
        // source stages' blocks (a 1.5 s saw-tooth); chunks (±0.2 s); a slow
        // wander of ±0.4 s over minutes. Each splice asked for is made at once
        // at 35 ms.
        let mut p = DriftPolicy::new(BAND_S);
        let (mut cut, mut splices, mut worst) = (0.0, Vec::new(), 0.0f64);
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut t = 0.0f64;
        while t < 5.0 * 3600.0 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let chunk = (rng % 1000) as f64 / 1000.0 * 0.4 - 0.2;
            let ramp = 1.2 * (t / 420.0).min(1.0);
            let saw = 1.5 * ((t / 1.5).fract() - 0.5);
            let wander = 0.4 * (2.0 * std::f64::consts::PI * t / 240.0).sin();
            let level = 6.0 + ramp + 120e-6 * t + saw + chunk + wander - cut;
            let ask = p.sample(t, level);
            if ask != 0.0 {
                let s = 0.035f64.copysign(ask);
                cut += s;
                p.spliced(t, s);
                splices.push(t);
            }
            if let Some(d) = p.deviation() {
                worst = worst.max(d.abs());
            }
            t += 0.25;
        }
        // The ramp and the wander are not drift: nothing in the first hour.
        assert!(splices.first().is_some_and(|&s| s > 3600.0), "first splices at {:?}", &splices[..splices.len().min(4)]);
        // 0.43 s an hour, 35 ms a splice: about twelve an hour once at the edge.
        let hours = (5.0 * 3600.0 - splices[0]) / 3600.0;
        let rate = splices.len() as f64 / hours;
        assert!(rate > 8.0 && rate < 18.0, "{rate:.1} splices an hour");
        assert!(splices.windows(2).all(|w| w[1] - w[0] >= SPLICE_EVERY_S));
        assert!(worst < BAND_S + 0.1, "the level went {worst:.2} s off its target");
        let ppm = p.drift_ppm().expect("an hour of it");
        assert!((ppm - 120.0).abs() < 15.0, "{ppm:.1} ppm");
    }

    /// A tone at `db` dBFS (peak), `secs` long, with silences (start, length).
    fn signal(rate: u32, secs: f64, db: f64, silent: &[(f64, f64)]) -> Vec<f64> {
        let a = 10f64.powf(db / 20.0);
        (0..(secs * rate as f64) as usize)
            .map(|i| {
                let t = i as f64 / rate as f64;
                if silent.iter().any(|&(s, l)| t >= s && t < s + l) {
                    0.0
                } else {
                    a * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                }
            })
            .collect()
    }

    /// Run `x` through a splicer asked for `ask` frames, in pieces of 1000.
    fn through(x: &[f64], ask: i64, place: Place) -> (Vec<f64>, Option<Splice>) {
        let mut sp = Splicer::new(44_100);
        sp.ask(ask);
        let mut out = Vec::new();
        let mut made = None;
        for c in x.chunks(1000) {
            if let Some(s) = sp.push(c, c, place, &mut |a, _| out.extend_from_slice(a)) {
                made = Some(s);
            }
        }
        sp.flush(&mut |a, _| out.extend_from_slice(a));
        (out, made)
    }

    #[test]
    fn a_cut_or_a_repeat_goes_in_a_silence_and_nothing_else_changes() {
        let rate = 44_100;
        let x = signal(rate, 4.0, -10.0, &[(2.0, 0.5)]);
        // Cut 100 ms: in the silence, all of it.
        let (y, s) = through(&x, 4410, Place::Anywhere);
        let s = s.expect("a splice in the silence");
        assert_eq!((s.frames, y.len()), (4410, x.len() - 4410));
        assert!(s.level_db <= SILENT_DBFS);
        let at = (2.0 * rate as f64) as usize;
        assert_eq!(&y[..at], &x[..at], "before the place nothing changed");
        let tail = x.len() - at - 4410;
        assert_eq!(&y[y.len() - tail..], &x[x.len() - tail..], "after it: the same stream");
        // Repeat 100 ms: the stream is that much longer, no louder.
        let (y, s) = through(&x, -4410, Place::Anywhere);
        assert_eq!((s.unwrap().frames, y.len()), (-4410, x.len() + 4410));
        assert!(y.iter().all(|v| v.abs() <= 10f64.powf(-10.0 / 20.0) + 1e-12));
    }

    #[test]
    fn in_music_it_waits_for_a_quiet_place_and_cuts_a_little() {
        let rate = 44_100;
        // Loud, then a quiet passage (-55 dB), then loud again.
        let mut x = signal(rate, 3.0, -6.0, &[]);
        x.extend(signal(rate, 0.4, -55.0, &[]));
        x.extend(signal(rate, 3.0, -6.0, &[]));
        let (y, s) = through(&x, 4410, Place::Anywhere);
        let s = s.expect("a splice in the quiet passage");
        assert!(s.frames >= 441 && s.frames <= (MUSIC_MAX_S * rate as f64) as i64, "{s:?}");
        assert!(s.under_db < -40.0 && s.corr > 0.9, "{s:?}");
        assert_eq!(y.len() as i64, x.len() as i64 - s.frames);
        // The loud parts went through untouched.
        let a = (3.0 * rate as f64) as usize;
        assert_eq!(&y[..a], &x[..a]);
        // Waiting for a song: nothing.
        let (y, s) = through(&x, 4410, Place::Wait);
        assert!(s.is_none() && y == x);
    }

    #[test]
    fn with_no_quiet_place_it_waits_and_lets_the_stream_through() {
        let x = signal(44_100, 5.0, -6.0, &[]);
        let (y, s) = through(&x, 4410, Place::Anywhere);
        assert!(s.is_none());
        assert_eq!(y, x);
    }
}
