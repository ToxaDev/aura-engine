//! The drum network on a stream = a file's (`KitRun`) on the same drums.

use super::super::super::kit::{KitRun, OVERLAP};
use super::*;

const SR: f64 = 44_100.0;

/// A stand-in network: each piece a share of the chunk, turned by where in
/// the chunk a sample is (a stream's chunks must be cut where a file's are
/// for the two to agree).
fn fake(l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
    let mut y = vec![0f32; NP * 2 * CHUNK];
    for p in 0..NP {
        for (ch, x) in [l, r].into_iter().enumerate() {
            let o = &mut y[(p * 2 + ch) * CHUNK..(p * 2 + ch + 1) * CHUNK];
            for k in 0..CHUNK {
                o[k] = x[k] * (p + 1) as f32 / NP as f32 * (1.0 + 0.2 * ((k as f32) * 0.000_37 + p as f32).sin());
            }
        }
    }
    Ok(y)
}

/// Drums: a kick every half second, a snare between, hats every eighth, a
/// crash every four seconds (the right a little louder), noise for the rest.
fn drums(n: usize) -> (Vec<f32>, Vec<f32>) {
    let mut s = 99u32;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s as f32 / u32::MAX as f32 * 2.0 - 1.0
    };
    let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
    for i in 0..n {
        let t = i as f64 / SR;
        let (kick, snare, hat, crash) = (t % 0.5, (t + 0.25) % 1.0, t % 0.125, t % 4.0);
        let mut d = 0f64;
        if kick < 0.08 {
            d += (std::f64::consts::TAU * 60.0 * kick).sin() * (-kick / 0.03).exp() * 0.6;
        }
        if snare < 0.06 {
            d += rnd() as f64 * (-snare / 0.02).exp() * 0.3;
        }
        if hat < 0.02 {
            d += (rnd() - rnd()) as f64 * (-hat / 0.005).exp() * 0.08;
        }
        let c = if crash < 1.5 { rnd() as f64 * (-crash / 0.4).exp() * 0.1 } else { 0.0 };
        l[i] = (d + c) as f32;
        r[i] = (d + 1.3 * c) as f32;
    }
    (l, r)
}

/// The stream's drums pushed in blocks of any length, the chunks run as they
/// are due: the pieces' sound where it is whole, measured frame by frame, is
/// a file's (`KitRun` and its pieces' levels), and the onset function and
/// the coherence with it.
#[test]
fn the_streams_chunks_and_frames_are_a_files() {
    assert_eq!(OVERLAP, 2);
    let n = CHUNK * 4 + 54_321;
    let (l, r) = drums(n);
    // the file's
    let mut net = |a: &[f32], b: &[f32]| fake(a, b);
    let mut run = KitRun::new(n, OVERLAP);
    run.push(&mut net, &l, &r).unwrap();
    let file = run.finish(&mut net, -10.0).unwrap();
    // the stream's: the drums, then silence after them (a file's last chunks hear zeros past its end)
    let mut st = DrumRun::new(0);
    let silence = || std::iter::repeat(0f32).take(2 * CHUNK);
    let (l2, r2): (Vec<f32>, Vec<f32>) = (l.iter().cloned().chain(silence()).collect(), r.iter().cloned().chain(silence()).collect());
    let mut at = 0;
    let mut k = 1usize;
    while at < l2.len() {
        let e = (at + k * 7919 % 90_001 + 1).min(l2.len());
        st.push(&l2[at..e], &r2[at..e]);
        at = e;
        k += 1;
        while let Some((i, a, b)) = st.pick(0, 0) {
            st.add(i, &fake(&a, &b).unwrap()).unwrap();
        }
    }
    assert_eq!(st.skipped, 0);
    assert!(st.prov() > st.fin() && st.covered(0, st.prov()));
    // the file's frames: levels and fields to n / 512, the onset function to n / 256
    let nf = n / HOP + 1;
    assert!(st.fin() >= (nf + 1) * HOP + 1024, "whole to {} of {n}", st.fin());
    let mut feats = DrumFeats::new(60.0);
    feats.make(&st, 0, nf + 1);
    for (p, pc) in file.pieces.iter().enumerate() {
        let (mut worst_db, mut worst_coh) = (0f32, 0f32);
        for j in 0..pc.db.len() {
            let f = feats.get(j).unwrap();
            assert!(f.ok);
            worst_db = worst_db.max((f.db[p] - pc.db[j]).abs());
        }
        for j in 0..pc.coh.len() {
            worst_coh = worst_coh.max((feats.get(j).unwrap().coh[p] - pc.coh[j]).abs());
        }
        assert!(worst_db < 1e-4, "{}: levels {worst_db} dB off", pc.name);
        assert!(worst_coh < 1e-5, "{}: coherence {worst_coh} off", pc.name);
        // the hits: the stream's onset function and candidates at the file's own percentiles are the file's
        let m = n / kit::ON_HOP + 1;
        let flux: Vec<f32> = (0..=m / 2).flat_map(|j| feats.get(j).unwrap().flux[p]).take(m).collect();
        let pow: Vec<f32> = (0..=m / 2).flat_map(|j| feats.get(j).unwrap().pow[p]).take(m).collect();
        let top = kit::percentile32(&flux, 99.5) + 1e-9;
        let cand = candidates(&flux, &pow, top, kit::DETECT[p].0);
        let lv: Vec<f32> = cand.iter().map(|c| c.1).collect();
        let loud = kit::percentile32(&lv, 95.0);
        let got: Vec<f32> = cand
            .iter()
            .filter(|c| c.1 >= loud - kit::DETECT[p].1)
            .map(|c| ((c.0 * kit::ON_HOP) as f64 / SR) as f32)
            .collect();
        assert!(pc.onsets.len() > 10, "{}: {:?}", pc.name, pc.onsets);
        assert_eq!(got, pc.onsets, "{}", pc.name);
    }
}

/// The last chunk's own half is there before the next chunk makes it whole;
/// its frames made again then are what one run over whole sound gives.
#[test]
fn a_chunks_own_half_is_made_whole_by_the_next() {
    let n = CHUNK * 3;
    let (l, r) = drums(n);
    let mut st = DrumRun::new(0);
    st.push(&l[..CHUNK + CHUNK / 2], &r[..CHUNK + CHUNK / 2]);
    while let Some((i, a, b)) = st.pick(0, 0) {
        st.add(i, &fake(&a, &b).unwrap()).unwrap();
    }
    let (fin1, prov1) = (st.fin(), st.prov());
    assert!(prov1 > fin1);
    let mut feats = DrumFeats::new(60.0);
    let (e_fin, e_prov) = ((fin1 - 1024) / HOP, (prov1 - 1024) / HOP);
    feats.make(&st, 0, e_fin);
    feats.make(&st, e_fin, e_prov);
    let early = feats.get(e_prov - 3).unwrap().db;
    st.push(&l[CHUNK + CHUNK / 2..], &r[CHUNK + CHUNK / 2..]);
    while let Some((i, a, b)) = st.pick(0, 0) {
        st.add(i, &fake(&a, &b).unwrap()).unwrap();
    }
    assert!(st.fin() >= prov1, "what was the chunk's own is whole now");
    feats.make(&st, e_fin, e_prov);
    let mut one = DrumFeats::new(60.0);
    one.make(&st, 0, e_prov);
    for j in e_fin..e_prov {
        let (a, b) = (feats.get(j).unwrap(), one.get(j).unwrap());
        for p in 0..NP {
            assert_eq!(a.db[p], b.db[p], "frame {j} piece {p}");
            assert_eq!(a.flux[p], b.flux[p], "frame {j} piece {p}");
            assert!((a.coh[p] - b.coh[p]).abs() < 1e-3, "frame {j} piece {p}: {} vs {}", a.coh[p], b.coh[p]);
        }
    }
    // the half that was the chunk's own changed when the next one came
    assert_ne!(feats.get(e_prov - 3).unwrap().db, early);
}

/// Behind: a chunk whose sound would all be heard before it is ready is not
/// run — the grid starts afresh ahead of what is heard; the samples between
/// have no pieces' sound.
#[test]
fn behind_the_place_heard_the_grid_starts_afresh() {
    let n = CHUNK * 4;
    let (l, r) = drums(n);
    let mut st = DrumRun::new(0);
    st.push(&l, &r);
    let heard = CHUNK * 2;
    let (i, a, b) = st.pick(heard, 1000).unwrap();
    assert_eq!(i, 0, "the new grid's first chunk");
    assert_eq!(st.skipped, 1);
    st.add(i, &fake(&a, &b).unwrap()).unwrap();
    assert!(!st.covered(0, 1000) && !st.covered(CHUNK, CHUNK + 10), "nothing where the grid did not run");
    assert!(st.covered(heard + 1000, st.prov()));
    assert!(st.fin() >= heard + 1000);
    // a chunk of the grid before, coming late, is let go
    let before = st.prov();
    st.add(7, &fake(&a, &b).unwrap()).unwrap();
    assert_eq!(st.prov(), before);
}
