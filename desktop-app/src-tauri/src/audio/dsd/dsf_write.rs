//! DSF (Sony) writer: the read side's layout (`dsf.rs`) written out. Little-
//! endian chunks, 4096-byte blocks per channel, least significant bit first,
//! the last block group zero-padded, an ID3v2 tag at the end.
//!
//! The writer takes the engine's MSB-first bytes and streams them: it holds at
//! most one block per channel, so a DSD1024 track never sits in memory.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use super::dsf::BIT_REVERSE;
use super::DsdRate;

/// Bytes per channel in one block, as every DSF file in the wild has it.
pub const BLOCK: usize = 4096;

/// The "DSD " chunk (28), the "fmt " chunk (52) and the "data" header (12).
const HEADER: u64 = 28 + 52 + 12;

/// A running FNV-1a hash of each channel's MSB-first bytes: what the writer
/// was given, so a read-back can be checked against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelHashes(pub Vec<u64>);

pub fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

pub const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

pub struct DsfWriter {
    file: BufWriter<File>,
    rate: DsdRate,
    channels: usize,
    /// Bytes of the current block, per channel (MSB first).
    pending: Vec<Vec<u8>>,
    /// Bytes per channel given so far.
    bytes: u64,
    hashes: Vec<u64>,
}

impl DsfWriter {
    /// Creates `path` (mono or stereo) with the header's sizes left to
    /// `finish`.
    pub fn create(path: &Path, rate: DsdRate, channels: usize) -> Result<DsfWriter, String> {
        if !(1..=2).contains(&channels) {
            return Err(format!("DSF: {} channels (mono and stereo only)", channels));
        }
        let file = File::create(path).map_err(|e| format!("Cannot create {}: {}", path.display(), e))?;
        let mut w = DsfWriter {
            file: BufWriter::with_capacity(1 << 20, file),
            rate,
            channels,
            pending: vec![Vec::with_capacity(BLOCK); channels],
            bytes: 0,
            hashes: vec![FNV_START; channels],
        };
        let head = w.header(0, 0, 0);
        w.put(&head)?;
        Ok(w)
    }

    /// The three headers for `samples` one-bit samples per channel, `data`
    /// bytes of block groups and an ID3 tag of `id3` bytes after them.
    fn header(&self, samples: u64, data: u64, id3: u64) -> Vec<u8> {
        let data_chunk = 12 + data;
        let total = HEADER + data + id3;
        let meta = if id3 > 0 { HEADER + data } else { 0 };
        let mut h = Vec::with_capacity(HEADER as usize);
        h.extend_from_slice(b"DSD ");
        h.extend_from_slice(&28u64.to_le_bytes());
        h.extend_from_slice(&total.to_le_bytes());
        h.extend_from_slice(&meta.to_le_bytes());
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&52u64.to_le_bytes());
        // Version 1, format 0 (DSD raw), channel type (1 mono, 2 stereo),
        // channels, rate, bits per sample (1: LSB first).
        let ch = self.channels as u32;
        for v in [1u32, 0, ch, ch, self.rate.bit_rate(), 1] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&samples.to_le_bytes());
        h.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_chunk.to_le_bytes());
        h
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.file.write_all(bytes).map_err(|e| format!("DSF write failed: {}", e))
    }

    /// Appends the next bytes of every channel: one slice per channel, all
    /// the same length, MSB first.
    pub fn write(&mut self, chans: &[&[u8]]) -> Result<(), String> {
        assert_eq!(chans.len(), self.channels);
        let n = chans[0].len();
        assert!(chans.iter().all(|c| c.len() == n), "DSF: channels of unequal length");
        for (h, c) in self.hashes.iter_mut().zip(chans) {
            *h = fnv1a(*h, c);
        }
        self.bytes += n as u64;
        let mut at = 0;
        while at < n {
            let room = BLOCK - self.pending[0].len();
            let run = room.min(n - at);
            for (p, c) in self.pending.iter_mut().zip(chans) {
                p.extend_from_slice(&c[at..at + run]);
            }
            at += run;
            if self.pending[0].len() == BLOCK {
                self.flush_group()?;
            }
        }
        Ok(())
    }

    /// Writes the held blocks as one group, LSB first, zero-padded.
    fn flush_group(&mut self) -> Result<(), String> {
        let mut out = [0u8; BLOCK];
        for ch in 0..self.channels {
            out.fill(0);
            for (o, &b) in out.iter_mut().zip(&self.pending[ch]) {
                *o = BIT_REVERSE[b as usize];
            }
            self.pending[ch].clear();
            self.file.write_all(&out).map_err(|e| format!("DSF write failed: {}", e))?;
        }
        Ok(())
    }

    /// Bytes per channel written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Pads the last group, appends `id3` (an ID3v2 tag, or nothing) and
    /// fills in the header for `samples` one-bit samples per channel (at
    /// most eight per byte given). Returns the hash of each channel's bytes.
    pub fn finish(mut self, samples: u64, id3: &[u8]) -> Result<ChannelHashes, String> {
        assert!(samples <= self.bytes * 8 && samples + 8 > self.bytes * 8, "DSF: {} samples in {} bytes", samples, self.bytes);
        if !self.pending[0].is_empty() {
            self.flush_group()?;
        }
        let data = self.bytes.div_ceil(BLOCK as u64) * (BLOCK * self.channels) as u64;
        if !id3.is_empty() {
            self.put(id3)?;
        }
        let head = self.header(samples, data, id3.len() as u64);
        let mut file = self.file.into_inner().map_err(|e| format!("DSF write failed: {}", e.error()))?;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&head))
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("DSF write failed: {}", e))?;
        Ok(ChannelHashes(self.hashes))
    }
}

/// An ID3v2.3 tag of text frames: `TPE1`/`TIT2` and the like as
/// (frame id, text), and `TXXX` user frames as (description, value) — the
/// key/value list the FLAC path writes as VORBIS_COMMENT. UTF-16 with a BOM,
/// so any text survives in every reader. Empty when there is nothing to say.
pub fn id3v2_tag(text: &[(&str, &str)], user: &[(String, String)]) -> Vec<u8> {
    fn utf16(s: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(&[0xFF, 0xFE]);
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
    }
    fn frame(id: &[u8], body: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(body);
    }
    let mut frames = Vec::new();
    for (id, value) in text.iter().filter(|(_, v)| !v.is_empty()) {
        let mut body = vec![1u8];
        utf16(value, &mut body);
        frame(id.as_bytes(), &body, &mut frames);
    }
    for (desc, value) in user {
        let mut body = vec![1u8];
        utf16(desc, &mut body);
        body.extend_from_slice(&[0, 0]);
        utf16(value, &mut body);
        frame(b"TXXX", &body, &mut frames);
    }
    if frames.is_empty() {
        return Vec::new();
    }
    let n = frames.len() as u32;
    let mut out = b"ID3\x03\x00\x00".to_vec();
    out.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
    out.extend_from_slice(&frames);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsd::dsf::{test_files, DsdRead, DsfReader};

    fn write_file(chans: &[Vec<u8>], rate: DsdRate, samples: u64, id3: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, ChannelHashes) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.dsf");
        let mut w = DsfWriter::create(&path, rate, chans.len()).unwrap();
        // In uneven pieces, so block boundaries fall inside a write.
        let n = chans[0].len();
        let mut at = 0;
        let mut step = 1;
        while at < n {
            let e = (at + step).min(n);
            let parts: Vec<&[u8]> = chans.iter().map(|c| &c[at..e]).collect();
            w.write(&parts).unwrap();
            at = e;
            step = step * 3 + 7;
        }
        let h = w.finish(samples, id3).unwrap();
        (dir, path, h)
    }

    #[test]
    fn tiny_file_has_the_hand_computed_header() {
        // Mono DSD64, three bytes: 0x01 0x80 0xF0, 20 samples.
        let (_d, path, _) = write_file(&[vec![0x01, 0x80, 0xF0]], DsdRate::DSD64, 20, &[]);
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 92 + 4096);
        let mut want = Vec::new();
        want.extend_from_slice(b"DSD \x1c\0\0\0\0\0\0\0");
        want.extend_from_slice(&[0x5c, 0x10, 0, 0, 0, 0, 0, 0]); // 4188 bytes in all
        want.extend_from_slice(&[0; 8]); // no tag
        want.extend_from_slice(b"fmt \x34\0\0\0\0\0\0\0");
        want.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]); // v1, raw, mono, 1 ch
        want.extend_from_slice(&[0x00, 0x11, 0x2b, 0x00]); // 2 822 400 Hz
        want.extend_from_slice(&[1, 0, 0, 0]); // 1 bit: LSB first
        want.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0]);
        want.extend_from_slice(&[0x00, 0x10, 0, 0, 0, 0, 0, 0]); // block 4096, reserved
        want.extend_from_slice(b"data");
        want.extend_from_slice(&[0x0c, 0x10, 0, 0, 0, 0, 0, 0]); // 12 + 4096
        assert_eq!(&b[..92], &want[..]);
        assert_eq!(&b[92..95], &[0x80, 0x01, 0x0F]); // bit order flipped
        assert!(b[95..].iter().all(|&x| x == 0));
    }

    #[test]
    fn matches_the_reference_layout_and_reads_back() {
        for (rate, chans, samples) in [
            (DsdRate::DSD64, 2usize, 8 * 10_000u64),
            (DsdRate::new(48_000, 128).unwrap(), 1, 8 * 4096),
            (DsdRate::new(48_000, 1024).unwrap(), 2, 8 * 9000 - 3),
            (DsdRate::new(44_100, 512).unwrap(), 2, 5),
        ] {
            let n = samples.div_ceil(8) as usize;
            let data: Vec<Vec<u8>> = (0..chans).map(|c| test_files::noise(n, 7 + c as u64)).collect();
            let (_d, path, h) = write_file(&data, rate, samples, &[]);
            let b = std::fs::read(&path).unwrap();
            assert_eq!(b, test_files::dsf(&data, rate.bit_rate(), samples, BLOCK, false, None), "{:?}", rate);
            let mut r = DsfReader::open(&path).unwrap();
            assert_eq!(r.info().rate, rate);
            assert_eq!(r.info().channels, chans);
            assert_eq!(r.info().samples, samples);
            let back = test_files::read_all(&mut r, 3000);
            assert_eq!(back, data);
            let want: Vec<u64> = data.iter().map(|c| fnv1a(FNV_START, c)).collect();
            assert_eq!(h.0, want);
        }
    }

    #[test]
    fn tags_go_at_the_end_and_read_back() {
        use symphonia::core::meta::StandardTag;
        let tag = id3v2_tag(
            &[("TPE1", "Artíst"), ("TIT2", "Тест"), ("TALB", "")],
            &[("AURA_CHAIN".to_string(), "DC,ISP".to_string())],
        );
        let data = vec![vec![0x69u8; 5000], vec![0x96; 5000]];
        let (_d, path, _) = write_file(&data, DsdRate::DSD128, 40_000, &tag);
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b, test_files::dsf(&data, DsdRate::DSD128.bit_rate(), 40_000, BLOCK, false, Some(&tag)));
        let r = DsfReader::open(&path).unwrap();
        let tags = &r.info().tags;
        assert!(tags.iter().any(|(_, t)| matches!(t, StandardTag::Artist(a) if a.as_str() == "Artíst")), "{:?}", tags);
        assert!(tags.iter().any(|(_, t)| matches!(t, StandardTag::TrackTitle(a) if a.as_str() == "Тест")));
        // The user frame is there byte for byte.
        let txxx = b.windows(4).position(|w| w == b"TXXX").expect("TXXX frame");
        assert!(txxx > 92 + 2 * 8192);
        assert!(id3v2_tag(&[("TPE1", "")], &[]).is_empty());
    }

    #[test]
    fn refuses_more_than_two_channels() {
        let dir = tempfile::tempdir().unwrap();
        assert!(DsfWriter::create(&dir.path().join("x.dsf"), DsdRate::DSD64, 3).is_err());
    }
}
