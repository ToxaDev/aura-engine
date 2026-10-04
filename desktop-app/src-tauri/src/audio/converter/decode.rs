use crate::audio::converter::state::*;
use crate::audio::converter::types::AudioFile;
use crate::audio::tag_text::{self, Field};
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
    STEP_LISTENER.with(|l| {
        if let Ok(mut l) = l.try_borrow_mut() {
            if let Some(hear) = l.as_mut() {
                hear(s);
            }
        }
    });
}

thread_local! {
    /// Hears the status lines this thread sets (`with_step_listener`).
    static STEP_LISTENER: std::cell::RefCell<Option<Box<dyn FnMut(&str)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `listener` hearing every status line this thread sets
/// meanwhile. The player's preparations take their steps from them: the
/// engine announces each source stage before it runs it.
pub fn with_step_listener<R>(listener: impl FnMut(&str) + 'static, f: impl FnOnce() -> R) -> R {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            STEP_LISTENER.with(|l| {
                if let Ok(mut l) = l.try_borrow_mut() {
                    *l = None;
                }
            });
        }
    }
    STEP_LISTENER.with(|l| *l.borrow_mut() = Some(Box::new(listener)));
    let _off = Off;
    f()
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
    let mut format = symphonia::default::get_probe()
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
    let frames = if chained_ogg(path) { ogg_links_frames(&mut *format, frames) } else { frames };
    Some((frames, rate, channels))
}

/// An Ogg file whose last page belongs to another logical stream than its
/// first: a chained one — its container counts only the first link. (Streams
/// side by side, the other way to get there, read through without a seam.)
/// Reads the first page header and the last 64 KB.
pub(crate) fn chained_ogg(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 27];
    if f.read_exact(&mut head).is_err() || &head[..4] != b"OggS" {
        return false;
    }
    let serial = |b: &[u8]| u32::from_le_bytes([b[14], b[15], b[16], b[17]]);
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let tail_len = len.min(65_536);
    let mut tail = vec![0u8; tail_len as usize];
    if f.seek(SeekFrom::Start(len - tail_len)).is_err() || f.read_exact(&mut tail).is_err() {
        return false;
    }
    let last = (0..tail.len().saturating_sub(26)).rev().find(|&i| &tail[i..i + 4] == b"OggS" && tail[i + 4] == 0);
    last.is_some_and(|i| serial(&tail[i..]) != serial(&head))
}

/// The frames `decode_file` makes of a chained Ogg file: its first link's and
/// those of every next one at the same rate with the same channels, read
/// link by link from the demuxer (no decode). The format reader is spent.
pub(crate) fn ogg_links_frames(format: &mut dyn symphonia::core::formats::FormatReader, first: u64) -> u64 {
    let of = |format: &dyn symphonia::core::formats::FormatReader| {
        format.tracks().iter().find_map(|t| {
            let p = audio_params(t)?;
            Some((t.num_frames, p.sample_rate, p.channels.as_ref().map(|c| c.count()).unwrap_or(2)))
        })
    };
    let Some((_, rate, channels)) = of(format) else { return first };
    let mut total = first;
    loop {
        match format.next_packet() {
            Ok(Some(_)) => {}
            Err(symphonia::core::errors::Error::ResetRequired) => match of(format) {
                Some((Some(n), r, c)) if r == rate && c == channels => total += n,
                _ => break,
            },
            _ => break,
        }
    }
    total
}

/// A lossy codec by the name the badges show; None for a lossless one (PCM,
/// FLAC, ALAC, WavPack…).
fn lossy_codec(codec: symphonia::core::codecs::audio::AudioCodecId) -> Option<&'static str> {
    use symphonia::core::codecs::audio::well_known::*;
    match codec {
        CODEC_ID_MP1 => Some("MP1"),
        CODEC_ID_MP2 => Some("MP2"),
        CODEC_ID_MP3 => Some("MP3"),
        CODEC_ID_AAC => Some("AAC"),
        CODEC_ID_VORBIS => Some("Vorbis"),
        CODEC_ID_OPUS => Some("Opus"),
        _ => None,
    }
}

/// The lossy codec of a file's audio, from its container header (no decode);
/// None for a lossless one, or a file that cannot be read.
pub fn probe_lossy_codec(path: &Path) -> Option<&'static str> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    let track = format.tracks().iter().find(|t| audio_params(t).is_some())?;
    lossy_codec(audio_params(track)?.codec)
}

/// The audio codec parameters of a track, if it carries audio at all.
fn audio_params(
    track: &symphonia::core::formats::Track,
) -> Option<&symphonia::core::codecs::audio::AudioCodecParameters> {
    track.codec_params.as_ref().and_then(|p| p.audio())
}

/// Where a decode puts its frames (`decode_into`).
trait Sink {
    fn push(&mut self, l: f64, r: f64);
    fn len(&self) -> usize;
    fn drain(&mut self, range: std::ops::Range<usize>);
    fn truncate(&mut self, keep: usize);
    /// Frame `i`, as f64.
    fn frame(&self, i: usize) -> (f64, f64);
    /// The largest sample magnitude, and how many frames are past full
    /// scale.
    fn peak_over(&self) -> (f64, u64) {
        let mut peak = 0.0_f64;
        let mut over = 0_u64;
        for i in 0..self.len() {
            let (l, r) = self.frame(i);
            let m = l.abs().max(r.abs());
            if m > peak {
                peak = m;
            }
            if m > 1.0 {
                over += 1;
            }
        }
        (peak, over)
    }
}

/// `decode_file`'s frames: two f64 channel buffers.
#[derive(Default)]
struct F64Planes {
    l: Vec<f64>,
    r: Vec<f64>,
}

impl F64Planes {
    fn with_capacity(n: usize) -> F64Planes {
        F64Planes { l: Vec::with_capacity(n), r: Vec::with_capacity(n) }
    }
}

impl Sink for F64Planes {
    fn push(&mut self, l: f64, r: f64) {
        self.l.push(l);
        self.r.push(r);
    }
    fn len(&self) -> usize {
        self.l.len()
    }
    fn drain(&mut self, range: std::ops::Range<usize>) {
        self.l.drain(range.clone());
        self.r.drain(range);
    }
    fn truncate(&mut self, keep: usize) {
        self.l.truncate(keep);
        self.r.truncate(keep);
    }
    fn frame(&self, i: usize) -> (f64, f64) {
        (self.l[i], self.r[i])
    }
}

/// 2^-31: symphonia hands an integer sample over as integer / 2^31 — i16,
/// i24 and i32 alike (`append_packet`).
const INT_SCALE: f64 = 1.0 / 2_147_483_648.0;

/// `x` as the i32 that reads back as `x` to the bit (`q as f64 · 2^-31`),
/// if there is one: a fraction of the grid, −0.0, a NaN or a value out of
/// range has none.
fn exact_i32(x: f64) -> Option<i32> {
    let q = (x * 2_147_483_648.0) as i32;
    ((q as f64 * INT_SCALE).to_bits() == x.to_bits()).then_some(q)
}

/// Decoded channels kept in half the memory of f64 where the samples allow
/// it: a sample is stored as the i32 that reads back as the decoder's f64
/// to the bit, and the first sample that has none (a float or lossy
/// source) turns the whole buffer into f64, as `decode_file` keeps it. A
/// converted file — 24-bit FLAC — stays integer from start to end.
pub enum Planes {
    Int { l: Vec<i32>, r: Vec<i32> },
    Float { l: Vec<f64>, r: Vec<f64> },
}

impl Planes {
    fn with_capacity(n: usize) -> Planes {
        Planes::Int { l: Vec::with_capacity(n), r: Vec::with_capacity(n) }
    }

    /// Frames held.
    pub fn len(&self) -> usize {
        match self {
            Planes::Int { l, .. } => l.len(),
            Planes::Float { l, .. } => l.len(),
        }
    }

    /// Bytes the samples take.
    pub fn bytes(&self) -> usize {
        match self {
            Planes::Int { l, .. } => l.len() * 2 * std::mem::size_of::<i32>(),
            Planes::Float { l, .. } => l.len() * 2 * std::mem::size_of::<f64>(),
        }
    }

    /// Frames from `from` on into `out_l`/`out_r`, as the f64 the decoder
    /// made: as many as both hold and there are. Returns how many.
    pub fn read(&self, from: usize, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len().min(out_r.len()).min(self.len().saturating_sub(from));
        match self {
            Planes::Int { l, r } => {
                for i in 0..n {
                    out_l[i] = l[from + i] as f64 * INT_SCALE;
                    out_r[i] = r[from + i] as f64 * INT_SCALE;
                }
            }
            Planes::Float { l, r } => {
                out_l[..n].copy_from_slice(&l[from..from + n]);
                out_r[..n].copy_from_slice(&r[from..from + n]);
            }
        }
        n
    }

    /// Every frame as f64 (tests).
    #[cfg(test)]
    pub fn to_f64(&self) -> (Vec<f64>, Vec<f64>) {
        let (mut l, mut r) = (vec![0.0; self.len()], vec![0.0; self.len()]);
        self.read(0, &mut l, &mut r);
        (l, r)
    }

    /// Into f64, every sample as it reads.
    fn promote(&mut self) {
        if let Planes::Int { l, r } = self {
            let widen = |v: &[i32], cap: usize| {
                let mut out = Vec::with_capacity(cap.max(v.len()));
                out.extend(v.iter().map(|&q| q as f64 * INT_SCALE));
                out
            };
            let (fl, fr) = (widen(l, l.capacity()), widen(r, r.capacity()));
            *self = Planes::Float { l: fl, r: fr };
        }
    }
}

impl Sink for Planes {
    fn push(&mut self, l: f64, r: f64) {
        if let Planes::Int { l: il, r: ir } = self {
            if let (Some(a), Some(b)) = (exact_i32(l), exact_i32(r)) {
                il.push(a);
                ir.push(b);
                return;
            }
            self.promote();
        }
        if let Planes::Float { l: fl, r: fr } = self {
            fl.push(l);
            fr.push(r);
        }
    }
    fn len(&self) -> usize {
        Planes::len(self)
    }
    fn drain(&mut self, range: std::ops::Range<usize>) {
        match self {
            Planes::Int { l, r } => {
                l.drain(range.clone());
                r.drain(range);
            }
            Planes::Float { l, r } => {
                l.drain(range.clone());
                r.drain(range);
            }
        }
    }
    fn truncate(&mut self, keep: usize) {
        match self {
            Planes::Int { l, r } => {
                l.truncate(keep);
                r.truncate(keep);
            }
            Planes::Float { l, r } => {
                l.truncate(keep);
                r.truncate(keep);
            }
        }
    }
    fn frame(&self, i: usize) -> (f64, f64) {
        match self {
            Planes::Int { l, r } => (l[i] as f64 * INT_SCALE, r[i] as f64 * INT_SCALE),
            Planes::Float { l, r } => (l[i], r[i]),
        }
    }
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
    out: &mut impl Sink,
) {
    let ch = decoded.spec().channels().count().max(1);
    decoded.copy_to_vec_interleaved(interleaved);

    for frame in interleaved.chunks_exact(ch) {
        let l = frame[0];
        out.push(l, if ch >= 2 { frame[1] } else { l });
    }
}

/// One logical stream of a file — the whole file, but for a chained Ogg one,
/// which is several, one after another: where its samples begin, and the
/// encoder's priming and padding frames at its edges.
struct Link {
    start: usize,
    delay: usize,
    padding: usize,
}

impl Link {
    /// Drop the link's priming from its head and its padding from its tail,
    /// so the framing silence never reaches the FIR chain. Its samples are the
    /// last ones in the buffers.
    fn trim(&self, out: &mut impl Sink) {
        let (start, delay, padding) = (self.start, self.delay, self.padding);
        if delay > 0 && out.len() - start > delay {
            out.drain(start..start + delay);
        }
        if padding > 0 && out.len() - start > padding {
            let keep = out.len() - padding;
            out.truncate(keep);
        }
        if delay > 0 || padding > 0 {
            crate::aelog!(
                "[CONV] Gapless trim: -{} priming, -{} padding samples",
                delay, padding
            );
        }
    }
}

/// Decode audio file using symphonia
pub fn decode_file(path: &Path) -> Result<AudioFile, String> {
    let (out, d) = decode_into(path, F64Planes::with_capacity)?;
    Ok(AudioFile {
        samples_l: out.l,
        samples_r: out.r,
        sample_rate: d.sample_rate,
        channels: d.channels,
        artist: d.artist,
        title: d.title,
        lossy: d.lossy,
    })
}

/// A file decoded as `decode_file` decodes it — the same samples — into
/// `Planes`: as exact integers where every sample is one (the player's disk
/// chain; a converted file is 24-bit FLAC). With the file's rate.
pub fn decode_file_pcm(path: &Path) -> Result<(Planes, u32), String> {
    let (out, d) = decode_into(path, Planes::with_capacity)?;
    Ok((out, d.sample_rate))
}

/// What a decode tells about the file besides its samples.
struct Decoded {
    sample_rate: u32,
    channels: usize,
    artist: String,
    title: String,
    lossy: Option<&'static str>,
}

/// The decode, into the sink `make` gives for the frames the container
/// announces.
fn decode_into<S: Sink>(path: &Path, make: impl FnOnce(usize) -> S) -> Result<(S, Decoded), String> {
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

    // Extract metadata: every revision the reader holds (ID3v2 in front of
    // an MP3 stream, ID3v1 at its end), read as the player reads them
    // (player/probe.rs, audio/tag_text.rs) — ID3v2 before ID3v1, a Windows
    // code page in a Latin-1 frame as written, a title read so that lost a
    // byte giving way to the file's name.
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let (mut artist_f, mut title_f) = (Field::default(), Field::default());
    let mut metadata = format.metadata();
    while let Some(revision) = metadata.current() {
        let reader = revision.info.short_name;
        for tag in &revision.media.tags {
            match &tag.std {
                Some(StandardTag::Artist(v)) => artist_f.offer(reader, v.as_str()),
                Some(StandardTag::TrackTitle(v)) => title_f.offer(reader, v.as_str()),
                _ => {}
            }
        }
        if !metadata.is_latest() {
            metadata.pop();
        } else {
            break;
        }
    }
    let cyr = tag_text::file_reads_1251([&artist_f, &title_f].into_iter().filter_map(Field::id3));
    let (artist, _) = artist_f.read(cyr, &stem);
    let (mut title, title_1251) = title_f.read(cyr, &stem);
    if title_1251 && title.contains('\u{FFFD}') {
        title.clear();
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
    let mut track_id = track.id;
    let mut lossy = lossy_codec(params.codec);

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

    // HE-AAC (AAC with SBR, and PS): the HE-AAC decoder (the FDK AAC decoder:
    // player/radio/he_aac.rs) reads what symphonia's skips — the top octave,
    // the stereo of a mono core — and gives the full rate; symphonia would
    // give the core alone, or refuse an explicitly signalled one.
    if let Some(core) = he_aac_core(&params) {
        let tb_rate = track.time_base.map(|t| t.denom.get() / t.numer.get());
        return decode_he_aac(format, track_id, core, (enc_delay, enc_padding, tb_rate), make(n_frames_hint * 2), artist, title);
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


    let mut out = make(n_frames_hint);
    let mut packet_decode_errors: u64 = 0;

    let mut interleaved: Vec<f64> = Vec::new();
    let mut link = Link { start: 0, delay: enc_delay, padding: enc_padding };
    let mut links = 1usize;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A chained Ogg file (songs of a recorded stream, files joined end
            // to end): the next logical stream begins. It goes on where this
            // one ends at the same rate with the same channels; one that is
            // not ends the decode at the seam — a track has one rate.
            Err(symphonia::core::errors::Error::ResetRequired) => {
                let next = format.tracks().iter().find_map(|t| {
                    let p = audio_params(t)?;
                    Some((t.id, p.clone(), t.delay.unwrap_or(0) as usize, t.padding.unwrap_or(0) as usize))
                });
                let Some((next_id, next_params, next_delay, next_padding)) = next else {
                    crate::aelog!(
                        "[CONV] WARNING: decode stopped at link {} of a chained Ogg file ({} samples decoded): it carries no audio",
                        links + 1,
                        out.len()
                    );
                    break;
                };
                let next_channels = next_params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
                if next_params.sample_rate != Some(sample_rate) || next_channels != channels {
                    crate::aelog!(
                        "[CONV] WARNING: decode stopped at link {} of a chained Ogg file ({} samples decoded): it is {} Hz, {} ch, the file before it {} Hz, {} ch",
                        links + 1,
                        out.len(),
                        next_params.sample_rate.unwrap_or(0),
                        next_channels,
                        sample_rate,
                        channels
                    );
                    break;
                }
                decoder = match symphonia::default::get_codecs().make_audio_decoder(&next_params, &decode_opts) {
                    Ok(d) => d,
                    Err(e) => {
                        crate::aelog!(
                            "[CONV] WARNING: decode stopped at link {} of a chained Ogg file ({} samples decoded): {}",
                            links + 1,
                            out.len(),
                            e
                        );
                        break;
                    }
                };
                link.trim(&mut out);
                link = Link { start: out.len(), delay: next_delay, padding: next_padding };
                track_id = next_id;
                lossy = lossy.or(lossy_codec(next_params.codec));
                links += 1;
                continue;
            }
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
                    out.len(),
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

        append_packet(decoded, &mut interleaved, &mut out);
    }

    if out.len() == 0 {
        return Err("No audio samples decoded".to_string());
    }
    if packet_decode_errors > 0 {
        crate::aelog!(
            "[CONV] WARNING: {} corrupt packets skipped during decode",
            packet_decode_errors
        );
    }

    // ── Gapless trim (MP3/AAC; Ogg's start and end bounds) ──
    // The last link's (every other one was trimmed at its seam): encoder
    // priming from its head, padding from its tail.
    link.trim(&mut out);
    if links > 1 {
        crate::aelog!("[CONV] Chained Ogg: {} links decoded one after another", links);
    }

    // Lossy decoders and float WAV legitimately produce samples past full
    // scale. Worth a line: it is the difference between a file the output
    // ceiling brings down and one that was clipped on the way in, and until
    // 1.2.9 this engine did the clipping itself.
    let (peak, over) = out.peak_over();
    if over > 0 {
        crate::aelog!(
            "[CONV] Source peaks at +{:.2} dBFS — {} frames ({:.3}%) above full scale, kept as decoded; the output ceiling scales them down",
            20.0 * peak.log10(),
            over,
            100.0 * over as f64 / out.len().max(1) as f64
        );
    }

    crate::aelog!(
        "[CONV] Decoded: {}Hz {}ch {} samples, artist='{}', title='{}'",
        sample_rate,
        channels,
        out.len(),
        artist,
        title
    );

    Ok((out, Decoded { sample_rate, channels, artist, title, lossy }))
}

/// The core of an AAC track that goes to the HE-AAC decoder: SBR or PS
/// signalled in its AudioSpecificConfig, or (ADTS, or nothing signalled) a
/// core at HE-AAC's rates, where SBR rides unannounced.
fn he_aac_core(p: &symphonia::core::codecs::audio::AudioCodecParameters) -> Option<crate::player::radio::adts::Header> {
    use crate::player::radio::adts::{parse_asc, Header, RATES};
    if p.codec != symphonia::core::codecs::audio::well_known::CODEC_ID_AAC {
        return None;
    }
    let core = match p.extra_data.as_deref() {
        Some(asc) => parse_asc(asc).filter(|a| a.sbr || a.ps || a.core.he_core()).map(|a| a.core)?,
        None => {
            let rate_index = RATES.iter().position(|&r| Some(r) == p.sample_rate)? as u8;
            Header { object: 2, rate_index, channels: p.channels.as_ref()?.count() as u8, len: 0 }
        }
    };
    (core.object == 2 && core.he_core() && (1..=2).contains(&core.channels)).then_some(core)
}

/// An HE-AAC track through the HE-AAC decoder: each raw AAC frame given an
/// ADTS header of its core, as a radio stream's come, and at the end the
/// frames the decoder still holds. The container's priming and padding
/// (`gapless`: delay, padding, the time base's rate) are trimmed as on
/// symphonia's path, at the output's rate.
fn decode_he_aac<S: Sink>(
    mut format: Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    core: crate::player::radio::adts::Header,
    gapless: (usize, usize, Option<u32>),
    mut out: S,
    artist: String,
    title: String,
) -> Result<(S, Decoded), String> {
    use crate::player::radio::adts::Header;
    let mut dec = crate::player::radio::he_aac::HeAac::new(&core).map_err(|e| format!("Codec error: {}", e))?;
    let (mut frame, mut pcm) = (Vec::new(), Vec::new());
    let mut errors = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                crate::aelog!("[CONV] WARNING: decode stopped early ({} samples decoded): {}", out.len(), e);
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        frame.clear();
        if Header::parse(&packet.data).map_or(false, |h| h.len == packet.data.len()) {
            frame.extend_from_slice(&packet.data);
        } else {
            frame.extend_from_slice(&core.bytes(packet.data.len()));
            frame.extend_from_slice(&packet.data);
        }
        pcm.clear();
        // A packet that does not decode is counted; what the decoder gave in
        // its place, if anything, keeps the file's time.
        if let Err(e) = dec.push(&frame, &mut pcm) {
            errors += 1;
            if errors <= 5 {
                crate::aelog!("[CONV] WARNING: corrupt packet: {}", e);
            }
        }
        let ch = dec.format().channels.max(1) as usize;
        for f in pcm.chunks_exact(ch) {
            out.push(f[0], if ch >= 2 { f[1] } else { f[0] });
        }
    }
    // The last frames the decoder holds back until it is told the stream
    // is over.
    pcm.clear();
    if let Err(e) = dec.finish(&mut pcm) {
        crate::aelog!("[CONV] WARNING: the decoder's last frames: {}", e);
    }
    let ch = dec.format().channels.max(1) as usize;
    for f in pcm.chunks_exact(ch) {
        out.push(f[0], if ch >= 2 { f[1] } else { f[0] });
    }
    if out.len() == 0 {
        return Err("No audio samples decoded".to_string());
    }
    if errors > 0 {
        crate::aelog!("[CONV] WARNING: {} corrupt packets during decode", errors);
    }
    let fmt = dec.format();
    let (delay, padding, tb_rate) = gapless;
    let at_out = |n: usize| tb_rate.filter(|&r| r > 0 && r != fmt.rate).map_or(n, |r| n * fmt.rate as usize / r as usize);
    Link { start: 0, delay: at_out(delay), padding: at_out(padding) }.trim(&mut out);
    let (peak, _) = out.peak_over();
    if peak > 1.0 {
        crate::aelog!("[CONV] Source peaks at +{:.2} dBFS, kept as decoded; the output ceiling scales them down", 20.0 * peak.log10());
    }
    crate::aelog!(
        "[CONV] Decoded: {} (the FDK AAC decoder, a {} Hz core) {}Hz {}ch {} samples, artist='{}', title='{}'",
        dec.codec(),
        core.rate(),
        fmt.rate,
        fmt.channels,
        out.len(),
        artist,
        title
    );
    let lossy = Some(dec.codec());
    Ok((out, Decoded { sample_rate: fmt.rate, channels: fmt.channels as usize, artist, title, lossy }))
}

/// Chained Ogg files for tests: Ogg FLAC links built here, page by page.
#[cfg(test)]
pub(crate) mod test_ogg {
    /// Ogg's CRC-32: polynomial 0x04C11DB7, not reflected, from 0.
    fn ogg_crc(data: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &b in data {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
            }
        }
        crc
    }

    /// One Ogg page carrying one whole packet.
    fn ogg_page(out: &mut Vec<u8>, flags: u8, granule: u64, serial: u32, seq: u32, packet: &[u8]) {
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        assert!(lacing.len() <= 255, "one packet fits one page");
        let start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        out.extend_from_slice(packet);
        let crc = ogg_crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
    }

    pub(crate) const LINK_BLOCK: usize = 1024;

    /// One logical stream of Ogg FLAC, 16-bit stereo in frames of LINK_BLOCK
    /// (the length a multiple of it), appended to `out`.
    pub(crate) fn ogg_flac_link(out: &mut Vec<u8>, serial: u32, rate: u32, l: &[i32], r: &[i32]) {
        use flacenc::bitsink::ByteSink;
        use flacenc::component::{BitRepr, StreamInfo};
        use flacenc::error::Verify;
        use flacenc::source::{Fill, FrameBuf};
        let mut cfg = flacenc::config::Encoder::default();
        cfg.block_size = LINK_BLOCK;
        cfg.multithread = false;
        let cfg = cfg.into_verified().expect("config");
        let verbatim = crate::audio::converter::encode::verbatim_config(LINK_BLOCK).expect("verbatim config");
        let mut info = StreamInfo::new(rate as usize, 2, 16).expect("stream info");
        info.set_block_sizes(LINK_BLOCK, LINK_BLOCK).expect("block sizes");
        let mut sink = ByteSink::new();
        info.write(&mut sink).expect("stream info bytes");
        // The identification packet: mapping 1.0, the number of header
        // packets unknown, then STREAMINFO as the last metadata block.
        let mut id = vec![0x7F];
        id.extend_from_slice(b"FLAC");
        id.extend_from_slice(&[1, 0, 0, 0]);
        id.extend_from_slice(b"fLaC");
        id.extend_from_slice(&[0x80, 0, 0, 34]);
        id.extend_from_slice(sink.as_slice());
        assert_eq!(id.len(), 51);
        ogg_page(out, 0x02, 0, serial, 0, &id);
        let mut fb = FrameBuf::with_size(2, LINK_BLOCK).expect("frame buffer");
        let frames = l.len() / LINK_BLOCK;
        for f in 0..frames {
            let block: Vec<i32> = (f * LINK_BLOCK..(f + 1) * LINK_BLOCK).flat_map(|i| [l[i], r[i]]).collect();
            fb.fill_interleaved(&block).expect("fill");
            let frame = crate::audio::converter::encode::encode_frame(std::slice::from_ref(&cfg), &verbatim, &fb, LINK_BLOCK, f, &info)
                .expect("frame");
            let mut s = ByteSink::new();
            frame.write(&mut s).expect("frame bytes");
            let flags = if f + 1 == frames { 0x04 } else { 0 };
            ogg_page(out, flags, ((f + 1) * LINK_BLOCK) as u64, serial, f as u32 + 1, s.as_slice());
        }
    }

    /// 16-bit stereo noise, the same for the same seed.
    pub(crate) fn noise(seed: u32, frames: usize) -> (Vec<i32>, Vec<i32>) {
        let mut x = seed;
        let mut next = move || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 16) as i32 & 0x7FFF) - 0x4000
        };
        let l = (0..frames).map(|_| next()).collect();
        let r = (0..frames).map(|_| next()).collect();
        (l, r)
    }

    /// A chained Ogg FLAC file of noise: one link per (frames, rate).
    pub(crate) fn chained(links: &[(usize, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, &(frames, rate)) in links.iter().enumerate() {
            let (l, r) = noise(i as u32 + 1, frames);
            ogg_flac_link(&mut out, 0x1000 + i as u32, rate, &l, &r);
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
        let (mut out, mut scratch) = (F64Planes::default(), Vec::new());
        append_packet(decoded, &mut scratch, &mut out);
        (out.l, out.r)
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

    // ── The disk chain's decode: exact integers where they are ─────────

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// A sample is kept as an i32 only when it reads back as the same f64 to
    /// the bit; a fraction of the grid, −0.0, a NaN or a value out of range
    /// is not.
    #[test]
    fn an_integer_is_kept_only_when_it_reads_back_to_the_bit() {
        assert_eq!(exact_i32(0.5), Some(1 << 30));
        assert_eq!(exact_i32(-1.0), Some(i32::MIN));
        assert_eq!(exact_i32(1.0 / 2_147_483_648.0), Some(1));
        assert_eq!(exact_i32(8_388_607.0 / 8_388_608.0), Some(8_388_607 << 8));
        assert_eq!(exact_i32(0.0), Some(0));
        for x in [1.0, 1.5, 0.1, 1.0 / 4_294_967_296.0, -0.0, f64::NAN, f64::INFINITY, -2.0] {
            assert_eq!(exact_i32(x), None, "{x}");
        }
    }

    /// Integers stay integers; the first sample that is none turns the
    /// buffer into f64, every sample as it was, and the trims act on either.
    #[test]
    fn planes_turn_to_f64_at_the_first_sample_that_is_no_integer() {
        let mut p = Planes::with_capacity(8);
        p.push(0.5, -0.25);
        p.push(-1.0, 0.0);
        p.push(0.75, 0.125);
        assert!(matches!(p, Planes::Int { .. }));
        assert_eq!(p.bytes(), 3 * 8);
        Sink::drain(&mut p, 1..2);
        p.push(0.1, 0.5);
        p.push(0.375, -0.0);
        assert!(matches!(p, Planes::Float { .. }));
        assert_eq!(p.bytes(), 4 * 16);
        Sink::truncate(&mut p, 3);
        let (l, r) = p.to_f64();
        assert_eq!(bits(&l), bits(&[0.5, 0.75, 0.1]));
        assert_eq!(bits(&r), bits(&[-0.25, 0.125, 0.5]));
    }

    /// A converted file's kind of signal, on the 24-bit grid: two tones with
    /// a little noise, and from frame 1000 a full-scale sine that sits on
    /// both rails (sin(π/2) and sin(3π/2) are ±1 exactly).
    pub(crate) fn grid_signal(n: usize, seed: u32) -> (Vec<f64>, Vec<f64>) {
        let grid = 8_388_608.0;
        let mut x = seed;
        let mut at = |i: usize, f: f64| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (x >> 8) as f64 / (1u64 << 24) as f64 - 0.5;
            let v = if (1_000..1_500).contains(&i) {
                (std::f64::consts::PI * i as f64 / 8.0).sin()
            } else {
                0.5 * (f * i as f64).sin() + 0.01 * noise
            };
            (v * grid).round().clamp(-grid, grid - 1.0) / grid
        };
        let l = (0..n).map(|i| at(i, 0.0652)).collect();
        let r = (0..n).map(|i| at(i, 0.0981)).collect();
        (l, r)
    }

    /// A 32-bit float WAV of `l`/`r` (format 3, IEEE float).
    fn write_float_wav(path: &Path, rate: u32, l: &[f64], r: &[f64]) {
        let n = l.len().min(r.len());
        let data = (n * 8) as u32;
        let mut b = Vec::with_capacity(46 + n * 8);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(38 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&18u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            b.extend_from_slice(&(l[i] as f32).to_le_bytes());
            b.extend_from_slice(&(r[i] as f32).to_le_bytes());
        }
        std::fs::write(path, b).expect("write the test file");
    }

    /// The disk chain's decode is `decode_file`'s, sample for sample: the
    /// converter's 24-bit FLAC, a 16-bit WAV and a chained Ogg FLAC kept as
    /// integers in half the memory, a float WAV as f64. And `decode_file`
    /// still gives the 24-bit file's codes exactly — what the converter and
    /// the player's variants decode is what it was.
    #[test]
    fn the_disk_chains_decode_is_decode_files_to_the_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let n = 20_000usize;
        let grid = 8_388_608.0;
        let (mut l, mut r) = grid_signal(n, 99);
        l[200..900].fill(0.0);
        r[200..900].fill(0.0);
        let flac = dir.path().join("converted.flac");
        crate::audio::converter::encode::encode_flac(&l, &r, 96_000, &flac, &[]).expect("encode");
        let wav16 = dir.path().join("cd.wav");
        crate::player::chain::write_test_wav16(&wav16, 44_100, &l, &r);
        let ogg = dir.path().join("chained.ogg");
        std::fs::write(&ogg, chained(&[(2 * LINK_BLOCK, 44_100), (3 * LINK_BLOCK, 44_100)])).expect("write");
        let float = dir.path().join("float.wav");
        let quiet: Vec<f64> = l.iter().map(|v| v * 0.3).collect();
        write_float_wav(&float, 48_000, &quiet, &r);

        // The encoder's codes, read back as the decoder reads them (an integer
        // 0 is +0.0, whatever the sign of what was rounded to it).
        let codes: Vec<f64> = l.iter().map(|v| ((v * grid).round().clamp(-grid, grid - 1.0) as i32) as f64 / grid).collect();
        assert_eq!(bits(&decode_file(&flac).expect("decode").samples_l), bits(&codes), "decode_file: the codes");

        for (path, int) in [(&flac, true), (&wav16, true), (&ogg, true), (&float, false)] {
            let a = decode_file(path).expect("decode");
            let (p, rate) = decode_file_pcm(path).expect("decode for the disk chain");
            let name = path.display();
            assert_eq!(rate, a.sample_rate, "{name}");
            assert_eq!(matches!(p, Planes::Int { .. }), int, "{name}");
            assert_eq!(p.bytes(), a.samples_l.len() * if int { 8 } else { 16 }, "{name}");
            let (pl, pr) = p.to_f64();
            assert_eq!(bits(&pl), bits(&a.samples_l), "{name} L");
            assert_eq!(bits(&pr), bits(&a.samples_r), "{name} R");
        }
    }

    // ── A chained Ogg file, built here (test_ogg) ───────────────────────

    use super::test_ogg::{chained, noise, ogg_flac_link, LINK_BLOCK};

    fn as_f64(parts: &[&[i32]]) -> Vec<f64> {
        parts.iter().flat_map(|p| p.iter()).map(|&s| s as f64 / 32_768.0).collect()
    }

    fn decode_bytes(bytes: &[u8]) -> AudioFile {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chained.ogg");
        std::fs::write(&path, bytes).expect("write");
        decode_file(&path).expect("decode")
    }

    /// A chained Ogg file (songs of a recorded stream, files joined end to
    /// end) goes on through its seam: links of one rate and channel count
    /// come out one after the other, whole, bit for bit. The decode used to
    /// stop at the first seam — the demuxer's reset taken for an error.
    #[test]
    fn a_chained_ogg_file_goes_on_through_its_seam() {
        let (al, ar) = noise(1, 4 * LINK_BLOCK);
        let (bl, br) = noise(2, 3 * LINK_BLOCK);
        let mut file = Vec::new();
        ogg_flac_link(&mut file, 0x1111, 44_100, &al, &ar);
        ogg_flac_link(&mut file, 0x2222, 44_100, &bl, &br);
        let a = decode_bytes(&file);
        assert_eq!(a.sample_rate, 44_100);
        assert_eq!(a.samples_l.len(), 7 * LINK_BLOCK, "both links, whole");
        assert_eq!(a.samples_l, as_f64(&[&al, &bl]));
        assert_eq!(a.samples_r, as_f64(&[&ar, &br]));

        // One link alone: as it always came out.
        let mut one = Vec::new();
        ogg_flac_link(&mut one, 0x1111, 44_100, &al, &ar);
        let a = decode_bytes(&one);
        assert_eq!(a.samples_l, as_f64(&[&al]));
        assert_eq!(a.samples_r, as_f64(&[&ar]));
    }

    /// A link at another rate ends the decode at the seam: what came before
    /// it, whole, at its own rate (a track has one rate).
    #[test]
    fn a_chained_ogg_file_stops_at_a_link_of_another_rate() {
        let (al, ar) = noise(3, 2 * LINK_BLOCK);
        let (bl, br) = noise(4, 2 * LINK_BLOCK);
        let mut file = Vec::new();
        ogg_flac_link(&mut file, 0x3333, 44_100, &al, &ar);
        ogg_flac_link(&mut file, 0x4444, 48_000, &bl, &br);
        let a = decode_bytes(&file);
        assert_eq!(a.sample_rate, 44_100);
        assert_eq!(a.samples_l, as_f64(&[&al]));
        assert_eq!(a.samples_r, as_f64(&[&ar]));
    }

    /// The container counts a chained file's first link only: the frames the
    /// decode makes of it are those of every link up to one of another rate.
    #[test]
    fn a_chained_ogg_files_frames_are_its_links_of_one_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chained.ogg");
        let links = [(4 * LINK_BLOCK, 44_100), (3 * LINK_BLOCK, 44_100), (2 * LINK_BLOCK, 48_000), (LINK_BLOCK, 44_100)];
        std::fs::write(&path, chained(&links)).expect("write");
        assert_eq!(probe_input_frames(&path), Some(((7 * LINK_BLOCK) as u64, 44_100, 2)));
        assert_eq!(decode_file(&path).expect("decode").samples_l.len(), 7 * LINK_BLOCK);

        std::fs::write(&path, chained(&[(4 * LINK_BLOCK, 44_100)])).expect("write");
        assert_eq!(probe_input_frames(&path), Some(((4 * LINK_BLOCK) as u64, 44_100, 2)));
    }

    /// HE-AAC files made here by the FDK AAC encoder — v1 and v2, in ADTS
    /// (.aac) and in MP4 (.m4a, with the AudioSpecificConfig of its core
    /// alone, as the encoder gives it and ffmpeg copies an ADTS stream, and
    /// with one that signals SBR and PS: object type 5 or 29): each decodes
    /// at the full rate, named as it is, every frame of it to the last, and
    /// the MP4 ones give the ADTS one's samples.
    #[test]
    fn he_aac_files_decode_at_their_full_rate_every_frame_mp4_as_adts() {
        use crate::player::radio::adts::Header;
        use crate::player::radio::he_aac::tests::{bright, encode, frames_of, mp4, Profile};
        // An AudioSpecificConfig that signals SBR (object type 5) or PS (29)
        // on a core of object type 2: the core's rate index and channels, the
        // output's rate index, then the core's GASpecificConfig (all zero).
        let signalled = |aot: u32, core: &Header, out_index: u32| -> Vec<u8> {
            let fields = [(aot, 5), (u32::from(core.rate_index), 4), (u32::from(core.channels), 4), (out_index, 4), (2, 5), (0, 3)];
            let (bits, n) = fields.iter().fold((0u32, 0u32), |(b, n), &(v, w)| ((b << w) | v, n + w));
            (bits << (32 - n)).to_be_bytes().to_vec()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        for (profile, aot, name, bitrate) in [(Profile::He, 5, "HE-AAC", 48_000), (Profile::HeV2, 29, "HE-AAC v2", 32_000)] {
            let (stream, asc) = encode(profile, 44_100, 2, bitrate, &bright(44_100, 3.0));
            let frames = frames_of(&stream);
            let core = Header::parse(&frames[0]).unwrap();
            let files = [
                ("he.aac", stream.clone()),
                ("core.m4a", mp4(&asc, 44_100, 2, &frames, 2048)),
                ("signalled.m4a", mp4(&signalled(aot, &core, 4), 44_100, 2, &frames, 2048)),
            ];
            let decoded: Vec<AudioFile> = files
                .iter()
                .map(|(file, bytes)| {
                    let path = dir.path().join(file);
                    std::fs::write(&path, bytes).expect("write");
                    let f = decode_file(&path).unwrap_or_else(|e| panic!("{} {}: {}", name, file, e));
                    eprintln!("{} {}: {} Hz, {} ch, {:?}, {} frames", name, file, f.sample_rate, f.channels, f.lossy, f.samples_l.len());
                    assert_eq!((f.sample_rate, f.channels, f.lossy), (44_100, 2, Some(name)), "{}", file);
                    assert_eq!(f.samples_l.len(), frames.len() * 2048, "{} {}: every frame", name, file);
                    f
                })
                .collect();
            for (f, (file, _)) in decoded.iter().zip(&files).skip(1) {
                assert!(f.samples_l == decoded[0].samples_l && f.samples_r == decoded[0].samples_r, "{} {}: as the ADTS file", name, file);
            }
        }
    }

    /// HE-AAC files (dumps of live stations, kept outside the repository):
    /// AURA_TEST_HE_FILES="a.aac;a.m4a;…" — the same frames in ADTS and in
    /// MP4 (`ffmpeg -i a.aac -c copy a.m4a`). Each decodes through the HE-AAC
    /// decoder at the full rate, named HE-AAC; the MP4 one gives the ADTS
    /// one's samples (placed by the first second, the container's priming
    /// trimmed).
    #[test]
    #[ignore]
    fn he_aac_dumps_decode_at_their_full_rate() {
        let Ok(list) = std::env::var("AURA_TEST_HE_FILES") else {
            eprintln!("SKIPPED: AURA_TEST_HE_FILES is not set");
            return;
        };
        let files: Vec<AudioFile> = list
            .split(';')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let f = decode_file(Path::new(p)).expect("decodes");
                eprintln!("{}: {} Hz, {} ch, {:?}, {} frames", p, f.sample_rate, f.channels, f.lossy, f.samples_l.len());
                assert_eq!((f.sample_rate, f.channels), (44_100, 2), "{}", p);
                assert!(f.lossy.map_or(false, |c| c.starts_with("HE-AAC")), "{}: {:?}", p, f.lossy);
                f
            })
            .collect();
        for pair in files.chunks_exact(2) {
            let (a, m) = (&pair[0].samples_l, &pair[1].samples_l);
            let probe = &m[44_100..44_100 + 4096];
            let at = (0..a.len() - 4096).find(|&i| a[i..i + 4096] == *probe).expect("the MP4 decode is in the ADTS one");
            let shift = at as i64 - 44_100;
            let n = (m.len() as i64).min(a.len() as i64 - shift) as usize;
            let same = (44_100..n).all(|i| a[(i as i64 + shift) as usize] == m[i]);
            eprintln!("MP4 against ADTS: shifted {} frames, the same samples from the first second on: {}", shift, same);
            assert!(same);
        }
    }

    /// A decode names the lossy codec it came from — the decoder's, not the
    /// extension: an MP3 named .wav is MP3; Ogg FLAC and a WAV are lossless.
    #[test]
    fn a_decode_names_its_lossy_codec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mp3 = dir.path().join("track.wav");
        std::fs::write(&mp3, crate::audio::tag_text::test_mp3::make_mp3(&[], None)).expect("write");
        assert_eq!(decode_file(&mp3).expect("decode").lossy, Some("MP3"));
        assert_eq!(probe_lossy_codec(&mp3), Some("MP3"));
        let ogg = dir.path().join("track.ogg");
        std::fs::write(&ogg, chained(&[(2 * LINK_BLOCK, 44_100)])).expect("write");
        assert_eq!(decode_file(&ogg).expect("decode").lossy, None);
        assert_eq!(probe_lossy_codec(&ogg), None);
        let wav = dir.path().join("track.flac");
        crate::player::chain::write_test_wav16(&wav, 44_100, &[0.1; 64], &[0.1; 64]);
        assert_eq!(decode_file(&wav).expect("decode").lossy, None);
    }
}
