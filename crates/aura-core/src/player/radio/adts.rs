//! ADTS, the frames AAC comes in on internet radio: the header, the frames
//! of a stream that may start anywhere, and which decoder a stream goes to.
//!
//! symphonia decodes AAC-LC to the same numbers as a file. HE-AAC (AAC+) is
//! an AAC-LC core at half the rate with SBR on top (v2: a mono core with PS
//! for the stereo), and ADTS says so nowhere: the SBR rides in fill
//! elements a core decoder skips, so symphonia would give the core alone,
//! 22.05 or 24 kHz without its top octave. A core at 24 kHz or below goes
//! to the HE-AAC decoder (`he_aac`, the FDK AAC decoder), which reads the
//! SBR and the PS and gives the full rate; every other ADTS stream stays
//! with symphonia.

/// Sample rates by `sampling_frequency_index`.
pub const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// The highest core rate that is taken as HE-AAC's (half of 48 kHz).
pub const HE_CORE_MAX: u32 = 24000;

/// One ADTS frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The MPEG-4 audio object type (the header's profile + 1): 2 is AAC-LC.
    pub object: u8,
    pub rate_index: u8,
    /// The channel configuration: 1 mono, 2 stereo (0: in the stream's PCE).
    pub channels: u8,
    /// The whole frame, header included, in bytes.
    pub len: usize,
}

impl Header {
    /// A header without its CRC.
    pub const LEN: usize = 7;

    /// The header at the start of `b`, when one is there.
    pub fn parse(b: &[u8]) -> Option<Header> {
        // 12 bits of sync, the MPEG version, layer 00 (MPEG audio's layers
        // are 01..11: a header of theirs is never taken for this one).
        if b.len() < Self::LEN || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
            return None;
        }
        let rate_index = (b[2] >> 2) & 0x0F;
        if rate_index as usize >= RATES.len() {
            return None;
        }
        let header_len = if b[1] & 1 == 1 { 7 } else { 9 };
        let len = ((b[3] as usize & 3) << 11) | ((b[4] as usize) << 3) | (b[5] as usize >> 5);
        if len <= header_len {
            return None;
        }
        Some(Header { object: (b[2] >> 6) + 1, rate_index, channels: ((b[2] & 1) << 2) | (b[3] >> 6), len })
    }

    pub fn rate(&self) -> u32 {
        RATES[self.rate_index as usize]
    }

    /// The same stream: what holds for every frame of it.
    pub fn same_stream(&self, o: &Header) -> bool {
        self.object == o.object && self.rate_index == o.rate_index && self.channels == o.channels
    }

    /// The header (no CRC) of a frame of this stream with `payload` bytes
    /// after it (a file's raw AAC frames are framed with it for Windows'
    /// decoder; test streams are made with it).
    pub fn bytes(&self, payload: usize) -> [u8; 7] {
        let len = payload + Self::LEN;
        [
            0xFF,
            0xF1,
            ((self.object - 1) << 6) | (self.rate_index << 2) | (self.channels >> 2),
            ((self.channels & 3) << 6) | ((len >> 11) as u8 & 3),
            (len >> 3) as u8,
            ((len as u8 & 7) << 5) | 0x1F,
            0xFC,
        ]
    }

    /// The stream's AudioSpecificConfig: object type, rate, channels.
    pub fn asc(&self) -> [u8; 2] {
        let v = (self.object as u16) << 11 | (self.rate_index as u16) << 7 | (self.channels as u16) << 3;
        v.to_be_bytes()
    }

    /// A core at HE-AAC's rates: it goes to the decoder that reads SBR.
    pub fn he_core(&self) -> bool {
        self.rate() <= HE_CORE_MAX
    }
}

/// What an MP4 file's AudioSpecificConfig says of its AAC: the core's
/// header fields, and whether SBR (HE-AAC) or PS (HE-AAC v2) is signalled —
/// explicitly (object type 5 or 29) or by the backward-compatible
/// extensions after the core's config (0x2b7, 0x548).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asc {
    pub core: Header,
    pub sbr: bool,
    pub ps: bool,
}

/// An AudioSpecificConfig read (ISO 14496-3 1.6.2.1); None for one this
/// player does not take (an explicit rate, an escaped object type).
pub fn parse_asc(b: &[u8]) -> Option<Asc> {
    let mut pos = 0usize;
    let mut bits = |n: usize| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *b.get(pos / 8)?;
            v = v << 1 | ((byte >> (7 - pos % 8)) & 1) as u32;
            pos += 1;
        }
        Some(v)
    };
    let mut object = bits(5)? as u8;
    let rate_index = bits(4)? as u8;
    let channels = bits(4)? as u8;
    if object == 31 || rate_index as usize >= RATES.len() {
        return None;
    }
    let (mut sbr, mut ps) = (false, false);
    if object == 5 || object == 29 {
        // Explicit: the SBR rate, then the core's own object type.
        sbr = true;
        ps = object == 29;
        if bits(4)? == 15 {
            return None;
        }
        object = bits(5)? as u8;
        if object == 22 {
            bits(4)?;
        }
    } else if object == 2 {
        // GASpecificConfig: frame length, core coder, extension flag; then
        // what may follow it.
        bits(1)?;
        if bits(1)? == 1 {
            bits(14)?;
        }
        bits(1)?;
        if bits(11) == Some(0x2b7) && bits(5) == Some(5) && bits(1) == Some(1) {
            sbr = true;
            if bits(4) != Some(15) && bits(11) == Some(0x548) {
                ps = bits(1) == Some(1);
            }
        }
    }
    Some(Asc { core: Header { object, rate_index, channels, len: 0 }, sbr, ps })
}

/// The length of an MPEG audio (MP1/2/3) frame whose header starts `b`.
fn mpeg_frame(b: &[u8]) -> Option<(usize, u32)> {
    const KBPS: [[u16; 15]; 5] = [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    // Version: 0 MPEG-2.5, 2 MPEG-2, 3 MPEG-1 (1 reserved); layer: 1 III, 2 II, 3 I.
    let (version, layer) = ((b[1] >> 3) & 3, (b[1] >> 1) & 3);
    let (bri, sri, pad) = ((b[2] >> 4) as usize, ((b[2] >> 2) & 3) as usize, ((b[2] >> 1) & 1) as usize);
    if version == 1 || layer == 0 || bri == 0 || bri == 15 || sri == 3 {
        return None;
    }
    let row = match (version == 3, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, _) => 2,
        (false, 3) => 3,
        (false, _) => 4,
    };
    let br = KBPS[row][bri] as usize * 1000;
    let rate = [[11025, 12000, 8000], [0, 0, 0], [22050, 24000, 16000], [44100, 48000, 32000]][version as usize][sri];
    let len = match layer {
        3 => (12 * br / rate + pad) * 4,
        2 => 144 * br / rate + pad,
        _ if version == 3 => 144 * br / rate + pad,
        _ => 72 * br / rate + pad,
    };
    // The stream's identity for the chain: version, layer, rate.
    Some((len, (version as u32) << 24 | (layer as u32) << 20 | rate as u32))
}

/// Frames in a row that must follow each other (each header where the last
/// frame ends) for a stream to be taken as ADTS — or as MPEG audio.
const CHAIN: usize = 3;
/// Read this much without telling, a stream is left to symphonia's probe.
pub const SNIFF_MAX: usize = 64 * 1024;

/// What the first bytes of a stream are, for the decoder to be chosen by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sniffed {
    /// ADTS: its first frame's header, and where that frame starts.
    Adts(Header, usize),
    /// Anything else, or nothing that could be told: symphonia probes it.
    Other,
    /// Too little yet to tell.
    More,
}

enum Chain {
    Yes,
    No,
    Short,
}

/// Do `CHAIN` frames follow each other from the start of `b`?
fn chain(b: &[u8], frame: impl Fn(&[u8]) -> Option<(usize, u32)>) -> Chain {
    let Some((mut len, id)) = frame(b) else { return Chain::No };
    let mut at = 0;
    for _ in 1..CHAIN {
        at += len;
        if at + 7 > b.len() {
            return Chain::Short;
        }
        match frame(&b[at..]) {
            Some((l, i)) if i == id => len = l,
            _ => return Chain::No,
        }
    }
    Chain::Yes
}

fn adts_frame(b: &[u8]) -> Option<(usize, u32)> {
    Header::parse(b).map(|h| (h.len, (h.object as u32) << 16 | (h.rate_index as u32) << 8 | h.channels as u32))
}

/// The ID3v2 tag a stream may start with: its length, or None while it is
/// not all in yet.
fn id3_len(b: &[u8]) -> Option<usize> {
    if b.len() < 3 || &b[..3] != b"ID3" {
        return Some(0);
    }
    if b.len() < 10 {
        return None;
    }
    let size = b[6..10].iter().fold(0usize, |s, &x| s << 7 | (x & 0x7F) as usize);
    Some(10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 })
}

/// Tell ADTS from anything else by the first bytes (`eof`: no more will
/// come). An Ogg, FLAC or MP4 start is not ADTS; otherwise the first place
/// where `CHAIN` ADTS frames follow each other decides, unless `CHAIN` MPEG
/// audio frames do first.
pub fn sniff(b: &[u8], eof: bool) -> Sniffed {
    if b.starts_with(b"OggS") || b.starts_with(b"fLaC") || b.get(4..8) == Some(b"ftyp") {
        return Sniffed::Other;
    }
    let give_up = if eof || b.len() >= SNIFF_MAX { Sniffed::Other } else { Sniffed::More };
    let Some(start) = id3_len(b) else { return give_up };
    for i in start..b.len() {
        if b[i] != 0xFF {
            continue;
        }
        let rest = &b[i..];
        match chain(rest, adts_frame) {
            Chain::Yes => return Sniffed::Adts(Header::parse(rest).expect("a chain starts with a header"), i),
            Chain::Short if !eof => return give_up,
            _ => {}
        }
        match chain(rest, mpeg_frame) {
            Chain::Yes => return Sniffed::Other,
            Chain::Short if !eof => return give_up,
            _ => {}
        }
    }
    give_up
}

/// ADTS frames out of a byte stream that may start anywhere and carry
/// damage: a frame is handed on once the header after it is seen (or the
/// stream is over); bytes that are not a frame of the stream are skipped.
pub struct Framer {
    buf: Vec<u8>,
    pos: usize,
    stream: Option<Header>,
    /// Bytes skipped to find frames.
    pub skipped: u64,
}

impl Framer {
    pub fn new() -> Framer {
        Framer { buf: Vec::new(), pos: 0, stream: None, skipped: 0 }
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// The next whole frame, header included, into `out`; `eof`: no more
    /// bytes will come, so the last frame needs no header after it.
    pub fn next(&mut self, eof: bool, out: &mut Vec<u8>) -> Option<Header> {
        loop {
            let b = &self.buf[self.pos..];
            if b.len() < Header::LEN {
                if eof && !b.is_empty() {
                    self.skipped += b.len() as u64;
                    self.pos = self.buf.len();
                }
                return None;
            }
            let h = Header::parse(b).filter(|h| self.stream.map_or(true, |s| s.same_stream(h)));
            if let Some(h) = h {
                if b.len() < h.len + if eof { 0 } else { Header::LEN } {
                    if !eof {
                        return None;
                    }
                } else if eof && b.len() < h.len + Header::LEN
                    || Header::parse(&b[h.len..]).map_or(false, |n| n.same_stream(&h))
                {
                    out.clear();
                    out.extend_from_slice(&b[..h.len]);
                    self.pos += h.len;
                    self.stream = Some(h);
                    return Some(h);
                }
            }
            // Not a frame of the stream here: on to the next sync byte.
            let n = b[1..].iter().position(|&x| x == 0xFF).map_or(b.len(), |p| p + 1);
            self.pos += n;
            self.skipped += n as u64;
        }
    }
}

/// A frame known by its bytes (FNV-1a; the length mixed in).
pub fn frame_id(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x0100_0000_01b3)) ^ b.len() as u64
}

/// Where a new connection's first frames lie among those already decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// This many of them repeat what was had: the stream goes on after them.
    Repeats(usize),
    /// Not among them: audio was lost between the connections (a gap).
    New,
}

/// The frames a decoder has had, by their bytes, for a new connection to be
/// placed among: a server's burst repeats the newest frames it sent, byte
/// for byte, so the decoder can go on from the first frame it has not had,
/// as if the connection had never broken. (An HE-AAC decoder made afresh
/// would not give the same samples as the one that went on: SBR carries
/// its envelopes and noise from frame to frame, so its output cannot be
/// placed sample for sample, as `join` does for the other codecs.)
pub struct Trail {
    seen: std::collections::VecDeque<u64>,
    cap: usize,
}

impl Trail {
    /// Keeps the last `cap` frames.
    pub fn new(cap: usize) -> Trail {
        Trail { seen: std::collections::VecDeque::with_capacity(cap), cap: cap.max(1) }
    }

    /// A frame went to the decoder.
    pub fn had(&mut self, id: u64) {
        if self.seen.len() == self.cap {
            self.seen.pop_front();
        }
        self.seen.push_back(id);
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Where the frames `new` (a new connection's, from its first) lie
    /// among those had, when it can be told; None while it cannot yet.
    ///
    /// A burst that starts among the frames had: the newest place it
    /// matches, up to the last frame had (a burst is the newest audio a
    /// server has; frames alike, as of silence, repeat a little of it at
    /// worst). A burst longer than what is kept starts before it: then the
    /// last frame had is found among the new ones, all that is kept matching
    /// up to it. Neither: not told yet — the last frame may still come (the
    /// caller gives up after a while: audio was lost).
    pub fn place(&self, new: &[u64]) -> Option<Placed> {
        let first = *new.first()?;
        let n = self.seen.len();
        if n == 0 {
            return Some(Placed::New);
        }
        for p in (0..n).rev().filter(|&p| self.seen[p] == first) {
            let span = n - p;
            if self.seen.range(p..).zip(new).all(|(a, b)| a == b) {
                return (new.len() >= span).then_some(Placed::Repeats(span));
            }
        }
        let last = self.seen[n - 1];
        for k in (0..new.len()).filter(|&k| new[k] == last) {
            let m = k.min(n - 1);
            if self.seen.range(n - 1 - m..).zip(&new[k - m..=k]).all(|(a, b)| a == b) {
                return Some(Placed::Repeats(k + 1));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream of ADTS frames: `n` frames of the header's stream with
    /// payloads of varying lengths, filled with bytes that hold no sync.
    pub fn frames(h: Header, n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            let payload = 100 + (i * 37) % 300;
            v.extend_from_slice(&h.bytes(payload));
            v.extend((0..payload).map(|k| (k % 200) as u8 + 1));
        }
        v
    }

    fn he() -> Header {
        // HE-AAC's core: AAC-LC at 24 kHz, stereo.
        Header { object: 2, rate_index: 6, channels: 2, len: 0 }
    }

    #[test]
    fn a_header_reads_back_what_it_was_written_with() {
        for (object, rate_index, channels, payload) in [(2, 3, 2, 371), (2, 7, 1, 5000), (2, 6, 2, 8184 - 7), (1, 4, 6, 1)] {
            let h = Header { object, rate_index, channels, len: 0 };
            let b = h.bytes(payload);
            let p = Header::parse(&b).expect("parses");
            assert_eq!(p, Header { len: payload + 7, ..h });
            assert!(p.same_stream(&h));
        }
        let h = Header::parse(&he().bytes(10)).unwrap();
        assert_eq!((h.rate(), h.he_core()), (24000, true));
        let lc = Header { rate_index: 3, ..he() };
        assert!(!lc.he_core(), "48 kHz is AAC-LC's");
        // AAC-LC, 44.1 kHz, stereo: object type 2, index 4, channels 2.
        assert_eq!(Header { object: 2, rate_index: 4, channels: 2, len: 0 }.asc(), [0x12, 0x10]);
    }

    #[test]
    fn mpeg_audio_headers_are_not_taken_for_adts() {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz.
        let mp3 = [0xFF, 0xFB, 0x90, 0x64];
        assert_eq!(Header::parse(&[mp3[0], mp3[1], mp3[2], mp3[3], 0, 0, 0]), None);
        assert_eq!(mpeg_frame(&mp3).map(|x| x.0), Some(417));
        // And an ADTS header is not MPEG audio.
        assert_eq!(mpeg_frame(&he().bytes(100)), None);
    }

    #[test]
    fn adts_is_found_from_anywhere_after_a_tag_or_a_broken_frame() {
        let s = frames(he(), 10);
        assert_eq!(sniff(&s, false), Sniffed::Adts(Header::parse(&s).unwrap(), 0));
        // The stream starts inside a frame.
        let cut = &s[150..];
        let first = s.len() - cut.len();
        let at = (0..cut.len()).find(|&i| Header::parse(&cut[i..]).is_some()).unwrap();
        assert!(matches!(sniff(cut, false), Sniffed::Adts(h, i) if i == at && h.rate() == 24000), "{}", first);
        // After an ID3 tag.
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x05hello".to_vec();
        tagged.extend_from_slice(&s);
        assert!(matches!(sniff(&tagged, false), Sniffed::Adts(_, 15)));
        // A tag not all in yet: more.
        assert_eq!(sniff(&tagged[..12], false), Sniffed::More);
    }

    #[test]
    fn too_little_waits_and_other_streams_are_left_to_symphonia() {
        // Two frames and a bit: the third header is not in yet.
        let s = frames(he(), 10);
        assert_eq!(sniff(&s[..200], false), Sniffed::More);
        assert_eq!(sniff(&s[..200], true), Sniffed::Other, "the stream ended before telling");
        assert_eq!(sniff(b"OggS\x00\x02", false), Sniffed::Other);
        assert_eq!(sniff(b"fLaC\x00\x00\x00\x22", false), Sniffed::Other);
        // MPEG audio: three frames in a row.
        let mut mp3 = Vec::new();
        for _ in 0..4 {
            mp3.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            mp3.extend(std::iter::repeat(0x11).take(413));
        }
        assert_eq!(sniff(&mp3, false), Sniffed::Other);
        // Nothing to tell in a lot of bytes: symphonia decides.
        assert_eq!(sniff(&vec![0x22; SNIFF_MAX], false), Sniffed::Other);
    }

    #[test]
    fn the_framer_hands_on_whole_frames_in_any_split_and_skips_damage() {
        let s = frames(he(), 12);
        let want: Vec<Vec<u8>> = {
            let mut v = vec![];
            let mut at = 0;
            while at < s.len() {
                let h = Header::parse(&s[at..]).unwrap();
                v.push(s[at..at + h.len].to_vec());
                at += h.len;
            }
            v
        };
        for split in [1, 7, 100, 999, s.len()] {
            let mut f = Framer::new();
            let mut got = vec![];
            let mut out = vec![];
            for part in s.chunks(split) {
                f.push(part);
                while f.next(false, &mut out).is_some() {
                    got.push(out.clone());
                }
            }
            while f.next(true, &mut out).is_some() {
                got.push(out.clone());
            }
            assert_eq!(got, want, "split {}", split);
            assert_eq!(f.skipped, 0);
        }
        // Started mid-frame, and one frame's header damaged: the rest comes.
        let mut d = s[50..].to_vec();
        let second = want[0].len() - 50;
        d[second + 1] = 0x00;
        let mut f = Framer::new();
        f.push(&d);
        let mut got = vec![];
        let mut out = vec![];
        while f.next(true, &mut out).is_some() {
            got.push(out.clone());
        }
        assert_eq!(got, want[2..].to_vec());
        assert!(f.skipped > 0);
    }

    /// Bits, most significant first, packed into bytes (the last one padded).
    fn pack(fields: &[(u32, usize)]) -> Vec<u8> {
        let bits: Vec<u8> = fields.iter().flat_map(|&(v, n)| (0..n).rev().map(move |i| (v >> i & 1) as u8)).collect();
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | x << (7 - i))).collect()
    }

    #[test]
    fn an_audio_specific_config_says_whether_sbr_and_ps_ride_on_its_core() {
        let lc = |sfi: u32, ch: u32| vec![(2, 5), (sfi, 4), (ch, 4), (0, 3)];
        // AAC-LC 44.1 kHz stereo (0x1210): nothing more.
        let a = parse_asc(&[0x12, 0x10]).unwrap();
        assert_eq!((a.core.object, a.core.rate(), a.core.channels, a.sbr, a.ps), (2, 44100, 2, false, false));
        // AAC-LC at 22.05 kHz, nothing signalled: a core at HE-AAC's rates.
        let a = parse_asc(&pack(&lc(7, 2))).unwrap();
        assert!(!a.sbr && a.core.he_core());
        // Explicit HE-AAC (object type 5): the core's rate first, then SBR's, then the core's type.
        let a = parse_asc(&pack(&[(5, 5), (7, 4), (2, 4), (4, 4), (2, 5), (0, 3)])).unwrap();
        assert_eq!((a.core.object, a.core.rate(), a.core.channels, a.sbr, a.ps), (2, 22050, 2, true, false));
        // Explicit HE-AAC v2 (29): a mono core with PS.
        let a = parse_asc(&pack(&[(29, 5), (6, 4), (1, 4), (3, 4), (2, 5), (0, 3)])).unwrap();
        assert_eq!((a.core.rate(), a.core.channels, a.sbr, a.ps), (24000, 1, true, true));
        // Backward-compatible: the core's config, then 0x2b7 SBR (and 0x548 PS).
        let mut f = lc(7, 1);
        f.extend([(0x2b7, 11), (5, 5), (1, 1), (4, 4), (0x548, 11), (1, 1)]);
        let a = parse_asc(&pack(&f)).unwrap();
        assert_eq!((a.core.rate(), a.core.channels, a.sbr, a.ps), (22050, 1, true, true));
        // An escaped object type or an explicit rate is not taken.
        assert_eq!(parse_asc(&pack(&[(31, 5), (0, 6), (4, 4), (2, 4)])), None);
        assert_eq!(parse_asc(&pack(&[(2, 5), (15, 4), (0, 24), (2, 4)])), None);
    }

    #[test]
    fn a_new_connection_is_placed_among_the_frames_had() {
        let ids: Vec<u64> = (0..100u64).map(|i| frame_id(&i.to_le_bytes())).collect();
        let mut t = Trail::new(64);
        for &id in &ids {
            t.had(id);
        }
        // Only the last 64 are kept: frames 36..100.
        // A burst from frame 70: 30 frames repeat, told once they are all in.
        assert_eq!(t.place(&ids[70..80]), None);
        assert_eq!(t.place(&ids[70..100]), Some(Placed::Repeats(30)));
        assert_eq!(t.place(&ids[70..]), Some(Placed::Repeats(30)));
        // From frame 99: one repeats.
        assert_eq!(t.place(&ids[99..]), Some(Placed::Repeats(1)));
        // A burst longer than what is kept (from frame 20): told when the
        // last frame had comes among the new ones — 80 repeat.
        assert_eq!(t.place(&ids[20..60]), None);
        assert_eq!(t.place(&ids[20..100]), Some(Placed::Repeats(80)));
        // Never had: not told — the caller gives up after a while (a gap).
        assert_eq!(t.place(&[frame_id(b"elsewhere")]), None);
        // A burst that matches at first and then does not: not this place.
        let mut wrong = ids[90..95].to_vec();
        wrong.push(frame_id(b"x"));
        assert_eq!(t.place(&wrong), None);
        assert_eq!(t.place(&[]), None);
        assert_eq!(Trail::new(8).place(&ids[..3]), Some(Placed::New), "nothing had: new");
        // Frames alike (silence): the newest place, the least repeated.
        let mut s = Trail::new(16);
        let quiet = frame_id(b"quiet");
        for _ in 0..5 {
            s.had(quiet);
        }
        assert_eq!(s.place(&[quiet, quiet, quiet, ids[0]]), Some(Placed::Repeats(1)));
    }
}
