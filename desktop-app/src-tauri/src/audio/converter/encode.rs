use crate::audio::converter::state::*;
use crate::audio::converter::types::{AudioFile, ConvertSettings};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

/// Build output filename
pub fn build_output_name(
    audio: &AudioFile,
    settings: &ConvertSettings,
    src_path: &Path,
    actual_out_rate: u32,
    apod_tag: Option<&str>,
) -> String {
    // ── Base name from source filename (not metadata tags) ──
    let base = src_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // ── Filter window ──
    let window = match settings.win_type {
        0 => "Hamming",
        1 => "Hann",
        2 => "Blackman",
        3 => "Nuttall",
        4 => "Kaiser",
        _ => "Custom",
    };
    let filter_name = if settings.custom_filter_path.is_some() {
        "AURA".to_string()
    } else {
        window.to_string()
    };

    // ── Taps (compact) ──
    let taps_str = if settings.taps >= 1_000_000 {
        let m = settings.taps as f64 / 1_000_000.0;
        if m == m.floor() {
            format!("{:.0}M", m)
        } else {
            format!("{:.1}M", m)
        }
    } else if settings.taps >= 1000 {
        let k = settings.taps / 1000;
        format!("{}K", k)
    } else {
        format!("{}", settings.taps)
    };

    // ── Sample rate (compact) ──
    let rate_khz = actual_out_rate as f64 / 1000.0;
    let rate_str = if rate_khz == rate_khz.floor() {
        format!("{:.0}k", rate_khz)
    } else {
        format!("{:.1}k", rate_khz)
    };

    // ── Source info ──
    let src_rate_khz = audio.sample_rate as f64 / 1000.0;
    let src_str = if src_rate_khz == src_rate_khz.floor() {
        format!("{:.0}k", src_rate_khz)
    } else {
        format!("{:.1}k", src_rate_khz)
    };

    // ── Build tag chain: compact dot-separated ──
    // Format: [AE · 44.1k→384k · Kaiser 10M · f64 · AA · HP]
    let mut tags: Vec<String> = Vec::new();

    // Source → Output rate
    if audio.sample_rate != actual_out_rate {
        tags.push(format!("{}→{}", src_str, rate_str));
    } else {
        tags.push(rate_str);
    }

    // Filter + taps
    tags.push(format!("{} {}", filter_name, taps_str));

    // Precision
    tags.push(format!("f{}", settings.precision));

    // Apodizing — reflects what ACTUALLY ran in prepare ("AA" adaptive,
    // "Apod[-M/-S]" static preset, nothing when the detector skipped),
    // not merely the settings the user had enabled.
    if let Some(tag) = apod_tag {
        tags.push(tag.to_string());
    }

    // Advanced DSP
    if settings.hybrid_phase {
        tags.push("HP".to_string());
    }

    format!("{} [AE · {}].flac", base, tags.join(" · "))
}
/// Encode f64 PCM to FLAC using ffmpeg
pub fn encode_flac(
    samples_l: &[f64],
    samples_r: &[f64],
    sample_rate: u32,
    output_path: &Path,
) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-y", // overwrite
            "-f",
            "f64le", // input format: raw 64-bit float LE
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            "2", // stereo
            "-i",
            "pipe:0", // stdin
            "-c:a",
            "flac", // FLAC codec
            "-sample_fmt",
            "s32", // ffmpeg stores 24-bit audio in s32 buffers
            // Explicit 24-bit FLAC (otherwise ffmpeg's behaviour depends on
            // codec defaults: some builds write a 32-bit FLAC stream, where
            // the bottom 8 bits are pure dither noise we don't want stored).
            "-bits_per_raw_sample",
            "24",
            "-compression_level",
            "8", // max compression
        ])
        .arg(output_path.to_str().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start ffmpeg: {}. Is ffmpeg installed?", e))?;

    let mut stdin = child.stdin.take().ok_or("Failed to get ffmpeg stdin")?;

    // Interleave L/R and write as f64le
    let total = samples_l.len();
    let chunk_size = 8192;
    // A cancelled or failed encode must never leave a half-written FLAC at
    // the destination path — it is indistinguishable from a good file until
    // the user tries to play it.
    let cleanup_partial = || {
        let _ = std::fs::remove_file(output_path);
    };

    for start in (0..total).step_by(chunk_size) {
        if CONV_CANCEL.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_partial();
            return Err("Cancelled".to_string());
        }
        let end = (start + chunk_size).min(total);
        let mut buf: Vec<u8> = Vec::with_capacity((end - start) * 16);
        for i in start..end {
            buf.extend_from_slice(&samples_l[i].to_le_bytes());
            buf.extend_from_slice(&samples_r[i].to_le_bytes());
        }
        if let Err(e) = stdin.write_all(&buf) {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_partial();
            return Err(format!("ffmpeg write error: {}", e));
        }
    }

    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|e| format!("ffmpeg error: {}", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_partial();
        return Err(format!("ffmpeg failed: {}", stderr));
    }

    Ok(())
}
