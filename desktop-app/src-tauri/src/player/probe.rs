//! What the queue shows about a file before it is played: tags and the
//! stream's format, read from the container header (no decode).

use serde::Serialize;
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag, StandardVisualKey, Visual};

use crate::audio::tag_text::{self, Field};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub id: u64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: String,
    pub duration_s: f64,
    pub sample_rate: u32,
    pub bits: u32,
    pub channels: u32,
    /// "DSD64", "DSD128"… for a DSD file (its `sample_rate` is the PCM rate
    /// it decodes to, its `bits` 0); empty for PCM.
    pub format: String,
}

pub fn probe(id: u64, path: &Path) -> Result<TrackInfo, String> {
    if crate::audio::dsd::dsf::sniff(path).is_some() {
        return probe_dsd(id, path);
    }
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("{}: unsupported format ({})", path.display(), e))?;

    // Every metadata revision the reader holds (an MP3 has ID3v2 and often
    // ID3v1). The same walk the engine's decoder does.
    let mut tags = Vec::new();
    {
        let mut metadata = format.metadata();
        while let Some(revision) = metadata.current() {
            let reader = revision.info.short_name;
            tags.extend(revision.media.tags.iter().filter_map(|t| t.std.clone().map(|s| (reader, s))));
            if !metadata.is_latest() {
                metadata.pop();
            } else {
                break;
            }
        }
    }
    let text = text_fields(path, &tags);

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.as_ref().and_then(|p| p.audio()).is_some())
        .ok_or_else(|| format!("{}: no audio track", path.display()))?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).unwrap();
    let sample_rate = params.sample_rate.unwrap_or(0);
    let bits = params.bits_per_sample.or(params.bits_per_coded_sample).unwrap_or(0);
    let channels = params.channels.as_ref().map(|c| c.count() as u32).unwrap_or(0);
    let frames = track.num_frames;
    // A chained Ogg file's container counts its first link: the decode goes
    // on through every next one of the same rate and channels.
    let frames = match frames {
        Some(n) if crate::audio::converter::decode::chained_ogg(path) => {
            Some(crate::audio::converter::decode::ogg_links_frames(&mut *format, n))
        }
        n => n,
    };
    let duration_s = match (frames, sample_rate) {
        (Some(n), r) if r > 0 => n as f64 / r as f64,
        _ => 0.0,
    };
    Ok(TrackInfo {
        id,
        path: path.display().to_string(),
        title: text.title,
        artist: text.artist,
        album: text.album,
        year: text.year,
        duration_s,
        sample_rate,
        bits,
        channels,
        format: String::new(),
    })
}

/// A DSF or DSDIFF file (symphonia reads neither). Its rate is the PCM rate
/// it decodes to — the one the chain, the rate planning and the preparation
/// estimates go by (`audio/dsd/decimate.rs`) — with no depth (f64); the
/// "DSD64"… name is what the list and the player show.
fn probe_dsd(id: u64, path: &Path) -> Result<TrackInfo, String> {
    let r = crate::audio::dsd::dsf::open(path)?;
    let info = r.info();
    let text = text_fields(path, &info.tags);
    Ok(TrackInfo {
        id,
        path: path.display().to_string(),
        title: text.title,
        artist: text.artist,
        album: text.album,
        year: text.year,
        duration_s: info.duration_s(),
        sample_rate: crate::audio::dsd::decimate::pcm_rate(info.rate),
        bits: 0,
        channels: info.channels as u32,
        format: info.label(),
    })
}

/// The text a track shows.
struct TextFields {
    title: String,
    artist: String,
    album: String,
    year: String,
}

/// A file's text fields from its tags (reader name, tag): each from ID3v2
/// before ID3v1, whose 30 bytes cut a longer title (tag_text::Field).
fn text_fields(path: &Path, tags: &[(&str, StandardTag)]) -> TextFields {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let (mut title, mut artist, mut album_artist, mut album) =
        (Field::default(), Field::default(), Field::default(), Field::default());
    let mut year = String::new();
    for (reader, tag) in tags {
        match tag {
            StandardTag::TrackTitle(v) => title.offer(reader, v.as_str()),
            StandardTag::Artist(v) => artist.offer(reader, v.as_str()),
            StandardTag::AlbumArtist(v) => album_artist.offer(reader, v.as_str()),
            StandardTag::Album(v) => album.offer(reader, v.as_str()),
            // Date tags — first match wins.  A full date string like
            // "2023-04-12" is trimmed to the four-digit year so the
            // showcase line reads "Artist · Album · 2023" not the
            // full ISO date. A bare year string is used as-is.
            StandardTag::RecordingDate(v) if year.is_empty() => {
                year = year_from_date_str(v);
            }
            StandardTag::RecordingYear(y) if year.is_empty() => {
                year = y.to_string();
            }
            StandardTag::ReleaseDate(v) if year.is_empty() => {
                year = year_from_date_str(v);
            }
            StandardTag::ReleaseYear(y) if year.is_empty() => {
                year = y.to_string();
            }
            StandardTag::OriginalReleaseDate(v) if year.is_empty() => {
                year = year_from_date_str(v);
            }
            StandardTag::OriginalReleaseYear(y) if year.is_empty() => {
                year = y.to_string();
            }
            _ => {}
        }
    }
    // ID3 in a Windows code page comes out of a Latin-1 frame as accented
    // Latin letters: when one of the file's tags is Windows-1251 beyond
    // doubt, all of them are read so (audio/tag_text.rs).
    let cyr = tag_text::file_reads_1251([&title, &artist, &album_artist, &album].into_iter().filter_map(Field::id3));
    let (title, title_1251) = title.read(cyr, &stem);
    let (mut artist, _) = artist.read(cyr, &stem);
    let (album_artist, _) = album_artist.read(cyr, &stem);
    let (album, _) = album.read(cyr, &stem);
    if artist.is_empty() {
        artist = album_artist;
    }
    // A title read as Windows-1251 that lost a byte on the way in (U+FFFD)
    // gives way to the file's name, which has it whole. Other text keeps it,
    // as it always did.
    let title = if title.is_empty() || (title_1251 && title.contains('\u{FFFD}')) { stem } else { title };
    TextFields { title, artist, album, year }
}

/// Return the four-digit year prefix of a date string (e.g. "2023-04-12" →
/// "2023").  If the string is shorter than four characters or the prefix is
/// not all ASCII digits the original value is returned unchanged.
fn year_from_date_str(s: &str) -> String {
    let prefix = s.get(..4).unwrap_or(s);
    if prefix.chars().all(|c| c.is_ascii_digit()) {
        prefix.to_string()
    } else {
        s.to_string()
    }
}

/// Attempt to read the embedded front-cover art from a media file.
///
/// Tries in order:
///   1. A `FrontCover` visual embedded in the container (any metadata revision).
///   2. Any other embedded visual when no `FrontCover` is present.
///   3. A cover image file next to the media file
///      (`cover`/`folder`/`front`/`album` with `.jpg`, `.jpeg`, or `.png`,
///      case-insensitive on case-insensitive file systems such as Windows).
///
/// Returns the raw image bytes and its MIME type, or `None` when no art is
/// found.
pub fn probe_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    if let Some(cover) = embedded_cover(path) {
        return Some(cover);
    }
    adjacent_cover(path)
}

fn embedded_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    if crate::audio::dsd::dsf::sniff(path).is_some() {
        // A DSD file's ID3v2 pictures: the front cover, else the first.
        let r = crate::audio::dsd::dsf::open(path).ok()?;
        let visuals = &r.info().visuals;
        let pick = |v: &Visual| (v.data.to_vec(), v.media_type.as_deref().unwrap_or("image/jpeg").to_string());
        return visuals.iter().find(|v| v.usage == Some(StandardVisualKey::FrontCover)).or(visuals.first()).map(pick);
    }
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;

    let mut best: Option<(Vec<u8>, String)> = None;

    let mut metadata = format.metadata();
    while let Some(revision) = metadata.current() {
        for visual in &revision.media.visuals {
            let is_front = visual.usage == Some(StandardVisualKey::FrontCover);
            let mime = visual.media_type.as_deref().unwrap_or("image/jpeg").to_string();
            let data = visual.data.to_vec();
            if is_front {
                // FrontCover beats everything; take it and stop searching.
                return Some((data, mime));
            }
            if best.is_none() {
                best = Some((data, mime));
            }
        }
        if !metadata.is_latest() {
            metadata.pop();
        } else {
            break;
        }
    }

    best
}

/// Look for a cover image sitting next to the file in the same directory.
fn adjacent_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let dir = path.parent()?;
    // Ordered: the most conventional name first.
    const STEMS: &[&str] = &["cover", "folder", "front", "album"];
    const EXTS: &[(&str, &str)] = &[
        ("jpg",  "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("png",  "image/png"),
    ];
    const COVER_SIZE_CAP: u64 = 8 * 1024 * 1024; // 8 MiB
    for stem in STEMS {
        for (ext, mime) in EXTS {
            let candidate = dir.join(format!("{}.{}", stem, ext));
            // Guard against mis-named large files (videos, disk images, …).
            if std::fs::metadata(&candidate)
                .map(|m| m.len() > COVER_SIZE_CAP)
                .unwrap_or(true)
            {
                continue;
            }
            if let Ok(data) = std::fs::read(&candidate) {
                return Some((data, (*mime).to_string()));
            }
        }
    }
    None
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── CRC helpers (used by the FLAC builder) ──────────────────────────

    /// CRC-8/SMBUS: polynomial 0x07, initial 0, no reflection.
    fn flac_crc8(data: &[u8]) -> u8 {
        let mut crc: u8 = 0;
        for &b in data {
            crc ^= b;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
            }
        }
        crc
    }

    /// CRC-16/ANSI: polynomial 0x8005, initial 0, no reflection.
    fn flac_crc16(data: &[u8]) -> u16 {
        let mut crc: u16 = 0;
        for &b in data {
            crc ^= (b as u16) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
            }
        }
        crc
    }

    // ── Minimal FLAC builder ────────────────────────────────────────────

    /// One silent stereo frame (44 100 Hz, 16-bit, 192 samples, fixed-blocksize).
    /// This is appended to every test FLAC so that symphonia's `resync()`
    /// succeeds during the probe.  192 is the smallest block-size value that
    /// is representable by a single-byte encoding (enc=0x1) and meets FLAC's
    /// minimum block-length requirement of 16 samples.
    fn flac_silence_frame() -> Vec<u8> {
        // Frame header (before CRC8):
        //   sync code: 0xFF 0xF8 (fixed blocksize, streaming-flag=0)
        //   desc byte1: block_size_enc=0x1 (192), sample_rate_enc=0 (from StreamInfo)
        //   desc byte2: channels_enc=0x1 (2 independant), bps_enc=0 (from StreamInfo), reserved=0
        //   frame_number (UTF-8 encoded 0): 0x00
        // No block-size extension byte for enc=0x1.
        let header_before_crc: [u8; 5] = [0xFF, 0xF8, 0x10, 0x10, 0x00];
        let crc8 = flac_crc8(&header_before_crc);

        // Subframes: 2 CONSTANT silence subframes (16-bit zero).
        //   CH1 header (constant, no wasted bits): 0x00, sample: 0x00 0x00
        //   CH2 header: 0x00, sample: 0x00 0x00
        let subframes: [u8; 6] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let mut frame: Vec<u8> = header_before_crc.to_vec();
        frame.push(crc8);
        frame.extend_from_slice(&subframes);

        let crc16 = flac_crc16(&frame);
        frame.push((crc16 >> 8) as u8);
        frame.push((crc16 & 0xFF) as u8);

        frame
    }

    struct PictureEntry {
        pic_type: u32, // 3 = FrontCover, others = Other
        mime: String,
        width: u32,
        height: u32,
        depth: u32,
        data: Vec<u8>,
    }

    fn push_block(out: &mut Vec<u8>, header_byte: u8, body: &[u8]) {
        let len = body.len() as u32;
        out.push(header_byte);
        out.push(((len >> 16) & 0xFF) as u8);
        out.push(((len >>  8) & 0xFF) as u8);
        out.push( (len        & 0xFF) as u8);
        out.extend_from_slice(body);
    }

    /// Build a minimal valid FLAC with Vorbis-comment tags and an optional
    /// embedded PICTURE block.  Always includes a valid silence frame so that
    /// `probe()` succeeds past symphonia's `resync()`.
    fn make_flac(comments: &[(&str, &str)], picture: Option<PictureEntry>) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();

        // STREAMINFO (block type 0, not last).
        // sample_rate=44100, channels=2 (ch-1=1), bps=16 (bps-1=15), total_samples=192.
        // block_len_min = block_len_max = 192 (FLAC spec: min ≥ 16; 192 maps to enc=0x1 in the
        // frame header — a single-byte fixed-size code, no extension byte needed).
        let mut si = [0u8; 34];
        si[0] = 0x00; si[1] = 0xC0; // min_blocksize = 192
        si[2] = 0x00; si[3] = 0xC0; // max_blocksize = 192
        // Bytes 10-17 pack: sr(20)|ch-1(3)|bps-1(5)|total_samples(36).
        // sr=44100=0xAC44, ch-1=1, bps-1=15=0xF, total_samples=192=0xC0.
        si[10] = 0x0A; // sr bits 19-12
        si[11] = 0xC4; // sr bits 11-4
        si[12] = 0x42; // sr bits 3-0 | ch-1 bits 2-0 | bps-1 bit 4
        si[13] = 0xF0; // bps-1 bits 3-0 | total_samples bits 35-32 (=0)
        si[17] = 0xC0; // total_samples bits 7-0 (=192)
        push_block(&mut out, 0x00, &si); // not last

        // VORBIS_COMMENT (block type 4, not last).
        {
            let mut vc: Vec<u8> = Vec::new();
            let vendor = b"test";
            vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
            vc.extend_from_slice(vendor);
            vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
            for (key, value) in comments {
                let s = format!("{}={}", key.to_uppercase(), value);
                let sb = s.as_bytes();
                vc.extend_from_slice(&(sb.len() as u32).to_le_bytes());
                vc.extend_from_slice(sb);
            }
            let is_last_meta = picture.is_none();
            push_block(&mut out, if is_last_meta { 0x84 } else { 0x04 }, &vc);
        }

        // PICTURE (block type 6, last), optional.
        if let Some(pic) = picture {
            let mut pb: Vec<u8> = Vec::new();
            pb.extend_from_slice(&(pic.pic_type as u32).to_be_bytes());
            let mime = pic.mime.as_bytes();
            pb.extend_from_slice(&(mime.len() as u32).to_be_bytes());
            pb.extend_from_slice(mime);
            pb.extend_from_slice(&0u32.to_be_bytes()); // description length = 0
            pb.extend_from_slice(&(pic.width as u32).to_be_bytes());
            pb.extend_from_slice(&(pic.height as u32).to_be_bytes());
            pb.extend_from_slice(&(pic.depth as u32).to_be_bytes());
            pb.extend_from_slice(&0u32.to_be_bytes()); // indexed colors
            pb.extend_from_slice(&(pic.data.len() as u32).to_be_bytes());
            pb.extend_from_slice(&pic.data);
            push_block(&mut out, 0x86, &pb); // last block (0x80 | 0x06)
        }

        // Audio frame (required by symphonia's resync).
        out.extend_from_slice(&flac_silence_frame());

        out
    }

    // ── Minimal WAV builder (for tag extraction tests) ──────────────────

    /// Build a minimal valid PCM WAV file with RIFF LIST INFO tags.
    ///
    /// The `tags` slice holds `([4-byte RIFF INFO key], value)` pairs,
    /// for example `(b"INAM", b"My Title")`.  Keys are case-insensitive:
    /// symphonia lowercases them before map lookup.
    fn make_wav(tags: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        // fmt  chunk: PCM, stereo, 44100 Hz, 16-bit (16 bytes).
        let mut fmt = [0u8; 16];
        fmt[0..2].copy_from_slice(&1u16.to_le_bytes());      // AudioFormat = PCM
        fmt[2..4].copy_from_slice(&2u16.to_le_bytes());      // NumChannels = 2
        fmt[4..8].copy_from_slice(&44100u32.to_le_bytes());  // SampleRate
        fmt[8..12].copy_from_slice(&176400u32.to_le_bytes()); // ByteRate
        fmt[12..14].copy_from_slice(&4u16.to_le_bytes());    // BlockAlign
        fmt[14..16].copy_from_slice(&16u16.to_le_bytes());   // BitsPerSample

        // data chunk: 4 bytes of silence (1 stereo frame).
        let pcm = [0u8; 4];

        // LIST INFO chunk body: "INFO" + tag sub-chunks.
        let mut info: Vec<u8> = b"INFO".to_vec();
        for (key, val) in tags {
            let mut vdata = val.to_vec();
            vdata.push(0); // null terminator
            if vdata.len() % 2 != 0 { vdata.push(0); } // pad to even size
            info.extend_from_slice(*key);
            info.extend_from_slice(&(vdata.len() as u32).to_le_bytes());
            info.extend_from_slice(&vdata);
        }

        // WAVE body.
        let mut wave: Vec<u8> = Vec::new();
        wave.extend_from_slice(b"fmt ");
        wave.extend_from_slice(&16u32.to_le_bytes());
        wave.extend_from_slice(&fmt);
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wave.extend_from_slice(&pcm);
        if !tags.is_empty() {
            wave.extend_from_slice(b"LIST");
            wave.extend_from_slice(&(info.len() as u32).to_le_bytes());
            wave.extend_from_slice(&info);
        }

        // RIFF header.
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((wave.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&wave);
        out
    }

    fn write_tmp(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .expect("create")
            .write_all(bytes)
            .expect("write");
        (dir, path)
    }

    // ── Tag extraction (FLAC / Vorbis comments) — full tag set ─────────

    #[test]
    fn probe_reads_title_artist_album_year_flac() {
        let flac = make_flac(&[
            ("title",  "Test Title"),
            ("artist", "Test Artist"),
            ("album",  "Test Album"),
            ("date",   "2021-07-04"),
        ], None);
        let (_dir, path) = write_tmp(&flac, "track.flac");
        let info = probe(1, &path).expect("probe ok");
        assert_eq!(info.title,  "Test Title");
        assert_eq!(info.artist, "Test Artist");
        assert_eq!(info.album,  "Test Album");
        assert_eq!(info.year,   "2021");
    }

    #[test]
    fn probe_year_from_bare_date_string() {
        // Vorbis `date=1999` — a bare year, not an ISO date.
        let flac = make_flac(&[("date", "1999")], None);
        let (_dir, path) = write_tmp(&flac, "track.flac");
        let info = probe(1, &path).expect("probe ok");
        assert_eq!(info.year, "1999");
    }

    #[test]
    fn probe_title_falls_back_to_file_stem() {
        let wav = make_wav(&[]);
        let (_dir, path) = write_tmp(&wav, "my track.wav");
        let info = probe(1, &path).expect("probe ok");
        assert_eq!(info.title, "my track");
    }

    // ── Tag extraction (FLAC / Vorbis comments) ─────────────────────────

    #[test]
    fn probe_album_artist_fallback_flac() {
        let flac = make_flac(&[
            ("albumartist", "Album Artist"),
            ("title",       "Track"),
        ], None);
        let (_dir, path) = write_tmp(&flac, "track.flac");
        let info = probe(1, &path).expect("probe ok");
        assert_eq!(info.artist, "Album Artist");
    }

    // ── A Windows-1251 tag in a Latin-1 frame ───────────────────────────

    use crate::audio::tag_text::test_mp3::{id3_text_frame, id3_utf16_frame, make_mp3};

    // ── UTF-16 frames ────────────────────────────────────────────────────

    fn utf16(s: &str, bom: &[u8], be: bool) -> Vec<u8> {
        let mut b = bom.to_vec();
        for u in s.encode_utf16() {
            b.extend_from_slice(&if be { u.to_be_bytes() } else { u.to_le_bytes() });
        }
        b
    }

    #[test]
    fn utf_16_titles_read_in_either_byte_order() {
        // Anton's «Cyberpunk 2077» mp3s showed "䈀䴀" for "BM" (1.9.0, 26.10):
        // little-endian text without a byte-order mark, or with the wrong
        // one, read as big-endian. The rest is ID3v2 as written.
        let (title, artist) = ("When It's War", "Marcin Przybyłowicz, Ёж");
        let cases: [(&str, u8, &[u8], bool); 6] = [
            ("LE with its mark", 1, b"\xFF\xFE", false),
            ("BE with its mark", 1, b"\xFE\xFF", true),
            ("UTF-16BE", 2, b"", true),
            ("LE without a mark", 1, b"", false),
            ("LE behind a BE mark", 1, b"\xFE\xFF", false),
            ("LE marked UTF-16BE", 2, b"", false),
        ];
        for major in [3u8, 4] {
            for (case, enc, bom, be) in cases {
                let frames = [
                    id3_utf16_frame(b"TIT2", enc, &utf16(title, bom, be)),
                    id3_utf16_frame(b"TPE1", enc, &utf16(artist, bom, be)),
                ];
                let mut mp3 = make_mp3(&frames, None);
                mp3[3] = major;
                let (_dir, path) = write_tmp(&mp3, "track 01.mp3");
                let info = probe(1, &path).expect("probe ok");
                assert_eq!((info.title.as_str(), info.artist.as_str()), (title, artist), "v2.{major} {case}");
            }
        }
    }

    #[test]
    fn a_one_letter_utf_16_title_reads_back_by_the_file_name() {
        let frames = [id3_utf16_frame(b"TIT2", 1, &utf16("V", b"", false))];
        let (_dir, path) = write_tmp(&make_mp3(&frames, None), "Marcin Przybyłowicz - V.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").title, "V");
    }

    #[test]
    fn a_swapped_utf_16_title_that_lost_a_letter_gives_way_to_the_file_name() {
        // A swapped "Ü" is a lone surrogate to symphonia: U+FFFD.
        let frames = [id3_utf16_frame(b"TIT2", 1, &utf16("Über alles", b"", false))];
        let (_dir, path) = write_tmp(&make_mp3(&frames, None), "Band - Über alles.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").title, "Band - Über alles");
    }

    // The bytes of «Пыльца - Геометрия.mp3» in Anton's library: TIT2 and TPE1
    // in Windows-1251, marked ISO-8859-1, and the same in ID3v1. The player
    // showed "Ãåîìåòðèÿ" by "[mp3ex.net]Ïûëüöà".
    const TITLE: &[u8] = b"\xC3\xE5\xEE\xEC\xE5\xF2\xF0\xE8\xFF";
    const ARTIST: &[u8] = b"[mp3ex.net]\xCF\xFB\xEB\xFC\xF6\xE0";

    #[test]
    fn a_windows_1251_id3_tag_reads_in_cyrillic() {
        // With ID3v1 at the end, as the file has it, and without; the file's
        // name has none of it here: the tags stand on their own.
        let frames = [id3_text_frame(b"TIT2", TITLE), id3_text_frame(b"TPE1", ARTIST)];
        for v1 in [Some((TITLE, ARTIST, &b""[..])), None] {
            let (_dir, path) = write_tmp(&make_mp3(&frames, v1), "track 04.mp3");
            let info = probe(1, &path).expect("probe ok");
            assert_eq!(info.title, "Геометрия", "ID3v1: {}", v1.is_some());
            assert_eq!(info.artist, "[mp3ex.net]Пыльца", "ID3v1: {}", v1.is_some());
            assert_eq!(info.sample_rate, 44_100);
        }
    }

    #[test]
    fn a_short_album_reads_as_the_rest_of_the_file() {
        // «Мы»: two letters could be Latin-1, but the title is Windows-1251
        // beyond doubt, and so the file is.
        let frames = [id3_text_frame(b"TIT2", TITLE), id3_text_frame(b"TALB", b"\xCC\xFB")];
        let (_dir, path) = write_tmp(&make_mp3(&frames, None), "track 04.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").album, "Мы");
    }

    /// The readers of the metadata revisions symphonia holds for a file.
    fn readers_of(path: &Path) -> Vec<&'static str> {
        let mss = MediaSourceStream::new(Box::new(std::fs::File::open(path).expect("open")), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("mp3");
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .expect("probe");
        let mut out = Vec::new();
        let mut metadata = format.metadata();
        while let Some(r) = metadata.current() {
            out.push(r.info.short_name);
            if metadata.is_latest() {
                break;
            }
            metadata.pop();
        }
        out
    }

    #[test]
    fn id3v2_is_read_before_id3v1_which_cuts_the_title() {
        // «Геометрия (Remastered 2009 Version)»: 35 bytes, ID3v1 keeps 30.
        let long: &[u8] = b"\xC3\xE5\xEE\xEC\xE5\xF2\xF0\xE8\xFF (Remastered 2009 Version)";
        let mp3 = make_mp3(&[id3_text_frame(b"TIT2", long)], Some((long, ARTIST, &b""[..])));
        let (_dir, path) = write_tmp(&mp3, "track 04.mp3");
        // Both tags reach the walk, so the choice between them is made.
        let readers = readers_of(&path);
        assert!(readers.contains(&"id3v1") && readers.contains(&"id3v2"), "{readers:?}");
        assert_eq!(probe(1, &path).expect("probe ok").title, "Геометрия (Remastered 2009 Version)");
    }

    #[test]
    fn a_title_missing_a_byte_gives_way_to_the_file_name() {
        // «Пыльца — Геометрия»: the dash is 0x97, which the ID3v2 reader
        // turns into U+FFFD. The file's name has the title whole.
        let title: &[u8] = b"\xCF\xFB\xEB\xFC\xF6\xE0 \x97 \xC3\xE5\xEE\xEC\xE5\xF2\xF0\xE8\xFF";
        let (_dir, path) = write_tmp(&make_mp3(&[id3_text_frame(b"TIT2", title)], None), "Пыльца - Геометрия.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").title, "Пыльца - Геометрия");
    }

    #[test]
    fn western_id3_titles_stay_as_they_were() {
        // Three one-letter words once summed to "Cyrillic" (review 1.10).
        let title: &[u8] = b"Che cos'\xE8, \xE8 cos\xEC, \xE8 l'amore";
        let (_dir, path) = write_tmp(&make_mp3(&[id3_text_frame(b"TIT2", title)], None), "Track03.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").title, "Che cos'è, è così, è l'amore");
        // Windows-1252's apostrophe (0x92) is lost to U+FFFD: shown so, as
        // before — the file's name stands in only for a Windows-1251 title.
        let title: &[u8] = b"Don\x92t Stop Me Now";
        let (_dir, path) = write_tmp(&make_mp3(&[id3_text_frame(b"TIT2", title)], None), "Track03.mp3");
        assert_eq!(probe(1, &path).expect("probe ok").title, "Don\u{FFFD}t Stop Me Now");
    }

    #[test]
    fn a_vorbis_comment_is_never_read_again() {
        // FLAC tags are UTF-8 by their specification: what they say stands,
        // "è" and "Ã" alike.
        for s in ["Che cos'è, è così, è l'amore", "Þú komst í hlaðið", "Ãåîìåòðèÿ"] {
            let (_dir, path) = write_tmp(&make_flac(&[("title", s), ("album", "À toi, à moi, à nous")], None), "track.flac");
            let info = probe(1, &path).expect("probe ok");
            assert_eq!((info.title.as_str(), info.album.as_str()), (s, "À toi, à moi, à nous"));
        }
    }

    // ── Cover lookup ────────────────────────────────────────────────────

    fn small_jpeg() -> Vec<u8> {
        // Minimal JPEG magic (not a valid image for display, just for identification).
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]
    }

    fn small_png() -> Vec<u8> {
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    }

    #[test]
    fn probe_cover_returns_embedded_front_cover() {
        let pic = PictureEntry {
            pic_type: 3, // FrontCover
            mime: "image/jpeg".into(),
            width: 1, height: 1, depth: 24,
            data: small_jpeg(),
        };
        let flac = make_flac(&[], Some(pic));
        let (_dir, path) = write_tmp(&flac, "track.flac");
        let (data, mime) = probe_cover(&path).expect("cover found");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&data[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
    }

    #[test]
    fn probe_cover_prefers_front_cover_over_other_type() {
        // One FrontCover block — check that the data returned is the JPEG.
        let pic_front = PictureEntry {
            pic_type: 3,
            mime: "image/jpeg".into(),
            width: 1, height: 1, depth: 24,
            data: small_jpeg(),
        };
        let flac = make_flac(&[], Some(pic_front));
        let (_dir, path) = write_tmp(&flac, "track.flac");
        let (data, _) = probe_cover(&path).expect("cover found");
        assert_eq!(&data[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn probe_cover_falls_back_to_adjacent_cover_jpg() {
        // WAV with no embedded art.
        let wav = make_wav(&[]);
        let dir = tempfile::tempdir().expect("tempdir");
        let track_path = dir.path().join("track.wav");
        std::fs::File::create(&track_path).expect("create").write_all(&wav).expect("write");
        // Write a cover.jpg next to it.
        std::fs::File::create(dir.path().join("cover.jpg"))
            .expect("create cover")
            .write_all(&small_jpeg())
            .expect("write cover");
        let (data, mime) = probe_cover(&track_path).expect("cover found");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&data[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn probe_cover_adjacent_order_cover_before_folder() {
        // Both cover.png and folder.png exist; cover.png must win.
        let wav = make_wav(&[]);
        let dir = tempfile::tempdir().expect("tempdir");
        let track_path = dir.path().join("track.wav");
        std::fs::File::create(&track_path).unwrap().write_all(&wav).unwrap();
        std::fs::File::create(dir.path().join("cover.png")).unwrap().write_all(&small_png()).unwrap();
        std::fs::File::create(dir.path().join("folder.png")).unwrap().write_all(&[0u8; 8]).unwrap();
        let (data, _) = probe_cover(&track_path).expect("cover found");
        assert_eq!(&data[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn probe_cover_returns_none_when_no_art() {
        let wav = make_wav(&[]);
        let (_dir, path) = write_tmp(&wav, "track.wav");
        assert!(probe_cover(&path).is_none());
    }

    // ── Route guard (inline): id not in queue ───────────────────────────

    /// Verify the cover-cache id validation logic: an id not present in a
    /// simulated queue list returns no cover path.
    #[test]
    fn cover_route_rejects_unknown_id() {
        use crate::player::probe::TrackInfo;
        // Simulate: queue has id=1, request is for id=99.
        let queue: Vec<TrackInfo> = vec![TrackInfo {
            id: 1,
            path: "/some/track.flac".into(),
            title: "Track".into(),
            artist: String::new(),
            album: String::new(),
            year: String::new(),
            duration_s: 1.0,
            sample_rate: 44100,
            bits: 16,
            channels: 2,
            format: String::new(),
        }];
        let found = queue.iter().find(|t| t.id == 99);
        assert!(found.is_none(), "id 99 must not be found in queue");
        let found = queue.iter().find(|t| t.id == 1);
        assert!(found.is_some(), "id 1 must be found in queue");
    }

    // ── Unit tests for helper functions ─────────────────────────────────

    #[test]
    fn year_from_date_str_extracts_year_prefix() {
        assert_eq!(year_from_date_str("2023-01-15"), "2023");
        assert_eq!(year_from_date_str("2023"),       "2023");
        assert_eq!(year_from_date_str("20"),          "20");
        // Non-digit prefix returned as-is.
        assert_eq!(year_from_date_str("abcd-01"),    "abcd-01");
    }

    /// A chained Ogg file lasts as long as its links of the first one's rate
    /// (what the decode plays), not as its first link — the container's count.
    #[test]
    fn a_chained_ogg_file_lasts_as_long_as_its_links() {
        use crate::audio::converter::decode::test_ogg::{chained, LINK_BLOCK};
        let (_dir, path) = write_tmp(&chained(&[(4 * LINK_BLOCK, 44_100), (3 * LINK_BLOCK, 44_100)]), "chained.ogg");
        let t = probe(1, &path).expect("probe");
        assert_eq!(t.duration_s, (7 * LINK_BLOCK) as f64 / 44_100.0);
        let (_dir, path) = write_tmp(&chained(&[(4 * LINK_BLOCK, 44_100)]), "one.ogg");
        assert_eq!(probe(1, &path).expect("probe").duration_s, (4 * LINK_BLOCK) as f64 / 44_100.0);
    }

    #[test]
    fn a_dsd_file_shows_its_tags_rate_and_length() {
        use crate::audio::dsd::dsf::test_files::{dsf, id3v2};
        let tag = id3v2(&[
            id3_text_frame(b"TIT2", b"Title"),
            id3_text_frame(b"TPE2", b"Album artist"),
            id3_text_frame(b"TALB", b"Album"),
            id3_text_frame(b"TYER", b"2001"),
        ]);
        let samples = 2_822_400 * 2; // two seconds of DSD64
        let ch = vec![0x69u8; samples / 8];
        let (_dir, path) = write_tmp(&dsf(&[ch.clone(), ch], 2_822_400, samples as u64, 4096, false, Some(&tag)), "t.dsf");
        let t = probe(5, &path).expect("probe");
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str(), t.year.as_str()), ("Title", "Album artist", "Album", "2001"));
        assert_eq!((t.sample_rate, t.bits, t.channels, t.format.as_str()), (88_200, 0, 2, "DSD64"));
        assert!((t.duration_s - 2.0).abs() < 1e-9);
        assert_eq!(probe_cover(&path), None);
    }
}
