//! A track decoded for the analysis: stereo f32 at 44.1 kHz (the rate the
//! separation network was trained at), resampled while decoding so a long
//! hi-res file never sits in memory at its own rate.

use std::path::Path;

use rubato::{FftFixedIn, Resampler};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::core::SR;

/// Left and right at 44.1 kHz; `stop` is asked now and then (true = give up).
pub fn decode_44k(path: &Path, stop: &dyn Fn() -> bool) -> Result<(Vec<f32>, Vec<f32>), String> {
    if crate::audio::dsd::dsf::sniff(path).is_some() {
        return decode_dsd_44k(path, stop);
    }
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("unsupported format: {e}"))?;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.as_ref().and_then(|p| p.audio()).is_some())
        .ok_or("no audio track")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("no audio track")?.clone();
    let rate = params.sample_rate.ok_or("unknown sample rate")?;
    let track_id = track.id;
    let enc_delay = track.delay.unwrap_or(0) as usize;
    let enc_padding = track.padding.unwrap_or(0) as usize;
    let hint_frames = track.num_frames.unwrap_or(0) as f64 * SR as f64 / rate as f64;
    let mut opts = AudioDecoderOptions::default();
    opts.gapless = false;
    opts.verify = false;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &opts)
        .map_err(|e| format!("codec error: {e}"))?;

    let mut rs = if rate != SR {
        Some(FftFixedIn::<f32>::new(rate as usize, SR as usize, 8192, 2, 2).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut out_l: Vec<f32> = Vec::with_capacity(hint_frames as usize + 16384);
    let mut out_r: Vec<f32> = Vec::with_capacity(hint_frames as usize + 16384);
    let (mut in_l, mut in_r): (Vec<f32>, Vec<f32>) = (Vec::new(), Vec::new());
    let mut interleaved: Vec<f32> = Vec::new();
    let mut skip = enc_delay;
    let mut total_in = 0usize;
    let mut since_check = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let ch = decoded.spec().channels().count().max(1);
        decoded.copy_to_vec_interleaved(&mut interleaved);
        for f in interleaved.chunks_exact(ch) {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            in_l.push(f[0]);
            in_r.push(if ch >= 2 { f[1] } else { f[0] });
            total_in += 1;
        }
        match rs.as_mut() {
            Some(rs) => {
                while in_l.len() >= rs.input_frames_next() {
                    let n = rs.input_frames_next();
                    let o = rs.process(&[&in_l[..n], &in_r[..n]], None).map_err(|e| e.to_string())?;
                    out_l.extend_from_slice(&o[0]);
                    out_r.extend_from_slice(&o[1]);
                    in_l.drain(..n);
                    in_r.drain(..n);
                }
            }
            None => {
                out_l.append(&mut in_l);
                out_r.append(&mut in_r);
            }
        }
        since_check += interleaved.len() / ch;
        if since_check > rate as usize {
            since_check = 0;
            if stop() {
                return Err("stopped".into());
            }
        }
    }
    let delay = if let Some(rs) = rs.as_mut() {
        // what is left, then silence until the resampler's delay is out
        let o = rs.process_partial(Some(&[&in_l[..], &in_r[..]]), None).map_err(|e| e.to_string())?;
        out_l.extend_from_slice(&o[0]);
        out_r.extend_from_slice(&o[1]);
        let want = (total_in as f64 * SR as f64 / rate as f64).round() as usize + rs.output_delay();
        while out_l.len() < want {
            let o = rs.process_partial::<&[f32]>(None, None).map_err(|e| e.to_string())?;
            if o[0].is_empty() {
                break;
            }
            out_l.extend_from_slice(&o[0]);
            out_r.extend_from_slice(&o[1]);
        }
        rs.output_delay()
    } else {
        0
    };
    // the resampler's delay off the front, the encoder's padding off the end
    let len = (total_in.saturating_sub(enc_padding) as f64 * SR as f64 / rate as f64).round() as usize;
    let take = |v: Vec<f32>| -> Vec<f32> {
        let mut v: Vec<f32> = v.into_iter().skip(delay).collect();
        v.resize(len, 0.0);
        v
    };
    if total_in == 0 {
        return Err("no audio decoded".into());
    }
    Ok((take(out_l), take(out_r)))
}

/// A DSF or DSDIFF file: decoded whole to PCM by the engine's DSD decoder
/// (`audio/dsd/decimate.rs`, 88.2 or 96 kHz), then brought to 44.1 kHz.
fn decode_dsd_44k(path: &Path, stop: &dyn Fn() -> bool) -> Result<(Vec<f32>, Vec<f32>), String> {
    let mut reader = crate::audio::dsd::dsf::open(path)?;
    let (l, r, rate) = crate::audio::dsd::decimate::decode_all(&mut *reader)?;
    if stop() {
        return Err("stopped".into());
    }
    let (l, r): (Vec<f32>, Vec<f32>) = (l.iter().map(|&v| v as f32).collect(), r.iter().map(|&v| v as f32).collect());
    let mut rs = FftFixedIn::<f32>::new(rate as usize, SR as usize, 8192, 2, 2).map_err(|e| e.to_string())?;
    let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
    let mut at = 0;
    while l.len() - at >= rs.input_frames_next() {
        let n = rs.input_frames_next();
        let o = rs.process(&[&l[at..at + n], &r[at..at + n]], None).map_err(|e| e.to_string())?;
        out_l.extend_from_slice(&o[0]);
        out_r.extend_from_slice(&o[1]);
        at += n;
    }
    let o = rs.process_partial(Some(&[&l[at..], &r[at..]]), None).map_err(|e| e.to_string())?;
    out_l.extend_from_slice(&o[0]);
    out_r.extend_from_slice(&o[1]);
    let delay = rs.output_delay();
    let len = (l.len() as f64 * SR as f64 / rate as f64).round() as usize;
    while out_l.len() < len + delay {
        let o = rs.process_partial::<&[f32]>(None, None).map_err(|e| e.to_string())?;
        if o[0].is_empty() {
            break;
        }
        out_l.extend_from_slice(&o[0]);
        out_r.extend_from_slice(&o[1]);
    }
    let take = |v: Vec<f32>| -> Vec<f32> {
        let mut v: Vec<f32> = v.into_iter().skip(delay).collect();
        v.resize(len, 0.0);
        v
    };
    Ok((take(out_l), take(out_r)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_dsd_file_comes_out_at_44k() {
        use crate::audio::dsd::decimate::test_signal::modulate;
        use crate::audio::dsd::dsf::test_files::{dsf, write};
        let bytes = 120_000;
        let ch = modulate(|t| 0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin(), 2_822_400.0, bytes * 8);
        let (_d, path) = write(&dsf(&[ch.clone(), ch], 2_822_400, bytes as u64 * 8, 4096, false, None), "s.dsf");
        let (l, r) = super::decode_44k(&path, &|| false).expect("decode");
        assert_eq!(l.len(), bytes * 8 / 64);
        assert_eq!(l, r);
        let peak = l[3000..l.len() - 3000].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.01, "{peak}");
    }
}
