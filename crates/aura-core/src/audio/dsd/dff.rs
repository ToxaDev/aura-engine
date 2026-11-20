//! DSDIFF / DFF (Philips) reader: big-endian IFF, most significant bit first,
//! channel bytes interleaved. DST-compressed files are not read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use symphonia::core::meta::StandardTag;

use super::dsf::{rate_of, read_id3v2, text_tag, too_many_channels, DsdInfo, DsdRead};

/// A chunk header: its id and the size of its data (padded to even bytes
/// in the file).
fn chunk(file: &mut File) -> Option<([u8; 4], u64)> {
    let mut h = [0u8; 12];
    file.read_exact(&mut h).ok()?;
    Some((h[..4].try_into().unwrap(), u64::from_be_bytes(h[4..].try_into().unwrap())))
}

/// The chunks inside `body`, a run of local chunks held in memory.
fn sub_chunks(body: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 12 <= body.len() {
        let id: [u8; 4] = body[at..at + 4].try_into().unwrap();
        let size = u64::from_be_bytes(body[at + 4..at + 12].try_into().unwrap()) as usize;
        let end = (at + 12).saturating_add(size).min(body.len());
        out.push((id, &body[at + 12..end]));
        at = end + (size & 1);
    }
    out
}

/// DIAR / DITI: a 32-bit count and that many bytes of text.
fn counted_text(b: &[u8]) -> Option<String> {
    let n = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let text = b.get(4..4 + n.min(b.len() - 4))?;
    let s = String::from_utf8_lossy(text).trim_end_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A DSDIFF file: one byte of every channel in turn.
pub struct DffReader {
    path: PathBuf,
    file: File,
    info: DsdInfo,
    data_start: u64,
    pos: u64,
    scratch: Vec<u8>,
}

impl DffReader {
    pub fn open(path: &Path) -> Result<DffReader, String> {
        let bad = |what: &str| format!("{}: not a valid DSDIFF file ({})", path.display(), what);
        let mut file = File::open(path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let (id, _) = chunk(&mut file).ok_or_else(|| bad("too short"))?;
        let mut form = [0u8; 4];
        file.read_exact(&mut form).map_err(|_| bad("too short"))?;
        if &id != b"FRM8" || &form != b"DSD " {
            return Err(bad("no FRM8 DSD form"));
        }
        let (mut hz, mut channels, mut compression) = (0u32, 0usize, *b"DSD ");
        let mut data: Option<(u64, u64)> = None;
        let mut dst = false;
        let mut id3 = Vec::new();
        let (mut artist, mut title) = (None, None);
        let mut at = 16u64;
        while at + 12 <= len {
            file.seek(SeekFrom::Start(at)).map_err(|_| bad("seek"))?;
            let Some((id, size)) = chunk(&mut file) else { break };
            let body_at = at + 12;
            let small = |file: &mut File| -> Option<Vec<u8>> {
                let mut b = vec![0u8; size.min(16 << 20) as usize];
                file.read_exact(&mut b).ok().map(|_| b)
            };
            match &id {
                b"PROP" => {
                    let b = small(&mut file).ok_or_else(|| bad("PROP"))?;
                    if b.get(..4) != Some(b"SND ") {
                        return Err(bad("PROP is not SND"));
                    }
                    for (sid, s) in sub_chunks(&b[4..]) {
                        match &sid {
                            b"FS  " if s.len() >= 4 => hz = u32::from_be_bytes(s[..4].try_into().unwrap()),
                            b"CHNL" if s.len() >= 2 => channels = u16::from_be_bytes(s[..2].try_into().unwrap()) as usize,
                            b"CMPR" if s.len() >= 4 => compression = s[..4].try_into().unwrap(),
                            _ => {}
                        }
                    }
                }
                b"DSD " => data = Some((body_at, size.min(len.saturating_sub(body_at)))),
                b"DST " => dst = true,
                b"DIIN" => {
                    if let Some(b) = small(&mut file) {
                        for (sid, s) in sub_chunks(&b) {
                            match &sid {
                                b"DIAR" => artist = counted_text(s),
                                b"DITI" => title = counted_text(s),
                                _ => {}
                            }
                        }
                    }
                }
                b"ID3 " | b"id3 " => id3 = small(&mut file).unwrap_or_default(),
                _ => {}
            }
            at = body_at.saturating_add(size).saturating_add(size & 1);
        }
        if dst || &compression == b"DST " {
            return Err(format!("{}: DST-compressed DFF is not supported", path.display()));
        }
        if &compression != b"DSD " {
            return Err(bad("unknown compression"));
        }
        if channels == 0 {
            return Err(bad("no channels"));
        }
        if channels > 2 {
            return Err(too_many_channels(path, channels));
        }
        let rate = rate_of(path, hz)?;
        let (data_start, data_len) = data.ok_or_else(|| bad("no DSD sound data"))?;
        let mut info = DsdInfo {
            rate,
            channels,
            samples: data_len / channels as u64 * 8,
            tags: Vec::new(),
            visuals: Vec::new(),
        };
        read_id3v2(id3, &mut info);
        // DSDIFF's own artist and title after the ID3 tag's.
        info.tags.extend(artist.map(|a| text_tag(StandardTag::Artist, &a)));
        info.tags.extend(title.map(|t| text_tag(StandardTag::TrackTitle, &t)));
        Ok(DffReader { path: path.to_path_buf(), file, info, data_start, pos: 0, scratch: Vec::new() })
    }
}

impl DsdRead for DffReader {
    fn info(&self) -> &DsdInfo {
        &self.info
    }

    fn read(&mut self, out: &mut [&mut [u8]]) -> Result<usize, String> {
        let want = out.first().map_or(0, |o| o.len()) as u64;
        let n = want.min(self.info.bytes().saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        let ch = self.info.channels;
        self.scratch.resize(n * ch, 0);
        self.file
            .seek(SeekFrom::Start(self.data_start + self.pos * ch as u64))
            .and_then(|_| self.file.read_exact(&mut self.scratch))
            .map_err(|e| format!("DSDIFF read failed: {}", e))?;
        for (c, o) in out.iter_mut().enumerate().take(ch) {
            for (i, d) in o[..n].iter_mut().enumerate() {
                *d = self.scratch[i * ch + c];
            }
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
        Ok(Box::new(DffReader { path: self.path.clone(), file, info, data_start: self.data_start, pos: 0, scratch: Vec::new() }))
    }
}

/// DSDIFF files for tests.
#[cfg(test)]
pub(crate) mod test_files {
    fn ck(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u64).to_be_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    /// A DSDIFF file: `channels` as MSB-first bytes (interleaved on
    /// writing), compression "DSD " or "DST ", DIIN artist/title and an ID3
    /// chunk when given.
    pub fn dff(channels: &[Vec<u8>], hz: u32, compression: &[u8; 4], diin: Option<(&str, &str)>, id3: Option<&[u8]>) -> Vec<u8> {
        let mut snd = b"SND ".to_vec();
        snd.extend(ck(b"FS  ", &hz.to_be_bytes()));
        let mut chnl = (channels.len() as u16).to_be_bytes().to_vec();
        for i in 0..channels.len() {
            chnl.extend_from_slice(if i == 0 { b"SLFT" } else { b"SRGT" });
        }
        snd.extend(ck(b"CHNL", &chnl));
        let mut cmpr = compression.to_vec();
        cmpr.extend_from_slice(b"\x0cDSD Material"); // pstring; the odd total is padded
        snd.extend(ck(b"CMPR", &cmpr));
        let mut body = b"DSD ".to_vec();
        body.extend(ck(b"FVER", &0x0105_0000u32.to_be_bytes()));
        body.extend(ck(b"PROP", &snd));
        let n = channels.first().map_or(0, |c| c.len());
        let mut data = Vec::with_capacity(n * channels.len());
        for i in 0..n {
            for c in channels {
                data.push(c[i]);
            }
        }
        body.extend(ck(if compression == b"DST " { b"DST " } else { b"DSD " }, &data));
        if let Some((artist, title)) = diin {
            let counted = |s: &str| {
                let mut b = (s.len() as u32).to_be_bytes().to_vec();
                b.extend_from_slice(s.as_bytes());
                b
            };
            let mut d = ck(b"DIAR", &counted(artist));
            d.extend(ck(b"DITI", &counted(title)));
            body.extend(ck(b"DIIN", &d));
        }
        if let Some(t) = id3 {
            body.extend(ck(b"ID3 ", t));
        }
        ck(b"FRM8", &body)
    }
}

#[cfg(test)]
mod tests {
    use super::super::dsf::test_files::{id3v2, noise, read_all, write};
    use super::super::dsf::{open, sniff, Kind};
    use super::super::DsdRate;
    use super::test_files::dff;
    use super::*;
    use crate::audio::tag_text::test_mp3::id3_text_frame;

    fn has(tags: &[(&'static str, StandardTag)], reader: &str, want: &str) -> bool {
        tags.iter().any(|(n, t)| {
            *n == reader
                && match t {
                    StandardTag::TrackTitle(v) | StandardTag::Artist(v) => v.as_str() == want,
                    _ => false,
                }
        })
    }

    #[test]
    fn stereo_interleaved_with_odd_length_and_seek() {
        let (l, r) = (noise(12_345, 1), noise(12_345, 2));
        let (_d, path) = write(&dff(&[l.clone(), r.clone()], 2_822_400, b"DSD ", None, None), "a.dff");
        assert_eq!(sniff(&path), Some(Kind::Dff));
        let mut rd = open(&path).expect("open");
        assert_eq!((rd.info().rate, rd.info().channels, rd.info().samples), (DsdRate::DSD64, 2, 12_345 * 8));
        let got = read_all(&mut *rd, 1000);
        assert_eq!((&got[0], &got[1]), (&l, &r));
        rd.seek(12_000).unwrap();
        assert_eq!(read_all(&mut *rd, 64)[1], r[12_000..]);
        // An odd byte, in pieces of an odd size, the left channel too.
        rd.seek(7_777).unwrap();
        let got = read_all(&mut *rd, 333);
        assert_eq!((&got[0][..], &got[1][..]), (&l[7_777..], &r[7_777..]));
    }

    #[test]
    fn mono_and_the_48k_family() {
        let m = noise(3000, 9);
        let (_d, path) = write(&dff(&[m.clone()], 6_144_000, b"DSD ", None, None), "m.dff");
        let mut rd = open(&path).expect("open");
        assert_eq!((rd.info().rate, rd.info().channels), (DsdRate::new(48_000, 128).unwrap(), 1));
        assert_eq!(read_all(&mut *rd, 4096)[0], m);
    }

    #[test]
    fn dst_is_refused_clearly() {
        let (_d, path) = write(&dff(&vec![vec![0; 64]; 2], 2_822_400, b"DST ", None, None), "dst.dff");
        let err = open(&path).err().expect("refused");
        assert!(err.ends_with("DST-compressed DFF is not supported"), "{err}");
    }

    #[test]
    fn more_than_two_channels_are_refused() {
        let (_d, path) = write(&dff(&vec![vec![0x69; 64]; 6], 2_822_400, b"DSD ", None, None), "6.dff");
        assert!(open(&path).err().expect("refused").contains("6 channels are not supported"));
    }

    #[test]
    fn tags_id3_first_then_diin() {
        let tag = id3v2(&[id3_text_frame(b"TIT2", b"Id3 title")]);
        let (_d, path) = write(&dff(&vec![vec![0x69; 64]; 2], 2_822_400, b"DSD ", Some(("Diin artist", "Diin title")), Some(&tag)), "t.dff");
        let rd = open(&path).expect("open");
        let tags = &rd.info().tags;
        assert!(has(tags, "id3v2", "Id3 title"), "{tags:?}");
        assert!(has(tags, "dsdiff", "Diin artist") && has(tags, "dsdiff", "Diin title"), "{tags:?}");
        let first_title = tags.iter().find(|(_, t)| matches!(t, StandardTag::TrackTitle(_))).unwrap();
        assert_eq!(first_title.0, "id3v2");
    }
}
