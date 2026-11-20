//! The stand-in from the mix on a stream (AURA-VIS-SPEC-INSTRUMENTS.md F5):
//! a file's rough layer (`rough_layer`), made as the stream comes, song by
//! song — the song's frames from its first one, its loud level and its
//! bands' levels so far, the mix's parts followed frame by frame. The scenes
//! get it through the same `objects` a file's goes through.

use std::collections::HashMap;

use super::super::analysis::{self, BANDS, FULL_PARTS, HOP, NB, RAW, ROUGH_PARTS};
use super::super::parts::{Cell, Tracker};
use super::super::{Hist, TrackData, BAND_SLOTS, SLOTS, WIDE_N};
use super::pct::Pct;

const EPS: f32 = 1e-12;

/// One song's stand-in.
pub struct Rough {
    pub d: TrackData,
    tracker: Tracker<'static>,
    next_id: u32,
    /// Per frame: the mix's whole energy (dB, its STFT's — the loud level's
    /// scale) and its level (dB, mean power: what silence is told by).
    pub me: Vec<f32>,
    pub lv: Vec<f32>,
    ref_pct: Pct,
    band_pct: [Pct; BANDS],
    mix_pct: Pct,
}

/// A song's frames in, before the parts are followed over them.
pub struct Rows<'a> {
    pub band: &'a [f32],
    pub he: &'a [f32],
    pub hy: &'a [u16],
    pub hc: &'a [u16],
    pub me: &'a [f32],
    pub lv: &'a [f32],
}

impl Rough {
    pub fn new(path: String) -> Rough {
        let d = TrackData {
            path,
            len: 0,
            frames: 0,
            ref_db: -40.0,
            rough_band: Vec::new(),
            full_band: Vec::new(),
            rough_h: Hist::new(0, 1),
            full_h: Hist::new(0, FULL_PARTS.len()),
            seg_done: Vec::new(),
            cells: Vec::new(),
            full_infos: HashMap::new(),
            rough_infos: HashMap::new(),
            band_p90_full: [-60.0; BANDS],
            band_p90_rough: [-60.0; BANDS],
            full_wide: Vec::new(),
            wide_p90: [-60.0; WIDE_N],
            gpu: None,
            error: None,
            lineup: None,
            map: None,
            progress: 0.0,
            job_done: false,
        };
        Rough {
            d,
            tracker: Tracker::new(&ROUGH_PARTS[0], -40.0, -60.0),
            next_id: 0,
            me: Vec::new(),
            lv: Vec::new(),
            ref_pct: Pct::default(),
            band_pct: Default::default(),
            mix_pct: Pct::default(),
        }
    }

    pub fn frames(&self) -> usize {
        self.d.frames
    }

    /// Frames in at the song's end: their raw values, then the levels known
    /// by now and the mix's parts followed over them.
    pub fn push(&mut self, rows: Rows) {
        let n = rows.me.len();
        let j0 = self.d.frames;
        self.d.rough_band.extend_from_slice(rows.band);
        self.d.rough_h.e.extend_from_slice(rows.he);
        self.d.rough_h.y.extend_from_slice(rows.hy);
        self.d.rough_h.c.extend_from_slice(rows.hc);
        self.d.cells.resize((j0 + n) * SLOTS, Cell::default());
        self.me.extend_from_slice(rows.me);
        self.lv.extend_from_slice(rows.lv);
        self.d.frames = j0 + n;
        self.d.len = self.d.frames * HOP;
        for j in j0..j0 + n {
            // as a file's loud level: every fourth frame's energy
            if j % 4 == 0 {
                self.ref_pct.push(self.me[j]);
            }
            for (b, _, _) in BAND_SLOTS {
                self.band_pct[b].push(10.0 * (self.d.rough_band[(j * BANDS + b) * RAW] + EPS).log10());
            }
            self.mix_pct.push(10.0 * (self.d.rough_h.e(j, 0).iter().sum::<f32>() + EPS).log10());
        }
        // the levels so far (a file's: over the whole track)
        let ref_db = self.ref_pct.all(99.0).unwrap_or(-40.0).max(-90.0);
        self.d.ref_db = ref_db;
        let p90 = |p: &Pct| p.at(90.0, ref_db - 90.0).map_or(-60.0, |v| v - ref_db);
        for (b, _, _) in BAND_SLOTS {
            self.d.band_p90_rough[b] = p90(&self.band_pct[b]);
        }
        self.tracker.set_levels(ref_db, p90(&self.mix_pct));
        for j in j0..j0 + n {
            let (h, c) = self.d.rough_h.ec(j, 0);
            self.tracker.step(j, &h, &c, SLOTS, &mut self.next_id, &mut self.d.cells, &mut self.d.rough_infos);
        }
    }

    /// The frames from song frame `at` on, as the start of a song of their
    /// own (a song's start found after they came); this one ends there.
    pub fn split_off(&mut self, at: usize, path: String) -> Rough {
        let at = at.min(self.d.frames);
        let mut tail = Rough::new(path);
        let br = BANDS * RAW;
        tail.push(Rows {
            band: &self.d.rough_band[at * br..],
            he: &self.d.rough_h.e[at * NB..],
            hy: &self.d.rough_h.y[at * NB..],
            hc: &self.d.rough_h.c[at * NB..],
            me: &self.me[at..],
            lv: &self.lv[at..],
        });
        self.truncate(at);
        tail
    }

    /// The song ends at frame `at` (its frames past it are another's).
    pub fn truncate(&mut self, at: usize) {
        if at >= self.d.frames {
            return;
        }
        self.d.rough_band.truncate(at * BANDS * RAW);
        self.d.rough_h.e.truncate(at * NB);
        self.d.rough_h.y.truncate(at * NB);
        self.d.rough_h.c.truncate(at * NB);
        self.d.cells.truncate(at * SLOTS);
        self.me.truncate(at);
        self.lv.truncate(at);
        self.d.frames = at;
        self.d.len = at * HOP;
        // a part alive past the end keeps no cell there; its life stays as it was
    }
}

/// The rows of `frames` (session frames) from the mix `l`, `r` whose first
/// sample is session sample `base` (f32: the analysis' boundary, §4.1).
pub fn rows_of(st: &analysis::Stft, l: &[f32], r: &[f32], base: isize, frames: std::ops::Range<usize>, mix: (&[f64], &[f64], usize)) -> RowsOwned {
    let n = frames.len();
    let reg = analysis::region(st, &[(l, r)], false, base, frames.clone());
    let mut bl = vec![rustfft::num_complex::Complex32::new(0.0, 0.0); analysis::NFFT];
    let me = frames.clone().map(|j| 10.0 * (analysis::mix_energy(st, l, r, (j * HOP) as isize - base, &mut bl) + EPS).log10()).collect();
    let lv = frames.clone().map(|j| level_at(mix.0, mix.1, mix.2, j)).collect();
    let mut hy = vec![0u16; n * NB];
    let mut hc = vec![0u16; n * NB];
    for i in 0..n * NB {
        hy[i] = (analysis::height(reg.hf[i]) * 65535.0).round() as u16;
        hc[i] = (reg.hc[i].clamp(0.0, 1.0) * 65535.0).round() as u16;
    }
    RowsOwned { band: reg.band, he: reg.he, hy, hc, me, lv }
}

pub struct RowsOwned {
    pub band: Vec<f32>,
    pub he: Vec<f32>,
    pub hy: Vec<u16>,
    pub hc: Vec<u16>,
    pub me: Vec<f32>,
    pub lv: Vec<f32>,
}

impl RowsOwned {
    pub fn rows(&self, from: usize, to: usize) -> Rows<'_> {
        let br = BANDS * RAW;
        Rows {
            band: &self.band[from * br..to * br],
            he: &self.he[from * NB..to * NB],
            hy: &self.hy[from * NB..to * NB],
            hc: &self.hc[from * NB..to * NB],
            me: &self.me[from..to],
            lv: &self.lv[from..to],
        }
    }
}

/// A frame's level: the mean power (dB) of both channels over the 2048
/// samples around it (`stems::frame_db`), from f64 signals whose sample 0 is
/// session sample `base`.
pub fn level_at(l: &[f64], r: &[f64], base: usize, j: usize) -> f32 {
    let c = (j * HOP) as isize - base as isize;
    let a = (c - 1024).max(0) as usize;
    let b = ((c + 1024).max(0) as usize).min(l.len());
    let mut s = 0f64;
    for i in a..b.max(a) {
        s += (l[i] * l[i] + r[i] * r[i]) / 2.0;
    }
    (10.0 * (s / 2048.0 + 1e-20).log10()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stand-in made as a stream comes, in pieces, is a file's rough
    /// layer of the same sound: the same raw values frame for frame, and the
    /// same objects once its levels have seen the song (within their tenth
    /// of a dB).
    #[test]
    fn the_stand_in_on_a_stream_is_a_files_rough_layer() {
        let n = 44_100 * 12;
        let (l, r) = super::super::sep::tests::signal(n);
        let file = super::super::super::rough_layer("x.flac", &l, &r);
        let (l64, r64): (Vec<f64>, Vec<f64>) = (l.iter().map(|v| *v as f64).collect(), r.iter().map(|v| *v as f64).collect());
        let st = analysis::Stft::new();
        let mut song = Rough::new("radio:1:x#0".into());
        let frames = (n - 1024) / HOP;
        let mut j = 0;
        let mut k = 1;
        while j < frames {
            let e = (j + 17 * k % 97 + 1).min(frames);
            let rows = rows_of(&st, &l, &r, 0, j..e, (&l64, &r64, 0));
            song.push(rows.rows(0, e - j));
            j = e;
            k += 1;
        }
        assert_eq!(song.frames(), frames);
        let br = BANDS * RAW;
        // the bands' energies as the file's; the room's (weighed by how unalike the channels are) settles
        // over the frames before each piece, as a file's chunks do: within a dB
        let e = |v: &[f32], j: usize, b: usize| v[(j * BANDS + b) * RAW];
        for b in [analysis::B_BASS, analysis::B_HATS] {
            assert!((0..frames).all(|j| (e(&song.d.rough_band, j, b) - e(&file.rough_band, j, b)).abs() <= 1e-4 * e(&file.rough_band, j, b).max(1e-9)));
        }
        let db = |x: f32| 10.0 * (x + 1e-12).log10();
        let close = (0..frames).filter(|&j| (db(e(&song.d.rough_band, j, analysis::B_AMB)) - db(e(&file.rough_band, j, analysis::B_AMB))).abs() < 1.0).count();
        assert!(close as f64 >= 0.98 * frames as f64, "the room's energy: {close} of {frames} frames within a dB");
        assert!((song.d.ref_db - file.ref_db).abs() < 0.2, "loud level {} vs {}", song.d.ref_db, file.ref_db);
        // the parts: the main one of the mix lives where the file's does
        let main_at = |cells: &[Cell], j: usize| cells[j * SLOTS + 25].id != 0;
        let both = (200..frames).filter(|&j| main_at(&song.d.cells, j) == main_at(&file.cells, j)).count();
        assert!(both as f64 > 0.95 * (frames - 200) as f64, "{both} of {}", frames - 200);
        // a song's start found later: the frames past it are a song of their own
        let row = song.d.rough_band[600 * br..601 * br].to_vec();
        let tail = song.split_off(600, "radio:1:x#1".into());
        assert_eq!((song.frames(), tail.frames()), (600, frames - 600));
        assert_eq!(&tail.d.rough_band[..br], &row[..]);
    }
}
