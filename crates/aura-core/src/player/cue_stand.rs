//! A test stand for the window's look: mixes made here, each source at a place
//! known beforehand and sounding when it is known to, played through the stage
//! with the window watching, and every view scored against them: whether a
//! source that sounds is shown, how much its place shakes, whether it keeps its
//! number, how soon it shows and goes, and whether a kick drum moves anything
//! but itself.
//!
//! `cargo test --profile fast --bins player::cue::stand -- --ignored --nocapture`
//! prints the table; `STAND_OUT=<dir>` also writes each mix's views as text,
//! one line a view, a place a letter at its angle.

use super::*;

const SR: u32 = 48_000;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn bi(&mut self) -> f64 {
        2.0 * self.next() - 1.0
    }
}

/// RBJ's biquads.
#[derive(Clone, Copy)]
struct Bq {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Bq {
    fn make(kind: u8, hz: f64, q: f64) -> Bq {
        let w = 2.0 * PI * hz / SR as f64;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        let b = match kind {
            b'l' => [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            b'h' => [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            _ => [al, 0.0, -al],
        };
        let a0 = 1.0 + al;
        Bq { b: [b[0] / a0, b[1] / a0, b[2] / a0], a: [-2.0 * c / a0, (1.0 - al) / a0], z: [0.0; 2] }
    }
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

fn filt(x: &mut [f64], mut f: Vec<Bq>) {
    for v in x.iter_mut() {
        for b in f.iter_mut() {
            *v = b.run(*v);
        }
    }
}

/// A source of a mix: its sound, its place (degrees, left positive, as the
/// mix's pan puts it), the delay of its right channel, ms (time pan, left
/// first positive), and when it sounds.
struct Voice {
    name: &'static str,
    sig: Vec<f64>,
    deg: Box<dyn Fn(f64) -> f64 + Send + Sync>,
    ms: f64,
    on: Vec<(f64, f64)>,
    /// A kick: the others are scored for what it does to them.
    kick: bool,
}

fn len_of(secs: f64) -> usize {
    (secs * SR as f64) as usize
}

fn rms(x: &[f64]) -> f64 {
    (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Scales `x` to `db` dBFS over where it sounds.
fn level(mut x: Vec<f64>, db: f64) -> Vec<f64> {
    let loud: Vec<f64> = x.iter().copied().filter(|v| v.abs() > 1e-5).collect();
    let g = 10f64.powf(db / 20.0) / rms(&loud).max(1e-12);
    x.iter_mut().for_each(|v| *v *= g);
    x
}

/// Notes as (start s, length s, Hz): harmonics to 16 kHz falling `tilt` dB
/// an octave, an attack, then held or dying `decay` dB a second, a release.
struct Tone {
    harm: usize,
    tilt: f64,
    attack: f64,
    decay: f64,
    release: f64,
    vib: f64,
}

fn tone(total: f64, notes: &[(f64, f64, f64)], t: &Tone, rng: &mut Rng) -> Vec<f64> {
    let mut x = vec![0.0; len_of(total)];
    for &(t0, dur, f0) in notes {
        let (i0, n) = (len_of(t0), len_of(dur + t.release));
        let phases: Vec<f64> = (0..t.harm).map(|_| rng.next() * 2.0 * PI).collect();
        let vib_rate = 4.5 + rng.next();
        for i in 0..n {
            if i0 + i >= x.len() {
                break;
            }
            let s = i as f64 / SR as f64;
            let env = if s < t.attack {
                s / t.attack
            } else if s < dur {
                10f64.powf(-t.decay * (s - t.attack) / 20.0)
            } else {
                10f64.powf(-t.decay * (dur - t.attack) / 20.0) * (1.0 - (s - dur) / t.release).max(0.0)
            };
            let f = f0 * (1.0 + t.vib * (2.0 * PI * vib_rate * s).sin());
            let mut v = 0.0;
            for (h, ph) in phases.iter().enumerate() {
                let fh = f * (h + 1) as f64;
                if fh > 16_000.0 {
                    break;
                }
                let a = 10f64.powf(-t.tilt * ((h + 1) as f64).log2() / 20.0);
                v += a * (2.0 * PI * fh * s + ph).sin();
            }
            x[i0 + i] += env * v;
        }
    }
    x
}

/// Notes from a pentatonic scale over two octaves from `root`, each `step`
/// seconds long (held `hold` of it), between `from` and `to`.
fn melody(rng: &mut Rng, root: f64, from: f64, to: f64, step: f64, hold: f64) -> Vec<(f64, f64, f64)> {
    let scale = [0, 2, 4, 7, 9, 12, 14, 16, 19, 21];
    let mut v = Vec::new();
    let mut t = from;
    while t + step <= to + 1e-9 {
        let k = scale[(rng.next() * scale.len() as f64) as usize % scale.len()];
        v.push((t, step * hold, root * 2f64.powf(k as f64 / 12.0)));
        t += step;
    }
    v
}

/// Chords: three notes at once.
fn chords(rng: &mut Rng, root: f64, from: f64, to: f64, step: f64) -> Vec<(f64, f64, f64)> {
    let mut v = Vec::new();
    let mut t = from;
    let degs = [[0, 4, 7], [5, 9, 12], [7, 11, 14], [-3, 0, 4]];
    while t + step <= to + 1e-9 {
        for k in degs[(rng.next() * 4.0) as usize % 4] {
            v.push((t, step * 0.95, root * 2f64.powf(k as f64 / 12.0)));
        }
        t += step;
    }
    v
}

fn hits(from: f64, to: f64, every: f64, offset: f64) -> Vec<f64> {
    let mut v = Vec::new();
    let mut t = from + offset;
    while t < to {
        v.push(t);
        t += every;
    }
    v
}

fn kick(total: f64, at: &[f64]) -> Vec<f64> {
    let mut x = vec![0.0; len_of(total)];
    for &t0 in at {
        let i0 = len_of(t0);
        let mut ph = 0.0;
        for i in 0..len_of(0.45) {
            if i0 + i >= x.len() {
                break;
            }
            let s = i as f64 / SR as f64;
            ph += 2.0 * PI * (48.0 + 110.0 * (-s / 0.025).exp()) / SR as f64;
            x[i0 + i] += ph.sin() * (-s / 0.18).exp() + 0.3 * (2.0 * PI * 3000.0 * s).sin() * (-s / 0.002).exp();
        }
    }
    x
}

/// Noise bursts through a band, dying over `tau` s.
fn burst(total: f64, at: &[f64], lo: f64, hi: f64, tau: f64, rng: &mut Rng) -> Vec<f64> {
    let mut x = vec![0.0; len_of(total)];
    for &t0 in at {
        let i0 = len_of(t0);
        for i in 0..len_of(6.0 * tau) {
            if i0 + i >= x.len() {
                break;
            }
            x[i0 + i] += rng.bi() * (-(i as f64) / SR as f64 / tau).exp();
        }
    }
    filt(&mut x, vec![Bq::make(b'h', lo, 0.7), Bq::make(b'h', lo, 0.7), Bq::make(b'l', hi, 0.7), Bq::make(b'l', hi, 0.7)]);
    x
}

fn snare(total: f64, at: &[f64], rng: &mut Rng) -> Vec<f64> {
    let mut x = burst(total, at, 900.0, 7000.0, 0.09, rng);
    for &t0 in at {
        let i0 = len_of(t0);
        for i in 0..len_of(0.3) {
            if i0 + i < x.len() {
                let s = i as f64 / SR as f64;
                x[i0 + i] += 0.4 * (2.0 * PI * 190.0 * s).sin() * (-s / 0.06).exp();
            }
        }
    }
    x
}

/// The intervals a source sounds in from where it sounds.
fn on_of(x: &[f64]) -> Vec<(f64, f64)> {
    let w = len_of(0.02);
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let mut v: Vec<(f64, f64)> = Vec::new();
    let mut i = 0;
    while i < x.len() {
        let e = (i + w).min(x.len());
        let loud = x[i..e].iter().any(|s| s.abs() > 0.05 * peak);
        let t = i as f64 / SR as f64;
        if loud {
            match v.last_mut() {
                // A gap under 0.3 s is the same phrase.
                Some(last) if t - last.1 < 0.3 => last.1 = e as f64 / SR as f64,
                _ => v.push((t, e as f64 / SR as f64)),
            }
        }
        i = e;
    }
    v
}

fn voice(name: &'static str, sig: Vec<f64>, deg: f64) -> Voice {
    let on = on_of(&sig);
    Voice { name, sig, deg: Box::new(move |_| deg), ms: 0.0, on, kick: false }
}

/// Mixes the voices: each panned by the stage's own law, time panned by its
/// delay; and diffuse sound, `diffuse_db` dBFS of noise unlike in each channel.
fn mix(total: f64, vs: &[Voice], diffuse_db: Option<f64>, rng: &mut Rng) -> (Vec<f64>, Vec<f64>) {
    let n = len_of(total);
    let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
    for v in vs {
        let d = len_of(v.ms.abs() / 1000.0);
        for i in 0..n {
            let deg = (v.deg)(i as f64 / SR as f64);
            let rho = law_rho_db(PanLaw::Sin3, deg);
            let a = (10f64.powf(-rho / 20.0)).atan();
            let (sl, sr) = (if v.ms < 0.0 { i.checked_sub(d) } else { Some(i) }, if v.ms > 0.0 { i.checked_sub(d) } else { Some(i) });
            l[i] += sl.map_or(0.0, |j| v.sig[j]) * a.cos();
            r[i] += sr.map_or(0.0, |j| v.sig[j]) * a.sin();
        }
    }
    if let Some(db) = diffuse_db {
        let mut a: Vec<f64> = (0..n).map(|_| rng.bi()).collect();
        let mut b: Vec<f64> = (0..n).map(|_| rng.bi()).collect();
        for x in [&mut a, &mut b] {
            filt(x, vec![Bq::make(b'l', 6000.0, 0.7), Bq::make(b'h', 120.0, 0.7)]);
        }
        let g = 10f64.powf(db / 20.0) / rms(&a);
        for i in 0..n {
            l[i] += g * a[i];
            r[i] += g * b[i];
        }
    }
    let peak = l.iter().chain(&r).fold(0.0f64, |m, v| m.max(v.abs()));
    if peak > 0.95 {
        let g = 0.95 / peak;
        l.iter_mut().chain(r.iter_mut()).for_each(|v| *v *= g);
    }
    (l, r)
}

struct Mix {
    name: &'static str,
    total: f64,
    voices: Vec<Voice>,
    diffuse_db: Option<f64>,
}

const T: f64 = 12.0;

fn mixes() -> Vec<Mix> {
    let mut v = Vec::new();
    let mut rng = Rng::new(7);
    let piano = Tone { harm: 30, tilt: 9.0, attack: 0.005, decay: 6.0, release: 0.2, vib: 0.0 };
    let pad = Tone { harm: 24, tilt: 8.0, attack: 0.3, decay: 0.0, release: 0.4, vib: 0.002 };
    let lead = Tone { harm: 20, tilt: 5.0, attack: 0.04, decay: 1.0, release: 0.1, vib: 0.004 };
    let bass = Tone { harm: 10, tilt: 9.0, attack: 0.01, decay: 3.0, release: 0.08, vib: 0.0 };
    let beat = |a: f64, b: f64, every: f64, off: f64| hits(a, b, every, off);

    // A band: kick, bass and snare in the middle, hats right, piano left, pad
    // right, a voice in the middle.
    {
        let k = beat(0.0, T, 0.5, 0.0);
        let mut ks = voice("kick C", level(kick(T, &k), -14.0), 0.0);
        ks.kick = true;
        ks.on = vec![(0.0, T)];
        let mut hat = voice("hats R30", level(burst(T, &beat(0.0, T, 0.25, 0.125), 6000.0, 15000.0, 0.03, &mut rng), -30.0), -30.0);
        hat.on = vec![(0.0, T)];
        let mut sn = voice("snare C", level(snare(T, &beat(0.0, T, 1.0, 0.5), &mut rng), -20.0), 0.0);
        sn.on = vec![(0.0, T)];
        v.push(Mix {
            name: "band",
            total: T,
            voices: vec![
                ks,
                voice("bass C", level(tone(T, &melody(&mut rng, 55.0, 0.0, T, 0.5, 0.9), &bass, &mut rng), -20.0), 0.0),
                sn,
                hat,
                voice("piano L45", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.25, 1.0), &piano, &mut rng), -24.0), 45.0),
                voice("pad R60", level(tone(T, &chords(&mut rng, 196.0, 0.0, T, 2.0), &pad, &mut rng), -28.0), -60.0),
                voice("voice C", level(tone(T, &melody(&mut rng, 330.0, 0.0, T, 0.6, 0.9), &lead, &mut rng), -22.0), 0.0),
            ],
            diffuse_db: Some(-40.0),
        });
    }
    // Two sides held while a loud kick hits in the middle.
    {
        let k = beat(0.5, T, 0.5, 0.0);
        let mut ks = voice("kick C", level(kick(T, &k), -10.0), 0.0);
        ks.kick = true;
        ks.on = vec![(0.5, T)];
        v.push(Mix {
            name: "kick vs sides",
            total: T,
            voices: vec![
                ks,
                voice("guitar L50", level(tone(T, &melody(&mut rng, 196.0, 0.0, T, 0.3, 1.0), &piano, &mut rng), -26.0), 50.0),
                voice("strings R50", level(tone(T, &chords(&mut rng, 262.0, 0.0, T, 1.5), &pad, &mut rng), -26.0), -50.0),
            ],
            diffuse_db: None,
        });
    }
    // Sides playing on when a loud, dense bass and pad come in in the middle:
    // the sides must stay places all the while.
    {
        let t = 20.0;
        let mut b = voice("bass C", level(tone(t, &melody(&mut rng, 49.0, 6.0, t, 0.25, 0.95), &bass, &mut rng), -10.0), 0.0);
        b.on = vec![(6.0, t)];
        let mut p = voice("pad C", level(tone(t, &chords(&mut rng, 131.0, 6.0, t, 2.0), &pad, &mut rng), -14.0), 0.0);
        p.on = vec![(6.0, t)];
        v.push(Mix {
            name: "centre comes in",
            total: t,
            voices: vec![
                b,
                p,
                voice("guitar L50", level(tone(t, &melody(&mut rng, 196.0, 0.0, t, 0.3, 1.0), &piano, &mut rng), -26.0), 50.0),
                voice("strings R40", level(tone(t, &chords(&mut rng, 262.0, 0.0, t, 1.5), &pad, &mut rng), -28.0), -40.0),
                voice("lead L20", level(tone(t, &melody(&mut rng, 392.0, 0.0, t, 0.4, 0.8), &lead, &mut rng), -30.0), 20.0),
            ],
            diffuse_db: Some(-30.0),
        });
    }
    // Phrases coming and going over a quiet pad: each must show every time.
    {
        let mut fl = Vec::new();
        let mut tr = Vec::new();
        let mut t0 = 1.0;
        while t0 + 1.5 < T {
            fl.extend(melody(&mut rng, 523.0, t0, t0 + 1.5, 0.25, 0.95));
            t0 += 3.0;
        }
        let mut t0 = 2.2;
        while t0 + 1.2 < T {
            tr.extend(melody(&mut rng, 233.0, t0, t0 + 1.2, 0.3, 0.9));
            t0 += 2.6;
        }
        v.push(Mix {
            name: "phrases",
            total: T,
            voices: vec![
                voice("pad C", level(tone(T, &chords(&mut rng, 131.0, 0.0, T, 3.0), &pad, &mut rng), -32.0), 0.0),
                voice("flute L60", level(tone(T, &fl, &lead, &mut rng), -24.0), 60.0),
                voice("trumpet R35", level(tone(T, &tr, &lead, &mut rng), -24.0), -35.0),
            ],
            diffuse_db: Some(-45.0),
        });
    }
    // A source moving from left to right past drums in the middle.
    {
        let k = beat(0.0, T, 0.5, 0.0);
        let mut ks = voice("kick C", level(kick(T, &k), -16.0), 0.0);
        ks.kick = true;
        ks.on = vec![(0.0, T)];
        let mut mv = voice("synth L70→R70", level(tone(T, &melody(&mut rng, 392.0, 0.0, T, 0.2, 1.0), &lead, &mut rng), -24.0), 0.0);
        mv.deg = Box::new(|t: f64| 70.0 - 140.0 * (t / T));
        v.push(Mix { name: "moving", total: T, voices: vec![ks, mv], diffuse_db: None });
    }
    // Time pan: a delay alone puts one source left; a level pan puts another right.
    {
        let mut hs = voice("piano Haas 0.6ms", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.3, 1.0), &piano, &mut rng), -24.0), 0.0);
        hs.ms = 0.6;
        hs.deg = Box::new(|_| 90.0 * 0.6 / TIME_FULL_MS);
        v.push(Mix {
            name: "haas",
            total: T,
            voices: vec![hs, voice("organ R40", level(tone(T, &chords(&mut rng, 175.0, 0.0, T, 2.0), &pad, &mut rng), -26.0), -40.0)],
            diffuse_db: None,
        });
    }
    // Quiet parts at the sides of a loud middle.
    {
        let mut sh = voice("shaker L70 -20", level(burst(T, &beat(0.0, T, 0.125, 0.0), 3000.0, 9000.0, 0.03, &mut rng), -36.0), 70.0);
        sh.on = vec![(0.0, T)];
        v.push(Mix {
            name: "quiet sides",
            total: T,
            voices: vec![
                voice("voice C", level(tone(T, &melody(&mut rng, 294.0, 0.0, T, 0.5, 0.95), &lead, &mut rng), -18.0), 0.0),
                voice("bass C", level(tone(T, &melody(&mut rng, 49.0, 0.0, T, 0.5, 0.95), &bass, &mut rng), -18.0), 0.0),
                sh,
                voice("organ R50 -15", level(tone(T, &chords(&mut rng, 220.0, 0.0, T, 2.0), &pad, &mut rng), -33.0), -50.0),
            ],
            diffuse_db: Some(-45.0),
        });
    }
    // Two of a kind in the same range, one each side: two places, never one between.
    {
        v.push(Mix {
            name: "same range",
            total: T,
            voices: vec![
                voice("piano L30", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.25, 1.0), &piano, &mut rng), -24.0), 30.0),
                voice("piano R30", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.25, 1.0), &piano, &mut rng), -24.0), -30.0),
            ],
            diffuse_db: None,
        });
    }
    // Probes: one source alone, two.
    {
        v.push(Mix { name: "solo guitar", total: T, voices: vec![voice("guitar L50", level(tone(T, &melody(&mut rng, 196.0, 0.0, T, 0.3, 1.0), &piano, &mut rng), -26.0), 50.0)], diffuse_db: None });
        v.push(Mix { name: "solo pad", total: T, voices: vec![voice("pad R60", level(tone(T, &chords(&mut rng, 196.0, 0.0, T, 2.0), &pad, &mut rng), -28.0), -60.0)], diffuse_db: None });
        v.push(Mix {
            name: "piano and voice",
            total: T,
            voices: vec![
                voice("piano L45", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.25, 1.0), &piano, &mut rng), -24.0), 45.0),
                voice("voice C", level(tone(T, &melody(&mut rng, 330.0, 0.0, T, 0.6, 0.9), &lead, &mut rng), -22.0), 0.0),
            ],
            diffuse_db: None,
        });
    }
    {
        let sh = |rng: &mut Rng| {
            let mut sh = voice("shaker L70", level(burst(T, &hits(0.0, T, 0.125, 0.0), 3000.0, 9000.0, 0.03, rng), -36.0), 70.0);
            sh.on = vec![(0.0, T)];
            sh
        };
        let a = sh(&mut rng);
        v.push(Mix { name: "solo shaker", total: T, voices: vec![a], diffuse_db: Some(-45.0) });
        let a = sh(&mut rng);
        v.push(Mix {
            name: "shaker and voice",
            total: T,
            voices: vec![a, voice("voice C", level(tone(T, &melody(&mut rng, 294.0, 0.0, T, 0.5, 0.95), &lead, &mut rng), -18.0), 0.0)],
            diffuse_db: None,
        });
    }
    // Dense: seven across the stage, drums in the middle, a room.
    {
        let k = beat(0.0, T, 0.5, 0.0);
        let mut ks = voice("kick C", level(kick(T, &k), -14.0), 0.0);
        ks.kick = true;
        ks.on = vec![(0.0, T)];
        let mut hat = voice("hats R20", level(burst(T, &beat(0.0, T, 0.25, 0.125), 6000.0, 15000.0, 0.03, &mut rng), -32.0), -20.0);
        hat.on = vec![(0.0, T)];
        v.push(Mix {
            name: "dense",
            total: T,
            voices: vec![
                ks,
                voice("bass C", level(tone(T, &melody(&mut rng, 55.0, 0.0, T, 0.5, 0.9), &bass, &mut rng), -20.0), 0.0),
                hat,
                voice("piano L20", level(tone(T, &melody(&mut rng, 262.0, 0.0, T, 0.25, 1.0), &piano, &mut rng), -26.0), 20.0),
                voice("guitar L45", level(tone(T, &melody(&mut rng, 196.0, 0.0, T, 0.33, 1.0), &piano, &mut rng), -27.0), 45.0),
                voice("strings R45", level(tone(T, &chords(&mut rng, 262.0, 0.0, T, 2.0), &pad, &mut rng), -28.0), -45.0),
                voice("brass L75", level(tone(T, &melody(&mut rng, 175.0, 0.0, T, 0.75, 0.8), &lead, &mut rng), -28.0), 75.0),
                voice("organ R75", level(tone(T, &chords(&mut rng, 147.0, 0.0, T, 3.0), &pad, &mut rng), -30.0), -75.0),
            ],
            diffuse_db: Some(-38.0),
        });
    }
    v
}

/// The stage played over a mix with the window watching: every view it made.
fn views_of(l: Vec<f64>, r: Vec<f64>, rate: u32) -> (Vec<CueView>, String) {
    struct Mem {
        l: Vec<f64>,
        r: Vec<f64>,
        pos: u64,
    }
    impl Stage for Mem {
        fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
            let mut inside = 0;
            for i in 0..out_l.len() {
                let p = self.pos as usize;
                let (a, b) = if p < self.l.len() { (self.l[p], self.r[p]) } else { (0.0, 0.0) };
                out_l[i] = a;
                out_r[i] = b;
                inside += (p < self.l.len()) as usize;
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
    let ctl = Arc::new(CueControl::default());
    ctl.set(CueParams::default());
    ctl.looked_ms.store(now_ms(), Ordering::Relaxed);
    let n = l.len();
    let mut st = CueStage::with_control(Box::new(Mem { l, r, pos: 0 }), rate, 0, false, ctl.clone());
    let (mut a, mut b) = (vec![0.0; 480], vec![0.0; 480]);
    let mut views = Vec::new();
    let mut at = 0;
    while at < n {
        st.read(&mut a, &mut b);
        at += 480;
        ctl.looked_ms.store(now_ms(), Ordering::Relaxed);
        views.extend(ctl.views.lock().unwrap().drain(..));
    }
    let mut hist_line = String::new();
    if std::env::var("STAND_HIST").is_ok() {
        // The look's histogram at the end, every 3°, against its top.
        let h = &st.core.look.hist;
        let top = h.iter().cloned().fold(0.0, f64::max);
        let row: Vec<String> = (0..61).map(|c| {
            let s: f64 = h[c * 3..(c * 3 + 3).min(DEG_N)].iter().sum::<f64>() / 3.0;
            format!("{}", ((s / top) * 99.0).round() as i64)
        }).collect();
        hist_line = format!("   hist R90..L90: {}
", row.join(" "));
    }
    (views, hist_line)
}

/// A source's score over a mix.
#[derive(Default)]
struct Score {
    /// Views it sounded in, settled (not just begun or ended), and those it was shown in.
    due: usize,
    shown: usize,
    /// Times it went from shown to not while it sounded.
    drops: usize,
    /// Numbers it was shown under.
    ids: std::collections::BTreeSet<u32>,
    /// Its place off the truth: sum of squares, and the steps from view to view.
    err2: f64,
    step2: f64,
    steps: usize,
    /// Seconds to show each time it starts, and to go each time it stops.
    enter: Vec<f64>,
    leave: Vec<f64>,
    /// Its wave's mean height just after a kick, and between.
    kick_amp: (f64, usize),
    calm_amp: (f64, usize),
    /// Its weight the same way.
    kick_w: (f64, usize),
    calm_w: (f64, usize),
}

/// How near a place must be to a source to be it, degrees.
const MATCH_DEG: f64 = 12.0;
/// Not scored for this long after a source starts or stops, s: the look's memory.
const SETTLE_S: f64 = 0.5;

fn sounding(v: &Voice, t: f64) -> bool {
    v.on.iter().any(|&(a, b)| t >= a && t < b)
}

fn near_change(v: &Voice, t: f64) -> bool {
    v.on.iter().any(|&(a, b)| (t - a).abs() < SETTLE_S || (t - b).abs() < SETTLE_S)
}

/// Sources at one place are one place: the truth is clusters of them.
fn score(m: &Mix, views: &[CueView], trace: Option<&mut String>) -> (Vec<(String, Score)>, f64, f64) {
    let hop_s = frame_len(SR) as f64 / 4.0 / SR as f64;
    // Sources sharing a place are scored as one, the loudest's name.
    let kicks: Vec<f64> = m
        .voices
        .iter()
        .filter(|v| v.kick)
        .flat_map(|v| {
            let x = &v.sig;
            let peak = x.iter().fold(0.0f64, |m, s| m.max(s.abs()));
            let mut t = Vec::new();
            let mut last = -1.0;
            for (i, s) in x.iter().enumerate() {
                let tt = i as f64 / SR as f64;
                if s.abs() > 0.3 * peak && tt - last > 0.2 {
                    t.push(tt);
                    last = tt;
                }
            }
            t
        })
        .collect();
    let in_kick = |t: f64| kicks.iter().any(|&k| t >= k && t < k + 0.15);
    let mut scores: Vec<(String, Score)> = m.voices.iter().map(|v| (v.name.to_string(), Score::default())).collect();
    let mut prev: Vec<Option<(u32, f64)>> = vec![None; m.voices.len()];
    let mut was_on = vec![false; m.voices.len()];
    let mut pending_enter: Vec<Option<f64>> = vec![None; m.voices.len()];
    let mut pending_leave: Vec<Option<f64>> = vec![None; m.voices.len()];
    let (mut false_n, mut counted, mut false_w) = (0usize, 0usize, 0.0f64);
    let mut trace = trace;
    for view in views {
        let t = view.at_s;
        if t < 1.0 {
            continue;
        }
        // Each dot goes to the nearest source sounding, within reach.
        let mut used = vec![false; view.dots.len()];
        let mut got: Vec<Option<usize>> = vec![None; m.voices.len()];
        let mut order: Vec<usize> = (0..m.voices.len()).collect();
        order.sort_by(|&a, &b| rms(&m.voices[b].sig).total_cmp(&rms(&m.voices[a].sig)));
        for &i in &order {
            let v = &m.voices[i];
            let want = (v.deg)(t);
            let best = view
                .dots
                .iter()
                .enumerate()
                .filter(|(j, d)| !used[*j] && (d.deg - want).abs() < MATCH_DEG)
                .min_by(|a, b| (a.1.deg - want).abs().total_cmp(&(b.1.deg - want).abs()));
            // A source at a place another took already is that place's too.
            let shared = m.voices[..].iter().enumerate().any(|(o, ov)| o != i && got[o].is_some() && ((ov.deg)(t) - want).abs() < 3.0);
            if let Some((j, _)) = best {
                if sounding(v, t) || !shared {
                    used[j] = true;
                }
                got[i] = Some(j);
            } else if shared {
                got[i] = m.voices.iter().enumerate().find(|(o, ov)| *o != i && got[*o].is_some() && ((ov.deg)(t) - want).abs() < 3.0).and_then(|(o, _)| got[o]);
            }
        }
        counted += 1;
        false_n += used.iter().filter(|u| !**u).count();
        false_w += used.iter().zip(&view.dots).filter(|(u, _)| !**u).map(|(_, d)| d.weight).sum::<f64>();
        for (i, v) in m.voices.iter().enumerate() {
            let s = &mut scores[i].1;
            let on = sounding(v, t);
            let shown = got[i].is_some();
            if on && !was_on[i] {
                pending_enter[i] = Some(t);
                pending_leave[i] = None;
            }
            if !on && was_on[i] {
                pending_leave[i] = Some(t);
            }
            was_on[i] = on;
            if let (Some(t0), true) = (pending_enter[i], shown) {
                s.enter.push(t - t0);
                pending_enter[i] = None;
            }
            if let (Some(t0), false) = (pending_leave[i], shown) {
                s.leave.push(t - t0);
                pending_leave[i] = None;
            }
            if on && !near_change(v, t) {
                s.due += 1;
                if shown {
                    s.shown += 1;
                } else if prev[i].is_some() {
                    s.drops += 1;
                }
            }
            if let Some(j) = got[i] {
                let d = &view.dots[j];
                if on {
                    s.ids.insert(d.id);
                    let e = d.deg - (v.deg)(t);
                    s.err2 += e * e;
                    if let Some((pid, pdeg)) = prev[i] {
                        if pid == d.id {
                            s.step2 += (d.deg - pdeg).powi(2);
                            s.steps += 1;
                        }
                    }
                    if !v.kick {
                        let w = &d.wave;
                        let hops = w.len() / 2;
                        for h in 0..hops {
                            let ht = t - (hops - 1 - h) as f64 * hop_s;
                            let amp = 0.5 * (w[2 * h + 1] - w[2 * h]) as f64;
                            let slot = if in_kick(ht) { &mut s.kick_amp } else { &mut s.calm_amp };
                            slot.0 += amp;
                            slot.1 += 1;
                        }
                        let slot = if in_kick(t) { &mut s.kick_w } else { &mut s.calm_w };
                        slot.0 += d.weight;
                        slot.1 += 1;
                    }
                }
                prev[i] = Some((d.id, d.deg));
            } else {
                prev[i] = None;
            }
        }
        if let Some(tr) = trace.as_deref_mut() {
            let mut line = vec![b' '; 61];
            for v in m.voices.iter().filter(|v| sounding(v, t)) {
                let c = ((90.0 - (v.deg)(t)) / 3.0).round().clamp(0.0, 60.0) as usize;
                line[c] = b'.';
            }
            for d in &view.dots {
                let c = ((90.0 - d.deg) / 3.0).round().clamp(0.0, 60.0) as usize;
                line[c] = b'A' + (d.id % 26) as u8;
            }
            tr.push_str(&format!("{t:6.2} |{}| {}\n", String::from_utf8(line).unwrap(), view.dots.len()));
        }
    }
    let _ = SR;
    (scores, false_n as f64 / counted.max(1) as f64, false_w / counted.max(1) as f64)
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Runs every mix, prints the table, returns the total of the faults:
/// unseen share, drops, extra numbers, false places, kick effect.
pub(super) fn run_stand(print: bool) -> f64 {
    use std::fmt::Write as _;
    let out = std::env::var("STAND_OUT").ok();
    let only = std::env::var("STAND_ONLY").ok();
    let all: Vec<Mix> = mixes().into_iter().filter(|m| only.as_ref().is_none_or(|o| o.split(',').any(|w| m.name.contains(w)))).collect();
    let results: Vec<(String, f64)> = std::thread::scope(|sc| {
        let hs: Vec<_> = all
            .iter()
            .map(|m| {
                let out = out.clone();
                sc.spawn(move || {
                    let mut rng = Rng::new(99);
                    let (l, r) = mix(m.total, &m.voices, m.diffuse_db, &mut rng);
                    let (views, hist) = views_of(l, r, SR);
                    let mut trace = String::new();
                    let (scores, false_per_view, false_w) = score(m, &views, out.as_ref().map(|_| &mut trace));
                    if let Some(dir) = &out {
                        let _ = std::fs::create_dir_all(dir);
                        let _ = std::fs::write(format!("{dir}/{}.txt", m.name.replace(' ', "_")), &trace);
                    }
                    let mut txt = hist;
                    let mut fault = false_per_view;
                    let _ = writeln!(txt, "== {:<16} false/view {:.2} weight {:.2}", m.name, false_per_view, false_w);
                    for (name, s) in &scores {
                        let shown = if s.due > 0 { s.shown as f64 / s.due as f64 } else { f64::NAN };
                        let err = (s.err2 / s.shown.max(1) as f64).sqrt();
                        let jit = (s.step2 / s.steps.max(1) as f64).sqrt();
                        let kick = if s.kick_amp.1 > 0 && s.calm_amp.1 > 0 {
                            format!("{:5.2}", (s.kick_amp.0 / s.kick_amp.1 as f64) / (s.calm_amp.0 / s.calm_amp.1 as f64).max(1e-12))
                        } else {
                            String::new()
                        };
                        let _ = writeln!(
                            txt,
                            "   {:<20} {:>4.0}% drops {:<2} ids {:<2} err {:>4.1} jit {:>4.2} in {:>4.2} out {:>4.2} {}",
                            name,
                            100.0 * shown,
                            s.drops,
                            s.ids.len(),
                            err,
                            jit,
                            mean(&s.enter),
                            mean(&s.leave),
                            kick
                        );
                        if s.due > 0 {
                            fault += 10.0 * (1.0 - shown) + s.drops as f64 + (s.ids.len().max(1) - 1) as f64 + 0.1 * jit;
                        }
                    }
                    (txt, fault)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total: f64 = results.iter().map(|r| r.1).sum();
    if print {
        for (t, _) in &results {
            print!("{t}");
        }
        println!("== fault total {total:.2}");
    }
    total
}

/// A real track, no truth: how steady the places are. `STAND_FILE` =
/// path[,from s[,seconds]].
fn run_file(spec: &str) {
    let mut it = spec.split(',');
    let path = it.next().unwrap();
    let from: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let secs: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(60.0);
    let f = crate::audio::converter::decode::decode_file(std::path::Path::new(path)).unwrap();
    let rate = f.sample_rate;
    let a = ((from * rate as f64) as usize).min(f.samples_l.len());
    let e = (a + (secs * rate as f64) as usize).min(f.samples_l.len());
    let (l, r): (Vec<f64>, Vec<f64>) = (f.samples_l[a..e].iter().map(|&v| v as f64).collect(), f.samples_r[a..e].iter().map(|&v| v as f64).collect());
    let (views, hist) = views_of(l, r, rate);
    print!("{hist}");
    let mut trace = String::new();
    let mut counts = vec![0usize; MOST_PLACES + 1];
    let mut life: std::collections::BTreeMap<u32, (f64, f64, f64, usize, f64)> = Default::default();
    let mut prev: std::collections::HashMap<u32, f64> = Default::default();
    let (mut collapses, mut hist_n) = (0usize, Vec::<usize>::new());
    for v in &views {
        let t = v.at_s;
        if t < 1.0 {
            continue;
        }
        let n = v.dots.iter().filter(|d| d.weight > 0.0).count();
        counts[n.min(MOST_PLACES)] += 1;
        hist_n.push(n);
        let k = hist_n.len();
        if k > 44 {
            let mut w: Vec<usize> = hist_n[k - 44..k - 1].to_vec();
            w.sort();
            if n + 2 <= w[w.len() / 2] {
                collapses += 1;
            }
        }
        let mut now: std::collections::HashMap<u32, f64> = Default::default();
        for d in &v.dots {
            let e = life.entry(d.id).or_insert((t, t, 0.0, 0, d.deg));
            e.1 = t;
            if let Some(p) = prev.get(&d.id) {
                e.2 += (d.deg - p).powi(2);
                e.3 += 1;
            }
            now.insert(d.id, d.deg);
        }
        prev = now;
        let mut line = vec![b' '; 61];
        for d in &v.dots {
            let c = ((90.0 - d.deg) / 3.0).round().clamp(0.0, 60.0) as usize;
            line[c] = if d.weight > 0.0 { b'A' + (d.id % 26) as u8 } else { b'.' };
        }
        trace.push_str(&format!("{t:7.2} |{}| {}
", String::from_utf8(line).unwrap(), n));
    }
    if let Ok(dir) = std::env::var("STAND_OUT") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(format!("{dir}/file.txt"), &trace);
    }
    let span = views.last().map_or(0.0, |v| v.at_s) - 1.0;
    let lives: Vec<f64> = life.values().map(|e| e.1 - e.0).collect();
    let long = lives.iter().filter(|&&l| l >= 2.0).count();
    let jit: f64 = (life.values().map(|e| e.2).sum::<f64>() / life.values().map(|e| e.3).sum::<usize>().max(1) as f64).sqrt();
    println!("== file {path} {from}+{secs}s: views {}", hist_n.len());
    println!("   places shown: {:?} (views with 0..8)", counts);
    println!("   numbers {} ({:.1}/min), lived >=2s {}, mean life {:.2}s, step jitter {:.2} deg, collapses {}", life.len(), life.len() as f64 / span * 60.0, long, mean(&lives), jit, collapses);
}

/// Diagnostic: the plain energy-weighted angle histogram of every bin of every
/// frame of a mix (DUET style, no coherence, no memory), every 3°.
#[test]
#[ignore]
fn stand_raw() {
    let only = std::env::var("STAND_ONLY").unwrap_or_else(|_| "dense".into());
    let law = LawTable::new(CueParams::default().pan_law);
    let mut list: Vec<(String, Vec<f64>, Vec<f64>)> = Vec::new();
    if let Ok(spec) = std::env::var("STAND_FILE") {
        for one in spec.split(';') {
            let mut it = one.split(',');
            let path = it.next().unwrap();
            let from: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let secs: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(60.0);
            let f = crate::audio::converter::decode::decode_file(std::path::Path::new(path)).unwrap();
            let rate = f.sample_rate as f64;
            let a = ((from * rate) as usize).min(f.samples_l.len());
            let e = (a + (secs * rate) as usize).min(f.samples_l.len());
            // Every other sample at 96k: the bins as at 48k.
            let step = (rate / SR as f64).round().max(1.0) as usize;
            list.push((
                path.rsplit(['\\', '/']).next().unwrap().to_string(),
                f.samples_l[a..e].iter().step_by(step).map(|&v| v as f64).collect(),
                f.samples_r[a..e].iter().step_by(step).map(|&v| v as f64).collect(),
            ));
        }
    } else {
        for m in mixes().into_iter().filter(|m| only.split(',').any(|w| m.name.contains(w))) {
            let mut rng = Rng::new(99);
            let (l, r) = mix(m.total, &m.voices, m.diffuse_db, &mut rng);
            list.push((m.name.to_string(), l, r));
        }
    }
    for (name_m, l, r) in list {
        let n: usize = std::env::var("RAW_N").ok().and_then(|v| v.parse().ok()).unwrap_or(2048);
        let fft = FftPlanner::<f64>::new().plan_fft_forward(n);
        let win: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos()).collect();
        let mut h1 = vec![0.0; 61];
        let mut h3 = vec![0.0; 61];
        let mut hb = vec![0.0; 61];
        let mut hp = vec![0.0; 61];
        let mut ho = vec![0.0; 61];
        let mut slow = vec![Cov::default(); n / 2];
        let mut at = 0;
        while at + n <= l.len() {
            let mut b: Vec<C> = (0..n).map(|i| C::new(l[at + i] * win[i], r[at + i] * win[i])).collect();
            fft.process(&mut b);
            let bin_hz = SR as f64 / n as f64;
            let mut es = vec![0.0; n / 2];
            let mut cs = vec![0usize; n / 2];
            let mut band_e = vec![0.0; 40];
            let mut band_pe = vec![0.0; 40];
            let band = |k: usize| ((3.0 * (k as f64 * bin_hz / 50.0).max(1.0).log2()) as usize).min(39);
            for k in 2..n / 2 {
                let (z, w) = (b[k], b[n - k].conj());
                let (lk, rk) = (0.5 * (z + w), C::new(0.0, -0.5) * (z - w));
                let (el, er) = (lk.norm_sqr(), rk.norm_sqr());
                let e = el + er;
                es[k] = e;
                band_e[band(k)] += e;
                if e < 1e-12 {
                    continue;
                }
                let rho = (10.0 * (el / er.max(1e-30)).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
                let deg = law.deg(rho);
                let c = ((90.0 - deg) / 3.0).round().clamp(0.0, 60.0) as usize;
                cs[k] = c;
                h1[c] += e;
                h3[c] += e.powf(0.3);
            }
            for k in 3..n / 2 - 1 {
                if es[k] > es[k - 1] && es[k] >= es[k + 1] {
                    band_pe[band(k)] += es[k];
                }
            }
            // Onsets: what rose over the bin's recent past, at its own angle.
            for k in 2..n / 2 {
                let (z, w) = (b[k], b[n - k].conj());
                let (lk, rk) = (0.5 * (z + w), C::new(0.0, -0.5) * (z - w));
                let x = lk * rk.conj();
                let c = Cov { ll: lk.norm_sqr(), rr: rk.norm_sqr(), re: x.re, im: x.im };
                let o = slow[k];
                let d = Cov { ll: (c.ll - o.ll).max(0.0), rr: (c.rr - o.rr).max(0.0), re: c.re - o.re, im: c.im - o.im };
                let de = d.ll + d.rr;
                if de > 0.5 * (c.ll + c.rr) && de > 1e-12 {
                    let (alpha, _) = d.axis();
                    let a = alpha.clamp(1e-3, PI / 2.0 - 1e-3);
                    let rho = (20.0 * (a.cos() / a.sin()).log10()).clamp(-RHO_MAX_DB, RHO_MAX_DB);
                    let cc = ((90.0 - law.deg(rho)) / 3.0).round().clamp(0.0, 60.0) as usize;
                    let bb = band(k);
                    if band_e[bb] > 0.0 {
                        ho[cc] += de / band_e[bb] * band_e[bb].powf(0.3);
                    }
                }
                slow[k].go(&c, 0.25);
            }
            for k in 3..n / 2 - 1 {
                let bb = band(k);
                if band_e[bb] > 0.0 {
                    hb[cs[k]] += es[k] / band_e[bb] * band_e[bb].powf(0.3);
                }
                if es[k] > es[k - 1] && es[k] >= es[k + 1] && band_pe[bb] > 0.0 {
                    hp[cs[k]] += es[k] / band_pe[bb] * band_e[bb].powf(0.3);
                }
            }
            at += n / 4;
        }
        for (name, h) in [("e", &h1), ("e^0.3", &h3), ("band", &hb), ("peaks", &hp), ("onset", &ho)] {
            let top = h.iter().cloned().fold(0.0, f64::max);
            let row: Vec<String> = h.iter().map(|v| format!("{}", (v / top * 99.0).round() as i64)).collect();
            println!("== raw {} {} L90..R90: {}", name_m, name, row.join(" "));
        }
    }
}

#[test]
#[ignore]
fn stand_file() {
    if let Ok(spec) = std::env::var("STAND_FILE") {
        for one in spec.split(';').filter(|v| !v.is_empty()) {
            run_file(one);
        }
    }
}

#[test]
#[ignore]
fn stand() {
    run_stand(true);
}
