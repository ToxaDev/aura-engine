//! The stream's instruments end to end: a made-up song through the feed, the
//! segments taken apart (the true sources standing in for the network), the
//! songs' maps and the wire.

use super::feed::tests::TestShadow;
use super::notes::tests::Hum;
use super::notes::LNote;
use crate::spatial::notes::bp::Windows;
use super::*;
use crate::spatial::core::SOURCES;
use crate::spatial::kit::{CHUNK, NP};
use crate::spatial::stems;
use std::sync::atomic::Ordering;

const N_S: f64 = 30.0;

/// A song's six sources, made up: the kit (a kick every half second, a snare
/// between, hats every eighth; with `more`, toms from 12 s and a crash from
/// 16 s, once a second each, the crash to the right), a bass, a voice in the
/// middle from 5 s to 25 s, a guitar on the left all along and another on the
/// right from 10 s; the kit's pieces (in the drum network's order), and the
/// mix.
fn truth(n: usize, more: bool) -> (Vec<Vec<f32>>, Vec<Vec<f32>>, Vec<f32>, Vec<f32>) {
    let sr = SR as f64;
    let mut st = vec![vec![0f32; n]; SOURCES * 2];
    let mut pieces = vec![vec![0f32; n]; NP * 2];
    let mut noise = 12345u32;
    let mut rnd = move || {
        noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let mut noise2 = 777u32;
    let mut rnd2 = move || {
        noise2 = noise2.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (noise2 >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let tau = std::f64::consts::TAU;
    let pluck = |p: f64| (1.0 - (-p / 0.003).exp()) * (-p / 0.15).exp();
    for i in 0..n {
        let t = i as f64 / sr;
        let (kick, snare, hat) = (t % 0.5, (t + 0.25) % 1.0, t % 0.125);
        let mut kit = [(0f64, 0f64); NP];
        if kick < 0.08 {
            let v = (tau * 60.0 * kick).sin() * (-kick / 0.03).exp() * 0.6;
            kit[0] = (v, v);
        }
        if snare < 0.06 {
            let v = rnd() as f64 * (-snare / 0.02).exp() * 0.3;
            kit[1] = (v, v);
        }
        if hat < 0.02 {
            // highs: noise differenced
            let v = (rnd() - rnd()) as f64 * (-hat / 0.005).exp() * 0.08;
            kit[3] = (v, v);
        }
        if more {
            let tom = (t + 0.4) % 1.0;
            if t >= 12.0 && tom < 0.15 {
                let v = (tau * 110.0 * tom).sin() * (-tom / 0.05).exp() * 0.5;
                kit[2] = (v, v);
            }
            let crash = (t + 0.1) % 1.0;
            if t >= 16.0 && crash < 0.5 {
                let v = rnd2() as f64 * (-crash / 0.15).exp() * 0.15;
                kit[5] = (0.6 * v, v);
            }
        }
        let mut d = (0f64, 0f64);
        for (p, (a, b)) in kit.iter().enumerate() {
            pieces[p * 2][i] = *a as f32;
            pieces[p * 2 + 1][i] = *b as f32;
            d = (d.0 + a, d.1 + b);
        }
        let mut set = |s: usize, l: f64, r: f64| {
            st[s * 2][i] += l as f32;
            st[s * 2 + 1][i] += r as f32;
        };
        set(stems::DRUMS, d.0, d.1);
        let bass = (tau * if (t as usize) % 2 == 0 { 55.0 } else { 82.4 } * t).sin() * 0.3;
        set(stems::BASS, bass, bass);
        if (5.0..25.0).contains(&t) {
            let v = (tau * 300.0 * t + 3.0 * (tau * 5.0 * t).sin()).sin() * 0.25;
            set(stems::VOCALS, v, v);
        }
        let g = (tau * 440.0 * t).sin() * pluck(t % 0.4) * 0.3;
        set(stems::GUITAR, g, 0.25 * g);
        if t >= 10.0 {
            let g2 = (tau * 660.0 * t).sin() * pluck((t + 0.1) % 0.3) * 0.3;
            set(stems::GUITAR, 0.25 * g2, g2);
        }
    }
    let mix = |ch: usize| (0..n).map(|i| (0..SOURCES).map(|s| st[s * 2 + ch][i]).sum::<f32>()).collect::<Vec<f32>>();
    let (l, r) = (mix(0), mix(1));
    (st, pieces, l, r)
}

struct Rig {
    sh: Arc<TestShadow>,
    told: Arc<SongStarts>,
    live: Live,
    st: Vec<Vec<f32>>,
    /// The kit's true pieces: the drum network's stand-in gives them back.
    pieces: Vec<Vec<f32>>,
    l: Vec<f32>,
    r: Vec<f32>,
    at: usize,
    /// The separation runs (a card): else only the stand-in.
    sep_on: bool,
    /// The drum network runs (its session open) from the stream's second this.
    drums_from: Option<f64>,
    /// A cut made in the stream: (where, how long) — the sources past it come that much later.
    cut: Option<(usize, usize)>,
    /// The note network (or its stand-in): the sources' notes after each segment.
    notes: Option<Box<dyn Windows>>,
    /// Per round of the notes: the network's time and the rest's (ms).
    times: Vec<(f64, f64)>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(N_S, true)
    }

    fn with(secs: f64, sep_on: bool) -> Rig {
        Rig::make(secs, sep_on, false)
    }

    /// A song whose kit has toms and a crash too.
    fn kit(secs: f64) -> Rig {
        Rig::make(secs, true, true)
    }

    fn make(secs: f64, sep_on: bool, more: bool) -> Rig {
        let n = (secs * SR as f64) as usize;
        let (st, pieces, l, r) = truth(n, more);
        let sh = Arc::new(TestShadow::new(SR as u32));
        let told = Arc::new(SongStarts::default());
        let live = Live::new(7, sh.clone(), told.clone(), 0.0);
        Rig { sh, told, live, st, pieces, l, r, at: 0, sep_on, drums_from: None, cut: None, notes: None, times: Vec::new() }
    }

    /// A song of these sources (`[source·2 + channel]`; the mix their sum) with
    /// the note network `net`.
    fn of(st: Vec<Vec<f32>>, net: Box<dyn Windows>) -> Rig {
        let n = st[0].len();
        let mix = |ch: usize| (0..n).map(|i| (0..SOURCES).map(|s| st[s * 2 + ch][i]).sum::<f32>()).collect::<Vec<f32>>();
        let (l, r) = (mix(0), mix(1));
        let sh = Arc::new(TestShadow::new(SR as u32));
        let told = Arc::new(SongStarts::default());
        let live = Live::new(7, sh.clone(), told.clone(), 0.0);
        Rig { sh, told, live, st, pieces: Vec::new(), l, r, at: 0, sep_on: true, drums_from: None, cut: None, notes: Some(net), times: Vec::new() }
    }

    /// The drum network's chunks due (its stand-in: the true pieces of what
    /// each chunk reads), the listener at session sample `heard`.
    fn drum_chunks(&mut self, heard: usize) {
        self.live.drum_on = true;
        self.live.feed_drums();
        while let Some((i, _, _)) = self.live.drums.pick(heard, 0) {
            let a = self.live.drums.start(i);
            let mut y = vec![0f32; NP * 2 * CHUNK];
            for pc in 0..NP * 2 {
                for k in 0..CHUNK {
                    let t = a + k as isize;
                    if t >= 0 {
                        y[pc * CHUNK + k] = self.pieces[pc].get(t as usize).copied().unwrap_or(0.0);
                    }
                }
            }
            self.live.drums.add(i, &y).unwrap();
        }
    }

    /// The stream in up to second `to`, the segments taken apart (the
    /// sources being the true ones), the listener at `heard`.
    fn run(&mut self, to: f64, heard: f64) {
        self.live.heard_s = heard;
        let to = ((to * SR as f64) as usize).min(self.l.len());
        while self.at < to {
            let b = (self.at + SR / 4).min(to);
            let l: Vec<f64> = self.l[self.at..b].iter().map(|v| *v as f64).collect();
            let r: Vec<f64> = self.r[self.at..b].iter().map(|v| *v as f64).collect();
            self.sh.push(&l, &r);
            self.at = b;
            self.live.pull();
            if !self.sep_on {
                continue;
            }
            while let Some(k) = self.live.sep.pick(self.live.feed.end(), (heard * SR as f64) as usize, 0) {
                let a = self.live.sep.start(k);
                let mut out = vec![0f32; SOURCES * 2 * SEG];
                for sc in 0..SOURCES * 2 {
                    for i in 0..SEG {
                        let t = a + i + self.cut.filter(|c| a + i >= c.0).map_or(0, |c| c.1);
                        out[sc * SEG + i] = self.st[sc].get(t).copied().unwrap_or(0.0);
                    }
                }
                self.live.sep.add(k, &out);
                if self.notes.is_some() {
                    self.live.notes_on = true;
                    self.live.feed_notes();
                }
                if self.drums_from.is_some_and(|t| self.at as f64 >= t * SR as f64) {
                    self.drum_chunks((heard * SR as f64) as usize);
                }
                self.live.after_segment();
                if let Some(net) = self.notes.as_mut() {
                    if let Some(mut job) = self.live.notes_take() {
                        let t0 = std::time::Instant::now();
                        let out = job.run(net.as_mut());
                        let a = t0.elapsed().as_secs_f64() * 1000.0;
                        self.live.notes_put(job, out).unwrap();
                        self.times.push((a, t0.elapsed().as_secs_f64() * 1000.0 - a));
                    }
                }
            }
        }
    }

    fn ask(&mut self, at_s: f64) -> Vec<u8> {
        answer(Some(&mut self.live), Ask { track: track_id(7), at_s, state: 0, from_ms: -6000.0, to_ms: 3500.0, pack: true, on: true, open: true })
    }
}

fn head(b: &[u8]) -> Vec<f64> {
    (0..15).map(|i| f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())).collect()
}

/// A file's kick hits on the whole drums source (`kit_bands_of`: its bands'
/// onsets, the research's threshold), seconds.
fn file_kick_hits(rig: &Rig) -> Vec<f64> {
    let frames = rig.l.len() / HOP + 1;
    let srcs: Vec<(&[f32], &[f32])> = (0..SOURCES).map(|s| (&rig.st[s * 2][..], &rig.st[s * 2 + 1][..])).collect();
    let st = analysis::Stft::new();
    let reg = analysis::region(&st, &srcs, true, 0, 0..frames);
    let raw: Vec<f32> = (0..frames).flat_map(|j| reg.band[(j * analysis::BANDS + analysis::B_KICK) * analysis::RAW..][..analysis::RAW].to_vec()).collect();
    let ref_db = rig.live.songs[0].rough.d.ref_db;
    let db: Vec<f32> = (0..frames).map(|j| 10.0 * (raw[j * analysis::RAW] + 1e-12).log10() - ref_db).collect();
    let mut v: Vec<f32> = db.iter().cloned().filter(|d| *d > -90.0).collect();
    v.sort_by(|a, b| a.total_cmp(b));
    let fin = analysis::finish_band(&raw, frames, ref_db, &vec![v[v.len() * 9 / 10]; frames], false);
    let on: Vec<f32> = fin.iter().map(|f| f[5]).collect();
    crate::spatial::job::peaks(&on, 0.5).into_iter().map(|t| t as f64).collect()
}

fn names(s: &Song) -> Vec<String> {
    s.map.objects.iter().map(|o| o.name.clone()).collect()
}

/// A stream's song finds its instruments as it plays: the kit from the
/// drums' bands with its hits, the bass, the voice where it sings, a guitar
/// on the left, the room — in a file's order — and the guitar that comes in
/// on the right later; their values are there ahead of what is heard.
#[test]
fn a_streams_song_finds_its_instruments_as_it_plays() {
    let mut rig = Rig::new();
    rig.run(12.0, 0.0);
    let s = rig.live.songs.back().unwrap();
    assert_eq!(names(s), ["kick", "snare", "hh", "bass", "voice", "guitar", "ambience"], "found by 12 s");
    rig.run(N_S, 0.0);
    let s = rig.live.songs.back().unwrap();
    let ns = names(s);
    assert_eq!(&ns[..5], ["kick", "snare", "hh", "bass", "voice"]);
    assert_eq!(ns.iter().filter(|n| n.starts_with("guitar")).count(), 2, "the second guitar found: {ns:?}");
    assert_eq!(ns.last().unwrap(), "ambience", "no scene had the map yet: a file's order");
    let gl = s.map.objects.iter().position(|o| o.name.starts_with("guitar") && o.x < -0.3).expect("a guitar on the left");
    let gr = s.map.objects.iter().position(|o| o.name.starts_with("guitar") && o.x > 0.3).expect("a guitar on the right");
    assert!(gl < gr);
    // the kick's hits: a file's (its bands' onsets over the whole drums source), within a frame or two
    let kick = &s.map.objects[0];
    let fin = s.ready() as f64 / FPS - 1.0;
    let want: Vec<f64> = file_kick_hits(&rig).into_iter().filter(|t| *t > 0.3 && *t < fin).collect();
    let at = s.f0 as f64 / FPS;
    let near = want.iter().filter(|t| kick.hits.iter().any(|h| (*h as f64 + at - **t).abs() < 0.03)).count();
    assert!(want.len() > 10 && near + 1 >= want.len(), "kick hits {near} of {}: {:?}", want.len(), &kick.hits[..kick.hits.len().min(12)]);
    // the voice sounds where it sings
    let voice = &s.map.objects[4];
    let p = |t: f64| voice.presence[(t * FPS) as usize];
    assert!(p(3.0) < 30 && p(10.0) > 200 && p(20.0) > 200, "voice presence {} {} {}", p(3.0), p(10.0), p(20.0));
    // made from the separated sources, ahead of what is heard
    assert!(s.has((5.0 * FPS) as usize) && s.ready() as f64 / FPS > N_S - 8.0, "ready to {:.1} s", s.ready() as f64 / FPS);
}

/// Once a scene has had the map, its slots only grow: an instrument found
/// later takes the next slot at the end of the row; and the wire gives the
/// map's frames up to where they are known, its id, and a new make each time
/// frames given out were made again.
#[test]
fn once_shown_the_slots_only_grow_and_the_wire_says_what_is_known() {
    let mut rig = Rig::new();
    rig.run(12.0, 0.0);
    let b = rig.ask(5.0);
    let h = head(&b);
    assert_eq!(h[7], 2.0, "the heard frame from the instruments");
    assert!(h[12] > 0.0 && h[13] == 0.0, "the map's id; nothing on its way");
    let s = rig.live.songs.back().unwrap();
    assert!(s.shown);
    let (f0, n) = (h[2] as usize, h[3] as usize);
    assert_eq!(f0, 0);
    assert!(f0 + n <= s.f0 + s.ready(), "only frames whose values are known");
    assert_eq!(b.len(), 15 * 8 + n * SLOTS * VALS * 2);
    let made = h[14];
    rig.run(N_S, 0.0);
    let h2 = head(&rig.ask(5.0));
    assert_ne!(h2[14], made, "frames made again: the window is asked for again");
    let s = rig.live.songs.back().unwrap();
    let ns = names(s);
    assert_eq!(&ns[..7], ["kick", "snare", "hh", "bass", "voice", "guitar", "ambience"], "the slots it was shown");
    assert!(ns.len() == 8 && ns[7].starts_with("guitar") && s.map.objects[7].x > 0.3, "the late guitar at the end: {ns:?}");
}

/// Ready for the notes (the next stage): a note on an object follows it when
/// a slot is taken before it, and the map gives the notes about the place
/// heard in the session's seconds.
#[test]
fn the_map_takes_notes_in_the_sessions_seconds() {
    let mut rig = Rig::new();
    rig.told.push((3.0 * SR as f64) as i64, SR as u32);
    rig.run(16.0, 0.0);
    let s = rig.live.songs.back_mut().unwrap();
    let g = s.map.objects.iter().position(|o| o.name == "guitar").unwrap();
    let at = s.f0 as f64 / FPS;
    s.map.notes.push(crate::spatial::map::Note { obj: g as u8, key: 69.0, on: 6.0, off: 6.4, vel: 0.8, ghost: false });
    s.map.notes.push(crate::spatial::map::Note { obj: g as u8, key: 69.0, on: 0.5, off: 0.6, vel: 0.8, ghost: false });
    let (n, v) = (s.n, s.version);
    rig.live.heard_s = at + 12.0;
    let j: serde_json::Value = serde_json::from_str(&map_json_of(&rig.live, map_id(7, n, v)).unwrap()).unwrap();
    let notes = j["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "only the notes about the place heard: {notes:?}");
    assert_eq!(notes[0][0].as_u64(), Some(g as u64));
    assert!((notes[0][2].as_f64().unwrap() - (at + 6.0)).abs() < 1e-3, "in the session's seconds");
    // a slot taken before the guitar (no scene had the map): its note moves with it
    let s = rig.live.songs.back_mut().unwrap();
    assert!(!s.shown);
    s.insert(crate::spatial::map::MapObj { kind: crate::spatial::K_KICK, name: "kick 2".into(), ..Default::default() }, song::What::Room, s.f0);
    assert_eq!(s.map.notes[0].obj as usize, g + 1);
}

/// A station change: the place heard still the station before's (its end
/// playing out under the new session's name) lies outside the new stream —
/// the work waits; inside, it starts; a place before where the feed starts
/// (the work begun at such a place) starts it afresh.
#[test]
fn a_place_heard_outside_the_stream_waits_and_one_the_feed_cannot_give_starts_afresh() {
    let sh = Arc::new(TestShadow::new(SR as u32));
    let x = vec![0.1f64; SR * 10];
    sh.push(&x[..SR * 2], &x[..SR * 2]);
    let told = Arc::new(SongStarts::default());
    assert_eq!(fit(None, 8, 70.3, &*sh), Fit::Wait, "the station before's place");
    assert_eq!(fit(None, 8, 1.0, &*sh), Fit::Start);
    sh.push(&x[SR * 2..], &x[SR * 2..]);
    let l = Live::new(8, sh.clone(), told.clone(), 5.0);
    assert_eq!(l.feed.start(), (3.5 * SR as f64) as usize);
    assert_eq!(fit(Some(&l), 8, 4.0, &*sh), Fit::Keep);
    assert_eq!(fit(Some(&l), 8, 0.3, &*sh), Fit::Start, "a place before where the feed starts");
    assert_eq!(fit(Some(&l), 9, 4.0, &*sh), Fit::Start, "another session");
    assert_eq!(fit(Some(&l), 8, 70.3, &*sh), Fit::Wait);
}

/// A place heard before what the stream still holds (the shadow's history
/// behind a reader far ahead of the listener) starts the work once, from the
/// first frame the stream holds, and the work goes on ask after ask until the
/// place heard comes to it — not afresh at every ask (TASK-34: 928 times in
/// 150 s, the scene's note and its instruments flickering every second).
#[test]
fn a_place_heard_before_what_the_stream_holds_starts_the_work_once() {
    let mut sh = TestShadow::new(SR as u32);
    sh.first = (20.0 * SR as f64) as i64;
    let sh = Arc::new(sh);
    let x = vec![0.1f64; SR * 40];
    sh.push(&x, &x);
    let told = Arc::new(SongStarts::default());
    assert_eq!(fit(None, 8, 5.0, &*sh), Fit::Start, "none yet: one starts");
    let l = Live::new(8, sh.clone(), told, 5.0);
    assert_eq!(l.feed.start(), 20 * SR, "from the first frame the stream holds");
    for k in 0..100 {
        assert_eq!(fit(Some(&l), 8, 5.0 + 0.15 * k as f64, &*sh), Fit::Keep, "ask {k}: the work goes on");
    }
    assert_eq!(fit(Some(&l), 8, 21.0, &*sh), Fit::Keep, "the place heard has come to it");
    assert_eq!(fit(Some(&l), 9, 5.0, &*sh), Fit::Start, "another session");
}

/// The work far ahead of the place heard (a station's burst puts the stream
/// half a minute ahead) has let go of its feed's history behind it: the place
/// heard is still its own — its instruments are in the song's map — and no
/// ask starts it afresh (TASK-34: every 1.4 s the instruments went and came
/// back).
#[test]
fn the_work_far_ahead_of_the_place_heard_goes_on() {
    let mut rig = Rig::new();
    rig.run(N_S, 2.0);
    let l = &rig.live;
    assert!(l.feed.start() > (5.0 * SR as f64) as usize, "the feed has let go of the place heard: {}", l.feed.start());
    for k in 0..100 {
        let at = 2.0 + 0.15 * k as f64;
        assert_eq!(fit(Some(l), 7, at, &*rig.sh), Fit::Keep, "ask {k} at {at:.2} s");
    }
    let s = l.songs.front().unwrap();
    let j = (5.0 * FPS) as usize - s.f0;
    assert!(s.has(j), "the place heard has its instruments in the song's map");
}

/// A song's start the stream tells of begins a new map there: the song
/// before ends at it, the new one finds its instruments afresh, and the wire
/// gives nothing of the song before in a window over the start.
#[test]
fn a_song_start_from_the_stream_begins_a_new_map() {
    let mut rig = Rig::new();
    rig.told.push((14.0 * SR as f64) as i64, SR as u32);
    rig.run(N_S, 0.0);
    assert_eq!(rig.live.songs.len(), 2);
    let (a, b) = (&rig.live.songs[0], &rig.live.songs[1]);
    assert_eq!(a.end, Some(b.f0));
    assert!((b.f0 as f64 / FPS - 14.0).abs() < 0.02, "from {:.2} s", b.f0 as f64 / FPS);
    assert!(names(b).iter().any(|n| n == "kick") && names(b).iter().any(|n| n.starts_with("guitar")));
    let b2 = rig.ask(15.0);
    let h = head(&b2);
    assert_eq!(h[7], 2.0);
    assert_ne!(h[12] as u32 & 0xffff_0000, map_id(7, 0, 0) & 0xffff_0000, "the new song's map");
    let (f0, n) = (h[2] as usize, h[3] as usize);
    let k = rig.live.songs[1].f0 - f0;
    assert!(k > 0 && k < n);
    // the song before's frames in the window: empty
    let at = |j: usize, s: usize| u16::from_le_bytes(b2[15 * 8 + (j * SLOTS + s) * VALS * 2..][..2].try_into().unwrap());
    assert!((0..k).all(|j| (0..SLOTS).all(|s| at(j, s) == 0)));
}

/// Silence of two seconds between two sounds begins a new song when the
/// stream tells of none; the stream changed under the analysis (a cut)
/// starts it afresh there, as a new song.
#[test]
fn a_long_silence_and_a_cut_begin_new_songs() {
    let mut rig = Rig::new();
    let (a, b) = ((10.0 * SR as f64) as usize, (12.5 * SR as f64) as usize);
    for v in [&mut rig.l, &mut rig.r] {
        v[a..b].iter_mut().for_each(|x| *x = 0.0);
    }
    for v in rig.st.iter_mut() {
        v[a..b].iter_mut().for_each(|x| *x = 0.0);
    }
    rig.run(20.0, 0.0);
    assert_eq!(rig.live.songs.len(), 2, "a new song after the silence");
    assert!((rig.live.songs[1].f0 as f64 / FPS - 12.5).abs() < 0.05, "{:.2}", rig.live.songs[1].f0 as f64 / FPS);
    // a cut in the stream at 17 s (what follows moves earlier)
    let at = (17.0 * SR as f64) as usize;
    rig.sh.l.lock().unwrap().drain(at..at + SR / 2);
    rig.sh.r.lock().unwrap().drain(at..at + SR / 2);
    rig.sh.edits.fetch_add(1, Ordering::Relaxed);
    rig.cut = Some((at, SR / 2));
    rig.run(21.0, 0.0);
    assert_eq!(rig.live.songs.len(), 3, "afresh from the cut");
    let s2 = &rig.live.songs[2];
    assert!((s2.f0 as f64 / FPS - 17.0).abs() < 0.2, "{:.2}", s2.f0 as f64 / FPS);
    assert_eq!(s2.f0 + s2.rough.frames(), rig.live.rough_next, "the stand-in made again from there, frame for frame");
    assert!(rig.live.feed.restarts >= 1);
    // the new grid's first segment, once its 7.8 s are in
    rig.run(26.0, 0.0);
    let s2 = &rig.live.songs[2];
    assert!(s2.has((19.5 * FPS) as usize - s2.f0), "the instruments again, on the new grid");
}

/// Without a card for the network: the stand-in only, the feed keeps a
/// bounded stretch of the mix however long the stream plays.
#[test]
fn without_the_card_the_stand_in_plays_and_the_mix_held_stays_bounded() {
    let mut rig = Rig::with(60.0, false);
    rig.run(60.0, 0.0);
    assert_eq!(rig.live.sep.done, 0);
    let held = (rig.live.feed.end() - rig.live.feed.start()) as f64 / SR as f64;
    assert!(held < 22.0, "{held:.1} s of the mix held");
    let h = head(&answer(Some(&mut rig.live), Ask { track: track_id(7), at_s: 50.0, state: 0, from_ms: -6000.0, to_ms: 3500.0, pack: true, on: false, open: false }));
    assert_eq!((h[7], h[12], h[13]), (1.0, 0.0, 0.0), "the mix, no map, nothing on its way");
}

/// Behind (the listener ahead of what could be taken apart in time): the
/// segments it would hear before they are ready are skipped — their frames
/// are the stand-in's, and the indicator says the instruments are on their
/// way — and the newest one ahead of the listener goes.
#[test]
fn behind_the_listener_the_stand_in_shows() {
    let mut rig = Rig::new();
    rig.run(14.0, 13.0);
    assert_eq!(rig.live.sep.skipped, 1, "the first segment, heard before it could be ready");
    let h = head(&rig.ask(3.0));
    assert_eq!(h[7], 1.0, "the mix stands in");
    assert_eq!(h[12], 0.0, "no map for the scene then");
    assert_eq!(h[13], 1.0, "on its way");
    assert!(h[3] > 0.0, "the stand-in's frames");
    let h = head(&rig.ask(12.5));
    assert_eq!(h[7], 2.0, "the newest segment, ahead of the listener");
}

/// With the drum network the kit comes piece by piece: kick, snare and hats
/// from the song's start with the network's hits, and the toms and the crash
/// as instruments of their own once they play (eight hits, loud enough) — in
/// a file's order before a scene has had the map, where they sit.
#[test]
fn the_drum_network_brings_the_toms_and_the_crash_once_they_play() {
    let mut rig = Rig::kit(N_S);
    rig.drums_from = Some(0.0);
    rig.run(15.0, 0.0);
    let s = rig.live.songs.back().unwrap();
    let ns = names(s);
    assert!(!ns.iter().any(|n| n == "toms" || n == "crash"), "not before they play: {ns:?}");
    assert!(!s.map.kit_bands, "the kit from the drum network");
    rig.run(N_S, 0.0);
    let s = rig.live.songs.back().unwrap();
    let ns = names(s);
    assert_eq!(&ns[..5], ["kick", "snare", "toms", "hh", "crash"], "a file's order: {ns:?}");
    assert!(!ns.iter().any(|n| n == "ride"), "a piece that never plays is none");
    let obj = |name: &str| s.map.objects.iter().find(|o| o.name == name).unwrap();
    assert_eq!((obj("toms").kind, obj("crash").kind), (crate::spatial::K_TOMS, crate::spatial::K_CYMBAL));
    assert!(obj("crash").x > 0.1 && obj("toms").x.abs() < 0.05, "crash at {}, toms at {}", obj("crash").x, obj("toms").x);
    // the kick's hits: the true kicks, every half second
    let f0 = s.f0 as f64 / FPS;
    let fin = s.ready() as f64 / FPS - 1.0;
    let kick = obj("kick");
    let want: Vec<f64> = (1..60).map(|k| k as f64 * 0.5).filter(|t| *t > 1.0 && *t < fin).collect();
    let near = want.iter().filter(|t| kick.hits.iter().any(|h| (*h as f64 + f0 - **t).abs() < 0.025)).count();
    assert!(near + 1 >= want.len(), "kick hits {near} of {}", want.len());
    // the toms' from 12 s on, once a second, written from the frames held when they were found
    let th: Vec<f64> = obj("toms").hits.iter().map(|h| *h as f64 + f0).collect();
    assert!(th.len() >= 10 && th.iter().all(|t| *t > 12.0), "{th:?}");
    let p = |t: f64| obj("toms").presence[((t - f0) * FPS) as usize];
    assert!(p(8.0) == 0 && p(13.65) > 200, "toms presence {} {}", p(8.0), p(13.65));
}

/// The drum network's session opening in the middle of a song: kick, snare
/// and hats go over from the drums' bands to its pieces over half a second —
/// no drop in their energy faster than its own release, no presence lost,
/// no hit twice.
#[test]
fn the_kit_goes_over_from_the_bands_to_the_drum_network_without_a_jump() {
    let mut rig = Rig::kit(N_S);
    rig.run(12.0, 0.0);
    // the session opens late: the network starts where the listener is
    rig.drums_from = Some(0.0);
    rig.run(N_S, 11.0);
    let s = rig.live.songs.back().unwrap();
    let seam = (s.f0..s.f0 + s.ready()).find(|&j| rig.live.dfeats.get(j).is_some_and(|f| f.ok)).expect("the network's frames");
    let seam_s = seam as f64 / FPS;
    assert!((10.9..11.6).contains(&seam_s), "the network from {seam_s:.2} s");
    assert!(!s.map.kit_bands);
    let k = seam - s.f0;
    for name in ["kick", "snare", "hh"] {
        let o = s.map.objects.iter().find(|o| o.name == name).unwrap();
        let worst = (k - 86..k + 86).map(|j| o.energy[j] as i32 - o.energy[j + 1] as i32).max().unwrap();
        assert!(worst <= 40, "{name}: its energy drops {worst} in a frame at the seam");
        let low = (k - 43..k + 86).map(|j| o.presence[j]).min().unwrap();
        assert!(low > 200, "{name}: presence {low} at the seam: {:?} energy {:?}", &o.presence[k - 43..k + 86], &o.energy[k - 43..k + 86]);
        let at = seam_s - s.f0 as f64 / FPS;
        let hs: Vec<f64> = o.hits.iter().map(|h| *h as f64).filter(|t| (t - at).abs() < 1.5).collect();
        assert!(hs.len() >= 2 && hs.windows(2).all(|w| w[1] - w[0] >= 0.06), "{name}: {hs:?}");
    }
}

fn slot(s: &Song, name: &str) -> usize {
    s.map.objects.iter().position(|o| o.name == name).unwrap_or_else(|| panic!("no {name}: {:?}", names(s)))
}

/// The bass and the voice get their notes as the stream plays, on their own
/// places: the bass a note a second, 55 Hz and 82.4 Hz by turns, each from its
/// second's start; the voice about 300 Hz from 5 s to 25 s; and the map's json
/// gives them about the place heard in the session's seconds.
#[test]
fn the_bass_and_the_voice_get_their_notes_as_the_stream_plays() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    rig.run(20.0, 0.0);
    let v0 = rig.live.songs.back().unwrap().version;
    rig.run(N_S, 0.0);
    let s = rig.live.songs.back().unwrap();
    assert!(s.version != v0, "the notes moved the map's version");
    let (bass, voice) = (slot(s, "bass"), slot(s, "voice"));
    let at = s.f0 as f64 / FPS;
    let on_place = |o: usize| s.map.notes.iter().filter(move |n| n.obj as usize == o);
    // (the last segment taken apart ends at 25.35 s)
    for k in 1..26 {
        let want = if k % 2 == 0 { 33.0 } else { 40.0 };
        assert!(
            on_place(bass).any(|n| (n.on as f64 + at - k as f64).abs() < 0.06 && n.key == want),
            "the bass's note at {k} s: {:?}",
            on_place(bass).map(|n| (n.on, n.key)).collect::<Vec<_>>()
        );
    }
    // the bass flashes on its strong notes: from its first one on every hit is one of their onsets, none twice
    let bh = &s.map.objects[bass].hits;
    let strong: Vec<f32> = on_place(bass).filter(|n| !n.ghost).map(|n| n.on).collect();
    let first = strong.iter().cloned().fold(f32::INFINITY, f32::min);
    assert!(bh.windows(2).all(|w| w[1] - w[0] >= 0.06), "{bh:?}");
    assert!(bh.iter().filter(|h| **h >= first).all(|h| strong.contains(h)) && bh.iter().filter(|h| **h >= first).count() >= 20, "{bh:?}");
    let vn: Vec<&crate::spatial::map::Note> = on_place(voice).collect();
    assert!(vn.len() >= 10, "{} voice notes", vn.len());
    assert!(vn.iter().all(|n| (61.0..=63.0).contains(&n.key) && n.on as f64 + at > 4.9 && (n.on as f64 + at) < 25.1), "{vn:?}");
    assert!(s.map.notes.iter().all(|n| n.obj as usize == bass || n.obj as usize == voice || s.map.objects[n.obj as usize].name.starts_with("guitar")));
    assert!(s.map.notes.iter().all(|n| (0.0..=1.0).contains(&n.vel) && n.off >= n.on));
    rig.live.heard_s = at + 15.0;
    let j: serde_json::Value = serde_json::from_str(&map_json_of(&rig.live, map_id(7, s.n, s.version)).unwrap()).unwrap();
    let wire = j["notes"].as_array().unwrap();
    assert!(!wire.is_empty() && wire.iter().all(|w| w[3].as_f64().unwrap() >= at + 5.0), "from ten seconds before the place heard");
    assert!(wire.iter().any(|w| (w[2].as_f64().unwrap() - 20.0).abs() < 0.06 && w[0].as_u64() == Some(bass as u64)), "in the session's seconds");
}

/// While their source has too few notes to be split into its instruments,
/// the guitars' notes go to the guitars' places as the bass's and the voice's
/// go to theirs: the left one's A4 to the guitar on the left, the right one's
/// E5 (from 10 s) to the one on the right once it is found; each flashes on
/// its strong notes.
#[test]
fn the_guitars_notes_go_to_their_places_by_pan() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    // (never split: the notes stay on the places, as on a stream not split at all)
    rig.live.split = false;
    rig.run(N_S, 0.0);
    let s = rig.live.songs.back().unwrap();
    let gl = s.map.objects.iter().position(|o| o.name.starts_with("guitar") && o.x < -0.3).expect("a guitar on the left");
    let gr = s.map.objects.iter().position(|o| o.name.starts_with("guitar") && o.x > 0.3).expect("a guitar on the right");
    let at = s.f0 as f64 / FPS;
    let strong = |o: usize| s.map.notes.iter().filter(|n| n.obj as usize == o && !n.ghost).collect::<Vec<_>>();
    let (l, r) = (strong(gl), strong(gr));
    let keys = |v: &[&crate::spatial::map::Note]| v.iter().map(|n| (n.on as f64 + at, n.key)).collect::<Vec<_>>();
    assert!(l.iter().filter(|n| n.key.round() == 69.0).count() >= 20, "the left guitar's: {:?}", keys(&l));
    assert!(r.len() >= 5 && r.iter().all(|n| n.key.round() == 76.0 && n.on as f64 + at > 9.9), "the right guitar's: {:?}", keys(&r));
    assert!(!s.map.notes.iter().any(|n| n.key.round() == 69.0 && n.obj as usize == gr), "an A4 on the right");
    for o in [gl, gr] {
        let h = &s.map.objects[o].hits;
        assert!(h.windows(2).all(|w| w[1] - w[0] >= 0.06), "{h:?}");
        assert!(strong(o).iter().all(|n| h.iter().any(|x| (x - n.on).abs() <= 0.031)), "flashes on its notes (a chord once): {h:?}");
    }
}

/// The stream as a scene eight seconds behind it gets it, up to second `to`.
fn listen(rig: &mut Rig, from: f64, to: f64) {
    let mut t = from;
    while t <= to {
        let heard = (t - 8.0).max(0.0);
        rig.run(t, heard);
        // a scene asks (from then on the slots only grow)
        rig.ask(heard);
        t += 0.5;
    }
}

/// Once the guitars' source has notes enough it is split into its
/// instruments as a file's (as many as the file's split of these notes
/// finds): every one of them a guitar's alone — the left one's A4 on the left,
/// the right one's E5 on the right —, the places they had taken over (the left
/// one's the slot the guitar had before: nothing moves), each with its own
/// notes from there on, flashing on them and sounding where they sound.
#[test]
fn a_sources_notes_split_it_into_its_instruments_on_their_places() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    listen(&mut rig, 9.0, 12.0);
    let before = names(rig.live.songs.back().unwrap());
    let gs = slot(rig.live.songs.back().unwrap(), "guitar");
    listen(&mut rig, 12.5, N_S);
    let s = rig.live.songs.back().unwrap();
    let at = s.f0 as f64 / FPS;
    let ns = names(s);
    assert_eq!(&ns[..before.len()], &before[..], "the slots it had keep their places: {ns:?}");
    let figs: Vec<usize> = (0..ns.len()).filter(|&i| s.map.objects[i].kind == crate::spatial::K_GUITAR && s.is_inst(i)).collect();
    assert!(s.is_inst(gs) && s.map.objects[gs].x < -0.3, "the guitar's slot is the left one's: {ns:?}");
    // their notes from the split on: each one guitar's (the strong ones), both guitars there
    let late = |o: usize| s.map.notes.iter().filter(move |n| n.obj as usize == o && !n.ghost && n.on as f64 + at > 22.0).collect::<Vec<_>>();
    let side = |o: usize| if s.map.objects[o].x < 0.0 { 69.0 } else { 76.0 };
    for &o in &figs {
        assert!(late(o).iter().all(|n| n.key.round() == side(o)), "{} at {:+.2}: {:?}", ns[o], s.map.objects[o].x, late(o).iter().map(|n| n.key).collect::<Vec<_>>());
    }
    let l = figs.iter().copied().filter(|&o| side(o) == 69.0).max_by_key(|&o| late(o).len()).expect("one on the left");
    let r = figs.iter().copied().filter(|&o| side(o) == 76.0).max_by_key(|&o| late(o).len()).expect("one on the right");
    let (ln, rn) = (late(l), late(r));
    let total = |k: f32| figs.iter().filter(|&&o| side(o) == k).map(|&o| late(o).len()).sum::<usize>();
    assert!(total(69.0) >= 5 && total(76.0) >= 5 && ln.len() >= 2 && rn.len() >= 2, "{} and {} notes", total(69.0), total(76.0));
    for (o, notes) in [(l, &ln), (r, &rn)] {
        let ob = &s.map.objects[o];
        assert!(notes.iter().all(|n| ob.hits.iter().any(|h| (h - n.on).abs() <= 0.031)), "it flashes on its notes");
        // it sounds where its notes sound (inside what is written)
        let sounding = notes.iter().filter(|n| ((n.on as f64 + 0.05) * FPS) < ob.energy.len() as f64).filter(|n| ob.energy[((n.on as f64 + 0.05) * FPS) as usize] > 0).count();
        assert!(sounding * 10 >= notes.len() * 8, "{sounding} of {} notes sounding", notes.len());
    }
}

/// A new song starts its split afresh: its guitars are places again with
/// their notes on them until it has notes enough of its own.
#[test]
fn a_new_song_is_split_afresh() {
    let mut rig = Rig::with(40.0, true);
    rig.notes = Some(Box::new(Hum));
    listen(&mut rig, 9.0, 30.0);
    let s = rig.live.songs.back().unwrap();
    assert!((0..s.map.objects.len()).any(|i| s.is_inst(i)), "split by 30 s: {:?}", names(s));
    rig.told.push((32.0 * SR as f64) as i64, SR as u32);
    listen(&mut rig, 30.5, 40.0);
    let s = rig.live.songs.back().unwrap();
    assert!(rig.live.songs.len() >= 2, "a new song");
    assert!(!(0..s.map.objects.len()).any(|i| s.is_inst(i)), "the new song's guitars are places: {:?}", names(s));
}

/// Stage 3 against a file (TASK-27, the master's measure): real songs'
/// separated sources (the research's npz, `AURA_STEMS_CACHE`\<name>.npz, the
/// names in `AURA_STREAM_SONGS`) streamed to a scene eight seconds behind,
/// the pack's note network on the processor (`AURA_SPATIAL_PACK_DIR`), against
/// a file's analysis of the same sources. Per song and note instruments'
/// source: the instruments (stream / file), when each was born, the splits
/// and those that merged, the slots that moved (none wanted), how far an
/// instrument's place moved while it settled and how far from the file's
/// nearest, the share of the stream's notes on the instrument the file's same
/// note is on (the best pairing); a round's time (the network, then the rest);
/// a split over the song's notes made ten minutes long (the worst case).
/// cargo test --profile fast --bins stage_three_against_a_file -- --ignored --nocapture
#[test]
#[ignore]
fn stage_three_against_a_file() {
    use crate::spatial::notes::{bp, eval, objects};
    use std::collections::HashMap;
    let (Some(cache), Some(pack)) = (std::env::var_os("AURA_STEMS_CACHE"), std::env::var_os("AURA_SPATIAL_PACK_DIR")) else { return };
    let pack = std::path::PathBuf::from(pack);
    let model = std::env::var_os("AURA_NOTES_MODEL").map(std::path::PathBuf::from).unwrap_or_else(|| pack.join(crate::spatial::pack::PITCH));
    let songs = std::env::var("AURA_STREAM_SONGS").unwrap_or_else(|_| "stems/backinblack,stems/jam,stems/oceanwind,stems/laflaca".into());
    for name in songs.split(',') {
        let (len, data) = eval::npz_stems(&std::path::Path::new(&cache).join(format!("{name}.npz")));
        let st: Vec<Vec<f32>> = (0..SOURCES * 2).map(|c| data[c * len..(c + 1) * len].to_vec()).collect();
        drop(data);
        let mix: Vec<Vec<f32>> = (0..2).map(|ch| (0..len).map(|i| (0..SOURCES).map(|s| st[s * 2 + ch][i]).sum::<f32>()).collect()).collect();
        let stems = crate::spatial::stems::Stems { src: std::array::from_fn(|s| [&st[s * 2][..], &st[s * 2 + 1][..]]), mix: [&mix[0][..], &mix[1][..]] };
        let file = objects::analyse(&mut bp::Model::open(&pack, &model).unwrap(), &stems).unwrap();
        drop(mix);
        let secs = len as f64 / SR as f64;
        let mut rig = Rig::of(st, Box::new(bp::Model::open_threads(&pack, &model, 1).unwrap()));
        // (song, id) → its slot, its source and the second heard at its birth, its places while it settled
        let mut slot_of: HashMap<(u32, u32), usize> = HashMap::new();
        let mut born: HashMap<(u32, u32), (usize, f64)> = HashMap::new();
        let mut xs: HashMap<(u32, u32), Vec<f32>> = HashMap::new();
        let mut moved = 0;
        let mut t = 9.0;
        while t <= secs {
            let heard = t - 8.0;
            rig.run(t, heard);
            rig.ask(heard);
            for s in rig.live.songs.iter() {
                for (slot, id, p) in s.inst_view().0 {
                    if slot_of.insert((s.n, id), slot).is_some_and(|was| was != slot) {
                        moved += 1;
                    }
                    let b = *born.entry((s.n, id)).or_insert((p, heard));
                    if heard - b.1 <= 10.0 {
                        xs.entry((s.n, id)).or_default().push(s.map.objects[slot].x);
                    }
                }
            }
            t += 0.5;
        }
        let ms = |v: Vec<f64>| (v.iter().sum::<f64>() / v.len().max(1) as f64, v.iter().cloned().fold(0.0, f64::max));
        let (net, rest) = (ms(rig.times.iter().map(|x| x.0).collect()), ms(rig.times.iter().map(|x| x.1).collect()));
        eprintln!(
            "== {name}: {secs:.0} s, {} song(s); rounds {} — the network {:.0} ms (most {:.0}), the rest {:.0} ms (most {:.0}); slots moved {moved}",
            rig.live.songs.len(), rig.times.len(), net.0, net.1, rest.0, rest.1
        );
        for p in notes::INST..notes::SOURCES.len() {
            let stem = notes::SOURCES[p];
            let fobj: Vec<&objects::NoteObj> = file.objects.iter().filter(|o| o.stem as usize == stem).collect();
            let mut line = format!("  {:6} file {} [{}]", crate::spatial::place::nice(stem), fobj.len(), fobj.iter().map(|o| format!("{:+.2}", o.x)).collect::<Vec<_>>().join(" "));
            for s in rig.live.songs.iter() {
                let (figs, splits) = s.inst_view();
                let mine: Vec<(usize, u32)> = figs.iter().filter(|f| f.2 == p).map(|f| (f.0, f.1)).collect();
                let (on, n_split, merges) = splits[p - notes::INST];
                let births: Vec<String> = mine.iter().map(|(_, id)| format!("{:.0}", born[&(s.n, *id)].1 - s.f0 as f64 / FPS)).collect();
                let jitter = mine.iter().map(|(_, id)| xs.get(&(s.n, *id)).map_or(0.0, |v| v.iter().cloned().fold(f32::MIN, f32::max) - v.iter().cloned().fold(f32::MAX, f32::min))).fold(0.0, f32::max);
                let dx = mine.iter().map(|(sl, _)| fobj.iter().map(|o| (o.x - s.map.objects[*sl].x).abs()).fold(f32::MAX, f32::min)).fold(0.0, f32::max);
                // the stream's settled strong notes against the file's same notes
                let at = s.f0 as f64 / FPS;
                let mut pairs: HashMap<(u32, usize), usize> = HashMap::new();
                let mut paired = 0;
                for q in rig.live.placed[p].iter().filter(|q| !q.n.ghost && q.like.as_ref().is_some_and(|l| l.row.is_some())) {
                    let Some((_, id)) = q.to.filter(|t| t.0 == s.n && mine.iter().any(|m| m.1 == t.1)) else { continue };
                    let hit = fobj.iter().enumerate().find(|(_, o)| o.notes.iter().any(|m| !m.ghost && m.key.round() == q.n.key.round() && (m.on as f64 - q.n.on).abs() <= 0.05));
                    if let Some((j, _)) = hit {
                        *pairs.entry((id, j)).or_default() += 1;
                        paired += 1;
                    }
                }
                let mut v: Vec<((u32, usize), usize)> = pairs.into_iter().collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                let (mut used_f, mut used_j, mut agree) = (Vec::new(), Vec::new(), 0);
                for ((id, j), c) in v {
                    if !used_f.contains(&id) && !used_j.contains(&j) {
                        used_f.push(id);
                        used_j.push(j);
                        agree += c;
                    }
                }
                line += &format!(
                    " | song {} from {at:.0} s: stream {} born at [{}] s, split {} ({} splits, {} merged), x moved ≤ {jitter:.2} settling, ≤ {dx:.2} from the file's; notes paired {paired}, on the same {:.0} %",
                    s.n, mine.len(), births.join(" "), on, n_split, merges, if paired > 0 { 100.0 * agree as f64 / paired as f64 } else { f64::NAN }
                );
            }
            eprintln!("{line}");
        }
        // the worst case: the song's notes of each note source made ten minutes long, split once
        let reps = (600.0 / secs).ceil() as usize;
        let mut owned: Vec<(usize, Vec<notes::LNote>, Vec<crate::spatial::notes::stem::PRow>, Vec<(u64, f64, Option<u64>)>)> = Vec::new();
        for p in notes::INST..notes::SOURCES.len() {
            let src: Vec<&notes::Placed> = rig.live.placed[p].iter().filter(|q| q.like.as_ref().is_some_and(|l| l.row.is_some())).collect();
            let (mut ln, mut rows, mut ids) = (Vec::new(), Vec::new(), Vec::new());
            for k in 0..reps {
                let dt = k as f64 * secs;
                for q in &src {
                    let l = q.like.as_ref().unwrap();
                    ln.push(notes::LNote { on: q.n.on + dt, off: q.n.off + dt, ..q.n });
                    rows.push(l.row.unwrap());
                    ids.push((l.id + k as u64 * 10_000_000, l.lvl, l.parent.map(|x| x + k as u64 * 10_000_000)));
                }
            }
            owned.push((p, ln, rows, ids));
        }
        let t0 = std::time::Instant::now();
        let mut found = Vec::new();
        for (p, ln, rows, ids) in &owned {
            let ins: Vec<inst::In> = ln.iter().zip(rows).zip(ids).map(|((n, row), &(id, lvl, parent))| inst::In { id, n, row, lvl, parent }).collect();
            found.extend(inst::split(notes::SOURCES[*p], &ins, 0.0));
        }
        let kept = objects::absorb(found).len();
        eprintln!(
            "  worst case: {} + {} + {} notes ({:.0} s of song) split in {:.0} ms ({kept} instruments)",
            owned[0].1.len(), owned[1].1.len(), owned[2].1.len(), reps as f64 * secs, t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// The stream eight seconds ahead of what is heard: every note of the bass
/// that starts within the next 1.9 s is known by then, however far the
/// separation's segment has come.
#[test]
fn a_notes_start_is_known_before_it_is_heard() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    let mut t = 9.0;
    while t <= N_S {
        let heard = t - 8.0;
        rig.run(t, heard);
        let s = rig.live.songs.back().unwrap();
        let at = s.f0 as f64 / FPS;
        let bass = slot(s, "bass");
        for k in (heard.floor() as usize + 1)..=((heard + 1.9).floor() as usize) {
            assert!(
                s.map.notes.iter().any(|n| n.obj as usize == bass && (n.on as f64 + at - k as f64).abs() < 0.06),
                "at {t:.2} s in (heard {heard:.2} s) the bass's note at {k} s is not known"
            );
        }
        t += 0.25;
    }
}

/// On the same sound the stream's notes and flashes are a file's: the bass's
/// settled notes are the ones a file's analysis finds in the whole bass (the
/// same network, decoding and measuring) — start, end and key —, and its place
/// flashes where a file's place of that bass does (`map::place_hits` over the
/// onsets of the whole bass's sound and those notes).
#[test]
fn the_streams_notes_and_flashes_are_a_files_on_the_same_sound() {
    use crate::spatial::notes::{bp, stem};
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    rig.run(N_S, 0.0);
    let k = stems::BASS;
    let (l, r) = (&rig.st[k * 2][..], &rig.st[k * 2 + 1][..]);
    let src: [[&[f32]; 2]; 6] = std::array::from_fn(|s| [&rig.st[s * 2][..], &rig.st[s * 2 + 1][..]]);
    let tr = bp::transcribe(bp::post_of(&mut Hum, &bp::to22(l, r)).unwrap(), &bp::Params::default());
    let sn = stem::notes_of(tr, l, r, stem::mix_ref(&src));
    let inside = |on: f64, off: f64| on > 0.5 && off < 20.0;
    let file: Vec<(f64, f64, f32)> = sn.notes.iter().filter(|n| !n.weak && inside(n.on, n.off)).map(|n| (n.on, n.off, n.pitch as f32)).collect();
    let stream: Vec<(f64, f64, f32)> = rig.live.placed[0].iter().map(|q| (q.n.on, q.n.off, q.n.key)).filter(|n| inside(n.0, n.1)).collect();
    let same = |a: &(f64, f64, f32), b: &(f64, f64, f32)| (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6 && a.2 == b.2;
    assert!(file.len() >= 15, "{file:?}");
    assert!(file.iter().all(|f| stream.iter().any(|s| same(f, s))), "the file's {file:?}, the stream's {stream:?}");
    assert_eq!(file.len(), stream.len(), "the stream's {stream:?}");
    // the flashes: a file's place of the whole bass, its notes on it
    use crate::spatial::map::{self, MapObj, Note};
    let st = stems::Stems { src, mix: [&rig.l[..], &rig.r[..]] };
    let places = crate::spatial::place::place_objects(l, r, k, st.mix_db());
    assert_eq!(places.len(), 1);
    let on_it: Vec<Note> = sn
        .notes
        .iter()
        .filter(|n| !n.weak)
        .map(|n| Note { obj: 0, key: n.pitch as f32, on: n.on as f32, off: n.off as f32, vel: 0.0, ghost: n.ghost.is_some() })
        .collect();
    let mut objs = vec![MapObj { kind: crate::spatial::K_BASS, hits: places[0].onsets.clone(), ..Default::default() }];
    map::place_hits(&mut objs, &on_it, &|_, o| o.hits.clone());
    let s = rig.live.songs.back().unwrap();
    let at = s.f0 as f64 / FPS;
    let within = |t: f64| t > 0.5 && t < 20.0;
    let want: Vec<f64> = objs[0].hits.iter().map(|h| *h as f64).filter(|t| within(*t)).collect();
    let got: Vec<f64> = s.map.objects[slot(s, "bass")].hits.iter().map(|h| *h as f64 + at).filter(|t| within(*t)).collect();
    assert!(want.len() >= 15, "{want:?}");
    assert_eq!(want.len(), got.len(), "the file's {want:?}, the stream's {got:?}");
    assert!(want.iter().zip(&got).all(|(a, b)| (a - b).abs() < 1e-4), "the file's {want:?}, the stream's {got:?}");
}

/// The note network coming in in the middle of a song (its session opened
/// late): the bass flashes on the onsets of its sound until its first note,
/// then on its notes — never twice within 60 ms.
#[test]
fn notes_coming_in_take_over_the_flashes_without_a_double() {
    let mut rig = Rig::with(40.0, true);
    rig.run(26.0, 0.0);
    let s = rig.live.songs.back().unwrap();
    let before = s.map.objects[slot(s, "bass")].hits.clone();
    assert!(before.len() > 5, "the bass's own onsets: {before:?}");
    rig.notes = Some(Box::new(Hum));
    rig.run(40.0, 0.0);
    let s = rig.live.songs.back().unwrap();
    let b = slot(s, "bass");
    let hits = &s.map.objects[b].hits;
    let notes: Vec<f32> = s.map.notes.iter().filter(|n| n.obj as usize == b && !n.ghost).map(|n| n.on).collect();
    let first = notes.iter().cloned().fold(f32::INFINITY, f32::min);
    assert!(first > 5.0 && first < 16.0, "the notes from where the sources are still held: {first}");
    assert!(hits.windows(2).all(|w| w[1] - w[0] >= 0.06), "a flash twice: {hits:?}");
    assert!(hits.iter().filter(|h| **h < first - 0.06).all(|h| before.contains(h)), "{hits:?} vs {before:?}");
    assert!(hits.iter().filter(|h| **h < first).count() >= 3, "{hits:?}");
    assert!(hits.iter().filter(|h| **h >= first).all(|h| notes.contains(h)), "{hits:?}");
    assert!(hits.iter().filter(|h| **h >= first).count() >= 15, "{hits:?}");
}

/// A note of the stream's own that has started where it is heard and does
/// not come again with the next segment stays, ending where it is heard (its
/// flash dies away, it is not cut off); one still ahead goes.
#[test]
fn a_started_note_gone_from_the_next_segment_dies_away() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    rig.run(16.0, 0.0);
    let heard = 12.0;
    let started = LNote { on: heard - 0.05, off: heard + 1.0, key: 50.0, lvl: -15.0, pan: 0.0, ghost: false };
    let ahead = LNote { on: heard + 1.0, ..started };
    rig.live.own[0].extend([started, ahead]);
    rig.run(20.0, heard);
    let kept: Vec<&LNote> = rig.live.placed[0].iter().map(|q| &q.n).filter(|n| n.key == 50.0).collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!((kept[0].on - started.on).abs() < 1e-9 && (kept[0].off - (started.on + 0.12)).abs() < 1e-9, "{kept:?}");
    assert!(!rig.live.own[0].iter().any(|n| n.key == 50.0));
    let s = rig.live.songs.back().unwrap();
    let bass = slot(s, "bass");
    assert!(s.map.notes.iter().any(|n| n.key == 50.0 && n.obj as usize == bass));
}

/// A song's start the stream tells of cuts the notes there: the song before
/// keeps the ones starting before it, ending by it; the new song gets the
/// rest, in its own seconds.
#[test]
fn a_song_start_cuts_the_notes() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    rig.told.push((14.0 * SR as f64) as i64, SR as u32);
    rig.run(N_S, 0.0);
    assert_eq!(rig.live.songs.len(), 2);
    let (a, b) = (&rig.live.songs[0], &rig.live.songs[1]);
    let end = (b.f0 - a.f0) as f64 / FPS;
    assert!(!a.map.notes.is_empty() && a.map.notes.iter().all(|n| (n.on as f64) < end && n.off as f64 <= end + 1e-3));
    let b0 = b.f0 as f64 / FPS;
    assert!(b.map.notes.iter().all(|n| n.on >= 0.0));
    assert!(b.map.notes.iter().any(|n| (n.on as f64 + b0 - 15.0).abs() < 0.06), "the bass's note at 15 s in the new song");
}

/// What the drum network (stage 2) takes of the card beside the separation's
/// session — on a stream both stay open while it plays — and how long its
/// chunk and the separation's segment take side by side. The pack in
/// AURA_SPATIAL_PACK_DIR; the GPU only (nothing here runs on the CPU):
///   cargo test --profile fast --bins drum_network_beside_the_separation -- --ignored --nocapture
#[test]
#[ignore]
fn the_drum_network_beside_the_separation_on_the_card() {
    use crate::audio::gpu::dxgi_memory as dx;
    use crate::spatial::kit;
    let Some(pack) = std::env::var_os("AURA_SPATIAL_PACK_DIR").map(std::path::PathBuf::from) else { return };
    let card = || dx::largest_adapter().and_then(|(v, d)| dx::query(v, d).ok()).expect("the card's memory");
    let mb = |b: u64| b as f64 / (1u64 << 20) as f64;
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let m0 = card();
    eprintln!("before: this process {:.0} MB of the card, {:.0} MB free", mb(m0.usage), mb(m0.free()));
    let mut noise = 777u32;
    let mut rnd = move || {
        noise ^= noise << 13;
        noise ^= noise >> 17;
        noise ^= noise << 5;
        (noise as f32 / u32::MAX as f32 - 0.5) * 0.2
    };
    let l: Vec<f32> = (0..SEG).map(|_| rnd()).collect();
    let r: Vec<f32> = (0..SEG).map(|_| rnd()).collect();
    let mut sep = crate::spatial::core::Core::open_gpu(&pack, crate::spatial::core::GPU_NEEDS).expect("the separation on the GPU");
    for i in 0..3 {
        let t = Instant::now();
        sep.separate(&l, &r).unwrap();
        eprintln!("separation, segment {i}: {:.0} ms", ms(t));
    }
    let m1 = card();
    eprintln!("the separation's session: {:.0} MB ({:.0} MB free)", mb(m1.usage.saturating_sub(m0.usage)), mb(m1.free()));
    let t = Instant::now();
    let mut drums = kit::Kit::open(&pack, &pack.join(crate::spatial::pack::DRUMS), false).expect("the drum network on the GPU");
    eprintln!("the drum network opened in {:.0} ms (GPU {})", ms(t), drums.gpu);
    let mut worst = 0f64;
    for i in 0..8 {
        let t = Instant::now();
        drums.chunk(&l[..kit::CHUNK], &r[..kit::CHUNK]).unwrap();
        let took = ms(t);
        if i > 0 {
            worst = worst.max(took);
        }
        eprintln!("drum network, chunk {i}: {took:.0} ms");
    }
    let m2 = card();
    eprintln!(
        "the drum network's session beside it: {:.0} MB more ({:.0} MB free); a chunk at worst {worst:.0} ms after the first",
        mb(m2.usage.saturating_sub(m1.usage)),
        mb(m2.free())
    );
    let t = Instant::now();
    sep.separate(&l, &r).unwrap();
    eprintln!("a separation segment beside it: {:.0} ms", ms(t));
    let m3 = card();
    eprintln!("both sessions: {:.0} MB of the card", mb(m3.usage.saturating_sub(m0.usage)));
    drop(drums);
    let m4 = card();
    eprintln!("the drum network closed: {:.0} MB given back", mb(m3.usage.saturating_sub(m4.usage)));
}

/// The stream eight seconds ahead of what is heard, the scene asking for the
/// objects every 150 ms as the page does: once the instruments are there for
/// the place heard they stay — the place heard does not fall back to the mix
/// and the map's id does not go to none (the page would drop the map and say
/// the instruments are on their way again) from one ask to the next.
#[test]
fn once_there_the_instruments_stay_from_ask_to_ask() {
    let mut rig = Rig::new();
    rig.notes = Some(Box::new(Hum));
    let mut seen = Vec::new();
    let mut t = 9.0;
    while t <= N_S {
        let heard = t - 8.0;
        rig.run(t, heard);
        let h = head(&rig.ask(heard));
        seen.push((t, h[7], h[12], h[3]));
        t += 0.15;
    }
    let first = seen.iter().position(|x| x.1 == 2.0).expect("the instruments come");
    let flips: Vec<_> = seen[first..].iter().filter(|x| x.1 != 2.0 || x.2 == 0.0).collect();
    assert!(flips.is_empty(), "from {:.2} s: {} of {} asks without the instruments: {:?}", seen[first].0, flips.len(), seen.len() - first, &flips[..flips.len().min(12)]);
}
