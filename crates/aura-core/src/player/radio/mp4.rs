//! Fragmented MP4 (CMAF) HLS segments: the audio track's samples out of
//! their moof/mdat pairs, as the elementary stream the decoder reads from an
//! Icecast connection — AAC access units behind an ADTS header made from the
//! track's AudioSpecificConfig (HE-AAC as its AAC-LC core, the decoder finds
//! the SBR), MP3 frames as they are.

use super::ts::Es;

/// Why a fragmented MP4 stream cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mp4Error {
    /// Not what this player decodes, or not readable.
    Unsupported(String),
    /// Encrypted samples (CENC/CBCS).
    Encrypted(String),
}

fn unsupported(s: &str) -> Mp4Error {
    Mp4Error::Unsupported(format!("unsupported: HLS fMP4 {}", s))
}

/// The audio track of an initialization section (EXT-X-MAP).
#[derive(Clone, Debug, PartialEq)]
pub struct Init {
    pub track_id: u32,
    pub es: Es,
    /// ADTS fields: profile (object type − 1), sampling frequency index,
    /// channel configuration. None for MP3.
    adts: Option<(u8, u8, u8)>,
    /// The movie's default sample size for the track (trex), 0 if none.
    default_size: u32,
}

/// The boxes in `b`: (type, body, offset of the box in `b`).
fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8], usize)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= b.len() {
        let size = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
        let t = [b[at + 4], b[at + 5], b[at + 6], b[at + 7]];
        let (head, size) = match size {
            1 if at + 16 <= b.len() => {
                let s = u64::from_be_bytes(b[at + 8..at + 16].try_into().expect("8 bytes"));
                (16, usize::try_from(s).unwrap_or(usize::MAX))
            }
            0 => (8, b.len() - at),
            s => (8, s),
        };
        if size < head || at + size > b.len() {
            // A box cut short: what is there of it.
            out.push((t, &b[(at + head).min(b.len())..], at));
            break;
        }
        out.push((t, &b[at + head..at + size], at));
        at += size;
    }
    out
}

fn child<'a>(b: &'a [u8], t: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).into_iter().find(|(x, ..)| x == t).map(|(_, body, _)| body)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
}

/// An MPEG-4 descriptor at `b`: (tag, body, bytes taken).
fn descriptor(b: &[u8]) -> Option<(u8, &[u8], usize)> {
    let tag = *b.first()?;
    let (mut len, mut at) = (0usize, 1usize);
    for _ in 0..4 {
        let x = *b.get(at)?;
        at += 1;
        len = (len << 7) | usize::from(x & 0x7F);
        if x & 0x80 == 0 {
            break;
        }
    }
    let end = (at + len).min(b.len());
    Some((tag, &b[at..end], end))
}

/// The object type indication and the decoder specific info of an esds box.
fn esds(b: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (tag, es, _) = descriptor(b.get(4..)?)?;
    if tag != 0x03 {
        return None;
    }
    let flags = *es.get(2)?;
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + usize::from(*es.get(at)?);
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    let (tag, dc, _) = descriptor(es.get(at..)?)?;
    if tag != 0x04 {
        return None;
    }
    let oti = *dc.first()?;
    let dsi = dc.get(13..).and_then(descriptor).filter(|(t, ..)| *t == 0x05).map(|(_, d, _)| d.to_vec()).unwrap_or_default();
    Some((oti, dsi))
}

struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.at / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }

    fn object_type(&mut self) -> Option<u32> {
        let t = self.read(5)?;
        if t == 31 {
            return Some(32 + self.read(6)?);
        }
        Some(t)
    }
}

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// ADTS fields from an AudioSpecificConfig: (profile, sampling frequency
/// index, channel configuration). An explicit SBR/PS config (object type 5
/// or 29) gives its core (the first rate is the core's): the decoder finds
/// the SBR in the frames, as with an implicit HE-AAC stream.
pub fn asc_adts(asc: &[u8]) -> Option<(u8, u8, u8)> {
    let mut r = Bits { b: asc, at: 0 };
    let mut aot = r.object_type()?;
    let mut sfi = r.read(4)?;
    if sfi == 15 {
        let f = r.read(24)?;
        sfi = RATES.iter().position(|&x| x == f)? as u32;
    }
    let ch = r.read(4)?;
    if aot == 5 || aot == 29 {
        if r.read(4)? == 15 {
            r.read(24)?;
        }
        aot = r.object_type()?;
    }
    if !(1..=4).contains(&aot) || ch == 0 || ch > 7 {
        return None;
    }
    Some(((aot - 1) as u8, sfi as u8, ch as u8))
}

/// The 7 bytes of an ADTS header for a frame of `payload` bytes.
pub fn adts_header((profile, sfi, ch): (u8, u8, u8), payload: usize) -> [u8; 7] {
    let len = payload + 7;
    [
        0xFF,
        0xF1,
        (profile << 6) | (sfi << 2) | (ch >> 2),
        ((ch & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// Read an initialization section: its first audio track.
pub fn parse_init(b: &[u8]) -> Result<Init, Mp4Error> {
    let moov = child(b, b"moov").ok_or_else(|| unsupported("initialization without a movie box"))?;
    let mut defaults: Vec<(u32, u32)> = Vec::new();
    if let Some(mvex) = child(moov, b"mvex") {
        for (t, trex, _) in boxes(mvex) {
            if &t == b"trex" {
                if let (Some(id), Some(size)) = (be32(trex, 4), be32(trex, 16)) {
                    defaults.push((id, size));
                }
            }
        }
    }
    for (t, trak, _) in boxes(moov) {
        if &t != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else { continue };
        if child(mdia, b"hdlr").and_then(|h| h.get(8..12)) != Some(b"soun".as_slice()) {
            continue;
        }
        let tkhd = child(trak, b"tkhd").ok_or_else(|| unsupported("track without a header"))?;
        let track_id = if tkhd.first() == Some(&1) { be32(tkhd, 20) } else { be32(tkhd, 12) }.ok_or_else(|| unsupported("track header cut short"))?;
        let stsd = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).and_then(|s| child(s, b"stsd")).ok_or_else(|| unsupported("track without sample descriptions"))?;
        let Some((entry, body, _)) = boxes(stsd.get(8..).unwrap_or(&[])).into_iter().next() else {
            return Err(unsupported("track without sample descriptions"));
        };
        match &entry {
            b"enca" => return Err(Mp4Error::Encrypted("the HLS stream's audio is encrypted (fMP4)".into())),
            b"mp4a" => {}
            other => return Err(unsupported(&format!("audio '{}'", String::from_utf8_lossy(other)))),
        }
        let (oti, dsi) = body.get(28..).and_then(|c| child(c, b"esds")).and_then(esds).ok_or_else(|| unsupported("audio without its decoder configuration"))?;
        let default_size = defaults.iter().find(|(id, _)| *id == track_id).map_or(0, |(_, s)| *s);
        let (es, adts) = match oti {
            0x40 => {
                let a = asc_adts(&dsi).ok_or_else(|| unsupported("AAC configuration this player cannot carry"))?;
                (Es::Adts, Some(a))
            }
            // MPEG-2 AAC Main, LC, SSR: the profile is the object type; the
            // rate and channels are in the sample entry.
            0x66..=0x68 => {
                let rate = be32(body, 24).map_or(0, |r| r >> 16);
                let ch = body.get(16..18).map_or(2, |c| u16::from_be_bytes([c[0], c[1]])) as u8;
                let sfi = RATES.iter().position(|&x| x == rate).ok_or_else(|| unsupported("AAC at an unusual rate"))? as u8;
                (Es::Adts, Some((oti - 0x66, sfi, ch)))
            }
            0x69 | 0x6B => (Es::Mpeg, None),
            t => return Err(unsupported(&format!("audio object type 0x{:02x}", t))),
        };
        return Ok(Init { track_id, es, adts, default_size });
    }
    Err(unsupported("initialization without an audio track"))
}

/// The audio samples of a media segment (moof/mdat pairs) as elementary
/// stream, appended to `out`.
pub fn samples(init: &Init, seg: &[u8], out: &mut Vec<u8>) -> Result<(), Mp4Error> {
    for (t, moof, moof_at) in boxes(seg) {
        if &t != b"moof" {
            continue;
        }
        for (t, traf, _) in boxes(moof) {
            if &t != b"traf" {
                continue;
            }
            let tfhd = child(traf, b"tfhd").ok_or_else(|| unsupported("fragment without a header"))?;
            let flags = be32(tfhd, 0).unwrap_or(0) & 0x00FF_FFFF;
            if be32(tfhd, 4) != Some(init.track_id) {
                continue;
            }
            if child(traf, b"senc").is_some() {
                return Err(Mp4Error::Encrypted("the HLS stream's audio is encrypted (fMP4)".into()));
            }
            let mut at = 8;
            let mut base = moof_at as u64;
            if flags & 0x01 != 0 {
                base = tfhd.get(at..at + 8).map_or(base, |x| u64::from_be_bytes(x.try_into().expect("8 bytes")));
                at += 8;
            }
            if flags & 0x02 != 0 {
                at += 4;
            }
            if flags & 0x08 != 0 {
                at += 4;
            }
            let default_size = if flags & 0x10 != 0 { be32(tfhd, at).unwrap_or(0) } else { init.default_size };
            // Several runs: each goes on where the last ended, unless it says.
            let mut next = None::<u64>;
            for (t, trun, _) in boxes(traf) {
                if &t != b"trun" {
                    continue;
                }
                let tf = be32(trun, 0).unwrap_or(0) & 0x00FF_FFFF;
                let count = be32(trun, 4).unwrap_or(0) as usize;
                let mut p = 8;
                let mut data = next.unwrap_or(base);
                if tf & 0x01 != 0 {
                    let off = be32(trun, p).unwrap_or(0) as i32;
                    data = (base as i64 + i64::from(off)).max(0) as u64;
                    p += 4;
                }
                if tf & 0x04 != 0 {
                    p += 4;
                }
                let per = [0x100u32, 0x200, 0x400, 0x800].iter().filter(|&&f| tf & f != 0).count() * 4;
                let size_at = if tf & 0x100 != 0 { 4 } else { 0 };
                let mut pos = data as usize;
                for k in 0..count {
                    let size = if tf & 0x200 != 0 {
                        be32(trun, p + k * per + size_at).ok_or_else(|| unsupported("sample sizes cut short"))?
                    } else {
                        default_size
                    } as usize;
                    let sample = seg.get(pos..pos + size).ok_or_else(|| unsupported("samples outside the segment"))?;
                    if let Some(a) = init.adts {
                        out.extend_from_slice(&adts_header(a, size));
                    }
                    out.extend_from_slice(sample);
                    pos += size;
                }
                next = Some(pos as u64);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(t);
        b.extend_from_slice(body);
        b
    }

    /// An initialization section with one audio track (`id`) whose esds
    /// carries `oti` and `asc`; a video track before it.
    pub(crate) fn init(id: u32, oti: u8, asc: &[u8], entry: &[u8; 4]) -> Vec<u8> {
        let mut dsi = vec![0x05, asc.len() as u8];
        dsi.extend_from_slice(asc);
        let mut dc = vec![0x04, (13 + dsi.len()) as u8, oti, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        dc.extend_from_slice(&dsi);
        let mut es = vec![0x03, (3 + dc.len()) as u8, 0, 1, 0];
        es.extend_from_slice(&dc);
        let mut esds = vec![0u8; 4];
        esds.extend_from_slice(&es);
        let mut mp4a = vec![0u8; 28];
        mp4a[17] = 2;
        mp4a[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
        mp4a.extend_from_slice(&bx(b"esds", &esds));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&bx(entry, &mp4a));
        let track = |id: u32, handler: &[u8; 4], stsd: &[u8]| {
            let mut tkhd = vec![0u8; 84];
            tkhd[12..16].copy_from_slice(&id.to_be_bytes());
            let mut hdlr = vec![0u8; 24];
            hdlr[8..12].copy_from_slice(handler);
            let stbl = bx(b"stbl", &bx(b"stsd", stsd));
            let minf = bx(b"minf", &stbl);
            let mdia = bx(b"mdia", &[bx(b"mdhd", &[0u8; 24]), bx(b"hdlr", &hdlr), minf].concat());
            bx(b"trak", &[bx(b"tkhd", &tkhd), mdia].concat())
        };
        let mut trex = vec![0u8; 24];
        trex[4..8].copy_from_slice(&id.to_be_bytes());
        let moov = bx(
            b"moov",
            &[bx(b"mvhd", &[0u8; 100]), track(7, b"vide", &[0, 0, 0, 0, 0, 0, 0, 0]), track(id, b"soun", &stsd), bx(b"mvex", &bx(b"trex", &trex))].concat(),
        );
        [bx(b"ftyp", b"iso6\0\0\0\0"), moov].concat()
    }

    /// A media segment: one fragment of track `id` with `samples`, their
    /// sizes in its run, the data offset from the fragment's start.
    pub(crate) fn segment(id: u32, samples: &[Vec<u8>]) -> Vec<u8> {
        let tfhd = [vec![0, 0x02, 0, 0], id.to_be_bytes().to_vec()].concat();
        let mut trun = vec![0, 0, 0x03, 0x01];
        trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        trun.extend_from_slice(&[0, 0, 0, 0]);
        for s in samples {
            trun.extend_from_slice(&1024u32.to_be_bytes());
            trun.extend_from_slice(&(s.len() as u32).to_be_bytes());
        }
        let traf = bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"tfdt", &[0u8; 8]), bx(b"trun", &trun)].concat());
        let mut moof = bx(b"moof", &[bx(b"mfhd", &[0u8; 8]), traf].concat());
        // The data offset: past the fragment and the mdat header.
        let off = (moof.len() + 8) as u32;
        let at = moof.windows(4).position(|w| w == b"trun").expect("trun") + 4 + 8;
        moof[at..at + 4].copy_from_slice(&off.to_be_bytes());
        let mdat = bx(b"mdat", &samples.concat());
        [bx(b"styp", b"msdh\0\0\0\0"), moof, mdat].concat()
    }

    /// AudioSpecificConfigs to ADTS fields: AAC-LC; explicit HE-AAC (its
    /// core); a rate given in full; what ADTS cannot carry.
    #[test]
    fn audio_configs_become_adts_headers() {
        assert_eq!(asc_adts(&[0x11, 0x90]), Some((1, 3, 2)), "AAC-LC 48 kHz stereo");
        assert_eq!(asc_adts(&[0x12, 0x10]), Some((1, 4, 2)), "AAC-LC 44.1 kHz stereo");
        // Object type 5, core at 24 kHz (index 6), stereo, SBR at 48 kHz (3), core object type 2.
        assert_eq!(asc_adts(&[0x2B, 0x11, 0x88, 0x00]), Some((1, 6, 2)), "HE-AAC: its AAC-LC core");
        // A rate in full: 44 100 Hz.
        assert_eq!(asc_adts(&[0x17, 0x80, 0x56, 0x22, 0x10]), Some((1, 4, 2)));
        assert_eq!(asc_adts(&[0x11, 0x80]), None, "channels in a program config element");
        assert_eq!(asc_adts(&[]), None);
        // The header: the same as an encoder's for 107 bytes of AAC-LC 48 kHz stereo.
        assert_eq!(adts_header((1, 3, 2), 100), [0xFF, 0xF1, 0x4C, 0x80, 0x0D, 0x7F, 0xFC]);
    }

    /// The audio track's samples come out as ADTS frames, in order, past a
    /// video track; MP3 samples as they are; encrypted and other audio named.
    #[test]
    fn the_audio_track_comes_out_as_adts_frames() {
        let units: Vec<Vec<u8>> = (0..5u8).map(|k| vec![k; 10 + 7 * k as usize]).collect();
        let i = parse_init(&init(2, 0x40, &[0x11, 0x90], b"mp4a")).expect("the init");
        assert_eq!((i.track_id, i.es), (2, Es::Adts));
        let mut out = Vec::new();
        samples(&i, &segment(2, &units), &mut out).expect("the samples");
        let want: Vec<u8> = units.iter().flat_map(|s| [adts_header((1, 3, 2), s.len()).to_vec(), s.clone()].concat()).collect();
        assert_eq!(out, want);
        // Another track's fragment: nothing.
        out.clear();
        samples(&i, &segment(9, &units), &mut out).unwrap();
        assert!(out.is_empty());
        // MP3 in MP4.
        let i = parse_init(&init(1, 0x6B, &[], b"mp4a")).unwrap();
        out.clear();
        samples(&i, &segment(1, &units), &mut out).unwrap();
        assert_eq!(out, units.concat());
        assert_eq!(parse_init(&init(1, 0x40, &[0x11, 0x90], b"enca")), Err(Mp4Error::Encrypted("the HLS stream's audio is encrypted (fMP4)".into())));
        assert_eq!(parse_init(&init(1, 0xA5, &[], b"mp4a")), Err(unsupported("audio object type 0xa5")));
        assert_eq!(parse_init(b"junk"), Err(unsupported("initialization without a movie box")));
    }

    /// A recorded fMP4 init and segment (`AURA_HLS_FMP4="init;segment"`):
    /// the ADTS made of it decodes, frame for frame.
    /// `cargo test --profile fast --bins fmp4_recorded -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fmp4_recorded_segment_decodes() {
        use symphonia::core::codecs::audio::AudioDecoderOptions;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        let Ok(files) = std::env::var("AURA_HLS_FMP4") else { return };
        let (a, b) = files.split_once(';').expect("init;segment");
        let i = parse_init(&std::fs::read(a).unwrap()).expect("the init");
        let mut out = Vec::new();
        samples(&i, &std::fs::read(b).unwrap(), &mut out).expect("the samples");
        let mut hint = Hint::new();
        hint.with_extension("aac");
        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(out)), Default::default());
        let mut f = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).expect("ADTS");
        let track = f.tracks()[0].clone();
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).expect("audio").clone();
        let mut d = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default()).expect("a decoder");
        let (mut frames, mut packets, mut v) = (0usize, 0usize, Vec::<f64>::new());
        while let Ok(Some(p)) = f.next_packet() {
            let dec = d.decode(&p).expect("a frame");
            let ch = dec.spec().channels().count().max(1);
            dec.copy_to_vec_interleaved(&mut v);
            frames += v.len() / ch;
            packets += 1;
        }
        eprintln!("FMP4 {:?}: {} packets, {} frames at {:?} Hz", i.es, packets, frames, params.sample_rate);
        assert!(packets > 100 && frames >= (packets - 2) * 1024);
    }
}
