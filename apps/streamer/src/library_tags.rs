//! A music file's tags for the library: the text the player shows (read as the
//! engine reads it: ID3v2 before ID3v1, Windows-1251 when the file is so), plus
//! what the library sorts and groups by: album artist, genre, composer, track
//! and disc numbers. One open per file, the header only (no decode).

use std::path::Path;

use aura_core::audio::tag_text::{self, Field};
use serde::{Deserialize, Serialize};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub composer: String,
    pub genre: String,
    pub year: String,
    pub track_no: u32,
    pub disc_no: u32,
    pub duration_s: f64,
    pub sample_rate: u32,
    pub bits: u32,
    pub channels: u32,
    /// "DSD64"… for a DSD file (its `sample_rate` is then the DSD bit rate);
    /// empty for PCM.
    pub dsd: String,
    /// A picture inside the file.
    pub art: bool,
}

pub fn read(path: &Path) -> Result<Tags, String> {
    if aura_core::audio::dsd::dsf::sniff(path).is_some() {
        let r = aura_core::audio::dsd::dsf::open(path)?;
        let info = r.info();
        let mut t = text(path, &info.tags);
        t.duration_s = info.duration_s();
        t.sample_rate = info.rate.bit_rate() as u32;
        t.channels = info.channels as u32;
        t.dsd = info.label();
        t.art = !info.visuals.is_empty();
        return Ok(t);
    }
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut tags = Vec::new();
    let mut art = false;
    {
        let mut metadata = format.metadata();
        while let Some(revision) = metadata.current() {
            let reader = revision.info.short_name;
            tags.extend(revision.media.tags.iter().filter_map(|t| t.std.clone().map(|s| (reader, s))));
            art |= !revision.media.visuals.is_empty();
            if metadata.is_latest() {
                break;
            }
            metadata.pop();
        }
    }
    let mut t = text(path, &tags);
    t.art = art;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.as_ref().and_then(|p| p.audio()).is_some())
        .ok_or_else(|| format!("{}: no audio track", path.display()))?;
    let p = track.codec_params.as_ref().and_then(|p| p.audio()).unwrap();
    t.sample_rate = p.sample_rate.unwrap_or(0);
    t.bits = p.bits_per_sample.or(p.bits_per_coded_sample).unwrap_or(0);
    t.channels = p.channels.as_ref().map(|c| c.count() as u32).unwrap_or(0);
    if let (Some(n), r) = (track.num_frames, t.sample_rate) {
        if r > 0 {
            t.duration_s = n as f64 / r as f64;
        }
    }
    Ok(t)
}

fn year_of(s: &str) -> String {
    let p = s.get(..4).unwrap_or(s);
    if p.len() == 4 && p.chars().all(|c| c.is_ascii_digit()) { p.to_string() } else { String::new() }
}

/// "3/12" or "3": the number before the slash.
fn number_of(s: &str) -> u32 {
    s.split('/').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

fn text(path: &Path, tags: &[(&str, StandardTag)]) -> Tags {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut f: [Field; 6] = Default::default(); // title, artist, album artist, album, composer, genre
    let mut out = Tags::default();
    for (reader, tag) in tags {
        match tag {
            StandardTag::TrackTitle(v) => f[0].offer(reader, v),
            StandardTag::Artist(v) => f[1].offer(reader, v),
            StandardTag::AlbumArtist(v) => f[2].offer(reader, v),
            StandardTag::Album(v) => f[3].offer(reader, v),
            StandardTag::Composer(v) => f[4].offer(reader, v),
            StandardTag::Genre(v) => f[5].offer(reader, v),
            StandardTag::TrackNumber(n) if out.track_no == 0 => out.track_no = *n as u32,
            StandardTag::DiscNumber(n) if out.disc_no == 0 => out.disc_no = *n as u32,
            StandardTag::RecordingDate(v) | StandardTag::ReleaseDate(v) | StandardTag::OriginalReleaseDate(v)
                if out.year.is_empty() =>
            {
                out.year = year_of(v)
            }
            StandardTag::RecordingYear(y) | StandardTag::ReleaseYear(y) | StandardTag::OriginalReleaseYear(y)
                if out.year.is_empty() =>
            {
                out.year = y.to_string()
            }
            _ => {}
        }
    }
    let cyr = tag_text::file_reads_1251(f.iter().filter_map(Field::id3));
    let [title, artist, album_artist, album, composer, genre] = f.map(|x| x.read(cyr, &stem).0.trim().to_string());
    out.title = if title.is_empty() { stem } else { title };
    out.artist = if artist.is_empty() { album_artist.clone() } else { artist };
    out.album_artist = album_artist;
    out.album = album;
    out.composer = composer;
    out.genre = id3_genre(&genre);
    out
}

/// ID3's "(17)" or "17" for a numbered genre: the name, for the common ones.
fn id3_genre(g: &str) -> String {
    const NAMES: [&str; 26] = [
        "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge", "Hip-Hop", "Jazz", "Metal",
        "New Age", "Oldies", "Other", "Pop", "R&B", "Rap", "Reggae", "Rock", "Techno", "Industrial",
        "Alternative", "Ska", "Death Metal", "Pranks", "Soundtrack", "Euro-Techno",
    ];
    let n = g.trim().trim_start_matches('(').trim_end_matches(')');
    match n.parse::<usize>() {
        Ok(i) if i < NAMES.len() => NAMES[i].to_string(),
        _ => g.to_string(),
    }
}

/// "3/12" in a text tag some readers leave unparsed.
#[allow(dead_code)]
pub fn parse_number(s: &str) -> u32 {
    number_of(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_years_and_genres_read_as_people_wrote_them() {
        assert_eq!(parse_number("3/12"), 3);
        assert_eq!(parse_number(" 7 "), 7);
        assert_eq!(parse_number("x"), 0);
        assert_eq!(year_of("2003-03-25"), "2003");
        assert_eq!(year_of("March"), "");
        assert_eq!(id3_genre("(17)"), "Rock");
        assert_eq!(id3_genre("13"), "Pop");
        assert_eq!(id3_genre("Trip-Hop"), "Trip-Hop");
    }

    #[test]
    fn a_file_without_tags_is_named_by_its_file_and_an_album_artist_stands_in() {
        let p = Path::new("Music/Meteora/01 Foreword.flac");
        let t = text(p, &[]);
        assert_eq!(t.title, "01 Foreword");
        let aa = StandardTag::AlbumArtist(std::sync::Arc::new("Linkin Park".into()));
        let t = text(p, &[("flac", aa)]);
        assert_eq!(t.artist, "Linkin Park");
        assert_eq!(t.album_artist, "Linkin Park");
    }

    #[test]
    fn a_wav_written_here_reads_its_format() {
        let d = std::env::temp_dir().join(format!("aura-tags-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("tone.wav");
        crate::library::index::tests::write_wav(&p, 96_000, 24, 9600);
        let t = read(&p).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!((t.sample_rate, t.bits, t.channels), (96_000, 24, 2));
        assert!((t.duration_s - 0.1).abs() < 1e-6);
        assert_eq!(t.title, "tone");
    }
}
