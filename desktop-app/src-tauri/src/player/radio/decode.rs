//! The decoder thread: each connection's bytes through symphonia, as they
//! come, into the session's `LiveSource`.
//!
//! The first bytes tell an ADTS stream with an HE-AAC core (`adts`): it goes
//! to the HE-AAC decoder (`he_aac`: the FDK AAC decoder), which reads the
//! SBR and PS symphonia does not; everything else to symphonia, the sniffed
//! bytes first.
//!
//! The bytes are read through `ReadOnlySource` (no seeking: the format is
//! probed from what has arrived). A chained Ogg stream — Radio Paradise's
//! FLAC starts a new logical stream at every song — asks for a reset at the
//! join: the track and its decoder are opened again on the same bytes, so
//! the sound goes on without a break, and the new song's title comes from
//! its Vorbis comments. A connection that ends mid-audio is handed to the
//! joiner (`join.rs`): the next one goes on without a seam when it repeats
//! what was received, or meets it as a gap (`LiveSource::mark_gap`: the
//! unread tail fades out, the new frames fade in). What the joiner lets
//! through passes the splicer (`drift`: the stream as it came, but for the
//! rare splice that keeps the delay in its corridor), then the rack's
//! source stages, across a seamless join; a gap finishes them and starts
//! them afresh. What the splicer hands on goes into the live source's
//! shadow as it is, before the stages (`LiveSource::push_raw`): BIT-PERFECT
//! plays it.

use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, OnceLock};

use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::packet::Packet;

use super::adts::{self, Sniffed};
use super::drift::Splicer;
use super::he_aac::HeAac;
use super::join::{Joiner, PLACE_MORE_S, PLACE_WAIT};
use super::live::LiveSource;
use super::net::{Connected, PipeReader};
use super::RadioShared;
use crate::player::source_stages::SourceStages;

/// Start the decoder thread: one connection after another, until the
/// network thread hangs up (the session stopped).
pub fn spawn(shared: Arc<RadioShared>, rx: Receiver<Connected>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aura-radio-decode".into())
        .spawn(move || {
            // One stream across the connections (join.rs), and the source
            // stages it runs through, with the rate they were made for.
            let mut joiner: Option<Joiner> = None;
            let mut splicer: Option<Splicer> = None;
            let mut stages: Option<(u32, SourceStages)> = None;
            // The HE-AAC route's decoder, which goes on across connections.
            let mut carry = Carry::default();
            let mut before = false;
            while let Ok(conn) = rx.recv() {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                let n = conn.n;
                // A connection after a break known to have lost audio (HLS)
                // is met as a gap at once, not looked for in what was received.
                let gap = conn.gap;
                if gap {
                    if let Some(j) = joiner.as_mut() {
                        j.known_gap();
                    }
                }
                // A connection after another is placed against what was
                // received (decode_connection): its repeat cut off, or a gap
                // faded; after a known gap, not placed at all.
                let f = Feeds { joiner: &mut joiner, splicer: &mut splicer, stages: &mut stages };
                let r = decode_connection(&shared, conn, before && !gap, f, &mut carry);
                before = true;
                match r {
                    Ok(how) => crate::aelog!("[RADIO] decoder: connection #{} over ({})", n, how),
                    Err(e) => {
                        crate::aelog!("[RADIO] decoder: connection #{}: {}", n, e);
                        shared.info.lock().unwrap().last_error = Some(e.clone());
                        // A stream this player cannot decode will not decode
                        // on the next connection either.
                        if e.starts_with("unsupported") {
                            shared.fail(e);
                            break;
                        }
                    }
                }
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
            }
        })
        .expect("spawn the radio decoder thread")
}

fn hint_for(content_type: &str, url: &str) -> Hint {
    let mut hint = Hint::new();
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if !ct.is_empty() {
        hint.mime_type(&ct);
    }
    let ext = match ct.as_str() {
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/aac" | "audio/aacp" | "audio/x-aac" => Some("aac"),
        "application/ogg" | "audio/ogg" | "audio/vorbis" | "audio/opus" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    };
    let ext = ext.map(str::to_string).or_else(|| {
        let path = url.split(['?', '#']).next().unwrap_or("");
        path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| e.len() <= 4)
    });
    if let Some(e) = ext {
        hint.with_extension(&e);
    }
    hint
}

/// The decoders a stream may need: symphonia's, and libopus for Opus (an
/// Ogg Opus stream is demuxed by symphonia, which has no Opus decoder; the
/// first packet loses the OpusHead's pre-skip).
fn codecs() -> &'static CodecRegistry {
    static CODECS: OnceLock<CodecRegistry> = OnceLock::new();
    CODECS.get_or_init(|| {
        let mut r = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut r);
        r.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        r
    })
}

/// The audio track, its decoder, rate, channels and the codec's name.
fn open_track(format: &dyn FormatReader) -> Result<(u32, Box<dyn AudioDecoder>, u32, usize, String), String> {
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.as_ref().and_then(|p| p.audio()).is_some())
        .ok_or("no audio track in the stream")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("no audio track in the stream")?.clone();
    let rate = params.sample_rate.ok_or("the stream does not say its sample rate")?;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
    let mut opts = AudioDecoderOptions::default();
    opts.gapless = false;
    let codec = codecs()
        .get_audio_decoder(params.codec)
        .map(|d| d.codec.info.short_name.to_string())
        .unwrap_or_else(|| format!("{:?}", params.codec));
    let decoder = codecs()
        .make_audio_decoder(&params, &opts)
        .map_err(|e| format!("unsupported codec ({}): {}", codec, e))?;
    Ok((track.id, decoder, rate, channels, codec))
}

/// The newest "Artist - Title" the container carries (Vorbis comments).
fn container_title(format: &mut dyn FormatReader) -> Option<String> {
    let mut md = format.metadata();
    let rev = md.skip_to_latest()?;
    let (mut artist, mut title) = (None, None);
    for tag in &rev.media.tags {
        match &tag.std {
            Some(StandardTag::Artist(v)) => artist = Some(v.to_string()),
            Some(StandardTag::TrackTitle(v)) => title = Some(v.to_string()),
            _ => {}
        }
    }
    match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{} - {}", a, t)),
        (None, Some(t)) => Some(t),
        _ => None,
    }
}

/// One connection's bytes, decoded; `before`: one came before it, so it is
/// placed against what that one gave.
fn decode_connection(shared: &Arc<RadioShared>, conn: Connected, before: bool, f: Feeds, carry: &mut Carry) -> Result<String, String> {
    // What the stream is, from its first bytes; they go on to the decoder.
    let mut reader = conn.pipe.reader();
    let (head, sniffed) = sniff(&mut reader).map_err(|e| format!("read: {}", e))?;
    match sniffed {
        Sniffed::Adts(h, at) if h.he_core() => {
            crate::aelog!("[RADIO] ADTS with a {} Hz core: the FDK AAC decoder (SBR, PS)", h.rate());
            decode_he(shared, h, &head[at..], reader, before, f, carry)
        }
        _ => {
            carry.clear_he();
            let adts = match sniffed {
                Sniffed::Adts(h, _) => Some(h),
                _ => None,
            };
            decode_symphonia(shared, &conn, std::io::Cursor::new(head).chain(reader), before, f, carry, adts)
        }
    }
}

/// The next frames are a new connection's, for the joiner to place: their
/// repeat cut off, or a gap faded.
fn broken(f: &mut Feeds) {
    if let Some(j) = f.joiner.as_mut() {
        j.broken();
    }
}

/// The next frames are a new decoder's whose samples the joiner cannot
/// place against the old one's — HE-AAC's SBR, Opus and AAC's noise carry
/// their state from frame to frame, so a decoder made afresh never gives the
/// same numbers: they meet what was received as a gap at once. Where the
/// frames' bytes could place them, `adts::Trail` has looked already; a
/// second wait, for samples that cannot match, would only hold the stream
/// back.
fn known_gap(f: &mut Feeds) {
    if let Some(j) = f.joiner.as_mut() {
        j.known_gap();
    }
}

/// What a stream's own decoder keeps from one connection to the next: an
/// HE-AAC stream's (`he_aac`), or an Opus or AAC stream's.
#[derive(Default)]
struct Carry {
    he: Option<HeStream>,
    packets: Option<PacketStream>,
}

impl Carry {
    /// The HE-AAC decoder let go (on this thread, as it was made).
    fn clear_he(&mut self) {
        self.he = None;
    }
}

/// An HE-AAC stream's decoder and the frames it has had: a new connection
/// is placed among them by their bytes (`adts::Trail`), and the decoder
/// goes on from the first frame it has not had — the samples as if the
/// connection had never broken.
struct HeStream {
    dec: HeAac,
    first: adts::Header,
    trail: adts::Trail,
}

impl HeStream {
    fn new(first: adts::Header) -> Result<HeStream, String> {
        let frames = (super::join::TAIL_S * first.rate() as f64 / 1024.0) as usize;
        Ok(HeStream { dec: HeAac::new(&first)?, first, trail: adts::Trail::new(frames) })
    }
}

/// Read the first bytes until `adts::sniff` can tell what they are.
fn sniff(r: &mut PipeReader) -> std::io::Result<(Vec<u8>, Sniffed)> {
    let mut head = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = r.read(&mut chunk)?;
        head.extend_from_slice(&chunk[..n]);
        match adts::sniff(&head, n == 0) {
            Sniffed::More => {}
            s => return Ok((head, s)),
        }
    }
}

/// The session's state a connection's frames go through, across connections.
struct Feeds<'a> {
    joiner: &'a mut Option<Joiner>,
    splicer: &'a mut Option<Splicer>,
    stages: &'a mut Option<(u32, SourceStages)>,
}

/// Where a connection's decoded frames go: the joiner (a new connection
/// placed against what was received), the splicer, the rack's source
/// stages, the live source — in f64 all the way, as the decoder gave them
/// and as the stages computed them.
struct Feed<'a> {
    shared: &'a Arc<RadioShared>,
    live: Arc<LiveSource>,
    rate: u32,
    joiner: &'a mut Option<Joiner>,
    splicer: &'a mut Option<Splicer>,
    stages: &'a mut Option<(u32, SourceStages)>,
}

impl<'a> Feed<'a> {
    /// The stream is `codec` at `rate`: said in the status and the log, and
    /// its live source and source stages made ready.
    fn new(shared: &'a Arc<RadioShared>, f: Feeds<'a>, codec: &str, rate: u32, channels: usize, bits: Option<u32>) -> Result<Feed<'a>, String> {
        crate::aelog!("[RADIO] decoding {} at {} Hz, {} ch, {:?} bits", codec, rate, channels, bits);
        {
            let mut i = shared.info.lock().unwrap();
            i.codec = codec.to_string();
            i.rate = rate;
            i.channels = channels as u32;
            i.bits = bits;
        }
        let live = shared.live_for(rate)?;
        // The rack's source stages: the live source holds what they hand on.
        // They go on across a connection the joiner places without a seam; a
        // gap (below) or another rate starts them afresh.
        if !matches!(f.stages, Some((r, _)) if *r == rate) {
            *f.stages = Some((rate, SourceStages::new(&shared.source_plan(rate), shared.source_tally.clone())));
        }
        Ok(Feed { shared, live, rate, joiner: f.joiner, splicer: f.splicer, stages: f.stages })
    }

    /// The next frames are a new decoder's, met as a gap (`known_gap`).
    fn known_gap(&mut self) {
        if let Some(j) = self.joiner.as_mut() {
            j.known_gap();
        }
    }

    /// Decoded frames in, the left and right channels.
    fn push(&mut self, l64: &[f64], r64: &[f64]) {
        if l64.is_empty() {
            return;
        }
        let Feed { shared, live, rate, joiner, splicer, stages } = self;
        let (shared, rate) = (*shared, *rate);
        let mut i = shared.info.lock().unwrap();
        if i.t_first_pcm_ms.is_none() {
            i.t_first_pcm_ms = Some(shared.t0.elapsed().as_millis() as u64);
        }
        drop(i);
        // The joiner places a new connection sample for sample on the
        // frames as decoded; the source stages run on what it lets through.
        let j = joiner.get_or_insert_with(|| Joiner::new(rate));
        let sp = splicer.get_or_insert_with(|| Splicer::new(rate));
        let joined = j.push(l64, r64, |a, b, gap| {
            if gap {
                // What the splicer and the stages still hold goes in
                // before the gap (the live source fades it out, and the
                // new frames in). The fresh stages are made after it:
                // their tally starts from what the old ones closed.
                if let Some((_, st)) = stages.as_mut() {
                    sp.flush(&mut |x, y| through_stages(live, st, x, y));
                }
                if let Some((_, old)) = stages.take() {
                    finish_stages(old, live);
                }
                live.mark_gap();
                **stages = Some((rate, SourceStages::new(&shared.source_plan(rate), shared.source_tally.clone())));
            }
            let (_, st) = stages.as_mut().expect("the source stages are made above");
            // The source as it is, for the adaptive headroom's look.
            shared.first_look_push(a, b, rate);
            // The splice the drift policy asks for, if any, at a quiet
            // place; everything else as it came.
            sp.ask(shared.drift_ask());
            let place = shared.splice_place(sp.held());
            let spliced = sp.push(a, b, place, &mut |x, y| through_stages(live, st, x, y));
            shared.set_splicer_held(sp.held());
            if let Some(s) = spliced {
                shared.spliced(&s, rate);
            }
        });
        if let Some(d) = joined {
            shared.joined(d, rate);
        }
    }
}

/// Frames past the splicer: into the live source's shadow as they are, and
/// through the source stages, which hand theirs on to the live source.
fn through_stages(live: &LiveSource, st: &mut SourceStages, x: &[f64], y: &[f64]) {
    live.push_raw(x, y);
    st.push(x, y, &mut |p, q| live.push(p, q));
}

/// Interleaved frames of `ch` channels as left and right (a mono stream on
/// both; channels past two are not played).
fn split(interleaved: &[f64], ch: usize, l64: &mut Vec<f64>, r64: &mut Vec<f64>) {
    l64.clear();
    r64.clear();
    for frame in interleaved.chunks_exact(ch.max(1)) {
        l64.push(frame[0]);
        r64.push(if ch >= 2 { frame[1] } else { frame[0] });
    }
}

/// An ADTS stream with an HE-AAC core, through the HE-AAC decoder: frame by
/// frame as the bytes come. After a break the decoder of the last
/// connection goes on, from the first frame it has not had; frames not
/// among those it had (audio lost) start a new decoder, and the joiner
/// meets the two sides with fades.
fn decode_he<'a>(
    shared: &'a Arc<RadioShared>,
    first: adts::Header,
    head: &[u8],
    mut reader: PipeReader,
    before: bool,
    f: Feeds<'a>,
    carry: &mut Carry,
) -> Result<String, String> {
    use super::adts::{frame_id, Placed};
    carry.packets = None;
    let mut f = Some(f);
    let mut placing = before && carry.he.as_ref().map_or(false, |m| m.first.same_stream(&first) && !m.trail.is_empty());
    if !placing {
        carry.he = None;
        if before {
            known_gap(f.as_mut().expect("not taken yet"));
        }
        carry.he = Some(HeStream::new(first)?);
    }
    let ms = carry.he.as_mut().expect("made above");
    let give_up = ms.trail.len() + (PLACE_MORE_S * first.rate() as f64 / 1024.0) as usize;
    let mut held_since: Option<std::time::Instant> = None;
    let mut framer = adts::Framer::new();
    framer.push(head);
    let mut feed: Option<Feed> = None;
    let mut bufs = HeBufs::default();
    let (mut frame, mut held, mut held_ids) = (Vec::new(), Vec::<Vec<u8>>::new(), Vec::new());
    let mut chunk = vec![0u8; 16 * 1024];
    let mut eof = false;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok("stopped".into());
        }
        while framer.next(eof, &mut frame).is_some() {
            if !placing {
                he_frame(shared, ms, &frame, &mut f, &mut feed, &mut bufs)?;
                continue;
            }
            // A new connection's frames, held until placed among those had.
            held_ids.push(frame_id(&frame));
            held.push(frame.clone());
            let since = *held_since.get_or_insert_with(std::time::Instant::now);
            let lost = since.elapsed() > PLACE_WAIT || held.len() > give_up;
            let Some(p) = ms.trail.place(&held_ids).or_else(|| lost.then_some(Placed::New)) else {
                continue;
            };
            placing = false;
            let skip = match p {
                Placed::Repeats(n) => {
                    let out = ms.dec.format().rate;
                    let per_frame = 1024 * out as usize / first.rate() as usize;
                    shared.joined(super::join::Joined::Seamless { repeated: n * per_frame }, out);
                    n
                }
                Placed::New => {
                    crate::aelog!("[RADIO] the new connection's frames are not among the last {} decoded: a new decoder", ms.trail.len());
                    ms.dec = HeAac::new(&first)?;
                    ms.trail.clear();
                    known_gap(f.as_mut().expect("nothing decoded on this connection yet"));
                    0
                }
            };
            held_ids.clear();
            for fr in std::mem::take(&mut held).iter().skip(skip) {
                he_frame(shared, ms, fr, &mut f, &mut feed, &mut bufs)?;
            }
        }
        if eof {
            if framer.skipped > 0 {
                crate::aelog!("[RADIO] {} bytes between frames skipped on this connection", framer.skipped);
            }
            return Ok("end of stream".into());
        }
        let n = reader.read(&mut chunk).map_err(|e| format!("read: {}", e))?;
        eof = n == 0;
        framer.push(&chunk[..n]);
    }
}

/// The HE-AAC route's buffers, and its frames that would not decode.
#[derive(Default)]
struct HeBufs {
    pcm: Vec<f64>,
    l64: Vec<f64>,
    r64: Vec<f64>,
    errors: u64,
}

/// One frame through the decoder, its samples on to the feed (made at the
/// first samples, once the decoder has told its rate). A frame that does
/// not decode is counted; what the decoder gave in its place goes on.
fn he_frame<'a>(
    shared: &'a Arc<RadioShared>,
    ms: &mut HeStream,
    frame: &[u8],
    f: &mut Option<Feeds<'a>>,
    feed: &mut Option<Feed<'a>>,
    b: &mut HeBufs,
) -> Result<(), String> {
    ms.trail.had(adts::frame_id(frame));
    b.pcm.clear();
    if let Err(e) = ms.dec.push(frame, &mut b.pcm) {
        b.errors += 1;
        shared.info.lock().unwrap().decode_errors += 1;
        if b.errors <= 5 {
            crate::aelog!("[RADIO] a frame did not decode: {}", e);
        }
        if ms.dec.restarted() {
            // A new decoder goes on: its samples are not the old one's, so
            // the stream meets them as a gap.
            crate::aelog!("[RADIO] the HE-AAC decoder was made afresh: a gap");
            match (feed.as_mut(), f.as_mut()) {
                (Some(fd), _) => fd.known_gap(),
                (None, Some(fs)) => known_gap(fs),
                (None, None) => {}
            }
        }
    }
    if b.pcm.is_empty() {
        return Ok(());
    }
    let fmt = ms.dec.format();
    if let Some(fd) = feed.as_ref() {
        if fd.rate != fmt.rate {
            return Err(format!("unsupported: the stream changed rate {} → {} Hz", fd.rate, fmt.rate));
        }
    } else {
        let fs = f.take().expect("the feeds are taken once");
        *feed = Some(Feed::new(shared, fs, ms.dec.codec(), fmt.rate, fmt.channels as usize, None)?);
    }
    split(&b.pcm, fmt.channels as usize, &mut b.l64, &mut b.r64);
    feed.as_mut().expect("made above").push(&b.l64, &b.r64);
    Ok(())
}

/// What a decoder that goes on across connections was made for: an Opus
/// stream's OpusHead, an AAC stream's AudioSpecificConfig (its ADTS
/// header's: object type, rate, channels — or the container's).
fn carried_key(p: &AudioCodecParameters, adts: Option<adts::Header>) -> Option<Box<[u8]>> {
    match p.codec {
        CODEC_ID_OPUS => Some(p.extra_data.clone().unwrap_or_default()),
        CODEC_ID_AAC => adts.map(|h| Box::from(h.asc())).or_else(|| p.extra_data.clone()),
        _ => None,
    }
}

/// Anything else through symphonia: probed from the bytes as they come. A
/// new connection is placed by the joiner (join.rs), by its samples. Opus
/// and AAC are placed by their packets' bytes instead, their decoder going
/// on across the break: one made afresh would not give the same samples —
/// Opus carries its energies and filters from packet to packet, AAC draws
/// the noise of its noise-coded bands (PNS) from a generator of its own.
/// `adts`: the stream's first ADTS header, when it is one.
fn decode_symphonia(
    shared: &Arc<RadioShared>,
    conn: &Connected,
    bytes: impl Read + Send + Sync + 'static,
    before: bool,
    mut f: Feeds,
    carry: &mut Carry,
    adts: Option<adts::Header>,
) -> Result<String, String> {
    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(bytes)), Default::default());
    let hint = hint_for(&conn.content_type, &conn.url);
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("unsupported stream format ({}): {}", conn.content_type, e))?;
    let (mut tid, fresh, rate, channels, codec) = open_track(&*format)?;
    let (bits, key, per_packet) = {
        let p = format.tracks().iter().find(|t| t.id == tid).and_then(|t| t.codec_params.as_ref()).and_then(|p| p.audio());
        let per_packet = if p.is_some_and(|p| p.codec == CODEC_ID_AAC) { 1024.0 } else { 960.0 };
        (p.and_then(|p| p.bits_per_sample), p.and_then(|p| carried_key(p, adts)), per_packet)
    };
    // The stream's decoder from the last connection goes on when it was made
    // for the same stream, the new packets placed among those it had.
    let mut carried: Option<PacketPlace>;
    let mut placing = false;
    let mut decoder = match (key.as_ref(), carry.packets.take()) {
        (Some(k), Some(c)) if before && c.key == *k && !c.trail.is_empty() => {
            placing = true;
            carried = Some(PacketPlace { trail: c.trail, fresh: Some(fresh), held: Vec::new(), ids: Vec::new(), since: None });
            c.dec
        }
        (k, c) => {
            // A new decoder: one that would go on is placed by nothing but
            // its packets (`known_gap`), anything else by the joiner, sample
            // for sample.
            if before {
                if let Some(k) = k {
                    let why = match c {
                        None => "none went on from the last connection",
                        Some(c) if c.key != *k => "the stream is not the one it was made for",
                        Some(_) => "it had no packets",
                    };
                    crate::aelog!("[RADIO] a new {} decoder ({}): the break is a gap", codec, why);
                    known_gap(&mut f);
                } else {
                    broken(&mut f);
                }
            }
            let frames = (super::join::TAIL_S * rate as f64 / per_packet) as usize;
            carried = key.as_ref().map(|_| PacketPlace { trail: adts::Trail::new(frames), fresh: None, held: Vec::new(), ids: Vec::new(), since: None });
            fresh
        }
    };
    let mut feed = Feed::new(shared, f, &codec, rate, channels, bits)?;
    if let Some(t) = container_title(&mut *format) {
        shared.set_title(&t, "tags");
    }

    let mut bufs = PacketBufs::default();
    let result = loop {
        if shared.stop.load(Ordering::Acquire) {
            break Ok("stopped".into());
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break Ok("end of stream".into()),
            Err(SymError::ResetRequired) => {
                // A chained Ogg stream: the next song is a new logical stream.
                let (t2, d2, rate2, _, codec2) = match open_track(&*format) {
                    Ok(t) => t,
                    Err(e) => break Err(e),
                };
                if rate2 != rate {
                    break Err(format!("unsupported: the stream changed rate {} → {} Hz", rate, rate2));
                }
                tid = t2;
                decoder = d2;
                let resets = {
                    let mut i = shared.info.lock().unwrap();
                    i.resets += 1;
                    i.codec = codec2;
                    i.resets
                };
                // Every frame of the last song has gone in: the next one
                // begins at the stream's newest frame — unless its tags name
                // the song playing (RadioShared::chained).
                shared.chained(container_title(&mut *format).as_deref());
                crate::aelog!("[RADIO] new logical stream #{} (chained Ogg): decoder opened again", resets);
                continue;
            }
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok("end of stream".into())
            }
            Err(e) => break Err(format!("demux: {}", e)),
        };
        if packet.track_id != tid {
            continue;
        }
        let Some(op) = carried.as_mut() else {
            decode_packet(shared, &mut *decoder, &packet, &mut feed, &mut bufs);
            continue;
        };
        let id = adts::frame_id(&packet.data);
        if !placing {
            op.trail.had(id);
            decode_packet(shared, &mut *decoder, &packet, &mut feed, &mut bufs);
            continue;
        }
        // A new connection's packets, held until placed among those had.
        op.ids.push(id);
        op.held.push(packet);
        let since = *op.since.get_or_insert_with(std::time::Instant::now);
        let lost = since.elapsed() > PLACE_WAIT || op.held.len() > op.trail.len() + (PLACE_MORE_S * rate as f64 / per_packet) as usize;
        let Some(p) = op.trail.place(&op.ids).or_else(|| lost.then_some(adts::Placed::New)) else {
            continue;
        };
        placing = false;
        let skip = match p {
            adts::Placed::Repeats(n) => {
                let frames: usize = op.held[..n].iter().map(|p| p.dur.get() as usize).sum();
                shared.joined(super::join::Joined::Seamless { repeated: frames }, rate);
                n
            }
            adts::Placed::New => {
                crate::aelog!("[RADIO] the new connection's packets are not among the last {} decoded: a new decoder", op.trail.len());
                if let Some(d) = op.fresh.take() {
                    decoder = d;
                }
                op.trail.clear();
                // As `known_gap`: the fresh decoder's samples are not the old one's.
                if let Some(j) = feed.joiner.as_mut() {
                    j.known_gap();
                }
                0
            }
        };
        op.ids.clear();
        for p in std::mem::take(&mut op.held).iter().skip(skip) {
            op.trail.had(adts::frame_id(&p.data));
            decode_packet(shared, &mut *decoder, p, &mut feed, &mut bufs);
        }
    };
    // The decoder and the packets it had, for the next connection.
    if let (Some(k), Some(op)) = (key, carried) {
        carry.packets = Some(PacketStream { dec: decoder, key: k, trail: op.trail });
    }
    result
}

/// The symphonia route's buffers, and its packets that would not decode.
#[derive(Default)]
struct PacketBufs {
    interleaved: Vec<f64>,
    l64: Vec<f64>,
    r64: Vec<f64>,
    errors: u64,
}

/// One packet through the decoder, its frames on to the feed.
fn decode_packet(shared: &RadioShared, decoder: &mut dyn AudioDecoder, packet: &Packet, feed: &mut Feed, b: &mut PacketBufs) {
    let decoded = match decoder.decode(packet) {
        Ok(d) => d,
        Err(e) => {
            b.errors += 1;
            shared.info.lock().unwrap().decode_errors += 1;
            if b.errors <= 5 {
                crate::aelog!("[RADIO] skipped a packet: {}", e);
            }
            return;
        }
    };
    let ch = decoded.spec().channels().count().max(1);
    // In f64, as the file decoder hands on: the decoder's own samples (f32
    // or integers) widened, every value as it was.
    decoded.copy_to_vec_interleaved(&mut b.interleaved);
    split(&b.interleaved, ch, &mut b.l64, &mut b.r64);
    feed.push(&b.l64, &b.r64);
}

/// An Opus or AAC stream's decoder, what it was made for (`carried_key`),
/// and the packets it has had (by their bytes): kept from one connection to
/// the next.
struct PacketStream {
    dec: Box<dyn AudioDecoder>,
    key: Box<[u8]>,
    trail: adts::Trail,
}

/// A connection's packets on their way: the packets had, and after a break
/// the new ones held until placed among them (with the decoder made for
/// this connection, should they not be).
struct PacketPlace {
    trail: adts::Trail,
    fresh: Option<Box<dyn AudioDecoder>>,
    held: Vec<Packet>,
    ids: Vec<u64>,
    since: Option<std::time::Instant>,
}

/// The stages' last frames into the live source before a gap, and their
/// intersample repair to the log.
fn finish_stages(stages: SourceStages, live: &LiveSource) {
    if let Some(rep) = stages.finish(&mut |a, b| live.push(a, b)) {
        crate::aelog!(
            "[RADIO] intersample repair up to the gap: max {:+.2} dBTP, clusters {}, fixed {}, unfixed {}, hot={} left whole, residual {:+.2} dBTP",
            rep.max_dbtp, rep.clusters, rep.fixed, rep.unfixed, rep.hot, rep.residual_dbtp
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
    use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
    use symphonia::core::codecs::audio::AudioCodecParameters;
    use symphonia::core::packet::Packet;
    use symphonia::core::units::{Duration, Timestamp};

    /// A test stream from libopus's own encoder: a tone, quiet, different on
    /// each side, in 20 ms packets at 128 kbps; and the tone itself.
    fn opus_packets(secs: f64) -> (Vec<Vec<u8>>, Vec<f32>) {
        let tau = std::f32::consts::TAU;
        let n = (secs * 48_000.0) as usize / 960 * 960;
        let pcm: Vec<f32> = (0..n)
            .flat_map(|i| {
                let t = i as f32 / 48_000.0;
                [(t * 440.0 * tau).sin() * 0.25, (t * 1234.5 * tau).sin() * 0.2]
            })
            .collect();
        let mut packets = vec![];
        unsafe {
            let mut err = 0;
            let enc = opusic_sys::opus_encoder_create(48_000, 2, opusic_sys::OPUS_APPLICATION_AUDIO, &mut err);
            assert!(!enc.is_null() && err == 0, "the encoder: {}", err);
            opusic_sys::opus_encoder_ctl(enc, opusic_sys::OPUS_SET_BITRATE_REQUEST, 128_000i32);
            let mut buf = vec![0u8; 4000];
            for frame in pcm.chunks_exact(960 * 2) {
                let len = opusic_sys::opus_encode_float(enc, frame.as_ptr(), 960, buf.as_mut_ptr(), buf.len() as i32);
                assert!(len > 0, "encode: {}", len);
                packets.push(buf[..len as usize].to_vec());
            }
            opusic_sys::opus_encoder_destroy(enc);
        }
        (packets, pcm)
    }

    /// The OpusHead of a stereo stream at 48 kHz (RFC 7845).
    fn opus_head(pre_skip: u16) -> Box<[u8]> {
        let mut h = b"OpusHead".to_vec();
        h.extend_from_slice(&[1, 2]);
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&[0, 0, 0]);
        h.into_boxed_slice()
    }

    /// The decoder the radio makes for an Opus stream.
    fn opus_decoder(pre_skip: u16) -> Box<dyn AudioDecoder> {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_OPUS).with_sample_rate(48_000).with_channels(CHANNEL_LAYOUT_STEREO).with_extra_data(opus_head(pre_skip));
        let mut opts = AudioDecoderOptions::default();
        opts.gapless = false;
        codecs().make_audio_decoder(&params, &opts).expect("libopus is registered")
    }

    /// Packets through a decoder; the frames of each packet and the samples,
    /// interleaved.
    fn opus_run(dec: &mut dyn AudioDecoder, packets: &[Vec<u8>], out: &mut Vec<f32>) -> Vec<usize> {
        let (mut lens, mut v) = (vec![], Vec::<f32>::new());
        for (i, p) in packets.iter().enumerate() {
            let d = dec.decode(&Packet::new(0, Timestamp::new(i as i64 * 960), Duration::new(960), p.clone())).expect("decodes");
            lens.push(d.frames());
            d.copy_to_vec_interleaved(&mut v);
            out.extend_from_slice(&v);
        }
        lens
    }

    fn opus_decode(packets: &[Vec<u8>], pre_skip: u16) -> (Vec<usize>, Vec<f32>) {
        let mut out = vec![];
        let lens = opus_run(&mut *opus_decoder(pre_skip), packets, &mut out);
        (lens, out)
    }

    #[test]
    fn opus_goes_on_in_its_decoder_after_a_reconnect_placed_by_its_packets() {
        let (packets, _) = opus_packets(4.0);
        let (_, whole) = opus_decode(&packets, 312);
        let (cut, from) = (packets.len() * 6 / 10, packets.len() * 4 / 10);
        let mut dec = opus_decoder(312);
        let mut trail = adts::Trail::new(4096);
        let mut heard = vec![];
        opus_run(&mut *dec, &packets[..cut], &mut heard);
        for p in &packets[..cut] {
            trail.had(adts::frame_id(p));
        }
        // The next connection repeats from `from`: placed, the rest decoded on.
        let ids: Vec<u64> = packets[from..].iter().map(|p| adts::frame_id(p)).collect();
        assert_eq!(trail.place(&ids), Some(adts::Placed::Repeats(cut - from)));
        opus_run(&mut *dec, &packets[cut..], &mut heard);
        assert!(heard == whole, "as if unbroken: {} vs {} samples", heard.len(), whole.len());
    }

    #[test]
    fn opus_decodes_through_libopus_at_48_khz_in_stereo_the_first_packet_less_its_pre_skip() {
        let (packets, pcm) = opus_packets(2.0);
        let (lens, out) = opus_decode(&packets, 312);
        assert_eq!(lens[0], 960 - 312, "the pre-skip is cut from the first packet");
        assert!(lens[1..].iter().all(|&n| n == 960));
        // With the pre-skip gone the tone lines up with what was encoded
        // (libopus's look-ahead at 48 kHz is 312): the codec's difference only.
        let n = out.len().min(pcm.len()) - 2 * 960;
        let (mut sig, mut dif) = (0f64, 0f64);
        for i in 2 * 960..n {
            sig += (pcm[i] as f64).powi(2);
            dif += (pcm[i] as f64 - out[i] as f64).powi(2);
        }
        let db = 10.0 * (sig / dif).log10();
        assert!(db > 20.0, "the tone through Opus: only {:.1} dB under it", db);
    }

    /// An Ogg Opus dump of a live station, kept outside the repository
    /// (AURA_TEST_OPUS), demuxed by symphonia and decoded as the radio does,
    /// against libopus's own decode of it (AURA_TEST_OPUS_REF: f32le stereo
    /// 48 kHz, e.g. `ffmpeg -c:a libopus -i x.opus -f f32le ref.f32`); then a
    /// reconnect as the joiner meets it — the first connection's packets up to
    /// 60 %, the next one's from 40 % with a decoder of its own.
    #[test]
    #[ignore]
    fn an_ogg_opus_dump_decodes_as_libopus_does_and_goes_on_after_a_reconnect_as_if_unbroken() {
        use super::super::join::Joiner;
        let (Ok(path), Ok(ref_path)) = (std::env::var("AURA_TEST_OPUS"), std::env::var("AURA_TEST_OPUS_REF")) else {
            eprintln!("SKIPPED: AURA_TEST_OPUS / AURA_TEST_OPUS_REF not set");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(std::io::Cursor::new(bytes))), Default::default());
        let mut format =
            symphonia::default::get_probe().probe(&hint_for("audio/ogg", &path), mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
        let (tid, mut dec, rate, ch, codec) = open_track(&*format).unwrap();
        let mut packets = vec![];
        let mut whole = vec![];
        let mut v: Vec<f32> = vec![];
        while let Ok(Some(p)) = format.next_packet() {
            if p.track_id == tid {
                dec.decode(&p).unwrap().copy_to_vec_interleaved(&mut v);
                whole.extend_from_slice(&v);
                packets.push(p);
            }
        }
        let reference: Vec<f32> = std::fs::read(&ref_path).unwrap().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let n = whole.len().min(reference.len());
        let diff = (0..n).fold(0f32, |m, i| m.max((whole[i] - reference[i]).abs()));
        eprintln!("{}: {} at {} Hz, {} ch, {} packets; {} samples vs libopus {}, largest difference {:e}", path, codec, rate, ch, packets.len(), whole.len(), reference.len(), diff);
        // A reconnect as the radio meets it on Opus: the decoder goes on, the
        // new connection's packets placed among those it had by their bytes.
        let (cut, from) = (packets.len() * 6 / 10, packets.len() * 4 / 10);
        let (_, mut d, ..) = open_track(&*format).unwrap();
        let mut trail = adts::Trail::new(4096);
        let (mut heard, mut v) = (vec![], Vec::<f32>::new());
        for p in &packets[..cut] {
            trail.had(adts::frame_id(&p.data));
            d.decode(p).unwrap().copy_to_vec_interleaved(&mut v);
            heard.extend_from_slice(&v);
        }
        let ids: Vec<u64> = packets[from..cut].iter().map(|p| adts::frame_id(&p.data)).collect();
        let placed = trail.place(&ids);
        for p in &packets[cut..] {
            d.decode(p).unwrap().copy_to_vec_interleaved(&mut v);
            heard.extend_from_slice(&v);
        }
        // Beside it: a decoder made afresh for the next connection, as the
        // joiner would have to place it by its samples.
        let decode = |ps: &[Packet]| -> Vec<f32> {
            let (_, mut d, ..) = open_track(&*format).unwrap();
            let (mut out, mut v) = (vec![], Vec::<f32>::new());
            for p in ps {
                d.decode(p).unwrap().copy_to_vec_interleaved(&mut v);
                out.extend_from_slice(&v);
            }
            out
        };
        let (one, two) = (decode(&packets[..cut]), decode(&packets[from..]));
        let lr = |x: &[f32]| -> (Vec<f64>, Vec<f64>) {
            (x.chunks_exact(ch).map(|f| f[0] as f64).collect(), x.chunks_exact(ch).map(|f| f[ch.min(2) - 1] as f64).collect())
        };
        let mut j = Joiner::new(rate);
        let mut decided = None;
        let (l1, r1) = lr(&one);
        j.push(&l1, &r1, |_, _, _| {});
        j.broken();
        let (l2, r2) = lr(&two);
        for (a, b) in l2.chunks(4096).zip(r2.chunks(4096)) {
            decided = decided.or(j.push(a, b, |_, _, _| {}));
        }
        eprintln!("reconnect by packets: {:?}, as if unbroken: {}; a fresh decoder through the joiner: {:?}", placed, heard == whole, decided);
        assert!(diff < 1e-3, "not libopus's decode: {:e}", diff);
        assert_eq!(placed, Some(adts::Placed::Repeats(cut - from)));
        assert!(heard == whole, "the stream as if unbroken");
    }

    /// Interleaved stereo as left and right, in f64.
    fn sides(x: &[f32]) -> (Vec<f64>, Vec<f64>) {
        (x.iter().step_by(2).map(|&v| v as f64).collect(), x.iter().skip(1).step_by(2).map(|&v| v as f64).collect())
    }

    /// Two connections' decodes through a joiner, the second after a break
    /// and in pieces: the stream given out, what became of the new start,
    /// the gaps marked.
    fn rejoin(rate: u32, one: &[f32], two: &[f32]) -> (Vec<f64>, Vec<f64>, Option<super::super::join::Joined>, usize) {
        let mut j = Joiner::new(rate);
        let mut got = (vec![], vec![], 0usize);
        let mut decided = None;
        {
            let mut put = |a: &[f64], b: &[f64], gap: bool| {
                got.0.extend_from_slice(a);
                got.1.extend_from_slice(b);
                got.2 += gap as usize;
            };
            let (l1, r1) = sides(one);
            j.push(&l1, &r1, &mut put);
            j.broken();
            let (l2, r2) = sides(two);
            for (a, b) in l2.chunks(4_096).zip(r2.chunks(4_096)) {
                decided = decided.or(j.push(a, b, &mut put));
            }
        }
        (got.0, got.1, decided, got.2)
    }

    /// Interleaved 16-bit stereo that never repeats: chirps, and `noise` —
    /// which an encoder codes as noise (PNS), drawn by a decoder from its own
    /// generator, so one made afresh draws other noise there.
    fn programme_i16(rate: u32, secs: f64, noise: f64) -> Vec<i16> {
        use std::f64::consts::TAU;
        let mut x = 0x2545_f491u32;
        let n = (rate as f64 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            for k in 0..2 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let f0 = 220.0 + 70.0 * k as f64;
                let s = (TAU * f0 * t * (1.0 + t * 0.013)).sin() * 0.25
                    + (TAU * (3.0 * f0 + 17.0) * t * (1.0 + t * 0.007)).sin() * 0.1
                    + (TAU * 5_100.0 * t * (1.0 + t * 0.003 * (k as f64 + 1.0))).sin() * 0.05
                    + (x as f64 / u32::MAX as f64 - 0.5) * noise;
                v.push((s * 32767.0) as i16);
            }
        }
        v
    }

    /// An ADTS stream of AAC-LC made by the FDK AAC encoder from `ch`
    /// channels of interleaved `pcm`, at a bitrate where it codes noise as
    /// noise (PNS).
    fn adts_stream(rate: u32, ch: u32, pcm: &[i16]) -> Vec<u8> {
        super::super::he_aac::tests::encode(super::super::he_aac::tests::Profile::Lc, rate, ch, 32_000 * ch, pcm).0
    }

    /// An ADTS stream's whole frames.
    fn adts_frames(stream: &[u8]) -> Vec<Vec<u8>> {
        super::super::he_aac::tests::frames_of(stream)
    }

    /// The rack the stream tests below run: no stage but the DC high-pass,
    /// which a stream always runs.
    fn plain() -> crate::player::settings::PlayerSettings {
        crate::player::settings::PlayerSettings {
            isp: false,
            subsonic_hz: 0,
            apodizing: 0,
            adaptive_apodizer: false,
            declip: false,
            adaptive_headroom: false,
            ..Default::default()
        }
    }

    /// Connections of a stream, one after another, through the decoder
    /// thread as the network thread hands them on.
    fn run_connections(s: &crate::player::settings::PlayerSettings, content_type: &str, conns: Vec<Vec<u8>>) -> Arc<RadioShared> {
        use super::super::net::{Connected, Pipe};
        let shared = Arc::new(RadioShared::new("http://x/stream", s));
        let (tx, rx) = std::sync::mpsc::channel();
        let t = spawn(shared.clone(), rx);
        for (n, bytes) in conns.into_iter().enumerate() {
            let pipe = Pipe::new();
            pipe.write(&bytes);
            pipe.finish();
            let (content_type, url) = (content_type.to_string(), "http://x/stream".to_string());
            tx.send(Connected { pipe, content_type, url, n: n as u32 + 1, gap: false }).unwrap();
        }
        drop(tx);
        t.join().unwrap();
        shared
    }

    /// An AAC-LC stream whose encoder coded bands as noise (PNS), across a
    /// reconnect through the decoder thread: the decoder goes on, the new
    /// connection's packets placed among those it had by their bytes, and the
    /// live source holds the stream as one decoder gives it, to the bit —
    /// where a decoder made afresh would have drawn other noise.
    #[test]
    fn aac_goes_on_in_its_decoder_across_a_reconnect_noise_coded_bands_and_all() {
        use super::super::he_aac::tests::symphonia_pcm;
        use crate::player::source_stages::{tests::same_bits, SourcePlan};
        let rate = 44_100;
        let stream = adts_stream(rate, 2, &programme_i16(rate, 20.0, 0.1));
        let frames = adts_frames(&stream);
        let (cut, from) = (frames.len() * 6 / 10, frames.len() * 3 / 10);
        let whole = symphonia_pcm(&stream);
        let fresh = symphonia_pcm(&frames[from..].concat());
        let at = (cut - from) * 1024 * 2;
        assert!(fresh[at..].iter().zip(&whole[cut * 1024 * 2..]).any(|(a, b)| a != b), "a decoder made afresh draws other noise");
        let s = plain();
        let shared = run_connections(&s, "audio/aac", vec![frames[..cut].concat(), frames[from..].concat()]);
        let info = shared.info.lock().unwrap().clone();
        assert_eq!((info.joins, info.join_gaps, info.decode_errors), (1, 0, 0));
        let live = shared.live.get().expect("decoded").clone();
        // The same stages over one decoder's whole decode.
        let (wl, wr) = sides(&whole);
        let mut st = SourceStages::new(&SourcePlan::new(&s, rate), Default::default());
        let (mut rl, mut rr) = (vec![], vec![]);
        st.push(&wl, &wr, &mut |a, b| {
            rl.extend_from_slice(a);
            rr.extend_from_slice(b);
        });
        let n = live.end() as usize;
        let (mut gl, mut gr) = (vec![0.0; n], vec![0.0; n]);
        live.read(0, 0, &mut gl);
        live.read(1, 0, &mut gr);
        assert!(n + rate as usize > wl.len() && n <= rl.len(), "{} of {} frames", n, wl.len());
        assert!(same_bits(&gl, &rl[..n]) && same_bits(&gr, &rr[..n]), "the stream as one decoder gives it");
    }

    /// The next connection carries a stream made for another config (here
    /// mono after stereo): the decoder is made afresh, and the break is met
    /// as a gap.
    #[test]
    fn aac_for_another_config_after_a_reconnect_is_met_as_a_gap() {
        let rate = 44_100;
        let pcm = programme_i16(rate, 6.0, 0.0);
        let mono: Vec<i16> = pcm.iter().step_by(2).copied().collect();
        let (stereo, mono) = (adts_stream(rate, 2, &pcm), adts_stream(rate, 1, &mono));
        let shared = run_connections(&plain(), "audio/aac", vec![stereo, mono]);
        let info = shared.info.lock().unwrap().clone();
        assert_eq!((info.joins, info.join_gaps, info.channels), (0, 1, 1));
        assert_eq!(shared.live.get().expect("decoded").stats().gaps, 1);
    }

    /// An HE-AAC stream across a reconnect through the decoder thread: the
    /// HE-AAC decoder goes on, the new connection's frames placed among
    /// those it had by their bytes, and the live source
    /// holds the stream as one decoder gives it, to the bit — at twice the
    /// core's rate, named HE-AAC.
    #[test]
    fn he_aac_goes_on_in_its_decoder_across_a_reconnect() {
        use super::super::he_aac::tests::{bright, encode, Profile};
        use crate::player::source_stages::{tests::same_bits, SourcePlan};
        let rate = 44_100;
        let (stream, _) = encode(Profile::He, rate, 2, 48_000, &bright(rate, 12.0));
        let frames = adts_frames(&stream);
        // One decoder over the whole stream (the radio's never finishes it).
        let mut d = HeAac::new(&adts::Header::parse(&frames[0]).unwrap()).unwrap();
        let mut whole = vec![];
        for fr in &frames {
            d.push(fr, &mut whole).expect("decodes");
        }
        let (cut, from) = (frames.len() * 6 / 10, frames.len() * 3 / 10);
        let s = plain();
        let shared = run_connections(&s, "audio/aac", vec![frames[..cut].concat(), frames[from..].concat()]);
        let info = shared.info.lock().unwrap().clone();
        assert_eq!((info.codec.as_str(), info.rate, info.channels), ("HE-AAC", rate, 2));
        assert_eq!((info.joins, info.join_gaps, info.decode_errors), (1, 0, 0));
        let live = shared.live.get().expect("decoded").clone();
        let wl: Vec<f64> = whole.iter().step_by(2).copied().collect();
        let wr: Vec<f64> = whole.iter().skip(1).step_by(2).copied().collect();
        let mut st = SourceStages::new(&SourcePlan::new(&s, rate), Default::default());
        let (mut rl, mut rr) = (vec![], vec![]);
        st.push(&wl, &wr, &mut |a, b| {
            rl.extend_from_slice(a);
            rr.extend_from_slice(b);
        });
        let n = live.end() as usize;
        let (mut gl, mut gr) = (vec![0.0; n], vec![0.0; n]);
        live.read(0, 0, &mut gl);
        live.read(1, 0, &mut gr);
        assert!(n + rate as usize > wl.len() && n <= rl.len(), "{} of {} frames", n, wl.len());
        assert!(same_bits(&gl, &rl[..n]) && same_bits(&gr, &rr[..n]), "the stream as one decoder gives it");
    }

    /// AAC-LC as symphonia decodes it, a reconnect whose burst (35 s) is
    /// longer than the joiner's tail: the new decoder's frames are placed by
    /// the last received, exactly where the burst began, and the stream goes
    /// on without a gap — the old connection's frames, then the new one's
    /// from the first not had. Against one decoder it is the same to the bit
    /// up to the join and past it, until the new decoder draws noise of its
    /// own for a band the encoder coded as noise (PNS: a decoder's generator
    /// starts afresh with it). The stream is made on the spot by the FDK AAC
    /// encoder.
    #[test]
    fn aac_lc_after_a_burst_longer_than_the_tail_goes_on_without_a_seam() {
        use super::super::he_aac::tests::symphonia_pcm;
        use super::super::join::{Joined, TAIL_S};
        let rate = 44_100;
        let stream = adts_stream(rate, 2, &programme_i16(rate, 46.0, 0.0));
        let frames = adts_frames(&stream);
        let per_s = rate as f64 / 1024.0;
        let (cut, from) = ((40.0 * per_s) as usize, (5.0 * per_s) as usize);
        let whole = symphonia_pcm(&stream);
        let (one, two) = (symphonia_pcm(&frames[..cut].concat()), symphonia_pcm(&frames[from..].concat()));
        let (l, r, decided, gaps) = rejoin(rate, &one, &two);
        eprintln!("AAC-LC, {} frames; the reconnect: {:?}, gaps {}", frames.len(), decided, gaps);
        let repeated = (cut - from) * 1024;
        assert_eq!(decided, Some(Joined::Seamless { repeated }), "placed where the burst began");
        assert!(repeated as f64 > TAIL_S * rate as f64);
        assert_eq!(gaps, 0);
        let ((l1, r1), (l2, r2)) = (sides(&one), sides(&two));
        let want_l: Vec<f64> = l1.iter().chain(&l2[repeated..]).copied().collect();
        let want_r: Vec<f64> = r1.iter().chain(&r2[repeated..]).copied().collect();
        assert!(l == want_l && r == want_r, "the old connection's frames, then the new one's from the first not had");
        let (wl, wr) = sides(&whole);
        let (first, largest) = differences(&l, &r, &wl, &wr);
        eprintln!("against one decode: {} vs {} frames, the join at {}, first difference {:?}, largest {:e}", l.len(), wl.len(), l1.len(), first, largest);
        assert_eq!(l.len(), wl.len());
        assert!(first.map_or(true, |i| i >= l1.len()), "the stream up to the join is the old decoder's, untouched");
    }

    /// Where `l`, `r` first differ from `wl`, `wr` (to the bit), and by how
    /// much at most.
    fn differences(l: &[f64], r: &[f64], wl: &[f64], wr: &[f64]) -> (Option<usize>, f64) {
        let n = l.len().min(wl.len());
        let first = (0..n).find(|&i| l[i].to_bits() != wl[i].to_bits() || r[i].to_bits() != wr[i].to_bits());
        let largest = (0..n).fold(0f64, |m, i| m.max((l[i] - wl[i]).abs()).max((r[i] - wr[i]).abs()));
        (first, largest)
    }

    /// An MP3 file as a station's dump (AURA_TEST_MP3, outside the
    /// repository): the first connection its bytes up to 40 s, the next one
    /// its bytes from 5 s (a burst of 35 s, longer than the joiner's tail),
    /// each decoded by a decoder of its own as the radio does — the stream
    /// goes on as if it had never broken.
    #[test]
    #[ignore]
    fn an_mp3_dump_goes_on_after_a_burst_longer_than_the_tail_as_if_unbroken() {
        use super::super::join::{Joined, TAIL_S};
        let Ok(path) = std::env::var("AURA_TEST_MP3") else {
            eprintln!("SKIPPED: AURA_TEST_MP3 not set");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let decode = |b: &[u8]| -> (u32, Vec<f32>) {
            let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(std::io::Cursor::new(b.to_vec()))), Default::default());
            let mut format =
                symphonia::default::get_probe().probe(&hint_for("audio/mpeg", &path), mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
            let (tid, mut dec, rate, ch, _) = open_track(&*format).unwrap();
            assert_eq!(ch, 2, "a stereo file");
            let (mut out, mut v) = (vec![], Vec::<f32>::new());
            while let Ok(Some(p)) = format.next_packet() {
                if p.track_id == tid {
                    if let Ok(d) = dec.decode(&p) {
                        d.copy_to_vec_interleaved(&mut v);
                        out.extend_from_slice(&v);
                    }
                }
            }
            (rate, out)
        };
        let (rate, whole) = decode(&bytes);
        assert!(whole.len() / 2 > 50 * rate as usize, "a file of 50 s or more");
        let per_s = bytes.len() as f64 / (whole.len() / 2) as f64 * rate as f64;
        let (one, two) = (decode(&bytes[..(40.0 * per_s) as usize]).1, decode(&bytes[(5.0 * per_s) as usize..]).1);
        let (l, r, decided, gaps) = rejoin(rate, &one, &two);
        eprintln!("{}: {} Hz; the reconnect: {:?}, gaps {}", path, rate, decided, gaps);
        assert!(matches!(decided, Some(Joined::Seamless { repeated }) if repeated as f64 > TAIL_S * rate as f64), "{:?}", decided);
        assert_eq!(gaps, 0);
        let (wl, wr) = sides(&whole);
        let (first, largest) = differences(&l, &r, &wl, &wr);
        eprintln!("against one decode: {} vs {} frames, the join at {}, first difference {:?}, largest {:e}", l.len(), wl.len(), one.len() / 2, first, largest);
        assert!(l.len().min(wl.len()) > 45 * rate as usize && first.is_none(), "the stream as if it had never broken");
    }

    /// A stream from a track's first sample, through the radio's own way —
    /// the joiner, the splicer and the rack's source stages (DC, intersample
    /// repair, subsonic filter, apodizer) — into the live source, comes out
    /// as the file path prepares the track (`prepare_audio_phase`), to the
    /// bit, whatever sizes it is decoded in. Kept in f32, as the live source
    /// once kept it, it would not: the stages' output is no f32's.
    #[test]
    fn a_stream_through_the_feed_is_the_prepared_track_bit_for_bit() {
        use crate::player::settings::PlayerSettings;
        use crate::player::source_stages::tests::{programme, same_bits, write_wav16};
        let rate = 44_100u32;
        let n = 3 * rate as usize;
        let (l, r) = programme(n, rate);
        let path = std::env::temp_dir().join(format!("aura-radio-feed-{}.wav", std::process::id()));
        write_wav16(&path, rate, &l, &r);
        let a = crate::audio::converter::decode::decode_file(&path).expect("decode the test file");
        let s = PlayerSettings {
            iir_dc_blocking: true,
            isp: true,
            subsonic_hz: 15,
            apodizing: 2,
            adaptive_apodizer: false,
            declip: false,
            adaptive_headroom: false,
            fs_multiplier: 2,
            ..PlayerSettings::default()
        };
        let mut eng = s.to_engine();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let p = crate::audio::converter::pipeline::prepare::prepare_audio_phase(&path, &mut eng, &cancel, None)
            .expect("prepare the test file");
        let _ = std::fs::remove_file(&path);
        let shared = Arc::new(RadioShared::new("http://x/stream", &s));
        let (mut joiner, mut splicer, mut stages) = (None, None, None);
        let f = Feeds { joiner: &mut joiner, splicer: &mut splicer, stages: &mut stages };
        let mut feed = Feed::new(&shared, f, "PCM", rate, 2, Some(16)).expect("a feed at 44.1 kHz");
        let sizes = [1_152usize, 1, 4_608, 65_536, 333];
        let (mut at, mut k) = (0, 0);
        while at < n {
            let c = sizes[k % sizes.len()].min(n - at);
            feed.push(&a.samples_l[at..at + c], &a.samples_r[at..at + c]);
            at += c;
            k += 1;
        }
        drop(feed);
        // The stream's end: what the splicer and the stages still hold.
        let live = shared.live.get().expect("made by the feed").clone();
        let (_, mut st) = stages.take().expect("made by the feed");
        splicer.as_mut().expect("made by the feed").flush(&mut |x, y| through_stages(&live, &mut st, x, y));
        finish_stages(st, &live);
        assert_eq!(live.end(), n as i64);
        let (mut gl, mut gr) = (vec![0.0; n], vec![0.0; n]);
        live.read(0, 0, &mut gl);
        live.read(1, 0, &mut gr);
        assert!(same_bits(&gl, &p.audio_l) && same_bits(&gr, &p.audio_r), "the stream differs from the prepared track");
        // Beside it the shadow — what BIT-PERFECT plays: the track as decoded,
        // to the bit, every frame of it.
        live.read_raw(0, 0, &mut gl);
        live.read_raw(1, 0, &mut gr);
        assert!(same_bits(&gl, &a.samples_l) && same_bits(&gr, &a.samples_r), "the shadow differs from the decoded track");
        assert_eq!(live.stats().raw_missing, 0);
        let f32_kept = |x: &[f64]| -> Vec<f64> { x.iter().map(|&v| v as f32 as f64).collect() };
        let differ = f32_kept(&gl).iter().zip(&p.audio_l).filter(|(x, y)| x.to_bits() != y.to_bits()).count();
        assert!(differ > n / 2, "kept in f32, only {} of {} samples would differ", differ, n);
    }

    /// A FLAC stream as Radio Paradise sends one — chained Ogg FLAC, 16-bit,
    /// a new logical stream at each song — through the decoder thread: the
    /// shadow BIT-PERFECT plays holds the stream's own integers (each
    /// sample ×32768 is the one in the stream, exactly), across the songs'
    /// join, while the source stages (the DC high-pass a stream always runs)
    /// changed the frames beside it. The stream says it is 16-bit: the
    /// device takes it as 16-bit integers (`OutputMode::Direct`).
    #[test]
    fn a_flac_streams_shadow_holds_its_own_integers_across_a_chained_join() {
        use crate::audio::converter::decode::test_ogg::{noise, ogg_flac_link, LINK_BLOCK};
        let (rate, frames) = (44_100u32, 40 * LINK_BLOCK);
        let (al, ar) = noise(7, frames);
        let (bl, br) = noise(8, frames);
        let mut bytes = Vec::new();
        ogg_flac_link(&mut bytes, 0x7001, rate, &al, &ar);
        ogg_flac_link(&mut bytes, 0x7002, rate, &bl, &br);
        let shared = run_connections(&plain(), "application/ogg", vec![bytes]);
        let info = shared.info.lock().unwrap().clone();
        assert_eq!((info.rate, info.bits, info.resets, info.decode_errors), (rate, Some(16), 1, 0), "{info:?}");
        let live = shared.live.get().expect("decoded").clone();
        // What the splicer still holds at the stream's end is not in yet.
        let n = live.end() as usize;
        assert!(n > frames + frames / 2, "{} of {} frames", n, 2 * frames);
        let want_l: Vec<i32> = al.iter().chain(&bl).copied().collect();
        let want_r: Vec<i32> = ar.iter().chain(&br).copied().collect();
        let (mut sl, mut sr) = (vec![0.0; n], vec![0.0; n]);
        live.read_raw(0, 0, &mut sl);
        live.read_raw(1, 0, &mut sr);
        for i in 0..n {
            let (a, b) = (sl[i] * 32_768.0, sr[i] * 32_768.0);
            assert!(a == want_l[i] as f64 && b == want_r[i] as f64, "frame {}: ({}, {}) for ({}, {})", i, a, b, want_l[i], want_r[i]);
        }
        let (mut gl, mut gr) = (vec![0.0; n], vec![0.0; n]);
        live.read(0, 0, &mut gl);
        live.read(1, 0, &mut gr);
        assert!(gl.iter().zip(&sl).any(|(g, s)| g.to_bits() != s.to_bits()), "the stages' frames are not the shadow's");
        assert_eq!(live.stats().raw_missing, 0);
    }

    #[test]
    fn the_hint_follows_the_content_type_then_the_address() {
        // Only checks that nothing panics and an extension is found; the
        // probe itself is symphonia's.
        let _ = hint_for("audio/mpeg", "http://x/stream");
        let _ = hint_for("", "http://x/stream.flac?a=1");
        let _ = hint_for("application/ogg; charset=x", "http://x/flac");
    }
}
