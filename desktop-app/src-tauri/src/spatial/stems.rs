//! A track's six separated sources, whole, as the instruments analysis
//! takes them: the separation fills it, the kit, the places and the notes
//! read it. One gate says whether a source is there at all, so every part
//! of the analysis agrees on it.

/// The sources, in the separation network's order.
pub const DRUMS: usize = 0;
pub const BASS: usize = 1;
pub const OTHER: usize = 2;
pub const VOCALS: usize = 3;
pub const GUITAR: usize = 4;
pub const PIANO: usize = 5;

/// The whole track at 44.1 kHz: each source's left and right, and the mix.
pub struct Stems<'a> {
    /// `[source][channel]`, sources in `DRUMS` … `PIANO` order.
    pub src: [[&'a [f32]; 2]; 6],
    pub mix: [&'a [f32]; 2],
}

/// Samples between frames: 86.13 frames a second at 44.1 kHz.
pub const FRAME_HOP: usize = 512;
/// The window a frame's level is measured over.
const FRAME_WIN: usize = 2048;
/// A source is there when its loud level is at most this far under the
/// mix's (dB).
pub const PRESENT_DB: f64 = -30.0;

/// Mean power per frame (dB) of both channels: frame j is the 2048 samples
/// centred on sample 512·j, cut at the ends but always divided by 2048;
/// `ceil(len / 512)` frames (the research's `kit.frame_db`).
pub fn frame_db(l: &[f32], r: &[f32]) -> Vec<f64> {
    let n = l.len().min(r.len());
    let mut c = Vec::with_capacity(n + 1);
    c.push(0f64);
    let mut s = 0f64;
    for i in 0..n {
        let (a, b) = (l[i] as f64, r[i] as f64);
        s += (a * a + b * b) / 2.0;
        c.push(s);
    }
    (0..n.div_ceil(FRAME_HOP))
        .map(|j| {
            let a = (j * FRAME_HOP).saturating_sub(FRAME_WIN / 2).min(n);
            let b = (j * FRAME_HOP + FRAME_WIN / 2).min(n);
            10.0 * ((c[b] - c[a]) / FRAME_WIN as f64 + 1e-20).log10()
        })
        .collect()
}

/// numpy's `percentile` (linear between the closest ranks), q in 0…100.
pub fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let pos = q / 100.0 * (s.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    s[lo] + (s[hi] - s[lo]) * (pos - lo as f64)
}

/// The loud level of a stereo signal: p99 of its frame levels (dB).
pub fn loud_db(l: &[f32], r: &[f32]) -> f64 {
    percentile(&frame_db(l, r), 99.0)
}

impl Stems<'_> {
    /// The mix's loud level (dB): what source levels are measured against.
    pub fn mix_db(&self) -> f64 {
        loud_db(self.mix[0], self.mix[1])
    }

    /// Whether source `k` is there at all: its loud level at most 30 dB
    /// under the mix's (`mix_db` from `Stems::mix_db`). An empty source's
    /// leftovers of the others stay under that.
    pub fn present(&self, k: usize, mix_db: f64) -> bool {
        loud_db(self.src[k][0], self.src[k][1]) - mix_db >= PRESENT_DB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_scale_sine_is_minus_three_db() {
        // 20 whole periods in a window
        let w = 2.0 * std::f64::consts::PI * 20.0 / 2048.0;
        let x: Vec<f32> = (0..44_100).map(|i| (i as f64 * w).sin() as f32).collect();
        let db = frame_db(&x, &x);
        assert_eq!(db.len(), 44_100usize.div_ceil(512));
        // a frame inside the signal: the mean power of a sine is 1/2
        assert!((db[40] + 3.0103).abs() < 0.01, "{}", db[40]);
        // the first frame is half a window of signal, divided by the whole window
        assert!((db[0] - db[40] + 3.0103).abs() < 0.01);
    }

    /// Frame levels = the research's `kit.frame_db` on real stems
    /// (`AURA_NOTES_REFS`, the notes module's dumps).
    #[test]
    #[ignore]
    fn frame_levels_are_the_researchs() {
        let Some(d) = std::env::var_os("AURA_NOTES_REFS").map(std::path::PathBuf::from) else { return };
        for case in ["ow_other", "vakh_guitar"] {
            let x = crate::spatial::notes::bp::npy::f32s(&d.join(format!("{case}.x44.npy")));
            let (_, want) = crate::spatial::notes::bp::npy::read(&d.join(format!("{case}.frame_db.npy")));
            let n = x.len() / 2;
            let got = frame_db(&x[..n], &x[n..]);
            assert_eq!(got.len(), want.len());
            let err = got.iter().zip(&want).fold(0f64, |m, (a, b)| m.max((a - b).abs()));
            assert!(err < 1e-9, "{case}: {err:e} dB");
        }
    }

    #[test]
    fn percentiles_interpolate_as_numpy() {
        let v = [4.0, 1.0, 3.0, 2.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 100.0), 4.0);
        assert!((percentile(&v, 50.0) - 2.5).abs() < 1e-12);
        assert!((percentile(&v, 99.0) - 3.97).abs() < 1e-12);
    }

    #[test]
    fn a_source_30_db_under_the_mix_is_still_there() {
        let mix: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.05).sin()).collect();
        let quiet: Vec<f32> = mix.iter().map(|v| v * 10f32.powf(-29.0 / 20.0)).collect();
        let gone: Vec<f32> = mix.iter().map(|v| v * 10f32.powf(-31.0 / 20.0)).collect();
        let none: &[f32] = &[];
        let s = Stems {
            src: [[&quiet, &quiet], [&gone, &gone], [&mix, &mix], [none, none], [none, none], [none, none]],
            mix: [&mix, &mix],
        };
        let m = s.mix_db();
        assert!(s.present(0, m) && !s.present(1, m) && s.present(2, m) && !s.present(3, m));
    }
}
