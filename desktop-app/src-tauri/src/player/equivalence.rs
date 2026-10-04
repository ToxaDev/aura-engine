//! The player's streaming stages against the converter's whole-file code,
//! compared bit for bit from several start positions.

use super::blend::{self, Envelope, HybridStage};
use super::stages::{Stage, XtcStage};

/// A stage over a buffer held in memory, starting at `pos`.
struct BufStage {
    l: Vec<f64>,
    r: Vec<f64>,
    pos: u64,
}

impl Stage for BufStage {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let mut inside = 0;
        for i in 0..out_l.len() {
            let p = self.pos as usize + i;
            if p < self.l.len() {
                out_l[i] = self.l[p];
                out_r[i] = self.r[p];
                inside += 1;
            } else {
                out_l[i] = 0.0;
                out_r[i] = 0.0;
            }
        }
        self.pos += out_l.len() as u64;
        inside
    }
    fn position(&self) -> u64 {
        self.pos
    }
    fn total(&self) -> u64 {
        self.l.len() as u64
    }
}

fn noise(n: usize, seed: u64, amp: f64) -> Vec<f64> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    (0..n).map(|_| rng.gen_range(-amp..amp)).collect()
}

fn read_all(st: &mut dyn Stage, n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut l = vec![0.0; n];
    let mut r = vec![0.0; n];
    let mut off = 0;
    let mut step = 5_003;
    while off < n {
        let e = (off + step).min(n);
        st.read(&mut l[off..e], &mut r[off..e]);
        off = e;
        step = step * 5 / 4 + 1;
    }
    (l, r)
}

#[test]
fn xtc_stage_matches_apply_xtc() {
    let n = 120_000;
    let m = 2_001;
    let x_l = noise(n, 11, 0.5);
    let x_r = noise(n, 12, 0.5);
    let hd: Vec<f64> = noise(m, 13, 0.01);
    let hc: Vec<f64> = noise(m, 14, 0.01);
    let mut ref_l = x_l.clone();
    let mut ref_r = x_r.clone();
    crate::audio::converter::dsp::lab::xtc::apply_xtc(&mut ref_l, &mut ref_r, &hd, &hc);

    for &start in &[0u64, 4_321, 8_192 * 3 + 17, (n - 3_000) as u64] {
        let up_at = start - XtcStage::history(m, start);
        let up = Box::new(BufStage { l: x_l.clone(), r: x_r.clone(), pos: up_at });
        let mut st = XtcStage::new(up, &hd, &hc, start);
        let want = n - start as usize;
        let (gl, gr) = read_all(&mut st, want);
        for i in 0..want {
            let k = start as usize + i;
            assert!(
                gl[i].to_bits() == ref_l[k].to_bits() && gr[i].to_bits() == ref_r[k].to_bits(),
                "XTC start {} idx {}: {} {} vs {} {}",
                start, i, gl[i], gr[i], ref_l[k], ref_r[k]
            );
        }
    }
}

/// The same samples as a stream without an end, as a live source reports
/// itself (`total` u64::MAX), silence after them.
struct Endless(BufStage);

impl Stage for Endless {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        self.0.read(out_l, out_r);
        out_l.len()
    }
    fn position(&self) -> u64 {
        self.0.position()
    }
    fn total(&self) -> u64 {
        u64::MAX
    }
}

/// XTC on a live stream — a source without an end — gives the samples it
/// gives on the track, the converter's `apply_xtc`, to the bit. It used to add
/// the filter's length to that end, wrap around and keep only the first taps:
/// a radio went silent with XTC on.
#[test]
fn xtc_stage_on_a_stream_without_an_end_is_the_tracks() {
    let n = 120_000;
    let m = 2_001;
    let x_l = noise(n, 11, 0.5);
    let x_r = noise(n, 12, 0.5);
    let hd: Vec<f64> = noise(m, 13, 0.01);
    let hc: Vec<f64> = noise(m, 14, 0.01);
    let mut ref_l = x_l.clone();
    let mut ref_r = x_r.clone();
    crate::audio::converter::dsp::lab::xtc::apply_xtc(&mut ref_l, &mut ref_r, &hd, &hc);

    for &start in &[0u64, 8_192 * 3 + 17] {
        let up_at = start - XtcStage::history(m, start);
        let up = Box::new(Endless(BufStage { l: x_l.clone(), r: x_r.clone(), pos: up_at }));
        let mut st = XtcStage::new(up, &hd, &hc, start);
        assert_eq!(st.total(), u64::MAX, "a stream has no end");
        let want = n - start as usize;
        let (gl, gr) = read_all(&mut st, want);
        let k = |i: usize| start as usize + i;
        let bad = (0..want)
            .filter(|&i| gl[i].to_bits() != ref_l[k(i)].to_bits() || gr[i].to_bits() != ref_r[k(i)].to_bits())
            .count();
        assert_eq!(bad, 0, "XTC on a stream from {}: {} samples differ from the track's", start, bad);
        assert!(gl[want - 1_000..].iter().any(|v| v.abs() > 1e-3), "XTC on a stream went silent");
    }
}

#[test]
fn hybrid_stage_matches_batch_blend() {
    use crate::audio::hybrid_phase::{blend_outputs_stereo_inplace, catmull_env_at, BlendEnvelope};
    let sr = 352_800.0;
    let n = 352_800 * 3;
    // Two different but related signals, so the mid difference has
    // zero-crossings for the snap to find.
    let base_l: Vec<f64> = (0..n).map(|i| (i as f64 * 0.0021).sin() * 0.4).collect();
    let base_r: Vec<f64> = (0..n).map(|i| (i as f64 * 0.0017).cos() * 0.4).collect();
    let d = noise(n, 21, 0.05);
    let lin_l = base_l.clone();
    let lin_r = base_r.clone();
    let min_l: Vec<f64> = base_l.iter().zip(&d).map(|(a, b)| a + b).collect();
    let min_r: Vec<f64> = base_r.iter().zip(&d).map(|(a, b)| a - b).collect();

    // Analysis envelope at ~86 Hz with onsets every ~0.23 s.
    let asr = 86.13;
    let frames = (n as f64 / sr * asr) as usize + 4;
    let analysis: Vec<f64> = (0..frames).map(|f| if f % 20 < 3 { 0.9 } else { 0.05 }).collect();
    let fto = sr / asr;
    let env_full = BlendEnvelope {
        envelope: (0..n).map(|i| catmull_env_at(&analysis, fto, i)).collect(),
        analysis_envelope: analysis.clone(),
        analysis_sr: asr,
    };
    let mut ref_l = lin_l.clone();
    let mut ref_r = lin_r.clone();
    blend_outputs_stereo_inplace(&mut ref_l, &min_l, &mut ref_r, &min_r, &env_full, 0, sr);

    let env = Envelope { analysis, analysis_sr: asr };
    let plan = blend::scan_plan(&env, sr, n);
    assert!(plan.boundaries.len() > 10, "test needs switches: {}", plan.boundaries.len());
    for &start in &[0u64, 100_000, 352_800, 700_001] {
        let s0 = start.saturating_sub(blend::lead_in(sr));
        let a = Box::new(BufStage { l: lin_l.clone(), r: lin_r.clone(), pos: s0 });
        let b = Box::new(BufStage { l: min_l.clone(), r: min_r.clone(), pos: s0 });
        let mut st = HybridStage::new(a, b, &plan, sr, start);
        let want = n - start as usize;
        let (gl, gr) = read_all(&mut st, want);
        let mut bad = 0;
        for i in 0..want {
            let k = start as usize + i;
            if gl[i].to_bits() != ref_l[k].to_bits() || gr[i].to_bits() != ref_r[k].to_bits() {
                bad += 1;
                if bad < 5 {
                    eprintln!("HP start {} idx {}: {} vs {}", start, k, gl[i], ref_l[k]);
                }
            }
        }
        assert_eq!(bad, 0, "Hybrid-Phase from {}: {} samples differ", start, bad);
    }
}

/// The subsonic guard on the stream (`FirStage`) is the converter's
/// (`apply_subsonic_guard`): to the bit when its first block is the track's
/// (a start within half the filter of the beginning), and to within 1e-13
/// from further in, where its partitions fall on a grid of their own.
#[test]
fn fir_stage_matches_apply_subsonic_guard() {
    use super::stages::FirStage;
    use crate::audio::converter::apodize::{apply_subsonic_guard, design_subsonic_highpass};
    use std::sync::atomic::AtomicBool;

    let sr = 88_200u32;
    let n = 3 * sr as usize + 4_321;
    let (l, r) = (noise(n, 21, 0.5), noise(n, 22, 0.5));
    let (mut ref_l, mut ref_r) = (l.clone(), r.clone());
    apply_subsonic_guard(&mut ref_l, &mut ref_r, sr, 20, &AtomicBool::new(false)).unwrap();
    let h = design_subsonic_highpass(sr, 20);
    let d = FirStage::history(h.len());
    for &start in &[0u64, 1, 12_345, d, d + 1, 3 * 8_192 + d, 150_001] {
        let up = Box::new(BufStage { l: l.clone(), r: r.clone(), pos: start.saturating_sub(d) });
        let mut st = FirStage::new(up, &h, sr, start);
        let want = n - start as usize;
        let (gl, gr) = read_all(&mut st, want);
        let (mut bad, mut worst) = (0, 0.0f64);
        for i in 0..want {
            let k = start as usize + i;
            if gl[i].to_bits() != ref_l[k].to_bits() || gr[i].to_bits() != ref_r[k].to_bits() {
                bad += 1;
            }
            worst = worst.max((gl[i] - ref_l[k]).abs()).max((gr[i] - ref_r[k]).abs());
        }
        if start <= d {
            assert_eq!(bad, 0, "guard from {}: {} samples differ (worst {:e})", start, bad, worst);
        } else {
            assert!(worst <= 1e-13, "guard from {}: {:e} from the converter's", start, worst);
        }
    }
}
