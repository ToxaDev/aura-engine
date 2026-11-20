//! Percentiles of what a song has played so far: a level's every frame
//! counted in tenths of a dB — a file's analysis sorts the whole track, a
//! stream's grows frame by frame.

/// Levels, dB, −160 … +80 in tenths.
#[derive(Clone)]
pub struct Pct {
    bins: Vec<u32>,
}

const LO: f32 = -160.0;
const STEP: f32 = 0.1;
const NBINS: usize = 2400;

impl Default for Pct {
    fn default() -> Pct {
        Pct { bins: vec![0; NBINS] }
    }
}

impl Pct {
    pub fn push(&mut self, db: f32) {
        if !db.is_finite() {
            return;
        }
        let i = (((db - LO) / STEP).floor().max(0.0) as usize).min(NBINS - 1);
        self.bins[i] += 1;
    }

    /// Of the levels over `floor`, the one a share `q` (0…100) of them is
    /// under — as a file's `v[len · q / 100]` of the sorted levels; None when
    /// none is over the floor.
    pub fn at(&self, q: f64, floor: f32) -> Option<f32> {
        let first = ((((floor - LO) / STEP).floor().max(-1.0) + 1.0) as usize).min(NBINS);
        let total: u64 = self.bins[first..].iter().map(|&c| c as u64).sum();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * q / 100.0) as u64).min(total - 1);
        let mut seen = 0u64;
        for (i, &c) in self.bins.iter().enumerate().skip(first) {
            seen += c as u64;
            if seen > rank {
                return Some(LO + (i as f32 + 0.5) * STEP);
            }
        }
        None
    }

    /// The percentile of every level (no floor).
    pub fn all(&self, q: f64) -> Option<f32> {
        self.at(q, LO - 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_a_growing_song_are_its_sorted_levels_within_a_tenth() {
        let mut p = Pct::default();
        assert_eq!(p.all(99.0), None);
        let v: Vec<f32> = (0..1000).map(|i| -80.0 + (i as f32 * 0.07) % 70.0).collect();
        v.iter().for_each(|x| p.push(*x));
        let mut s = v.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        for q in [50.0, 90.0, 99.0] {
            let want = s[(s.len() as f64 * q / 100.0) as usize];
            assert!((p.all(q).unwrap() - want).abs() <= 0.1, "p{q}: {} vs {want}", p.all(q).unwrap());
        }
        // over a floor: only the levels above it count
        let above: Vec<f32> = s.iter().cloned().filter(|x| *x > -30.0).collect();
        let want = above[above.len() * 9 / 10];
        assert!((p.at(90.0, -30.0).unwrap() - want).abs() <= 0.15);
        assert_eq!(p.at(50.0, 100.0), None);
    }
}
