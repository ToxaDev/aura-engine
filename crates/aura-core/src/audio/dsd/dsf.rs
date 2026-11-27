//! DSF (Sony) reader: little-endian chunks, 4096-byte blocks per channel,
//! least significant bit first (flipped to the engine's MSB-first order).
//!
//! Also the pieces both DSD file readers share (`DsdInfo`, `DsdRead`) and
//! `open`, which tells a DSF file from a DSDIFF one by its first four bytes.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::meta::{StandardTag, Visual};

use super::DsdRate;

/// What a DSD file holds, read from its header.
#[derive(Clone, Debug)]
pub struct DsdInfo {
    pub rate: DsdRate,
    /// 1 or 2: wider files are refused when opened.
    pub channels: usize,
    /// One-bit samples per channel. The last byte of a channel may hold
    /// fewer than eight of them; the rest of its bits are not music.
    pub samples: u64,
    /// Text tags with the name of the reader that found them ("id3v2" for an
    /// ID3v2 tag, "dsdiff" for DSDIFF's own artist and title), in the order
    /// the file's tags should be preferred.
    pub tags: Vec<(&'static str, StandardTag)>,
    /// Pictures from the file's ID3v2 tag.
    pub visuals: Vec<Visual>,
}

impl DsdInfo {
    /// Bytes per channel: eight samples a byte, the last one maybe partial.
    pub fn bytes(&self) -> u64 {
        self.samples.div_ceil(8)
    }

    pub fn duration_s(&self) -> f64 {
        self.samples as f64 / self.rate.bit_rate() as f64
    }

    /// "DSD64", "DSD128"… — 48 kHz-family rates by their multiple as well.
    pub fn label(&self) -> String {
        format!("DSD{}", self.rate.multiple)
    }
}

/// A DSD file read as planar, MSB-first bytes.
pub trait DsdRead: Send {
    fn info(&self) -> &DsdInfo;

    /// Fills `out` — one slice per channel, all the same length — with the
    /// bytes from the current position on; returns how many bytes each
    /// channel got (0 at the end).
    fn read(&mut self, out: &mut [&mut [u8]]) -> Result<usize, String>;

    /// Moves to byte `byte` of every channel (sample `8 · byte`).
    fn seek(&mut self, byte: u64) -> Result<(), String>;

    /// The same file opened again at its start, with a handle of its own
    /// (no tags or pictures read again): a player's chain streams its data
    /// while another reads elsewhere in it.
    fn reopen(&self) -> Result<Box<dyn DsdRead>, String>;
}

/// A DSF or DSDIFF file, by its magic ("DSD " / "FRM8"); None for any other.
pub fn sniff(path: &Path) -> Option<Kind> {
    let mut magic = [0u8; 4];
    File::open(path).ok()?.read_exact(&mut magic).ok()?;
    match &magic {
        b"DSD " => Some(Kind::Dsf),
        b"FRM8" => Some(Kind::Dff),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dsf,
    Dff,
}

/// Opens a DSF or DSDIFF file by its magic.
pub fn open(path: &Path) -> Result<Box<dyn DsdRead>, String> {
    match sniff(path) {
        Some(Kind::Dsf) => Ok(Box::new(DsfReader::open(path)?)),
        Some(Kind::Dff) => Ok(Box::new(super::dff::DffReader::open(path)?)),
        None => Err(format!("{}: not a DSF or DSDIFF file", path.display())),
    }
}

/// Reverses the bit order of every byte value (DSF's LSB-first bytes into
/// the engine's MSB-first ones).
pub const BIT_REVERSE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = (i as u8).reverse_bits();
        i += 1;
    }
    t
};

/// The refusal for a file wider than the player's two channels.
pub(super) fn too_many_channels(path: &Path, channels: usize) -> String {
    format!(
        "{}: DSD files with {} channels are not supported (mono and stereo only)",
        path.display(),
        channels
    )
}

/// The rate of a DSD file whose header says `hz`, or why it cannot be played.
pub(super) fn rate_of(path: &Path, hz: u32) -> Result<DsdRate, String> {
    [44_100, 48_000]
        .into_iter()
        .find_map(|base| (hz % base == 0).then(|| DsdRate::new(base, hz / base)).flatten())
        .ok_or_else(|| format!("{}: unsupported DSD rate {} Hz", path.display(), hz))
}

/// The text tags and pictures of an ID3v2 tag held in `bytes`, read by
/// symphonia's reader, so a DSD file's tags read as any other file's
/// (code pages included, `audio/tag_text.rs`). A damaged tag reads as none.
pub(super) fn read_id3v2(bytes: Vec<u8>, info: &mut DsdInfo) {
    use symphonia::core::formats::probe::ProbeableMetadata;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return;
    }
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let Ok(mut reader) = symphonia::default::meta::Id3v2Reader::try_probe_new(mss, MetadataOptions::default()) else {
        return;
    };
    let Ok(buffer) = reader.read_all() else { return };
    let name = buffer.revision.info.short_name;
    let media = buffer.revision.media;
    info.tags.extend(media.tags.into_iter().filter_map(|t| t.std.map(|s| (name, s))));
    info.visuals.extend(media.visuals);
}

/// A text tag of DSDIFF's own (DIAR, DITI).
pub(super) fn text_tag(make: fn(Arc<String>) -> StandardTag, text: &str) -> (&'static str, StandardTag) {
    ("dsdiff", make(Arc::new(text.to_string())))
}

fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// A DSF file. Its data is a run of block groups: `block` bytes of channel
/// 0, then `block` bytes of channel 1…, the last group zero-padded.
pub struct DsfReader {
    path: PathBuf,
    file: File,
    info: DsdInfo,
    data_start: u64,
    block: u64,
    lsb_first: bool,
    /// Bytes per channel read so far.
    pos: u64,
}

impl DsfReader {
    pub fn open(path: &Path) -> Result<DsfReader, String> {
        let bad = |what: &str| format!("{}: not a valid DSF file ({})", path.display(), what);
        let mut file = File::open(path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        // "DSD " (28 bytes), "fmt " (52), "data" header (12).
        let mut head = [0u8; 92];
        file.read_exact(&mut head).map_err(|_| bad("too short"))?;
        if &head[..4] != b"DSD " || &head[28..32] != b"fmt " {
            return Err(bad("no DSD and fmt chunks"));
        }
        let metadata_at = u64_le(&head, 20);
        let fmt_size = u64_le(&head, 32);
        let f = &head[40..];
        let (version, format_id) = (u32_le(f, 0), u32_le(f, 4));
        let channels = u32_le(f, 12) as usize;
        let hz = u32_le(f, 16);
        let bits = u32_le(f, 20);
        let samples = u64_le(f, 24);
        let block = u32_le(f, 32) as u64;
        if version != 1 || format_id != 0 {
            return Err(format!("{}: unsupported DSF format (version {}, format {})", path.display(), version, format_id));
        }
        let lsb_first = match bits {
            1 => true,
            8 => false,
            _ => return Err(bad("bits per sample")),
        };
        if channels == 0 || block == 0 {
            return Err(bad("no channels"));
        }
        if channels > 2 {
            return Err(too_many_channels(path, channels));
        }
        let rate = rate_of(path, hz)?;
        // The data chunk follows the fmt chunk, whatever size that declares.
        let data_hdr = 28 + fmt_size;
        let mut dh = [0u8; 12];
        file.seek(SeekFrom::Start(data_hdr)).and_then(|_| file.read_exact(&mut dh)).map_err(|_| bad("no data chunk"))?;
        if &dh[..4] != b"data" {
            return Err(bad("no data chunk"));
        }
        let data_start = data_hdr + 12;
        let data_len = u64_le(&dh, 4).saturating_sub(12);
        let mut info = DsdInfo { rate, channels, samples, tags: Vec::new(), visuals: Vec::new() };
        let group = block * channels as u64;
        let needed = info.bytes().div_ceil(block) * group;
        let avail = len.saturating_sub(data_start).min(data_len);
        if avail < needed {
            // A cut file: play its whole block groups.
            let have = (avail / group * block).min(info.bytes());
            crate::aelog!(
                "[DSD] {}: the data is cut short, {} of {} bytes per channel read",
                path.display(),
                have,
                info.bytes()
            );
            info.samples = info.samples.min(have * 8);
        }
        if metadata_at > 0 && metadata_at < len {
            let mut tag = Vec::new();
            if file.seek(SeekFrom::Start(metadata_at)).is_ok() && (&mut file).take(16 << 20).read_to_end(&mut tag).is_ok() {
                read_id3v2(tag, &mut info);
            }
        }
        Ok(DsfReader { path: path.to_path_buf(), file, info, data_start, block, lsb_first, pos: 0 })
    }
}

impl DsdRead for DsfReader {
    fn info(&self) -> &DsdInfo {
        &self.info
    }

    fn read(&mut self, out: &mut [&mut [u8]]) -> Result<usize, String> {
        let want = out.first().map_or(0, |o| o.len()) as u64;
        let n = want.min(self.info.bytes().saturating_sub(self.pos)) as usize;
        let channels = self.info.channels as u64;
        let mut done = 0usize;
        while done < n {
            let at = self.pos + done as u64;
            let (group, offset) = (at / self.block, at % self.block);
            let run = ((self.block - offset) as usize).min(n - done);
            for (ch, o) in out.iter_mut().enumerate().take(self.info.channels) {
                let from = self.data_start + (group * channels + ch as u64) * self.block + offset;
                let dst = &mut o[done..done + run];
                self.file
                    .seek(SeekFrom::Start(from))
                    .and_then(|_| self.file.read_exact(dst))
                    .map_err(|e| format!("DSF read failed: {}", e))?;
                if self.lsb_first {
                    for b in dst.iter_mut() {
                        *b = BIT_REVERSE[*b as usize];
                    }
                }
            }
            done += run;
        }
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, byte: u64) -> Result<(), String> {
        self.pos = byte.min(self.info.bytes());
        Ok(())
    }

    fn reopen(&self) -> Result<Box<dyn DsdRead>, String> {
        let file = File::open(&self.path).map_err(|e| format!("Cannot open {}: {}", self.path.display(), e))?;
        let info = DsdInfo { tags: Vec::new(), visuals: Vec::new(), ..self.info.clone() };
        Ok(Box::new(DsfReader { path: self.path.clone(), file, info, pos: 0, ..*self }))
    }
}

/// DSD files for tests, written as the formats lay them out.
#[cfg(test)]
pub(crate) mod test_files {
    /// A DSF file: `channels` given as MSB-first bytes (written LSB first
    /// unless `msb_first`), `samples` per channel, `block` bytes a block,
    /// an ID3v2 tag at the end when given.
    pub fn dsf(channels: &[Vec<u8>], hz: u32, samples: u64, block: usize, msb_first: bool, id3: Option<&[u8]>) -> Vec<u8> {
        let bytes = samples.div_ceil(8) as usize;
        let groups = bytes.div_ceil(block);
        let mut data = Vec::new();
        for g in 0..groups {
            for ch in channels {
                for i in 0..block {
                    let b = ch.get(g * block + i).copied().filter(|_| g * block + i < bytes).unwrap_or(0);
                    data.push(if msb_first { b } else { b.reverse_bits() });
                }
            }
        }
        let data_len = 12 + data.len() as u64;
        let total = 28 + 52 + data_len + id3.map_or(0, |t| t.len() as u64);
        let mut out = b"DSD ".to_vec();
        out.extend_from_slice(&28u64.to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        let meta = if id3.is_some() { 28 + 52 + data_len } else { 0 };
        out.extend_from_slice(&meta.to_le_bytes());
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&52u64.to_le_bytes());
        for v in [1u32, 0, if channels.len() == 1 { 1 } else { 2 }, channels.len() as u32, hz, if msb_first { 8 } else { 1 }] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&samples.to_le_bytes());
        out.extend_from_slice(&(block as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data);
        if let Some(t) = id3 {
            out.extend_from_slice(t);
        }
        out
    }

    /// An ID3v2.3 tag of the given frames (`tag_text::test_mp3::id3_text_frame`).
    pub fn id3v2(frames: &[Vec<u8>]) -> Vec<u8> {
        let body = frames.concat();
        let n = body.len() as u32;
        let mut out = b"ID3\x03\x00\x00".to_vec();
        out.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
        out.extend_from_slice(&body);
        out
    }

    /// Pseudo-random bytes, a different run for every `seed`.
    pub fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    pub fn write(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        (dir, path)
    }

    /// Every byte of every channel, read in pieces of `chunk`.
    pub fn read_all(r: &mut dyn super::DsdRead, chunk: usize) -> Vec<Vec<u8>> {
        let ch = r.info().channels;
        let mut out = vec![Vec::new(); ch];
        let mut bufs = vec![vec![0u8; chunk]; ch];
        loop {
            let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
            let n = r.read(&mut slices).expect("read");
            if n == 0 {
                break;
            }
            for (o, b) in out.iter_mut().zip(&bufs) {
                o.extend_from_slice(&b[..n]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_files::*;
    use super::*;
    use crate::audio::tag_text::test_mp3::id3_text_frame;

    #[test]
    fn bit_reverse_table() {
        assert_eq!(BIT_REVERSE[0x01], 0x80);
        assert_eq!(BIT_REVERSE[0x69], 0x96);
        assert_eq!(BIT_REVERSE[0xF0], 0x0F);
        assert!((0..=255u8).all(|b| BIT_REVERSE[BIT_REVERSE[b as usize] as usize] == b));
    }

    #[test]
    fn stereo_blocks_flip_and_partial_last_block() {
        // 2.5 blocks and an odd sample count: the last byte holds 3 samples.
        let block = 4096;
        let samples = (2 * block as u64 + 2048) * 8 - 5;
        let bytes = samples.div_ceil(8) as usize;
        let (l, r) = (noise(bytes, 1), noise(bytes, 2));
        let (_d, path) = write(&dsf(&[l.clone(), r.clone()], 2_822_400, samples, block, false, None), "a.dsf");
        assert_eq!(sniff(&path), Some(Kind::Dsf));
        let mut rd = open(&path).expect("open");
        let info = rd.info().clone();
        assert_eq!((info.rate, info.channels, info.samples), (DsdRate::DSD64, 2, samples));
        assert_eq!(info.label(), "DSD64");
        for chunk in [1000, 4096, 10_000] {
            rd.seek(0).unwrap();
            let got = read_all(&mut *rd, chunk);
            assert_eq!(got[0], l, "left, chunk {chunk}");
            assert_eq!(got[1], r, "right, chunk {chunk}");
        }
    }

    #[test]
    fn msb_first_dsf_and_seek() {
        let samples = 3 * 4096 * 8;
        let m = noise(3 * 4096, 7);
        let (_d, path) = write(&dsf(&[m.clone()], 5_644_800, samples, 4096, true, None), "m.dsf");
        let mut rd = open(&path).expect("open");
        assert_eq!((rd.info().rate, rd.info().channels), (DsdRate::DSD128, 1));
        rd.seek(5000).unwrap();
        assert_eq!(read_all(&mut *rd, 777)[0], m[5000..]);
    }

    #[test]
    fn rates_of_both_families() {
        let p = Path::new("x.dsf");
        assert_eq!(rate_of(p, 2_822_400), Ok(DsdRate::DSD64));
        assert_eq!(rate_of(p, 3_072_000), Ok(DsdRate::new(48_000, 64).unwrap()));
        assert_eq!(rate_of(p, 22_579_200), Ok(DsdRate::new(44_100, 512).unwrap()));
        assert!(rate_of(p, 1_411_200).is_err());
        assert!(rate_of(p, 2_822_401).is_err());
    }

    #[test]
    fn three_channels_are_refused() {
        let c = vec![0x69u8; 4096];
        let (_d, path) = write(&dsf(&[c.clone(), c.clone(), c], 2_822_400, 4096 * 8, 4096, false, None), "3.dsf");
        let err = open(&path).err().expect("refused");
        assert!(err.contains("3 channels are not supported"), "{err}");
    }

    #[test]
    fn id3v2_tag_at_the_end() {
        let tag = id3v2(&[id3_text_frame(b"TIT2", b"Song"), id3_text_frame(b"TPE1", b"Band"), id3_text_frame(b"TALB", b"Album")]);
        let (_d, path) = write(&dsf(&vec![vec![0x69; 100]; 2], 2_822_400, 800, 4096, false, Some(&tag)), "t.dsf");
        let rd = open(&path).expect("open");
        let tags = &rd.info().tags;
        assert!(tags.iter().any(|(n, t)| *n == "id3v2" && matches!(t, StandardTag::TrackTitle(v) if v.as_str() == "Song")), "{tags:?}");
        assert!(tags.iter().any(|(_, t)| matches!(t, StandardTag::Artist(v) if v.as_str() == "Band")));
        assert!(tags.iter().any(|(_, t)| matches!(t, StandardTag::Album(v) if v.as_str() == "Album")));
    }

    #[test]
    fn a_cut_file_reads_what_is_there() {
        let samples = 4 * 4096 * 8;
        let full = dsf(&[noise(4 * 4096, 3), noise(4 * 4096, 4)], 2_822_400, samples, 4096, false, None);
        let (_d, path) = write(&full[..full.len() - 4096 - 100], "cut.dsf");
        let mut rd = open(&path).expect("open");
        assert_eq!(rd.info().samples, 3 * 4096 * 8);
        assert_eq!(read_all(&mut *rd, 4096)[0].len(), 3 * 4096);
    }

    #[test]
    fn other_files_are_not_dsd() {
        let (_d, path) = write(b"RIFF....WAVE", "x.wav");
        assert_eq!(sniff(&path), None);
        assert!(open(&path).is_err());
    }
}
