use crate::audio::converter::state::*;
use crate::audio::converter::types::AudioFile;
use std::path::Path;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

pub fn set_status(s: &str) {
    *CONV_STATUS.lock().unwrap() = s.to_string();
    crate::aelog!("[CONV] {}", s);
}

/// Fast container probe: total frames + sample rate WITHOUT decoding.
/// Costs milliseconds (header parse only). Used by the prep thread to
/// estimate a file's preparation RAM before committing to the full decode —
/// a 38-minute 192 kHz album file must not be decoded+cloned+apodized in
/// parallel with another file's conversion.
/// Returns None when the container does not carry a frame count (some MP3s).
/// Frame count, sample rate and channel count, read from the container
/// header without decoding. Callers use the frames for RAM admission and the
/// channels for the pre-flight warning about sources wider than stereo.
pub fn probe_input_frames(path: &Path) -> Option<(u64, u32, usize)> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    let track = format.tracks().iter().find(|t| audio_params(t).is_some())?;
    let params = audio_params(track)?;
    let rate = params.sample_rate?;
    let frames = track.num_frames?;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
    Some((frames, rate, channels))
}

/// The audio codec parameters of a track, if it carries audio at all.
fn audio_params(
    track: &symphonia::core::formats::Track,
) -> Option<&symphonia::core::codecs::audio::AudioCodecParameters> {
    track.codec_params.as_ref().and_then(|p| p.audio())
}

/// Append one decoded packet to the channel buffers, as f64.
///
/// Copied straight into f64. It used to go through an i32 buffer, chosen
/// because an f32 one lost the 24th bit of 24-bit sources — and i32 does keep
/// every integer bit, but symphonia converts a float sample into i32 through
/// a **clamp** at ±1.0. MP3, AAC, Vorbis and Opus decoders, and float WAV,
/// legitimately produce samples beyond full scale: a file raised by MP3Gain
/// arrived with flat tops on 1.76 % of its frames, every one of them made
/// here, before the true-peak ceiling that exists to bring such a file down
/// ever saw the signal.
///
/// f64 takes both: i16, i24 and i32 divide by a power of two into values f64
/// holds exactly — the same bits the i32 path produced, verified by test —
/// and a float sample is widened, not clamped.
///
/// The other half of that clamp was inside symphonia itself, in the MP3 and
/// Vorbis decoders, where no buffer choice of ours could reach; 0.6.1 is the
/// first release with both removed, which is why the dependency moved.
///
/// A mono source is copied to both channels; channels past the second are
/// dropped.
fn append_packet(
    decoded: GenericAudioBufferRef<'_>,
    interleaved: &mut Vec<f64>,
    all_l: &mut Vec<f64>,
    all_r: &mut Vec<f64>,
) {
    let ch = decoded.spec().channels().count().max(1);
    decoded.copy_to_vec_interleaved(interleaved);

    for frame in interleaved.chunks_exact(ch) {
        let l = frame[0];
        all_l.push(l);
        all_r.push(if ch >= 2 { frame[1] } else { l });
    }
}

/// Decode audio file using symphonia
pub fn decode_file(path: &Path) -> Result<AudioFile, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open file: {}", e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {}", e))?;

    // Extract metadata
    let mut artist = String::new();
    let mut title = String::new();

    // Container metadata, then whatever the probe picked up ahead of it
    // (ID3v2 in front of an MP3 stream, for one) — the reader carries both.
    let mut metadata = format.metadata();
    while let Some(revision) = metadata.current() {
        for tag in &revision.media.tags {
            match &tag.std {
                Some(StandardTag::Artist(v)) if artist.is_empty() => artist = v.to_string(),
                Some(StandardTag::TrackTitle(v)) if title.is_empty() => title = v.to_string(),
                _ => {}
            }
        }
        if !metadata.is_latest() {
            metadata.pop();
        } else {
            break;
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
        .find(|t| audio_params(t).is_some())
        .ok_or("No audio track found")?;
    let params = audio_params(track).ok_or("No audio track found")?.clone();

    let sample_rate = params.sample_rate.ok_or("Unknown sample rate")?;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
    let track_id = track.id;

    // Gapless metadata (MP3/AAC): encoder priming and trailing padding frames
    // are decoded as ordinary audio unless we trim them explicitly.
    let enc_delay = track.delay.unwrap_or(0) as usize;
    let enc_padding = track.padding.unwrap_or(0) as usize;
    // Total-frame hint for pre-allocation (avoids ~24 doublings on long files).
    let n_frames_hint = track.num_frames.unwrap_or(0) as usize;

    if channels > 2 {
        // The user is asked about this before the batch starts (pre-flight in
        // `check_filters` → confirmation dialog). This line is the record of
        // what was actually done, for the session log.
        crate::aelog!(
            "[CONV] ═══ {} CHANNELS IN, 2 OUT ═══ front L/R converted, {} channel(s) discarded — not a downmix: centre, surrounds and LFE are dropped, not folded in.",
            channels,
            channels - 2
        );
    }

    // `gapless: false` — the priming and padding frames are trimmed below,
    // from the container's own counts, which is what every release before
    // this one did.
    let mut decode_opts = AudioDecoderOptions::default();
    decode_opts.gapless = false;
    decode_opts.verify = false;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &decode_opts)
        .map_err(|e| format!("Codec error: {}", e))?;


    let mut all_l: Vec<f64> = Vec::with_capacity(n_frames_hint);
    let mut all_r: Vec<f64> = Vec::with_capacity(n_frames_hint);
    let mut packet_decode_errors: u64 = 0;

    let mut interleaved: Vec<f64> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
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

        if packet.track_id != track_id {
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

        append_packet(decoded, &mut interleaved, &mut all_l, &mut all_r);
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

    // Lossy decoders and float WAV legitimately produce samples past full
    // scale. Worth a line: it is the difference between a file the output
    // ceiling brings down and one that was clipped on the way in, and until
    // 1.2.9 this engine did the clipping itself.
    let mut peak = 0.0_f64;
    let mut over = 0_u64;
    for (l, r) in all_l.iter().zip(all_r.iter()) {
        let m = l.abs().max(r.abs());
        if m > peak {
            peak = m;
        }
        if m > 1.0 {
            over += 1;
        }
    }
    if over > 0 {
        crate::aelog!(
            "[CONV] Source peaks at +{:.2} dBFS — {} frames ({:.3}%) above full scale, kept as decoded; the output ceiling scales them down",
            20.0 * peak.log10(),
            over,
            100.0 * over as f64 / all_l.len().max(1) as f64
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
        channels,
        artist,
        title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::{
        AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, Position,
    };
    use symphonia::core::audio::sample::{i24, Sample};

    fn spec(channels: usize) -> AudioSpec {
        let layout = if channels == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        } else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        };
        AudioSpec::new(44_100, layout)
    }

    /// A packet the way a decoder hands one over: one plane per channel.
    fn packet<S: Sample>(channels: &[&[S]]) -> AudioBuffer<S> {
        let frames = channels[0].len();
        let mut buf = AudioBuffer::<S>::new(spec(channels.len()), frames);
        buf.render_silence(Some(frames));
        for (i, samples) in channels.iter().enumerate() {
            buf.plane_mut(i).unwrap().copy_from_slice(samples);
        }
        buf
    }

    fn append(decoded: GenericAudioBufferRef<'_>) -> (Vec<f64>, Vec<f64>) {
        let (mut l, mut r, mut scratch) = (Vec::new(), Vec::new(), Vec::new());
        append_packet(decoded, &mut scratch, &mut l, &mut r);
        (l, r)
    }

    /// What 1.2.8 did: through an i32 buffer, then divide by 2^31.
    fn append_through_i32(decoded: GenericAudioBufferRef<'_>) -> Vec<f64> {
        let mut buf: Vec<i32> = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        buf.iter()
            .map(|&s| s as f64 * (1.0 / 2_147_483_648.0))
            .collect()
    }

    /// The bug. A lossy decoder's float output above full scale must reach
    /// the DSP as it is; through i32 these came out as exactly ±1.0.
    #[test]
    fn float_samples_beyond_full_scale_are_not_clamped() {
        let buf = packet::<f32>(&[&[1.5, -1.25, 0.5, 2.375], &[-1.5, 1.0, -0.75, -3.0]]);
        let (l, r) = append(buf.as_generic_audio_buffer_ref());
        assert_eq!(l, vec![1.5, -1.25, 0.5, 2.375]);
        assert_eq!(r, vec![-1.5, 1.0, -0.75, -3.0]);

        let clamped = append_through_i32(buf.as_generic_audio_buffer_ref());
        assert_eq!(clamped[0], 1.0 - 1.0 / 2_147_483_648.0, "the old path clamped it");
    }

    #[test]
    fn float64_samples_pass_unchanged() {
        let buf = packet::<f64>(&[&[1.0e-12, -7.5], &[4.25, -1.0]]);
        let (l, r) = append(buf.as_generic_audio_buffer_ref());
        assert_eq!(l, vec![1.0e-12, -7.5]);
        assert_eq!(r, vec![4.25, -1.0]);
    }

    /// 24-bit keeps its 24th bit, and every integer width comes out bit for
    /// bit what the i32 path gave — CD rips and hi-res FLAC must not move.
    #[test]
    fn integer_sources_are_exact_and_identical_to_the_i32_path() {
        let s24: Vec<i24> = [8_388_607, -8_388_608, 1, -1, 0, 4_194_305, -3_000_001]
            .iter()
            .map(|&v| i24::from(v))
            .collect();
        let b24 = packet::<i24>(&[&s24, &s24]);
        let (l, _) = append(b24.as_generic_audio_buffer_ref());
        assert_eq!(l[0], 8_388_607.0 / 8_388_608.0);
        assert_eq!(l[1], -1.0);
        assert_eq!(l[2], 1.0 / 8_388_608.0, "the least significant of 24 bits survives");

        let s16: Vec<i16> = vec![i16::MAX, i16::MIN, 1, -1, 0, 12_345, -23_456];
        let s32: Vec<i32> = vec![i32::MAX, i32::MIN, 1, -1, 0, 123_456_789, -987_654_321];
        let b16 = packet::<i16>(&[&s16, &s16]);
        let b32 = packet::<i32>(&[&s32, &s32]);
        assert_eq!(append(b16.as_generic_audio_buffer_ref()).0[0], 32_767.0 / 32_768.0);

        for buf in [b16.as_generic_audio_buffer_ref(), b24.as_generic_audio_buffer_ref(), b32.as_generic_audio_buffer_ref()] {
            let old = append_through_i32(buf.clone());
            let (l, r) = append(buf);
            let new: Vec<u64> = l
                .iter()
                .zip(&r)
                .flat_map(|(a, b)| [a.to_bits(), b.to_bits()])
                .collect();
            let old: Vec<u64> = old.iter().map(|x| x.to_bits()).collect();
            assert_eq!(new, old);
        }
    }

    #[test]
    fn mono_is_copied_to_both_channels() {
        let buf = packet::<f32>(&[&[0.25, 1.75, -2.0]]);
        let (l, r) = append(buf.as_generic_audio_buffer_ref());
        assert_eq!(l, vec![0.25, 1.75, -2.0]);
        assert_eq!(r, l);
    }
}
