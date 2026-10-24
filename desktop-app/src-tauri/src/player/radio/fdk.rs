//! The FDK AAC decoder (Rust, from AOSP: vendor/aac) for HE-AAC (`he_aac`):
//! a stream's ADTS frames one by one, out in f64.
//!
//! It is set to give the stream as it is: no limiter, no loudness
//! normalisation, no dynamic range control — left to its defaults it would
//! bring a stream that carries a programme reference level to −24 dB, and
//! hold its peaks under full scale. Its output runs one frame behind other
//! decoders' (Windows' own, symphonia's): the first frame a new decoder
//! gives is its start-up (silence), which is dropped, so that its samples
//! come at the places theirs did. A panic inside it is caught: that frame
//! is lost and a decoder made afresh goes on (`restarted`).

use aac::aac_dec::{AacDecoderError, AacDecoderInstance, DrcEffectTypeRequest, LimiterMode, OutputInfo, Param, TransportType};

use super::he_aac::Format;

/// One frame out at most: 2048 samples (with SBR) of 8 channels.
const OUT_MAX: usize = 2048 * 8;

#[cfg(test)]
thread_local! {
    /// The next frame panics inside the decoder's call, on this thread
    /// (tests: what a panic leaves behind).
    pub static PANIC_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The FDK AAC decoder on ADTS frames: AAC-LC, HE-AAC and HE-AAC v2, out at
/// the rate SBR gives.
pub struct FdkAac {
    dec: AacDecoderInstance,
    /// What the decoder writes a frame into, in its own format: it computes
    /// in f32. Each frame is taken into f64 as it comes out (`take`).
    frame: Vec<f32>,
    format: Format,
    /// Frames to drop before any is given: a new decoder's start-up.
    skip: usize,
    /// Made afresh after a panic, since `restarted` was last asked.
    restarted: bool,
}

impl FdkAac {
    pub fn new() -> Result<FdkAac, String> {
        Ok(FdkAac { dec: make()?, frame: vec![0.0; OUT_MAX], format: Format { rate: 0, channels: 0 }, skip: 1, restarted: false })
    }

    /// What it gives out (known once a frame is decoded).
    pub fn format(&self) -> Format {
        self.format
    }

    /// Made afresh after a panic since this was last asked: the stream goes
    /// on from a new decoder (another start, another state).
    pub fn restarted(&mut self) -> bool {
        std::mem::take(&mut self.restarted)
    }

    /// One ADTS frame (header and all) in; what the decoder gives for it onto
    /// `out`, interleaved, in f64. An error says the frame did not decode;
    /// what the decoder gave in its place (its concealment, which keeps the
    /// stream's time) is on `out` all the same.
    pub fn push(&mut self, frame: &[u8], out: &mut Vec<f64>) -> Result<(), String> {
        let FdkAac { dec, frame: buf, format, skip, .. } = &mut *self;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
            #[cfg(test)]
            if PANIC_NEXT.with(|p| p.replace(false)) {
                panic!("a test's panic inside the decoder");
            }
            let left = dec.fill(frame, frame.len()).map_err(|e| format!("AAC decoder: {:?}", e))?;
            if left > 0 {
                return Err(format!("AAC decoder: {} bytes of a frame not taken", left));
            }
            let mut error = None;
            loop {
                let (info, e) = match dec.decode(buf) {
                    Ok(i) => (i.output_info, None),
                    Err((AacDecoderError::NotEnoughBits, _)) => break,
                    Err((e, i)) => (i.output_info, Some(e)),
                };
                if let Some(e) = e {
                    error = Some(format!("AAC decoder: {:?}", e));
                    if e.is_output_valid() {
                        take(&info, buf, format, skip, out);
                    }
                    // The frame is spent: what follows is the next one's.
                    break;
                }
                take(&info, buf, format, skip, out);
            }
            error.map_or(Ok(()), Err)
        }));
        r.unwrap_or_else(|_| {
            // Its state is not to be trusted past a panic: a new one.
            self.dec = make()?;
            self.skip = 1;
            self.restarted = true;
            Err("AAC decoder: a frame it could not take (made afresh)".into())
        })
    }

    /// The end of the stream (a file's): the frame the decoder still holds,
    /// onto `out`.
    pub fn finish(&mut self, out: &mut Vec<f64>) -> Result<(), String> {
        let FdkAac { dec, frame: buf, format, skip, .. } = &mut *self;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match dec.drain(buf) {
            Ok(info) => {
                take(&info, buf, format, skip, out);
                Ok(())
            }
            Err((e, _)) => Err(format!("AAC decoder, at the end: {:?}", e)),
        }));
        r.unwrap_or_else(|_| Err("AAC decoder, at the end: it could not give its last frame".into()))
    }
}

/// A decoder set to give the stream as it is.
fn make() -> Result<AacDecoderInstance, String> {
    let mut d = AacDecoderInstance::new(TransportType::Mp4Adts);
    for p in [
        Param::PcmLimiterEnable(LimiterMode::Off),
        // Negative: no loudness normalisation, no MPEG-4 DRC.
        Param::DrcReferenceLevel(-1),
        // No MPEG-D DRC (xHE-AAC's).
        Param::UnidrcSetEffect(DrcEffectTypeRequest::Off),
    ] {
        d.set_param(p).map_err(|e| format!("AAC decoder: {:?}", e))?;
    }
    Ok(d)
}

/// A frame the decoder wrote into `buf` (`o` says how much of it), onto
/// `out` — unless it is a new decoder's start-up, still to be skipped.
fn take(o: &OutputInfo, buf: &[f32], format: &mut Format, skip: &mut usize, out: &mut Vec<f64>) {
    let n = (usize::from(o.frame_size) * usize::from(o.num_channels)).min(buf.len());
    if n == 0 {
        return;
    }
    *format = Format { rate: o.sampling_rate, channels: u32::from(o.num_channels) };
    if *skip > 0 {
        *skip -= 1;
        return;
    }
    // DSP_MANIFESTO §4.1: f32 ends here. The decoder computes in f32 and
    // gives its frame so; it is taken into f64 as it comes out, and nothing
    // of ours works on it in f32.
    out.extend(buf[..n].iter().map(|&v| f64::from(v)));
}
