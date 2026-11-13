//! A minimal MPEG transport stream demultiplexer for HLS segments: the audio
//! as its elementary stream — ADTS (AAC) or MPEG audio, the same bytes an
//! Icecast connection brings — and the timed ID3 tags beside it, for what
//! plays. PAT, then PMT, then the first audio stream the player decodes; PES
//! headers taken off, adaptation fields skipped. One demultiplexer goes on
//! across the segments of a connection: its tables, its continuity counters
//! and a PES that a segment boundary cuts carry over.

use std::collections::HashMap;

const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// The audio the player decodes, as an elementary stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Es {
    /// AAC in ADTS frames (stream type 0x0F).
    Adts,
    /// MPEG-1/2 audio: MP3, MP2 (stream types 0x03, 0x04).
    Mpeg,
}

impl Es {
    /// What the decoder is told the bytes are (decode.rs reads it as an
    /// Icecast server's content type).
    pub fn content_type(self) -> &'static str {
        match self {
            Es::Adts => "audio/aac",
            Es::Mpeg => "audio/mpeg",
        }
    }
}

/// Why a stream's audio cannot be taken out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TsError {
    /// Audio this player does not decode, or none at all.
    Unsupported(String),
    /// The audio is encrypted (SAMPLE-AES).
    Encrypted(String),
}

/// A table section (PAT, PMT) being gathered across packets.
struct Section {
    pid: u16,
    bytes: Vec<u8>,
}

pub struct TsDemux {
    /// The start of a packet that the last bytes in did not complete.
    carry: Vec<u8>,
    pmt_pid: Option<u16>,
    audio: Option<(u16, Es)>,
    id3_pid: Option<u16>,
    section: Option<Section>,
    /// The audio PES header gathered so far, while it is not complete.
    head: Vec<u8>,
    in_head: bool,
    /// The metadata PES gathered so far.
    meta: Vec<u8>,
    cc: HashMap<u16, u8>,
    /// Packets missing (continuity counters that jumped).
    pub lost: u64,
    /// Bytes skipped to find the packets' sync again.
    pub resynced: u64,
}

impl Default for TsDemux {
    fn default() -> Self {
        TsDemux::new()
    }
}

impl TsDemux {
    pub fn new() -> TsDemux {
        TsDemux {
            carry: Vec::new(),
            pmt_pid: None,
            audio: None,
            id3_pid: None,
            section: None,
            head: Vec::new(),
            in_head: false,
            meta: Vec::new(),
            cc: HashMap::new(),
            lost: 0,
            resynced: 0,
        }
    }

    /// The audio stream found in the PMT, once it is.
    pub fn es(&self) -> Option<Es> {
        self.audio.map(|(_, e)| e)
    }

    /// A new segment begins: it starts on a packet boundary (a piece of a
    /// packet left from the last one is dropped), and its counters may start
    /// afresh — a segmenter that makes each segment on its own does that, and
    /// a packet with the counter the last segment ended on is not a repeat.
    pub fn segment_starts(&mut self) {
        self.resynced += self.carry.len() as u64;
        self.carry.clear();
        self.cc.clear();
    }

    /// Take the next bytes of the stream: `audio` gets the elementary audio
    /// as it comes, `meta` each complete ID3 tag of the timed metadata.
    pub fn push(
        &mut self,
        data: &[u8],
        audio: &mut dyn FnMut(Es, &[u8]),
        meta: &mut dyn FnMut(&[u8]),
    ) -> Result<(), TsError> {
        self.carry.extend_from_slice(data);
        let mut at = 0;
        while self.carry.len() - at >= PACKET {
            if self.carry[at] != SYNC {
                // Lost the packets' rhythm: the next byte that starts one (and,
                // where it can be seen, is followed by another).
                let from = at + 1;
                let next = (from..self.carry.len()).find(|&i| {
                    self.carry[i] == SYNC && (i + PACKET >= self.carry.len() || self.carry[i + PACKET] == SYNC)
                });
                let to = next.unwrap_or(self.carry.len());
                self.resynced += (to - at) as u64;
                at = to;
                continue;
            }
            let mut p = [0u8; PACKET];
            p.copy_from_slice(&self.carry[at..at + PACKET]);
            at += PACKET;
            self.packet(&p, audio, meta)?;
        }
        self.carry.drain(..at);
        Ok(())
    }

    /// The stream ends here (a connection closed): the metadata PES held is
    /// read. The audio needs nothing: it went out as it came.
    pub fn flush(&mut self, meta: &mut dyn FnMut(&[u8])) {
        self.end_meta(meta);
    }

    fn packet(&mut self, p: &[u8; PACKET], audio: &mut dyn FnMut(Es, &[u8]), meta: &mut dyn FnMut(&[u8])) -> Result<(), TsError> {
        if p[1] & 0x80 != 0 {
            // The transport error indicator: the packet is damaged.
            return Ok(());
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        if pid == 0x1FFF {
            return Ok(());
        }
        let mut start = 4;
        if afc & 2 != 0 {
            start = 5 + p[4] as usize;
        }
        if afc & 1 == 0 || start >= PACKET {
            return Ok(());
        }
        // Continuity, per PID, for the packets that carry a payload: one
        // repeat is allowed (and dropped), a jump is packets lost.
        if let Some(&last) = self.cc.get(&pid) {
            if cc == last {
                return Ok(());
            }
            if cc != (last + 1) & 0x0F {
                self.lost += u64::from((cc + 16 - last - 1) & 0x0F).max(1);
            }
        }
        self.cc.insert(pid, cc);
        let payload = &p[start..];
        if pid == 0 || Some(pid) == self.pmt_pid {
            return self.psi(pid, pusi, payload);
        }
        if let Some((apid, es)) = self.audio {
            if pid == apid {
                self.audio_payload(es, pusi, payload, audio);
                return Ok(());
            }
        }
        if Some(pid) == self.id3_pid {
            if pusi {
                self.end_meta(meta);
            }
            self.meta.extend_from_slice(payload);
        }
        Ok(())
    }

    fn audio_payload(&mut self, es: Es, pusi: bool, payload: &[u8], audio: &mut dyn FnMut(Es, &[u8])) {
        if pusi {
            self.head.clear();
            self.in_head = true;
        }
        if !self.in_head {
            audio(es, payload);
            return;
        }
        self.head.extend_from_slice(payload);
        match pes_header_len(&self.head) {
            Some(n) if self.head.len() >= n => {
                self.in_head = false;
                if self.head.len() > n {
                    let rest = self.head[n..].to_vec();
                    audio(es, &rest);
                }
                self.head.clear();
            }
            Some(_) => {}
            None if self.head.len() >= 9 => {
                // Not a PES start: nothing to take from it.
                self.in_head = false;
                self.head.clear();
            }
            None => {}
        }
    }

    fn end_meta(&mut self, meta: &mut dyn FnMut(&[u8])) {
        if self.meta.is_empty() {
            return;
        }
        let pes = std::mem::take(&mut self.meta);
        if let Some(n) = pes_header_len(&pes) {
            if pes.len() > n {
                meta(&pes[n..]);
            }
        }
    }

    /// A packet of a table: gathered until its section is whole, then read.
    fn psi(&mut self, pid: u16, pusi: bool, payload: &[u8]) -> Result<(), TsError> {
        if pusi {
            let ptr = payload[0] as usize;
            if 1 + ptr >= payload.len() {
                return Ok(());
            }
            self.section = Some(Section { pid, bytes: payload[1 + ptr..].to_vec() });
        } else {
            match self.section.as_mut() {
                Some(s) if s.pid == pid => s.bytes.extend_from_slice(payload),
                _ => return Ok(()),
            }
        }
        let whole = {
            let s = self.section.as_ref().expect("set above");
            if s.bytes.len() < 3 {
                return Ok(());
            }
            let len = 3 + (((usize::from(s.bytes[1]) & 0x0F) << 8) | usize::from(s.bytes[2]));
            if s.bytes.len() < len {
                return Ok(());
            }
            s.bytes[..len].to_vec()
        };
        self.section = None;
        match whole[0] {
            0x00 => self.pat(&whole),
            0x02 => self.pmt(&whole),
            _ => Ok(()),
        }
    }

    fn pat(&mut self, s: &[u8]) -> Result<(), TsError> {
        // table_id, length (2), stream id (2), version, section, last section;
        // then the programs (4 bytes each); the CRC last.
        if s.len() < 12 {
            return Ok(());
        }
        let mut i = 8;
        while i + 4 <= s.len() - 4 {
            let program = u16::from_be_bytes([s[i], s[i + 1]]);
            let pid = (u16::from(s[i + 2] & 0x1F) << 8) | u16::from(s[i + 3]);
            if program != 0 {
                if self.pmt_pid != Some(pid) {
                    self.pmt_pid = Some(pid);
                }
                return Ok(());
            }
            i += 4;
        }
        Ok(())
    }

    fn pmt(&mut self, s: &[u8]) -> Result<(), TsError> {
        if s.len() < 16 || self.audio.is_some() {
            return Ok(());
        }
        let info_len = ((usize::from(s[10]) & 0x0F) << 8) | usize::from(s[11]);
        let mut i = 12 + info_len;
        let end = s.len() - 4;
        let mut other: Option<u8> = None;
        let mut encrypted: Option<u8> = None;
        while i + 5 <= end {
            let t = s[i];
            let pid = (u16::from(s[i + 1] & 0x1F) << 8) | u16::from(s[i + 2]);
            let es_len = ((usize::from(s[i + 3]) & 0x0F) << 8) | usize::from(s[i + 4]);
            let desc = &s[(i + 5).min(end)..(i + 5 + es_len).min(end)];
            match t {
                0x0F if self.audio.is_none() => self.audio = Some((pid, Es::Adts)),
                0x03 | 0x04 if self.audio.is_none() => self.audio = Some((pid, Es::Mpeg)),
                0x15 if self.id3_pid.is_none() && has_id3_format(desc) => self.id3_pid = Some(pid),
                // SAMPLE-AES: ADTS, AC-3, E-AC-3 (and video) with their
                // encrypted stream types.
                0xCF | 0xC1 | 0xC2 | 0xDB => encrypted = encrypted.or(Some(t)),
                // Audio this player does not decode: LATM AAC, AC-3, E-AC-3,
                // DTS, and the private stream DVB carries AC-3 in.
                0x11 | 0x81 | 0x87 | 0x82 | 0x86 | 0x06 => other = other.or(Some(t)),
                _ => {}
            }
            i += 5 + es_len;
        }
        if self.audio.is_some() {
            return Ok(());
        }
        if let Some(t) = encrypted {
            return Err(TsError::Encrypted(format!("the HLS stream's audio is encrypted (SAMPLE-AES, stream type 0x{:02x})", t)));
        }
        Err(TsError::Unsupported(match other {
            Some(t) => format!("unsupported: HLS audio stream type 0x{:02x}", t),
            None => "unsupported: no audio in the HLS stream".to_string(),
        }))
    }
}

/// Whether a PMT entry's descriptors name ID3 (the metadata descriptor's
/// format identifier, or a registration descriptor).
fn has_id3_format(desc: &[u8]) -> bool {
    desc.windows(4).any(|w| w == b"ID3 ")
}

/// The length of a PES header, once its first bytes are in: the fixed six,
/// and for the streams that have it, the optional header.
fn pes_header_len(b: &[u8]) -> Option<usize> {
    if b.len() < 6 || b[0] != 0 || b[1] != 0 || b[2] != 1 {
        return None;
    }
    let sid = b[3];
    // Streams without the optional header: program stream map, padding,
    // private stream 2, ECM, EMM, directory, DSMCC, H.222.1 type E.
    if matches!(sid, 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xFF | 0xF2 | 0xF8) {
        return Some(6);
    }
    if b.len() < 9 {
        return Some(9);
    }
    Some(9 + b[8] as usize)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Transport stream packets for a test: PAT, PMT and PES split across
    /// packets the way a muxer does it, with adaptation fields.
    pub(crate) struct Muxer {
        cc: HashMap<u16, u8>,
        pub out: Vec<u8>,
    }

    impl Muxer {
        pub(crate) fn new() -> Muxer {
            Muxer { cc: HashMap::new(), out: Vec::new() }
        }

        /// One packet: `payload` (≤ 184 − adaptation) after an adaptation
        /// field of `af` bytes (0: none), stuffed to 188.
        fn packet(&mut self, pid: u16, pusi: bool, af: usize, payload: &[u8]) {
            let cc = {
                let c = self.cc.entry(pid).or_insert(15);
                *c = (*c + 1) & 0x0F;
                *c
            };
            let room = PACKET - 4;
            // Fill the rest with an adaptation field of stuffing.
            let af_len = if payload.len() + af < room { room - payload.len() } else { af };
            let mut p = vec![SYNC, (if pusi { 0x40 } else { 0 }) | (pid >> 8) as u8, pid as u8, 0];
            if af_len > 0 {
                p[3] = 0x30 | cc;
                p.push((af_len - 1) as u8);
                if af_len > 1 {
                    p.push(0);
                    p.extend(std::iter::repeat(0xFF).take(af_len - 2));
                }
            } else {
                p[3] = 0x10 | cc;
            }
            p.extend_from_slice(payload);
            assert_eq!(p.len(), PACKET, "packet length");
            self.out.extend_from_slice(&p);
        }

        /// A PSI section in one packet (pointer field 0), CRC left zero.
        pub(crate) fn section(&mut self, pid: u16, body: &[u8]) {
            let mut b = vec![0u8];
            b.extend_from_slice(body);
            b.extend_from_slice(&[0, 0, 0, 0]);
            self.packet(pid, true, 0, &b);
        }

        pub(crate) fn pat(&mut self, pmt: u16) {
            let len = 5 + 4 + 4;
            self.section(0, &[0x00, 0xB0, len as u8, 0, 1, 0xC1, 0, 0, 0, 1, 0xE0 | (pmt >> 8) as u8, pmt as u8]);
        }

        /// A PMT with `(stream type, pid, descriptors)` entries.
        pub(crate) fn pmt(&mut self, pid: u16, streams: &[(u8, u16, &[u8])]) {
            let mut body = vec![0x02, 0xB0, 0, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0];
            for (t, p, d) in streams {
                body.extend_from_slice(&[*t, 0xE0 | (p >> 8) as u8, *p as u8, 0xF0, d.len() as u8]);
                body.extend_from_slice(d);
            }
            let len = body.len() - 3 + 4;
            body[1] = 0xB0 | (len >> 8) as u8;
            body[2] = len as u8;
            self.section(pid, &body);
        }

        /// A PES of `data` on `pid`, split over packets; `af` adaptation
        /// bytes in the first packet (a big one pushes the PES header into
        /// the next packet).
        pub(crate) fn pes(&mut self, pid: u16, sid: u8, data: &[u8], af: usize) {
            let mut b = vec![0, 0, 1, sid, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
            let len = b.len() - 6 + data.len();
            if len < 0x10000 {
                b[4] = (len >> 8) as u8;
                b[5] = len as u8;
            }
            b.extend_from_slice(data);
            let mut first = true;
            let mut at = 0;
            while at < b.len() {
                let a = if first { af } else { 0 };
                let room = PACKET - 4 - a;
                let n = room.min(b.len() - at);
                self.packet(pid, first, a, &b[at..at + n]);
                at += n;
                first = false;
            }
        }
    }

    pub(crate) fn adts_bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn run(d: &mut TsDemux, data: &[u8], chunk: usize) -> (Vec<u8>, Vec<Vec<u8>>, Result<(), TsError>) {
        let (mut a, mut m) = (Vec::new(), Vec::new());
        for c in data.chunks(chunk) {
            if let Err(e) = d.push(c, &mut |_, b| a.extend_from_slice(b), &mut |b| m.push(b.to_vec())) {
                return (a, m, Err(e));
            }
        }
        d.flush(&mut |b| m.push(b.to_vec()));
        (a, m, Ok(()))
    }

    /// The audio comes out as it went in, whatever sizes the bytes arrive
    /// in: PES headers off, adaptation fields skipped, a header pushed into
    /// the next packet by a long adaptation field, a PES carried over from
    /// one segment into the next; the timed ID3 tag comes out whole.
    #[test]
    fn the_audio_comes_out_of_the_packets_as_it_went_in() {
        let mut m = Muxer::new();
        m.pat(0x1000);
        m.pmt(0x1000, &[(0x0F, 0x100, &[]), (0x15, 0x101, &[0x26, 0x0D, 0xFF, 0xFF, b'I', b'D', b'3', b' ', 0xFF, b'I', b'D', b'3', b' ', 0, 0])]);
        let a1 = adts_bytes(1000, 1);
        let a2 = adts_bytes(5000, 2);
        let a3 = adts_bytes(10, 3);
        m.pes(0x100, 0xC0, &a1, 0);
        m.pes(0x101, 0xBD, b"ID3\x04\x00\x00\x00\x00\x00\x00", 0);
        m.pes(0x100, 0xC0, &a2, 180);
        m.pes(0x100, 0xC0, &a3, 7);
        let want: Vec<u8> = [a1, a2, a3].concat();
        for chunk in [1usize, 7, 188, 189, 1000, 1 << 20] {
            let mut d = TsDemux::new();
            let (a, meta, r) = run(&mut d, &m.out, chunk);
            assert!(r.is_ok());
            assert_eq!(d.es(), Some(Es::Adts));
            assert!(a == want, "chunk {chunk}: {} bytes of {}", a.len(), want.len());
            assert_eq!(meta, vec![b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec()], "chunk {chunk}");
            assert_eq!((d.lost, d.resynced), (0, 0));
        }
    }

    /// A segment's packets cut anywhere and a later segment: one
    /// demultiplexer goes on (its tables known, the counters running on);
    /// a missing packet is counted; junk before a packet is skipped.
    #[test]
    fn segments_go_on_and_losses_are_counted() {
        let mut m = Muxer::new();
        m.pat(0x1000);
        m.pmt(0x1000, &[(0x03, 0x100, &[])]);
        m.pes(0x100, 0xC0, &adts_bytes(600, 5), 0);
        let one = m.out.clone();
        m.out.clear();
        // The next segment: no tables, one packet of the PES missing.
        m.pes(0x100, 0xC0, &adts_bytes(900, 6), 0);
        let mut two = m.out.clone();
        two.drain(188..376);
        let mut d = TsDemux::new();
        let mut a = Vec::new();
        d.push(&one, &mut |e, b| {
            assert_eq!(e, Es::Mpeg);
            a.extend_from_slice(b)
        }, &mut |_| {}).unwrap();
        assert_eq!(a, adts_bytes(600, 5));
        let mut junk = vec![1u8, 2, 3, 4];
        junk.extend_from_slice(&two);
        a.clear();
        d.segment_starts();
        d.push(&junk, &mut |_, b| a.extend_from_slice(b), &mut |_| {}).unwrap();
        assert_eq!(d.lost, 1, "one packet missing");
        assert_eq!(d.resynced, 4, "the junk before the packets");
        assert_eq!(a.len(), 900 - 184, "the rest of the PES");
    }

    /// Audio the player does not decode, encrypted audio, no audio: named.
    #[test]
    fn what_cannot_be_played_is_named() {
        for (t, want) in [
            (0x81u8, TsError::Unsupported("unsupported: HLS audio stream type 0x81".into())),
            (0x11, TsError::Unsupported("unsupported: HLS audio stream type 0x11".into())),
            (0xCF, TsError::Encrypted("the HLS stream's audio is encrypted (SAMPLE-AES, stream type 0xcf)".into())),
            (0x1B, TsError::Unsupported("unsupported: no audio in the HLS stream".into())),
        ] {
            let mut m = Muxer::new();
            m.pat(0x1000);
            m.pmt(0x1000, &[(t, 0x100, &[])]);
            let mut d = TsDemux::new();
            let (_, _, r) = run(&mut d, &m.out, 188);
            assert_eq!(r, Err(want), "stream type {t:#x}");
        }
    }
}
