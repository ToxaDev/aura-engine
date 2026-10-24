//! A track played live against the same track converted, same rack. Heavy
//! (the full filter on the CPU, minutes); run by hand, one test at a time:
//!   AURA_FILTER_DIR=…/fir-optimizer/output [AURA_FVL_SETTINGS=<the rack as
//!   the page sends it>] cargo test --profile fast <test> -- --ignored --nocapture
//! - `live_chain_dump` (AURA_FVL_SRC, AURA_FVL_OUT): the live chain's whole
//!   output as interleaved f32 stereo at the chain's rate, no header.
//! - `converter_file_dump` (AURA_FVL_SRC, AURA_FVL_WORK, AURA_FVL_GPU=1 for
//!   the card): the converter's file, as the app's batch makes it, next to a
//!   copy of the source in the work folder.
//! - `level_parity_table` (AURA_FVL_LIST, paths separated by '|'): per track,
//!   the live gain before and after the output-peak probe against the scalar
//!   the converter's output stage takes on the finished render.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::chain::{build_chain, prepare_variant, Resources};
use super::settings::PlayerSettings;

fn rack() -> PlayerSettings {
    match std::env::var("AURA_FVL_SETTINGS") {
        Ok(j) => serde_json::from_str(&j).expect("AURA_FVL_SETTINGS must be the page's JSON"),
        Err(_) => PlayerSettings::default(),
    }
}

#[test]
#[ignore]
fn converter_file_dump() {
    let src = std::path::PathBuf::from(std::env::var("AURA_FVL_SRC").expect("AURA_FVL_SRC: the track"));
    let work = std::path::PathBuf::from(std::env::var("AURA_FVL_WORK").expect("AURA_FVL_WORK: a folder"));
    std::fs::create_dir_all(&work).unwrap();
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("flac");
    let copy = work.join(format!("src.{ext}"));
    std::fs::copy(&src, &copy).expect("copy the track");
    let mut cs = rack().to_engine();
    cs.use_gpu = std::env::var("AURA_FVL_GPU").map(|v| v == "1").unwrap_or(false);
    cs.precision = 64;
    let cancel = AtomicBool::new(false);
    let t0 = std::time::Instant::now();
    let prep = crate::audio::converter::pipeline::prepare::prepare_audio_phase(&copy, &mut cs, &cancel, None)
        .expect("prepare");
    let state = Arc::new(crate::audio::converter::state::FileConvState::new());
    let out = crate::audio::converter::process::process_one_prepared(&copy, prep, &cs, state).expect("convert");
    eprintln!("[FVL] converted in {:.1} s: {}", t0.elapsed().as_secs_f64(), out);
}

fn read_whole(chain: &mut super::chain::Chain, n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut l = vec![0.0f64; n];
    let mut r = vec![0.0f64; n];
    let mut done = 0;
    while done < n {
        let e = (done + (1 << 16)).min(n);
        let got = chain.stage.read(&mut l[done..e], &mut r[done..e]);
        if got == 0 { break; }
        done += got.min(e - done);
    }
    (l, r)
}

/// Per track (AURA_FVL_LIST, separated by '|'): the gain the live chain
/// plays at against the gain the converter's output stage takes on the
/// finished render of the same rack — the level the two differ by.
#[test]
#[ignore]
fn level_parity_table() {
    use crate::audio::converter::dsp::{lab::isp, true_peak};
    let list = std::env::var("AURA_FVL_LIST").expect("AURA_FVL_LIST");
    let s = rack();
    let db = |x: f64| 20.0 * x.max(1e-12).log10();
    let dir = tempfile::tempdir().unwrap();
    let res = Resources::new(dir.path().to_path_buf());
    for path in list.split('|').filter(|p| !p.is_empty()) {
        let cancel = AtomicBool::new(false);
        let v = Arc::new(prepare_variant(Path::new(path), &s, false, &cancel).expect("prepare"));
        let target = 10f64.powf(v.tp_target_dbtp / 20.0);
        let live = build_chain(&res, 1, "fvl", &v, &s, 0, None, None, 0).expect("chain");
        // The converter's output stage on the real render: the limiter's own
        // refusal rule, then the true-peak scalar.
        let probe = PlayerSettings { isp: false, ..s.clone() };
        let mut c = build_chain(&res, 1, "fvl", &v, &probe, 0, None, None, 0).expect("chain");
        let (mut l, mut r) = read_whole(&mut c, v.src.len() * v.l);
        let g0 = c.gain;
        for x in l.iter_mut().chain(r.iter_mut()) { *x /= g0; }
        let tp_raw = true_peak::measure_true_peak(&l, &r);
        let rep = if s.isp { isp::limit_output(&mut l, &mut r, target, v.out_rate) } else { None };
        let local = rep.as_ref().map_or(true, |p| p.fell_back.is_none());
        let tp_after = true_peak::measure_true_peak(&l, &r);
        let conv_gain = if tp_after > target { target / tp_after } else { 1.0 };
        // The gain the player took before the output-peak probe: from the
        // source's own 4× peaks and its local/global verdict.
        let over = db(v.tp_pred_lin) - v.tp_target_dbtp;
        let old_gain = if over <= 0.0 || (s.isp && v.lim_local && over <= 6.0) { 1.0 } else { target / v.tp_pred_lin };
        let name = Path::new(path).file_name().unwrap().to_string_lossy();
        eprintln!(
            "[FVL] {name} | src TP {:+.2} {} → old live gain {:+.2} dB | output TP {:+.2}, limiter {} ({}) | converter gain {:+.2} dB | new live gain {:+.2} dB | live − file: old {:+.2}, new {:+.2} dB",
            db(v.tp_pred_lin), if v.lim_local { "local" } else { "global" }, db(old_gain),
            db(tp_raw), if local { "local" } else { "refused" },
            rep.as_ref().and_then(|p| p.fell_back).unwrap_or("-"),
            db(conv_gain), db(live.gain), db(old_gain) - db(conv_gain), db(live.gain) - db(conv_gain)
        );
    }
}

/// The disk chain as the player plays a converted file (AURA_FVL_SRC: the
/// converted file): decoded the player's way, read 8192 frames at a time,
/// written like `live_chain_dump` to AURA_FVL_OUT.
#[test]
#[ignore]
fn disk_chain_dump() {
    let src = std::env::var("AURA_FVL_SRC").expect("AURA_FVL_SRC: the converted file");
    let out = std::env::var("AURA_FVL_OUT").expect("AURA_FVL_OUT: the f32 file to write");
    let disk = super::chain::disk_file(Path::new(&src)).expect("decode");
    let mut chain = super::chain::build_file_chain(1, disk, 0.0, Path::new(&src));
    let mut f = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(&out).unwrap());
    let mut bl = vec![0.0f64; 8192];
    let mut br = vec![0.0f64; 8192];
    let mut done = 0usize;
    let mut buf = Vec::with_capacity(8192 * 8);
    loop {
        let n = chain.stage.read(&mut bl, &mut br);
        if n == 0 {
            break;
        }
        buf.clear();
        for i in 0..n {
            buf.extend_from_slice(&(bl[i] as f32).to_le_bytes());
            buf.extend_from_slice(&(br[i] as f32).to_le_bytes());
        }
        f.write_all(&buf).unwrap();
        done += n;
    }
    f.flush().unwrap();
    eprintln!("[FVL] disk chain: rate {} gain {} wrote {} frames", chain.out_rate, chain.gain, done);
}

#[test]
#[ignore]
fn live_chain_dump() {
    let src = std::env::var("AURA_FVL_SRC").expect("AURA_FVL_SRC: the track");
    let out = std::env::var("AURA_FVL_OUT").expect("AURA_FVL_OUT: the f32 file to write");
    let s = rack();
    let cancel = AtomicBool::new(false);
    let t0 = std::time::Instant::now();
    let v = Arc::new(prepare_variant(Path::new(&src), &s, false, &cancel).expect("prepare"));
    let dir = tempfile::tempdir().unwrap();
    let res = Resources::new(dir.path().to_path_buf());
    // AURA_FVL_GPU=1: the card's convolver, as the player takes it for 30M.
    let gpu = if std::env::var("AURA_FVL_GPU").map(|v| v == "1").unwrap_or(false) {
        Some(super::gpu::ctx::GpuPolyCtx::try_build().expect("AURA_FVL_GPU=1 but no card"))
    } else {
        None
    };
    let mut chain = build_chain(&res, 1, "fvl", &v, &s, 0, gpu, None, 0).expect("chain");
    eprintln!(
        "[FVL] rate {} L {} tokens {:?} gpu {} | tp_pred {:+.2} dBTP, target {:+.2} dBTP, lim_local {}, gain {:+.2} dB | prepared in {:.1} s",
        chain.out_rate, v.l, chain.tokens, chain.gpu_on, chain.tp_pred_db, v.tp_target_dbtp, v.lim_local,
        20.0 * chain.gain.log10(), t0.elapsed().as_secs_f64()
    );
    let total = v.src.len() * v.l;
    let mut f = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(&out).unwrap());
    // The renderer reads 8192 frames at a time; AURA_FVL_BLOCK to match it.
    let block = std::env::var("AURA_FVL_BLOCK").ok().and_then(|b| b.parse().ok()).unwrap_or(1 << 16);
    let mut bl = vec![0.0f64; block];
    let mut br = vec![0.0f64; block];
    let mut done = 0usize;
    let mut buf = Vec::with_capacity(block * 8);
    while done < total {
        let want = (total - done).min(block);
        let n = chain.stage.read(&mut bl[..want], &mut br[..want]).min(want);
        if n == 0 {
            break;
        }
        buf.clear();
        for i in 0..n {
            buf.extend_from_slice(&(bl[i] as f32).to_le_bytes());
            buf.extend_from_slice(&(br[i] as f32).to_le_bytes());
        }
        f.write_all(&buf).unwrap();
        done += n;
    }
    f.flush().unwrap();
    eprintln!("[FVL] wrote {} frames ({:.1} s) in {:.1} s", done, done as f64 / chain.out_rate as f64, t0.elapsed().as_secs_f64());
}
