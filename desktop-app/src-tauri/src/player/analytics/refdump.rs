//! Reference dump test: runs all analytics modules on the reference signals
//! in the folder `AURA_ANALYTICS_REF` names (`sig\`, `real\`, their
//! manifests) and writes `results.json` there, with the same keys as
//! `expected.json`, for regression comparison. Without the variable the
//! tests say so and pass: the signals are not part of the repository.
//!
//! Signal file format: planar little-endian f64
//!   bytes [0 .. frames*8)       = left channel
//!   bytes [frames*8 .. 2*frames*8) = right channel
//!
//! LRA variants:
//!   lra_ebu      — short-term series at 100 ms hop (meter default)
//!   lra_libebur128 — same but decimated to 1 s hop (every 10th entry)
//!
//! Run with:
//!   set AURA_ANALYTICS_REF=<folder>
//!   cargo test --profile fast analytics_ref_dump -- --ignored --nocapture

use super::dr::compute_dr;
use super::loudness::LoudnessMeter;
use super::lra::{lra_ebu, lra_libebur128};
use super::peaks::{clip_report, engine_true_peak, lin_to_db, sample_peak, EburTruePeak};
use serde_json::{Map, Value};
use std::fs;

/// The reference folder, or `None` (with a note) when it is not set.
fn ref_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("AURA_ANALYTICS_REF").map(std::path::PathBuf::from);
    if dir.is_none() {
        println!("AURA_ANALYTICS_REF is not set: no reference signals, nothing to run");
    }
    dir
}

// ─── JSON helpers ─────────────────────────────────────────────────────────────

/// Serialize an f64 as a JSON number, or `null` when the value is NaN / ±inf.
fn fv(v: f64) -> Value {
    if v.is_finite() {
        Value::from(v)
    } else {
        Value::Null
    }
}

// ─── File I/O ─────────────────────────────────────────────────────────────────

fn load_manifest(path: &str) -> Vec<Value> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            println!("  manifest not found ({e}): {path}");
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Load a planar f64 file (all L then all R).
fn load_f64_planar(path: &str, frames: usize) -> Option<(Vec<f64>, Vec<f64>)> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("  file read error: {e}");
            return None;
        }
    };
    let need = frames * 2 * 8;
    if bytes.len() < need {
        println!(
            "  file too small: {} < {} bytes",
            bytes.len(),
            need
        );
        return None;
    }
    // Safe because: slice length is exact multiple of 8, and the allocation is
    // owned bytes from fs::read (aligned to 8 on this platform).
    // We transmute safely via bytemuck (requires the file to be LE f64, which
    // it is by construction — gen_signals.py writes numpy float64 LE).
    let floats: &[f64] = bytemuck::cast_slice(&bytes[..need]);
    let l = floats[..frames].to_vec();
    let r = floats[frames..2 * frames].to_vec();
    Some((l, r))
}

// ─── Analytics ────────────────────────────────────────────────────────────────

/// Run all analytics on one stereo signal and return a JSON value whose
/// structure mirrors the per-signal objects in `expected.json`.
fn run_signal(l: &[f64], r: &[f64], rate: f64) -> Value {
    // ── Loudness ──────────────────────────────────────────────────────────────
    let mut meter = LoudnessMeter::new(rate);
    meter.push(l, r);

    let i_lufs = meter.integrated();
    let (_, i_gate) = meter.integrated_detail();

    let m_series_f32 = meter.momentary_series();
    let st_series_f32 = meter.short_term_series();

    // Convert to f64 for LRA functions
    let st_series: Vec<f64> = st_series_f32.iter().map(|&v| v as f64).collect();

    // lra_ebu: 100 ms hop (full short-term series)
    let lra_e = lra_ebu(&st_series);
    // lra_libebur128: 1 s hop (every 10th entry)
    let st_1s: Vec<f64> = st_series.iter().copied().step_by(10).collect();
    let lra_l = lra_libebur128(&st_1s);

    // First 8 momentary values and first 30 short-term values for diagnostics
    let m8: Vec<Value> = m_series_f32
        .iter()
        .take(8)
        .map(|&v| fv(v as f64))
        .collect();
    let st30: Vec<Value> = st_series.iter().take(30).map(|&v| fv(v)).collect();

    let lra_variants = {
        let mut m = Map::new();
        m.insert("I_lufs".into(), fv(i_lufs));
        m.insert("I_gate_lufs".into(), fv(i_gate));
        // lra_ebu fields
        m.insert(
            "LRA_ebu".into(),
            lra_e.as_ref().map(|r| fv(r.lra)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_ebu_thr".into(),
            lra_e
                .as_ref()
                .map(|r| fv(r.rel_gate_lufs))
                .unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_ebu_plo".into(),
            lra_e.as_ref().map(|r| fv(r.p10)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_ebu_phi".into(),
            lra_e.as_ref().map(|r| fv(r.p95)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_ebu_n".into(),
            lra_e
                .as_ref()
                .map(|r| Value::from(r.n_used))
                .unwrap_or(Value::from(0usize)),
        );
        // lra_libebur128 fields
        m.insert(
            "LRA_libebur128".into(),
            lra_l.as_ref().map(|r| fv(r.lra)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_libebur128_plo".into(),
            lra_l.as_ref().map(|r| fv(r.p10)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_libebur128_phi".into(),
            lra_l.as_ref().map(|r| fv(r.p95)).unwrap_or(Value::Null),
        );
        m.insert(
            "LRA_libebur128_n".into(),
            lra_l
                .as_ref()
                .map(|r| Value::from(r.n_used))
                .unwrap_or(Value::from(0usize)),
        );
        m.insert("M_series_lufs".into(), Value::Array(m8));
        m.insert("ST_series_lufs".into(), Value::Array(st30));
        Value::Object(m)
    };

    // ── EBU true-peak ─────────────────────────────────────────────────────────
    let rate_u32 = rate as u32;
    let mut ebur = EburTruePeak::new(rate_u32);
    ebur.push(l, r);

    let factor: usize = if rate_u32 < 96_000 {
        4
    } else if rate_u32 < 192_000 {
        2
    } else {
        1
    };

    let tp_ebur = {
        let mut m = Map::new();
        m.insert("TP_L_lin".into(), fv(ebur.peak_l()));
        m.insert("TP_R_lin".into(), fv(ebur.peak_r()));
        m.insert("TP_max_lin".into(), fv(ebur.peak_max()));
        m.insert("TP_L_dbtp".into(), fv(ebur.peak_l_dbtp()));
        m.insert("TP_R_dbtp".into(), fv(ebur.peak_r_dbtp()));
        m.insert("TP_max_dbtp".into(), fv(lin_to_db(ebur.peak_max())));
        m.insert("factor".into(), Value::from(factor));
        Value::Object(m)
    };

    // ── Sample peak + clip analysis ───────────────────────────────────────────
    let (sp_l, sp_r) = sample_peak(l, r);
    let sp_max = sp_l.max(sp_r);

    let clips = clip_report(l, r, None);

    // Separate runs by channel (first_positions holds up to CLIP_POSITIONS_CAP
    // entries across both channels; L is processed first, so L entries appear
    // before R entries fill the remaining slots).
    let l_pos: Vec<(u64, u32)> = clips
        .first_positions
        .iter()
        .filter(|&&(_, _, ch)| ch == 0)
        .map(|&(pos, len, _)| (pos, len))
        .collect();
    let r_pos: Vec<(u64, u32)> = clips
        .first_positions
        .iter()
        .filter(|&&(_, _, ch)| ch == 1)
        .map(|&(pos, len, _)| (pos, len))
        .collect();

    let l_run_lens: Vec<Value> = l_pos.iter().map(|&(_, len)| Value::from(len)).collect();
    let r_run_lens: Vec<Value> = r_pos.iter().map(|&(_, len)| Value::from(len)).collect();
    let l_total: u64 = l_pos.iter().map(|&(_, len)| len as u64).sum();
    let r_total: u64 = r_pos.iter().map(|&(_, len)| len as u64).sum();

    let peaks_obj = {
        let mut m = Map::new();
        m.insert("SP_L_lin".into(), fv(sp_l));
        m.insert("SP_R_lin".into(), fv(sp_r));
        m.insert("SP_max_lin".into(), fv(sp_max));
        m.insert("SP_L_dbfs".into(), fv(lin_to_db(sp_l)));
        m.insert("SP_R_dbfs".into(), fv(lin_to_db(sp_r)));
        m.insert("SP_max_dbfs".into(), fv(lin_to_db(sp_max)));
        // clips_L
        let mut cl = Map::new();
        cl.insert("count".into(), Value::from(l_pos.len()));
        cl.insert("total_samples".into(), Value::from(l_total));
        cl.insert("runs".into(), Value::Array(l_run_lens));
        m.insert("clips_L".into(), Value::Object(cl));
        // clips_R
        let mut cr = Map::new();
        cr.insert("count".into(), Value::from(r_pos.len()));
        cr.insert("total_samples".into(), Value::from(r_total));
        cr.insert("runs".into(), Value::Array(r_run_lens));
        m.insert("clips_R".into(), Value::Object(cr));
        Value::Object(m)
    };

    // ── DR14 ─────────────────────────────────────────────────────────────────
    let dr_obj = {
        let mut m = Map::new();
        match compute_dr(l, r, rate_u32) {
            Some(dr) => {
                let n_top = ((dr.n_blocks as f64 * 0.2).floor() as usize).max(1);
                m.insert("dr_track".into(), Value::from(dr.dr_rounded));
                m.insert("dr_L".into(), fv(dr.l.dr_exact));
                m.insert("dr_R".into(), fv(dr.r.dr_exact));
                m.insert("dr_peak_L_lin".into(), fv(dr.l.peak2));
                m.insert("dr_peak_R_lin".into(), fv(dr.r.peak2));
                m.insert("seg_cnt".into(), Value::from(dr.n_blocks));
                m.insert("n_blk_used".into(), Value::from(n_top));
            }
            None => {
                m.insert("dr_track".into(), Value::Null);
                m.insert("dr_L".into(), Value::Null);
                m.insert("dr_R".into(), Value::Null);
                m.insert("dr_peak_L_lin".into(), Value::Null);
                m.insert("dr_peak_R_lin".into(), Value::Null);
                m.insert("seg_cnt".into(), Value::from(0usize));
                m.insert("n_blk_used".into(), Value::from(0usize));
            }
        }
        Value::Object(m)
    };

    // ── Engine (Lanczos-4) true peak ──────────────────────────────────────────
    let eng_tp_lin = engine_true_peak(l, r);
    let tp_engine = {
        let mut m = Map::new();
        m.insert("TP_max_lin".into(), fv(eng_tp_lin));
        m.insert("TP_max_dbtp".into(), fv(lin_to_db(eng_tp_lin)));
        Value::Object(m)
    };

    let mut out = Map::new();
    out.insert("lra_variants".into(), lra_variants);
    out.insert("tp_ebur128_sdroege".into(), tp_ebur);
    out.insert("tp_engine_lanczos4".into(), tp_engine);
    out.insert("peaks".into(), peaks_obj);
    out.insert("dr14".into(), dr_obj);
    Value::Object(out)
}

// ─── Test ─────────────────────────────────────────────────────────────────────

/// Read `manifest.json`, load each `.f64` signal file, run all analytics, and
/// write `results.json` for comparison against `expected.json`.
///
/// Run with:
/// ```sh
/// cargo test --profile fast analytics_ref_dump -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn analytics_ref_dump() {
    let Some(base) = ref_dir() else { return };
    let base = base.as_path();
    let sig_manifest = load_manifest(
        base.join("sig").join("manifest.json").to_str().unwrap(),
    );
    let real_manifest = load_manifest(
        base.join("real").join("manifest.json").to_str().unwrap(),
    );

    println!("sig entries: {}, real entries: {}", sig_manifest.len(), real_manifest.len());

    let mut results: Map<String, Value> = Map::new();

    for (entries, subdir) in [
        (sig_manifest, "sig"),
        (real_manifest, "real"),
    ] {
        for entry in &entries {
            let name = match entry["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let rate = entry["rate"].as_f64().unwrap_or(0.0);
            let frames = entry["frames"].as_u64().unwrap_or(0) as usize;

            if rate < 8_000.0 || rate > 768_000.0 {
                println!("SKIP {name}: rate {rate} out of range [8000, 768000]");
                let mut m = Map::new();
                m.insert("name".into(), Value::from(name.clone()));
                m.insert("rate".into(), fv(rate));
                m.insert("frames".into(), Value::from(frames));
                m.insert("skipped_reason".into(), Value::from("rate out of range"));
                results.insert(name, Value::Object(m));
                continue;
            }
            if frames == 0 {
                println!("SKIP {name}: zero frames");
                continue;
            }

            let f64_path = base.join(subdir).join(format!("{name}.f64"));
            print!("Loading {name} ({frames} frames @ {rate} Hz) ... ");

            let (l, r) = match load_f64_planar(f64_path.to_str().unwrap(), frames) {
                Some(lr) => lr,
                None => {
                    println!("SKIP");
                    let mut m = Map::new();
                    m.insert("name".into(), Value::from(name.clone()));
                    m.insert("rate".into(), fv(rate));
                    m.insert("frames".into(), Value::from(frames));
                    m.insert("skipped_reason".into(), Value::from("file not found or too small"));
                    results.insert(name, Value::Object(m));
                    continue;
                }
            };

            println!("running analytics ...");
            let mut result = run_signal(&l, &r, rate);
            result["name"] = Value::from(name.clone());
            result["rate"] = fv(rate);
            result["frames"] = Value::from(frames);
            result["source"] = Value::from(if subdir == "sig" { "synthetic" } else { "real" });

            results.insert(name, result);
        }
    }

    let json_str = serde_json::to_string_pretty(&Value::Object(results))
        .expect("JSON serialization failed");
    let results_path = base.join("results.json");
    fs::write(&results_path, &json_str).expect("Failed to write results.json");
    println!("\nWrote {}", results_path.display());
}

/// Timing test: measure whole-track S+B analysis time for the largest 44.1k real track.
///
/// Run with:
/// ```sh
/// cargo test --profile fast analytics_sb_timing -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn analytics_sb_timing() {
    let Some(base) = ref_dir() else { return };
    let base = base.as_path();
    let real_manifest = load_manifest(
        base.join("real").join("manifest.json").to_str().unwrap(),
    );

    // Find the largest 44.1k track.
    let entry = real_manifest.iter()
        .filter(|e| e["rate"].as_f64().unwrap_or(0.0) == 44100.0)
        .max_by_key(|e| e["frames"].as_u64().unwrap_or(0));

    let entry = match entry {
        Some(e) => e,
        None => { println!("No 44.1k real track found"); return; }
    };

    let name   = entry["name"].as_str().unwrap_or("unknown");
    let rate   = entry["rate"].as_f64().unwrap_or(44100.0);
    let frames = entry["frames"].as_u64().unwrap_or(0) as usize;
    let path   = base.join("real").join(format!("{name}.f64"));

    println!("Timing S+B analysis: {name} ({frames} frames @ {rate} Hz = {:.1} s)",
        frames as f64 / rate);

    let (l, r) = match load_f64_planar(path.to_str().unwrap(), frames) {
        Some(lr) => lr,
        None => { println!("File not found, skipping"); return; }
    };

    let t0 = std::time::Instant::now();
    let _ = run_signal(&l, &r, rate);
    let elapsed = t0.elapsed();

    let dur_s = frames as f64 / rate;
    println!(
        "S+B analysis: {:.3} s wall for {:.1} s audio ({:.2}x real time)",
        elapsed.as_secs_f64(), dur_s, dur_s / elapsed.as_secs_f64()
    );
}
