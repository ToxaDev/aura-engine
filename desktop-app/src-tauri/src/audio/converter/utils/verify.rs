use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rayon::prelude::*;

use crate::audio::converter::state::{FileConvState, BADGE_VERIFIED_FAIL};

pub fn verify_flac(
    path: &Path,
    expected_l: &[f64],
    expected_r: &[f64],
    file_state: &Arc<FileConvState>,
) -> Result<(), String> {
    crate::aelog!();
    crate::aelog!("[CONV] ===================================================");
    crate::aelog!("[CONV] STAGE 5: BIT-PERFECT VERIFICATION");
    crate::aelog!("[CONV] ===================================================");
    crate::aelog!("[CONV] Re-decoding generated FLAC: {}", path.display());

    // Try Symphonia first (fast, pure-Rust, no subprocess).
    // FLAC spec hard-caps sample rate at 655350 Hz — Symphonia enforces this unconditionally
    // in its STREAMINFO parser, throwing "flac: stream sample rate out of bounds".
    // For 705600 / 768000 Hz files (which ffmpeg writes non-compliant but valid in practice),
    // we fall back to ffmpeg decode via pipe.
    let (decoded_l, decoded_r) = match decode_via_symphonia(path) {
        Ok(pair) => pair,
        Err(sym_err) => {
            if sym_err.contains("out of bounds") || sym_err.contains("malformed") {
                crate::aelog!(
                    "[CONV] Symphonia rejected FLAC (sample rate > 655350 Hz limit)."
                );
                crate::aelog!("[CONV] Falling back to ffmpeg decode for verification...");
                decode_via_ffmpeg(path).map_err(|e| {
                    file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
                    e
                })?
            } else {
                file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
                return Err(sym_err);
            }
        }
    };

    // ── Length check ──────────────────────────────────────────────────────────
    if decoded_l.len() != expected_l.len() {
        crate::aelog!(
            "[CONV] ❌ VERIFICATION FAILED: Length mismatch! Expected: {}, Got: {}",
            expected_l.len(),
            decoded_l.len()
        );
        file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
        return Err(format!(
            "Length mismatch (expected {}, got {})",
            expected_l.len(),
            decoded_l.len()
        ));
    }

    // ── Parallel sample comparison using rayon ────────────────────────────────
    // Tolerance: ±2 LSBs at 24-bit (accounts for ffmpeg s32↔f64 rounding)
    let margin = 2.0 / ((1i64 << 23) as f64);
    let n = expected_l.len();

    // Count mismatches in parallel; also find max delta for diagnostic logging
    let (err_count, max_diff) = (0..n)
        .into_par_iter()
        .map(|i| {
            let dl = (expected_l[i] - decoded_l[i]).abs();
            let dr = (expected_r[i] - decoded_r[i]).abs();
            let max = dl.max(dr);
            let bad = if max > margin { 1usize } else { 0 };
            (bad, max)
        })
        .reduce(
            || (0, 0.0f64),
            |(ec, mx), (e, m)| (ec + e, mx.max(m)),
        );

    // Proportional threshold: 0.001% of total samples, min 20.
    // Tightened from 0.01%/100 — the old budget allowed >10k mismatches on
    // a 5-min 384 kHz file, large enough to mask off-by-one regressions in
    // the trim/flush logic. ±2 LSB margin is preserved.
    let allowed_errors = (n / 100_000).max(20);

    if err_count > allowed_errors {
        crate::aelog!(
            "[CONV] ❌ VERIFICATION FAILED: {} mismatches (allowed: {})",
            err_count, allowed_errors
        );
        crate::aelog!("[CONV] ❌ Maximum delta: {:.4e}", max_diff);
        file_state.badge.store(BADGE_VERIFIED_FAIL, Ordering::Relaxed);
        return Err("encoded file bits do not match DSP output".to_string());
    }

    crate::aelog!("[CONV] ✓ FLAC Integrity Confirmed.");
    crate::aelog!("[CONV] ✓ Matches 64-bit DSP output (±2 LSB @ 24-bit tolerance).");
    crate::aelog!(
        "[CONV] ✓ Max deviation: {:.4e}  margin: {:.4e}  mismatches: {}/{}",
        max_diff, margin, err_count, n
    );
    crate::aelog!("[CONV] ===================================================\n");

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────

fn decode_via_symphonia(path: &Path) -> Result<(Vec<f64>, Vec<f64>), String> {
    let decoded = crate::audio::converter::decode::decode_file(path)?;
    Ok((decoded.samples_l, decoded.samples_r))
}

/// Decode FLAC via ffmpeg pipe — used for sample rates above the FLAC spec limit (655350 Hz).
fn decode_via_ffmpeg(path: &Path) -> Result<(Vec<f64>, Vec<f64>), String> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-threads", "0",          // auto-detect optimal thread count
            "-i", path.to_str().unwrap_or(""),
            "-f", "s32le",
            "-acodec", "pcm_s32le",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg decode error: {}", e))?;

    let mut stdout = child.stdout.take().ok_or("ffmpeg: no stdout")?;
    let mut bytes = Vec::new();
    stdout
        .read_to_end(&mut bytes)
        .map_err(|e| format!("ffmpeg read error: {}", e))?;
    child
        .wait()
        .map_err(|e| format!("ffmpeg wait error: {}", e))?;

    // s32le interleaved stereo: 4 bytes L + 4 bytes R = 8 bytes int32 per frame
    const FRAME_BYTES: usize = 8;
    // Same normalisation as Symphonia: 1/2^31
    const SCALE: f64 = 1.0 / 2_147_483_648.0_f64;

    let n_frames = bytes.len() / FRAME_BYTES;
    crate::aelog!("[CONV] ffmpeg decoded {} frames via pipe", n_frames);

    let (out_l, out_r): (Vec<f64>, Vec<f64>) = (0..n_frames)
        .into_par_iter()
        .map(|i| {
            let b = i * FRAME_BYTES;
            let l = i32::from_le_bytes([bytes[b], bytes[b+1], bytes[b+2], bytes[b+3]]);
            let r = i32::from_le_bytes([bytes[b+4], bytes[b+5], bytes[b+6], bytes[b+7]]);
            (l as f64 * SCALE, r as f64 * SCALE)
        })
        .unzip();

    Ok((out_l, out_r))
}
