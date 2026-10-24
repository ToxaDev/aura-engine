//! Hybrid-Phase and alpha-HP on the multistage upsampler: each half (the linear blob and its minimum-phase twin)
//! runs as its own two-stage path — stage 0 on branch 0 at the source rate with its own plan, then the short chain —
//! and the two are blended at the output rate exactly as today. Each half keeps today's own trim (linear (N − 1)/2,
//! minimum phase the band-weighted delay), so both land on today's output frames: the blend sees what it sees today
//! to within the short chain's stopband, and the player's blend is the converter's bit for bit.

use std::f64::consts::PI;
use std::sync::Arc;

use super::blend::{scan_plan, AlphaStage, Envelope, HybridStage, PlanFeed};
use super::convolver::{Alignment, Bank, Input, PolyStream, SourceBuf, BLOCK};
use super::ms_player::{assert_bits, blob_linear, multi, noise, noise_filter, peak_db, read_odd, stage0_bank};
use super::stages::Stage;

/// Tone bursts with sharp attacks at random moments over a quiet bed: the onsets give the envelope its switches.
fn music(n: usize, rate: u32, seed: u64) -> (Vec<f64>, Vec<f64>) {
    let mut s = seed | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut l: Vec<f64> = (0..n).map(|_| 0.01 * (rnd() - 0.5)).collect();
    let mut r = l.clone();
    let mut at = 0usize;
    while at < n {
        at += ((0.08 + 0.35 * rnd()) * rate as f64) as usize;
        let f = 100.0 + 3_000.0 * rnd() * rnd();
        let amp = 0.1 + 0.4 * rnd();
        for k in 0..(0.3 * rate as f64) as usize {
            if at + k >= n {
                break;
            }
            let t = k as f64 / rate as f64;
            let v = amp * (-t * 12.0).exp() * (2.0 * PI * f * t).sin() + amp * 0.4 * (-t * 80.0).exp() * (rnd() - 0.5);
            l[at + k] += v;
            r[at + k] += 0.6 * v;
        }
    }
    (l, r)
}

/// An analysis envelope at ~86 Hz with an onset every ~0.23 s (the converter's `hybrid_stage_matches_batch_blend`
/// kind), long enough for `out_len` output frames at `out_rate`.
fn onsets(out_len: usize, out_rate: f64) -> Envelope {
    let asr = 86.13;
    let frames = (out_len as f64 / out_rate * asr) as usize + 4;
    Envelope { analysis: (0..frames).map(|f| if f % 20 < 3 { 0.9 } else { 0.05 }).collect(), analysis_sr: asr }
}

/// The whole-track envelope through the sidecar, as the player gets it (the live plan's HPSS on the same audio).
fn envelope_of(src: &SourceBuf, tag: &str) -> Envelope {
    let dir = std::env::temp_dir().join(format!("aura-ms-hp-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.flac");
    crate::audio::hpss_native::generate_and_save(&path, &src.l, &src.r, src.rate, &std::sync::atomic::AtomicBool::new(false))
        .unwrap();
    let (analysis, analysis_sr) = crate::audio::hybrid_phase::load_analysis_envelope(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    Envelope { analysis, analysis_sr }
}

/// Today's pair at ×`l`: the linear blob and its minimum-phase twin, as single-stage banks lined up as the player
/// lines them up (linear (N − 1)/2, minimum the band-weighted delay), and as stage-0 banks with the same trims.
fn pair(l: usize) -> ((Arc<Bank>, Arc<Bank>), (Arc<Bank>, Arc<Bank>)) {
    let out_rate = 44_100 * l as u32;
    let lin = blob_linear(l);
    let min = crate::audio::dsp_core::to_minimum_phase(&lin);
    let s_lin = Arc::new(Bank::from_coeffs("lin", &lin, l, Alignment::Linear, out_rate));
    let s_min = Arc::new(Bank::from_coeffs("min", &min, l, Alignment::BandWeighted, out_rate));
    let m_lin = stage0_bank(&lin, l, s_lin.delay);
    let m_min = stage0_bank(&min, l, s_min.delay);
    ((s_lin, s_min), (m_lin, m_min))
}

/// Each half lands on today's output frames: an impulse comes out of the multistage half at the frame today's
/// single-stage half puts it (linear: exactly L·k), and the two differ by the short chain's stopband only — so the
/// two halves stand at the same output index for the same input sample, as today.
#[test]
fn the_halves_land_on_todays_output_frames() {
    for l in [2usize, 8] {
        let n_in = 6_000;
        let k = 2_345usize;
        let (mut x_l, mut x_r) = (vec![0.0; n_in], vec![0.0; n_in]);
        x_l[k] = 0.9;
        x_r[k] = -0.45;
        let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
        let total = n_in * l;
        let ((s_lin, s_min), (m_lin, m_min)) = pair(l);
        let argmax = |v: &[f64]| (0..v.len()).max_by(|&a, &b| v[a].abs().total_cmp(&v[b].abs())).unwrap();
        let mut peaks = Vec::new();
        for (name, single, ms) in [("linear", &s_lin, &m_lin), ("minimum", &s_min, &m_min)] {
            let (al, ar) = read_odd(&mut PolyStream::new(single.clone(), src.clone(), 0), total);
            let (bl, br) = read_odd(&mut multi(ms, Input::File(src.clone()), 0, false), total);
            let (pa, pb) = (argmax(&al), argmax(&bl));
            assert_eq!(pa, pb, "x{l} {name}: the impulse at {pb}, today at {pa}");
            assert_eq!(argmax(&ar), argmax(&br), "x{l} {name}: right channel");
            let db = peak_db(&al, &bl).max(peak_db(&ar, &br));
            println!("[ms-hp] impulse x{l} {name}: peak at {pb} (today {pa}), null {db:.1} dBFS");
            assert!(db <= -200.0, "x{l} {name}: {db:.1} dBFS");
            peaks.push(pb);
        }
        assert_eq!(peaks[0], k * l, "x{l}: the linear half's impulse sits on L·k");
    }
}

/// Hybrid-Phase and alpha-HP on the multistage halves against today's single-stage halves, the same plan and
/// envelope, from the track start and from a seek: within the short chain's stopband (the blend weights are ≤ 1).
#[test]
fn multi_hybrid_and_alpha_null_against_single_below_200_db() {
    for l in [2usize, 8] {
        let out = 44_100.0 * l as f64;
        let n_in = 3 * 44_100;
        let (x_l, x_r) = music(n_in, 44_100, 0x51ab_c0de_1234_5678);
        let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
        let total = n_in * l;
        let env = onsets(total, out);
        let plan = scan_plan(&env, out, total);
        assert!(plan.boundaries.len() > 10, "x{l}: the plan switches: {}", plan.boundaries.len());
        let ((s_lin, s_min), (m_lin, m_min)) = pair(l);
        let single = |b: &Arc<Bank>, at: u64| -> Box<dyn Stage> { Box::new(PolyStream::new(b.clone(), src.clone(), at)) };
        let ms = |b: &Arc<Bank>, at: u64| -> Box<dyn Stage> { Box::new(multi(b, Input::File(src.clone()), at, false)) };
        for at in [0u64, total as u64 / 3 + 5] {
            let s0 = at.saturating_sub(super::blend::lead_in(out));
            let want = total - at as usize;
            let (al, ar) = read_odd(&mut HybridStage::new(single(&s_lin, s0), single(&s_min, s0), &plan, out, at), want);
            let (bl, br) = read_odd(&mut HybridStage::new(ms(&m_lin, s0), ms(&m_min, s0), &plan, out, at), want);
            let db = peak_db(&al, &bl).max(peak_db(&ar, &br));
            println!("[ms-hp] null x{l} Hybrid-Phase start {at}: {db:.1} dBFS");
            assert!(db <= -200.0, "x{l} Hybrid-Phase start {at}: {db:.1} dBFS");
            let (al, ar) = read_odd(&mut AlphaStage::new(single(&s_lin, at), single(&s_min, at), &env, out), want);
            let (bl, br) = read_odd(&mut AlphaStage::new(ms(&m_lin, at), ms(&m_min, at), &env, out), want);
            let db = peak_db(&al, &bl).max(peak_db(&ar, &br));
            println!("[ms-hp] null x{l} alpha-HP start {at}: {db:.1} dBFS");
            assert!(db <= -200.0, "x{l} alpha-HP start {at}: {db:.1} dBFS");
        }
    }
}

/// The player's multistage Hybrid-Phase and alpha-HP against the converter's (`ms_route::render_in_ram` for each
/// half — the linear half as `prepare` reads its blob, the minimum-phase half as `prepare_min` does, branch 0 and the
/// band-weighted trim — then `blend_outputs_stereo_inplace` / `_continuous`): bit for bit from five starts, on the
/// shipped chains, ×4 and ×8, branch 0 of the linear blob several blocks long.
#[test]
fn player_multi_hybrid_is_the_converters_bit_for_bit() {
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    use crate::audio::converter::pipeline::ms_route::{render_in_ram, MsStage0, SYMMETRY_TAPS};
    use crate::audio::converter::state::FileConvState;
    use crate::audio::hybrid_phase::{
        blend_outputs_stereo_continuous, blend_outputs_stereo_inplace, catmull_env_at, BlendEnvelope,
    };
    let dir = std::env::temp_dir().join(format!("aura-ms-hp-cross-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let n_in = 2 * BLOCK + 3_333;
    let (x_l, x_r) = music(n_in, 44_100, 0x0dd_ba11_5eed);
    let src = Arc::new(SourceBuf { l: x_l.clone(), r: x_r.clone(), rate: 44_100 });
    for (l, lin) in [(8usize, noise_filter(2 * BLOCK * 8 + 4_001, 93, true)), (4, blob_linear(4))] {
        let out_rate = 44_100 * l as u32;
        let out = out_rate as f64;
        let min = crate::audio::dsp_core::to_minimum_phase(&blob_linear(l));
        let chain = multistage_chain(UpsamplerMode::Multi, 30_000_000, 44_100, l).expect("a shipped chain");
        let save = |name: &str, h: &[f64]| {
            let path = dir.join(format!("{name}-x{l}.npy"));
            crate::audio::dsp_core::save_npy_f64(&path, h).unwrap();
            path.to_string_lossy().to_string()
        };
        let (p_lin, p_min) = (save("lin", &lin), save("min", &min));
        // The converter.
        let b = crate::audio::dsp_core::load_npy_f64_branch(&p_lin, l, 0).unwrap();
        let tail = lin[lin.len() - SYMMETRY_TAPS.min(lin.len() / 2)..].to_vec();
        let c_lin = MsStage0::new(b, &tail, None, l, chain, None, false);
        let b = crate::audio::dsp_core::load_npy_f64_branch(&p_min, l, 0).unwrap();
        let d_min = b.band_weighted_delay(out_rate);
        let c_min = MsStage0::with_d(b, d_min, l, chain, None, false);
        let fs = Arc::new(FileConvState::new());
        let total = n_in * l;
        let (cl, cr) = render_in_ram(&c_lin, &x_l, &x_r, n_in, total, false, 64, "lin", &fs, 0, 600).unwrap();
        let (ml, mr) = render_in_ram(&c_min, &x_l, &x_r, n_in, total, false, 64, "min", &fs, 620, 140).unwrap();
        let env = onsets(total, out);
        let blend_env = BlendEnvelope {
            envelope: (0..total).map(|i| catmull_env_at(&env.analysis, out / env.analysis_sr, i)).collect(),
            analysis_envelope: env.analysis.clone(),
            analysis_sr: env.analysis_sr,
        };
        let (mut hl, mut hr) = (cl.clone(), cr.clone());
        blend_outputs_stereo_inplace(&mut hl, &ml, &mut hr, &mr, &blend_env, 0, out);
        let (mut al, mut ar) = (cl, cr);
        blend_outputs_stereo_continuous(&mut al, &ml, &mut ar, &mr, &blend_env, 0, out);
        // The player, as `phase_stage` loads the pair (`pair_stage0`: `Bank::load_stage0`).
        let lin_bank = Arc::new(Bank::load_stage0(&p_lin, l, Alignment::Linear, out_rate, chain).unwrap());
        let min_bank = Arc::new(Bank::load_stage0(&p_min, l, Alignment::BandWeighted, out_rate, chain).unwrap());
        let (pl, pm) = (lin_bank.ms.clone().unwrap(), min_bank.ms.clone().unwrap());
        assert_eq!((pl.d, pl.k0), (c_lin.plan.d, c_lin.plan.k0), "x{l}: the linear half's plan");
        assert_eq!((pm.d, pm.k0, pm.c), (c_min.plan.d, c_min.plan.k0, c_min.plan.c), "x{l}: the minimum half's plan");
        let plan = scan_plan(&env, out, total);
        assert!(plan.boundaries.len() > 3, "x{l}: the plan switches: {}", plan.boundaries.len());
        let half = |b: &Arc<Bank>, at: u64| -> Box<dyn Stage> { Box::new(multi(b, Input::File(src.clone()), at, false)) };
        for s in [0usize, 9_999, BLOCK * l + 1, total / 2 + 3, total - 3_000] {
            let at = s as u64;
            let s0 = at.saturating_sub(super::blend::lead_in(out));
            let mut hp = HybridStage::new(half(&lin_bank, s0), half(&min_bank, s0), &plan, out, at);
            let (gl, gr) = read_odd(&mut hp, total - s);
            assert_bits(&format!("Hybrid-Phase x{l} start {s}"), (&gl, &gr), (&hl[s..], &hr[s..]));
            let mut ahp = AlphaStage::new(half(&lin_bank, at), half(&min_bank, at), &env, out);
            let (gl, gr) = read_odd(&mut ahp, total - s);
            assert_bits(&format!("alpha-HP x{l} start {s}"), (&gl, &gr), (&al[s..], &ar[s..]));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Multistage Hybrid-Phase as the player plays it otherwise: from a seek it plays what it plays from the start
/// (seek == play), with the plan made as it plays (`PlanFeed`, the live plan) it plays the whole track's plan, and on
/// a file whose source stages are still running (`GrowSource`) it plays the whole file — all bit for bit. The
/// halves share their input spectra (`SpectrumShare`) as `phase_stage` builds them.
#[test]
fn multi_hybrid_seek_live_plan_and_growing_file_play_the_whole_bit_for_bit() {
    use super::convolver::SpectrumShare;
    let l = 2usize;
    let rate = 44_100u32;
    let out = (rate as usize * l) as f64;
    let n_in = 8 * rate as usize;
    let (x_l, x_r) = music(n_in, rate, 0x7e57_ab1e);
    let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate });
    let total = n_in * l;
    let env = envelope_of(&src, "seek");
    let plan = scan_plan(&env, out, total);
    assert!(plan.boundaries.len() > 10, "the plan switches: {}", plan.boundaries.len());
    let (_, (m_lin, m_min)) = pair(l);
    let pair_of = |input: Input, at: u64, shared: bool| -> (Box<dyn Stage>, Box<dyn Stage>) {
        let share = shared.then(SpectrumShare::new);
        let up = |b: &Arc<Bank>, share: Option<Arc<SpectrumShare>>| -> Box<dyn Stage> {
            let p = b.ms.clone().unwrap();
            let s0 = super::stages::InterpStage::stage0_start(&p, at);
            let poly = PolyStream::with_input_shared(b.clone(), input.clone(), s0, share);
            Box::new(super::stages::InterpStage::new(Box::new(poly), &p, at, input.total(l)))
        };
        (up(&m_lin, share.clone()), up(&m_min, share))
    };
    // The whole track from its first sample, unshared halves.
    let (a, b) = pair_of(Input::File(src.clone()), 0, false);
    let (wl, wr) = read_odd(&mut HybridStage::new(a, b, &plan, out, 0), total);
    for at in [0u64, 3 * out as u64 + 777] {
        let s = at as usize;
        let s0 = at.saturating_sub(super::blend::lead_in(out));
        let want = total - s;
        // Seek: the whole plan, shared spectra.
        let (a, b) = pair_of(Input::File(src.clone()), s0, true);
        let (gl, gr) = read_odd(&mut HybridStage::new(a, b, &plan, out, at), want);
        assert_bits(&format!("seek {at}"), (&gl, &gr), (&wl[s..], &wr[s..]));
        // The live plan.
        let (a, b) = pair_of(Input::File(src.clone()), s0, true);
        let feed = PlanFeed::new(Input::File(src.clone()), rate, out, s0 as usize);
        let (gl, gr) = read_odd(&mut HybridStage::live(a, b, feed, out, at), want);
        assert_bits(&format!("live plan {at}"), (&gl, &gr), (&wl[s..], &wr[s..]));
        // A growing file.
        let grow = Arc::new(crate::player::grow::GrowSource::new(rate, n_in));
        let (w, s2) = (grow.clone(), src.clone());
        let producer = std::thread::spawn(move || {
            let mut k = 0;
            while k < s2.len() {
                let e = (k + 20_000).min(s2.len());
                w.push(&s2.l[k..e], &s2.r[k..e]);
                k = e;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            w.finish();
        });
        let (a, b) = pair_of(Input::Grow(grow), s0, true);
        let (gl, gr) = read_odd(&mut HybridStage::new(a, b, &plan, out, at), want);
        producer.join().unwrap();
        assert_bits(&format!("growing file {at}"), (&gl, &gr), (&wl[s..], &wr[s..]));
    }
}

/// `#[ignore]`: the real 30M ×8 44.1 kHz pair (`AURA_FILTER_DIR`) — Hybrid-Phase and alpha-HP in Multi against
/// Single, 1 s of noise from a seek 0.5 s in, the shipped ×8 chain.
#[test]
#[ignore]
fn multi_hybrid_against_single_on_the_30m_blobs() {
    use crate::audio::converter::dsp::multistage::{multistage_chain, UpsamplerMode};
    let Ok(dir) = std::env::var("AURA_FILTER_DIR") else {
        println!("[ms-hp] AURA_FILTER_DIR not set: skipped");
        return;
    };
    let dir = dir.trim_end_matches(['/', '\\']).to_string();
    let (l, out_rate) = (8usize, 352_800u32);
    let out = out_rate as f64;
    let chain = multistage_chain(UpsamplerMode::Multi, 30_000_000, 44_100, l).expect("the shipped x8 chain");
    let p_lin = format!("{dir}/fir_30M_352800_linear_phase.npy");
    let p_min = format!("{dir}/fir_30M_352800_minimum_phase.npy");
    let t = std::time::Instant::now();
    let s_lin = Arc::new(Bank::load(&p_lin, l, Alignment::Linear, out_rate).unwrap());
    let s_min = Arc::new(Bank::load(&p_min, l, Alignment::BandWeighted, out_rate).unwrap());
    let m_lin = Arc::new(Bank::load_stage0(&p_lin, l, Alignment::Linear, out_rate, chain).unwrap());
    let m_min = Arc::new(Bank::load_stage0(&p_min, l, Alignment::BandWeighted, out_rate, chain).unwrap());
    println!("[ms-hp] banks loaded in {:.1} s", t.elapsed().as_secs_f64());
    let (pl, pm) = (m_lin.ms.clone().unwrap(), m_min.ms.clone().unwrap());
    assert_eq!((pl.d, pm.d), (s_lin.delay, s_min.delay), "today's trims");
    println!("[ms-hp] linear d {} k0 {} c {} | minimum d {} k0 {} c {}", pl.d, pl.k0, pl.c, pm.d, pm.k0, pm.c);
    let n_in = 2 * 44_100;
    let (x_l, x_r): (Vec<f64>, Vec<f64>) =
        (noise(n_in, 101).iter().map(|v| v * 0.5).collect(), noise(n_in, 102).iter().map(|v| v * 0.5).collect());
    let src = Arc::new(SourceBuf { l: x_l, r: x_r, rate: 44_100 });
    let total = n_in * l;
    let env = onsets(total, out);
    let plan = scan_plan(&env, out, total);
    let at = (out / 2.0) as u64;
    let want = out as usize;
    let s0 = at.saturating_sub(super::blend::lead_in(out));
    let single = |b: &Arc<Bank>, at: u64| -> Box<dyn Stage> { Box::new(PolyStream::new(b.clone(), src.clone(), at)) };
    let ms = |b: &Arc<Bank>, at: u64| -> Box<dyn Stage> { Box::new(multi(b, Input::File(src.clone()), at, false)) };
    let (al, ar) = read_odd(&mut HybridStage::new(single(&s_lin, s0), single(&s_min, s0), &plan, out, at), want);
    let (bl, br) = read_odd(&mut HybridStage::new(ms(&m_lin, s0), ms(&m_min, s0), &plan, out, at), want);
    let hp = peak_db(&al, &bl).max(peak_db(&ar, &br));
    let (al, ar) = read_odd(&mut AlphaStage::new(single(&s_lin, at), single(&s_min, at), &env, out), want);
    let (bl, br) = read_odd(&mut AlphaStage::new(ms(&m_lin, at), ms(&m_min, at), &env, out), want);
    let ahp = peak_db(&al, &bl).max(peak_db(&ar, &br));
    println!("[ms-hp] 30M x8 44.1k null Multi vs Single: Hybrid-Phase {hp:.1} dBFS, alpha-HP {ahp:.1} dBFS");
    assert!(hp <= -200.0 && ahp <= -200.0, "Hybrid-Phase {hp:.1}, alpha-HP {ahp:.1} dBFS");
}
