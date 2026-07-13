use crate::audio::converter::state::*;
use crate::audio::converter::types::AudioFile;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub fn set_status(s: &str) {
    *CONV_STATUS.lock().unwrap() = s.to_string();
    crate::aelog!("[CONV] {}", s);
}
/// Decode audio file using symphonia
pub fn decode_file(path: &Path) -> Result<AudioFile, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open file: {}", e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {}", e))?;

    let mut format = probed.format;

    // Extract metadata
    let mut artist = String::new();
    let mut title = String::new();

    // Check container metadata
    if let Some(metadata) = format.metadata().current() {
        for tag in metadata.tags() {
            let key = tag.std_key;
            match key {
                Some(symphonia::core::meta::StandardTagKey::Artist) => {
                    artist = tag.value.to_string()
                }
                Some(symphonia::core::meta::StandardTagKey::TrackTitle) => {
                    title = tag.value.to_string()
                }
                _ => {}
            }
        }
    }

    // Check probed metadata too
    if let Some(metadata_rev) = probed.metadata.get() {
        if let Some(metadata) = metadata_rev.current() {
            for tag in metadata.tags() {
                let key = tag.std_key;
                match key {
                    Some(symphonia::core::meta::StandardTagKey::Artist) if artist.is_empty() => {
                        artist = tag.value.to_string()
                    }
                    Some(symphonia::core::meta::StandardTagKey::TrackTitle) if title.is_empty() => {
                        title = tag.value.to_string()
                    }
                    _ => {}
                }
            }
        }
    }

    // Fallback: use filename
    if title.is_empty() {
        title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();
    }

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or("No audio track found")?;

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or("Unknown sample rate")?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let track_id = track.id;

    // Gapless metadata (MP3/AAC): encoder priming and trailing padding frames
    // are decoded as ordinary audio unless we trim them explicitly.
    let enc_delay = track.codec_params.delay.unwrap_or(0) as usize;
    let enc_padding = track.codec_params.padding.unwrap_or(0) as usize;
    // Total-frame hint for pre-allocation (avoids ~24 doublings on long files).
    let n_frames_hint = track.codec_params.n_frames.unwrap_or(0) as usize;

    if channels > 2 {
        crate::aelog!(
            "[CONV] WARNING: source has {} channels — only the first two (L/R) are converted, the rest are discarded",
            channels
        );
    }

    let decode_opts = DecoderOptions {
        verify: false,
        ..Default::default()
    };

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &decode_opts)
        .map_err(|e| format!("Codec error: {}", e))?;


    let mut all_l: Vec<f64> = Vec::with_capacity(n_frames_hint);
    let mut all_r: Vec<f64> = Vec::with_capacity(n_frames_hint);
    let mut packet_decode_errors: u64 = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(e) => {
                // A non-EOF container error mid-stream means the rest of the
                // file is unreadable. Never swallow this silently — the user
                // must know the output is truncated.
                crate::aelog!(
                    "[CONV] WARNING: decode stopped early ({} samples decoded): {}",
                    all_l.len(),
                    e
                );
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                packet_decode_errors += 1;
                if packet_decode_errors <= 5 {
                    crate::aelog!("[CONV] WARNING: skipped corrupt packet: {}", e);
                }
                continue;
            }
        };

        let spec = *decoded.spec();
        let n_frames = decoded.capacity();
        // Use i32 buffer: symphonia normalises all integer PCM (16-bit, 24-bit, 32-bit int)
        // to the full i32 range via left-shift.  Dividing by 2^31 in f64 gives exact values
        // without the 1-bit mantissa loss that SampleBuffer::<f32> causes on 24-bit sources.
        // For float-format sources (f32 WAV) the result is identical to the f32 path.
        const SCALE_I32_TO_F64: f64 = 1.0 / 2_147_483_648.0_f64; // 1 / 2^31
        let mut sample_buf = SampleBuffer::<i32>::new(n_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();

        let ch = spec.channels.count().max(1);
        for frame in 0..(samples.len() / ch) {
            let l = samples[frame * ch] as f64 * SCALE_I32_TO_F64;
            let r = if ch >= 2 {
                samples[frame * ch + 1] as f64 * SCALE_I32_TO_F64
            } else {
                l
            };
            all_l.push(l);
            all_r.push(r);
        }
    }

    if all_l.is_empty() {
        return Err("No audio samples decoded".to_string());
    }
    if packet_decode_errors > 0 {
        crate::aelog!(
            "[CONV] WARNING: {} corrupt packets skipped during decode",
            packet_decode_errors
        );
    }

    // ── Gapless trim (MP3/AAC) ──
    // Drop encoder priming samples from the head and padding from the tail so
    // LAME/AAC framing silence never reaches the FIR chain.
    if enc_delay > 0 && all_l.len() > enc_delay {
        all_l.drain(..enc_delay);
        all_r.drain(..enc_delay);
    }
    if enc_padding > 0 && all_l.len() > enc_padding {
        let keep = all_l.len() - enc_padding;
        all_l.truncate(keep);
        all_r.truncate(keep);
    }
    if enc_delay > 0 || enc_padding > 0 {
        crate::aelog!(
            "[CONV] Gapless trim: -{} priming, -{} padding samples",
            enc_delay, enc_padding
        );
    }

    crate::aelog!(
        "[CONV] Decoded: {}Hz {}ch {} samples, artist='{}', title='{}'",
        sample_rate,
        channels,
        all_l.len(),
        artist,
        title
    );

    Ok(AudioFile {
        samples_l: all_l,
        samples_r: all_r,
        sample_rate,
        artist,
        title,
    })
}
