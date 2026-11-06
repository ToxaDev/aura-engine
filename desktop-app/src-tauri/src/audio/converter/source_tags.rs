//! The source file's own tags and pictures, carried into its render.
//!
//! A render used to carry only its own stamp: album, artist, track number
//! and cover art were left behind, so a converted album fell apart in a
//! library (some players still found a cover in the folder, which made it
//! look random). Here the source is read once more — its header only — and
//! what it says about the music goes into the output: VORBIS_COMMENT and
//! PICTURE blocks in a FLAC, ID3v2 frames and APIC in a DSF.
//!
//! What no longer describes the render is left out: ReplayGain and R128
//! (the level changed), the old encoder's name, a lossy codec's gapless
//! data, a channel mask (the render is stereo), and any AuraEngine tag of an
//! earlier render (the new stamp replaces it).

use std::path::Path;
use symphonia::core::meta::{ColorMode, RawValue, StandardTag, StandardVisualKey, Visual};

/// An embedded picture, in the terms of a FLAC PICTURE block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The ID3v2 / FLAC picture type: 3 = front cover.
    pub kind: u32,
    pub mime: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    /// Bits per pixel; 0 when unknown.
    pub depth: u32,
    /// Colours of an indexed picture (GIF); 0 for any other.
    pub colors: u32,
    pub data: Vec<u8>,
}

/// The front cover's picture type.
pub const FRONT_COVER: u32 = 3;

/// The largest FLAC metadata block payload (24-bit length).
pub const FLAC_BLOCK_MAX: usize = 0x00FF_FFFF;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceTags {
    /// VORBIS_COMMENT pairs (upper-case keys), in the source's order.
    pub text: Vec<(String, String)>,
    pub pictures: Vec<Picture>,
}

impl SourceTags {
    /// The render's tag list: the source's tags, then `ours` (the stamp and
    /// lab tags). A key `ours` sets is not taken from the source.
    pub fn with(&self, ours: Vec<(String, String)>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .text
            .iter()
            .filter(|(k, _)| !ours.iter().any(|(o, _)| o.eq_ignore_ascii_case(k)))
            .cloned()
            .collect();
        out.extend(ours);
        out
    }

    /// Artist and title as the converter repaired them (`decode.rs`: a
    /// Windows code page in a Latin-1 ID3 frame) in place of the raw ones —
    /// only where the source has the tag; a title taken from the file's name
    /// is not written as one.
    pub fn repair_artist_title(&mut self, artist: &str, title: &str) {
        for (key, fixed) in [("ARTIST", artist), ("TITLE", title)] {
            if fixed.is_empty() {
                continue;
            }
            if let Some(slot) = self.text.iter_mut().find(|(k, _)| k == key) {
                slot.1 = fixed.to_string();
            }
        }
    }
}

/// The tags and pictures of the file at `path`. Never an error: a file whose
/// tags cannot be read converts with none, as before.
pub fn read(path: &Path) -> SourceTags {
    let out = if crate::audio::dsd::dsf::sniff(path).is_some() {
        match crate::audio::dsd::dsf::open(path) {
            Ok(reader) => {
                let info = reader.info();
                let revisions = vec![(
                    "dsd",
                    info.tags.iter().filter_map(|(_, t)| std_pair(t)).collect::<Vec<_>>(),
                )];
                assemble(revisions, info.visuals.iter())
            }
            Err(e) => {
                crate::aelog!("[TAGS] {}: tags not read ({})", path.display(), e);
                SourceTags::default()
            }
        }
    } else {
        match read_symphonia(path) {
            Ok(t) => t,
            Err(e) => {
                crate::aelog!("[TAGS] {}: tags not read ({})", path.display(), e);
                SourceTags::default()
            }
        }
    };
    crate::aelog!(
        "[TAGS] source: {} tag(s), {} picture(s){}",
        out.text.len(),
        out.pictures.len(),
        if out.pictures.iter().any(|p| p.kind == FRONT_COVER) { " (front cover)" } else { "" }
    );
    out
}

fn read_symphonia(path: &Path) -> Result<SourceTags, String> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;

    let mut revisions = Vec::new();
    let mut visuals = Vec::new();
    let mut metadata = format.metadata();
    while let Some(revision) = metadata.current() {
        let reader = revision.info.short_name;
        let raw = matches!(reader, "flac" | "vorbis");
        let pairs = revision
            .media
            .tags
            .iter()
            .filter_map(|t| if raw { raw_pair(&t.raw.key, &t.raw.value) } else { t.std.as_ref().and_then(std_pair) })
            .collect::<Vec<_>>();
        revisions.push((reader, pairs));
        visuals.extend(revision.media.visuals.iter().cloned());
        if !metadata.is_latest() {
            metadata.pop();
        } else {
            break;
        }
    }
    Ok(assemble(revisions, visuals.iter()))
}

/// One tag list out of the file's revisions: the most faithful one whole
/// (a FLAC's or Ogg's own comments before an ID3v2 tag stuck in front of
/// it, any tag before ID3v1), then from the others the keys it lacks.
fn assemble<'a>(mut revisions: Vec<(&'static str, Vec<(String, String)>)>, visuals: impl Iterator<Item = &'a Visual>) -> SourceTags {
    fn rank(reader: &str) -> u8 {
        match reader {
            "flac" | "vorbis" => 0,
            "id3v1" => 2,
            _ => 1,
        }
    }
    revisions.sort_by_key(|(reader, _)| rank(reader));
    let mut text: Vec<(String, String)> = Vec::new();
    for (i, (_, pairs)) in revisions.into_iter().enumerate() {
        if i == 0 {
            text = pairs;
            continue;
        }
        let have: Vec<String> = text.iter().map(|(k, _)| k.clone()).collect();
        text.extend(pairs.into_iter().filter(|(k, _)| !have.contains(k)));
    }
    text.retain(|(k, v)| !v.is_empty() && !dropped(k));
    dedup(&mut text);

    let mut pictures: Vec<Picture> = Vec::new();
    let mut untyped: Vec<usize> = Vec::new();
    for v in visuals {
        if v.data.is_empty() || pictures.iter().any(|p| *p.data == *v.data) {
            continue;
        }
        if v.usage.is_none() {
            untyped.push(pictures.len());
        }
        pictures.push(picture(v));
    }
    // A picture of no stated kind (an MP4 cover, a FLAC picture of a type
    // symphonia does not name) is the front cover when nothing else is:
    // players look for type 3.
    if !pictures.iter().any(|p| p.kind == FRONT_COVER) {
        if let Some(&i) = untyped.first() {
            pictures[i].kind = FRONT_COVER;
        }
    }
    SourceTags { text, pictures }
}

/// Exact repeats only (two readers saying the same); two values of one key
/// (ARTIST twice) both stay, as Vorbis comments allow.
fn dedup(text: &mut Vec<(String, String)>) {
    let mut seen: Vec<(String, String)> = Vec::new();
    text.retain(|pair| {
        if seen.contains(pair) {
            false
        } else {
            seen.push(pair.clone());
            true
        }
    });
}

/// Keys that do not describe the render.
fn dropped(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.starts_with("AURA")
        || k.starts_with("REPLAYGAIN_")
        || k.starts_with("R128_")
        || k.starts_with("ITUNSMPB")
        || matches!(
            k.as_str(),
            "ENCODER" | "ENCODER_OPTIONS" | "ENCODERSETTINGS" | "ENCODING" | "WAVEFORMATEXTENSIBLE_CHANNEL_MASK"
                | "METADATA_BLOCK_PICTURE" | "COVERART" | "COVERARTMIME"
        )
}

fn raw_pair(key: &str, value: &RawValue) -> Option<(String, String)> {
    let v = match value {
        RawValue::String(s) => s.to_string(),
        RawValue::StringList(l) => l.join("; "),
        RawValue::UnsignedInt(n) => n.to_string(),
        RawValue::SignedInt(n) => n.to_string(),
        RawValue::Float(f) => f.to_string(),
        RawValue::Boolean(b) => (if *b { "1" } else { "0" }).to_string(),
        _ => return None,
    };
    let key = key.trim().to_ascii_uppercase();
    (!key.is_empty()).then_some((key, v))
}

/// A standard tag under its Vorbis comment name (the names MusicBrainz
/// Picard and the reference tools write), for a source whose own keys are
/// not Vorbis names (ID3, MP4, RIFF, APE, DSD).
fn std_pair(tag: &StandardTag) -> Option<(String, String)> {
    use StandardTag as T;
    let s = |k: &str, v: &str| Some((k.to_string(), v.to_string()));
    let n = |k: &str, v: u64| Some((k.to_string(), v.to_string()));
    match tag {
        T::Album(v) => s("ALBUM", v),
        T::AlbumArtist(v) => s("ALBUMARTIST", v),
        T::Artist(v) => s("ARTIST", v),
        T::TrackTitle(v) => s("TITLE", v),
        T::TrackSubtitle(v) => s("SUBTITLE", v),
        T::TrackNumber(v) => n("TRACKNUMBER", *v),
        T::TrackTotal(v) => n("TRACKTOTAL", *v),
        T::DiscNumber(v) => n("DISCNUMBER", *v),
        T::DiscTotal(v) => n("DISCTOTAL", *v),
        T::DiscSubtitle(v) => s("DISCSUBTITLE", v),
        T::Genre(v) => s("GENRE", v),
        T::RecordingDate(v) | T::ReleaseDate(v) => s("DATE", v),
        T::RecordingYear(y) | T::ReleaseYear(y) => n("DATE", u64::from(*y)),
        T::OriginalReleaseDate(v) | T::OriginalRecordingDate(v) => s("ORIGINALDATE", v),
        T::OriginalReleaseYear(y) | T::OriginalRecordingYear(y) => n("ORIGINALYEAR", u64::from(*y)),
        T::Composer(v) => s("COMPOSER", v),
        T::Conductor(v) => s("CONDUCTOR", v),
        T::Lyricist(v) => s("LYRICIST", v),
        T::Writer(v) => s("WRITER", v),
        T::Arranger(v) => s("ARRANGER", v),
        T::Performer(v) => s("PERFORMER", v),
        T::Ensemble(v) => s("ENSEMBLE", v),
        T::Producer(v) => s("PRODUCER", v),
        T::Engineer(v) => s("ENGINEER", v),
        T::MixEngineer(v) => s("MIXER", v),
        T::Remixer(v) => s("REMIXER", v),
        T::Comment(v) => s("COMMENT", v),
        T::Description(v) => s("DESCRIPTION", v),
        T::Copyright(v) => s("COPYRIGHT", v),
        T::License(v) => s("LICENSE", v),
        T::Label(v) => s("LABEL", v),
        T::Grouping(v) => s("GROUPING", v),
        T::Mood(v) => s("MOOD", v),
        T::Language(v) => s("LANGUAGE", v),
        T::Lyrics(v) => s("LYRICS", v),
        T::Bpm(v) => n("BPM", *v),
        T::InitialKey(v) => s("INITIALKEY", v),
        T::CompilationFlag(true) => s("COMPILATION", "1"),
        T::Work(v) => s("WORK", v),
        T::MovementName(v) => s("MOVEMENTNAME", v),
        T::MovementNumber(v) => n("MOVEMENT", *v),
        T::MovementTotal(v) => n("MOVEMENTTOTAL", *v),
        T::IdentIsrc(v) => s("ISRC", v),
        T::IdentBarcode(v) => s("BARCODE", v),
        T::IdentCatalogNumber(v) => s("CATALOGNUMBER", v),
        T::ReleaseCountry(v) => s("RELEASECOUNTRY", v),
        T::MediaFormat(v) => s("MEDIA", v),
        T::SortAlbum(v) => s("ALBUMSORT", v),
        T::SortAlbumArtist(v) => s("ALBUMARTISTSORT", v),
        T::SortArtist(v) => s("ARTISTSORT", v),
        T::SortTrackTitle(v) => s("TITLESORT", v),
        T::SortComposer(v) => s("COMPOSERSORT", v),
        T::MusicBrainzAlbumId(v) => s("MUSICBRAINZ_ALBUMID", v),
        T::MusicBrainzAlbumArtistId(v) => s("MUSICBRAINZ_ALBUMARTISTID", v),
        T::MusicBrainzArtistId(v) => s("MUSICBRAINZ_ARTISTID", v),
        T::MusicBrainzRecordingId(v) => s("MUSICBRAINZ_TRACKID", v),
        T::MusicBrainzReleaseTrackId(v) => s("MUSICBRAINZ_RELEASETRACKID", v),
        T::MusicBrainzReleaseGroupId(v) => s("MUSICBRAINZ_RELEASEGROUPID", v),
        T::MusicBrainzWorkId(v) => s("MUSICBRAINZ_WORKID", v),
        T::MusicBrainzReleaseStatus(v) => s("RELEASESTATUS", v),
        T::MusicBrainzReleaseType(v) => s("RELEASETYPE", v),
        _ => None,
    }
}

fn picture(v: &Visual) -> Picture {
    let kind = v.usage.map(picture_type).unwrap_or(0);
    let description = v
        .tags
        .iter()
        .find_map(|t| match &t.std {
            Some(StandardTag::Description(d)) => Some(d.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let (depth, colors) = match &v.color_mode {
        Some(ColorMode::Direct(model)) => (model.bits_per_pixel(), 0),
        Some(ColorMode::Indexed(p)) => (u32::from(p.bits_per_pixel.get()), 1u32 << p.bits_per_pixel.get().min(31)),
        _ => (0, 0),
    };
    Picture {
        kind,
        mime: v.media_type.clone().unwrap_or_else(|| sniff_mime(&v.data).to_string()),
        description,
        width: v.dimensions.map(|d| d.width).unwrap_or(0),
        height: v.dimensions.map(|d| d.height).unwrap_or(0),
        depth,
        colors,
        data: v.data.to_vec(),
    }
}

fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "image/"
    }
}

/// The ID3v2 / FLAC picture type of a symphonia visual key.
fn picture_type(key: StandardVisualKey) -> u32 {
    use StandardVisualKey as K;
    match key {
        K::Other => 0x00,
        K::FileIcon => 0x01,
        K::OtherIcon => 0x02,
        K::FrontCover => 0x03,
        K::BackCover => 0x04,
        K::Leaflet => 0x05,
        K::Media => 0x06,
        K::LeadArtistPerformerSoloist => 0x07,
        K::ArtistPerformer => 0x08,
        K::Conductor => 0x09,
        K::BandOrchestra => 0x0a,
        K::Composer => 0x0b,
        K::Lyricist => 0x0c,
        K::RecordingLocation => 0x0d,
        K::RecordingSession => 0x0e,
        K::Performance => 0x0f,
        K::ScreenCapture => 0x10,
        K::Illustration => 0x12,
        K::BandArtistLogo => 0x13,
        K::PublisherStudioLogo => 0x14,
        _ => 0x00,
    }
}

/// The payload of a FLAC PICTURE block (without its 4-byte header), or None
/// when it would not fit one.
pub fn flac_picture_payload(p: &Picture) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(32 + p.mime.len() + p.description.len() + p.data.len());
    out.extend_from_slice(&p.kind.to_be_bytes());
    out.extend_from_slice(&(p.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(p.mime.as_bytes());
    out.extend_from_slice(&(p.description.len() as u32).to_be_bytes());
    out.extend_from_slice(p.description.as_bytes());
    for x in [p.width, p.height, p.depth, p.colors, p.data.len() as u32] {
        out.extend_from_slice(&x.to_be_bytes());
    }
    out.extend_from_slice(&p.data);
    (out.len() <= FLAC_BLOCK_MAX).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::encode::encode_flac;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn jpeg(n: usize, seed: u8) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xE0];
        d.extend((0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)));
        d
    }

    fn cover(kind: u32, data: Vec<u8>) -> Picture {
        Picture { kind, mime: "image/jpeg".into(), description: String::new(), width: 0, height: 0, depth: 0, colors: 0, data }
    }

    fn tone() -> Vec<f64> {
        (0..4096).map(|i| (i as f64 * 0.01).sin() * 0.25).collect()
    }

    #[test]
    fn a_flac_source_gives_its_tags_and_pictures_without_the_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.flac");
        let tags = pairs(&[
            ("ALBUM", "Kind of Blue"), ("ARTIST", "Miles Davis"), ("ARTIST", "John Coltrane"), ("TITLE", "So What"),
            ("tracknumber", "1"), ("REPLAYGAIN_TRACK_GAIN", "-3.2 dB"), ("R128_TRACK_GAIN", "-512"),
            ("ENCODER", "reference libFLAC 1.4.3"), ("AURA_RENDER", "1"), ("CUSTOM_NOTE", "first pressing"),
        ]);
        let front = cover(FRONT_COVER, jpeg(2000, 1));
        let back = Picture { description: "back".into(), ..cover(4, jpeg(1500, 2)) };
        encode_flac(&tone(), &tone(), 44_100, &src, &tags, &[front.clone(), back.clone()]).unwrap();

        let got = read(&src);
        assert_eq!(
            got.text,
            pairs(&[
                ("ALBUM", "Kind of Blue"), ("ARTIST", "Miles Davis"), ("ARTIST", "John Coltrane"), ("TITLE", "So What"),
                ("TRACKNUMBER", "1"), ("CUSTOM_NOTE", "first pressing"),
            ])
        );
        assert_eq!(got.pictures.len(), 2);
        assert_eq!((got.pictures[0].kind, &got.pictures[0].data), (FRONT_COVER, &front.data));
        assert_eq!((got.pictures[1].kind, got.pictures[1].description.as_str()), (4, "back"));
        assert_eq!(got.pictures[1].mime, "image/jpeg");
    }

    #[test]
    fn a_render_keeps_them_and_stays_a_valid_render() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.flac");
        let front = cover(FRONT_COVER, jpeg(5000, 3));
        encode_flac(&tone(), &tone(), 44_100, &src, &pairs(&[("ALBUM", "A"), ("TITLE", "T")]), std::slice::from_ref(&front)).unwrap();
        let source = read(&src);

        let out = dir.path().join("out.flac");
        let ours = crate::audio::converter::stamp::stamp_tags(&[], false, None, 0, "");
        let tags = source.with(ours.clone());
        encode_flac(&tone(), &tone(), 88_200, &out, &tags, &source.pictures).unwrap();

        let back = read(&out);
        assert_eq!(back.text, pairs(&[("ALBUM", "A"), ("TITLE", "T")]), "the stamp is not a source tag");
        assert_eq!(back.pictures, vec![front]);
        let written = crate::audio::converter::stamp::read_render_stamp(&out).unwrap().unwrap();
        assert_eq!(written, tags);
        assert!(written.ends_with(&ours));
        assert!(crate::audio::converter::stamp::playable(&out));
        // The file decodes as before: the pictures sit in their own blocks.
        let decoded = crate::audio::converter::decode::decode_file(&out).unwrap().samples_l.len();
        assert_eq!(decoded, 4096);
    }

    #[test]
    fn our_keys_win_and_an_earlier_render_leaves_nothing_behind() {
        let source = SourceTags { text: pairs(&[("ALBUM", "A"), ("Aura_Stages", "old"), ("COMMENT", "x")]), pictures: vec![] };
        let merged = source.with(pairs(&[("AURA_STAGES", "FIR:1"), ("COMMENT", "ours")]));
        assert_eq!(merged, pairs(&[("ALBUM", "A"), ("AURA_STAGES", "FIR:1"), ("COMMENT", "ours")]));
        assert!(dropped("aura_render") && dropped("replaygain_album_peak") && dropped("METADATA_BLOCK_PICTURE"));
        assert!(!dropped("ALBUM") && !dropped("ENCODED-BY"));
    }

    #[test]
    fn a_picture_of_no_stated_kind_is_the_front_cover_when_none_is() {
        let untyped = Visual { media_type: None, dimensions: None, color_mode: None, usage: None, tags: vec![], data: jpeg(10, 0).into() };
        let got = assemble(vec![], std::iter::once(&untyped));
        assert_eq!(got.pictures.len(), 1);
        assert_eq!((got.pictures[0].kind, got.pictures[0].mime.as_str()), (FRONT_COVER, "image/jpeg"));
        // The same picture twice (two readers) is one.
        let got = assemble(vec![], [&untyped, &untyped].into_iter());
        assert_eq!(got.pictures.len(), 1);
        let typed = Visual { usage: Some(StandardVisualKey::FrontCover), data: jpeg(10, 9).into(), ..untyped.clone() };
        let got = assemble(vec![], [&untyped, &typed].into_iter());
        assert_eq!(got.pictures.iter().map(|p| p.kind).collect::<Vec<_>>(), vec![0, FRONT_COVER]);
    }

    #[test]
    fn the_most_faithful_tag_leads_and_the_others_fill_its_gaps() {
        let got = assemble(
            vec![
                ("id3v1", pairs(&[("TITLE", "So Wha"), ("GENRE", "Jazz")])),
                ("id3v2", pairs(&[("TITLE", "So What"), ("ALBUM", "Kind of Blue")])),
            ],
            std::iter::empty(),
        );
        assert_eq!(got.text, pairs(&[("TITLE", "So What"), ("ALBUM", "Kind of Blue"), ("GENRE", "Jazz")]));
        assert_eq!(std_pair(&StandardTag::TrackNumber(7)), Some(("TRACKNUMBER".into(), "7".into())));
        assert_eq!(std_pair(&StandardTag::ReleaseYear(1959)), Some(("DATE".into(), "1959".into())));
    }

    #[test]
    fn the_decode_repairs_artist_and_title_only_where_the_source_has_them() {
        let mut t = SourceTags { text: pairs(&[("ARTIST", "Êèíî"), ("ALBUM", "A")]), pictures: vec![] };
        t.repair_artist_title("Кино", "05 - Группа крови");
        assert_eq!(t.text, pairs(&[("ARTIST", "Кино"), ("ALBUM", "A")]));
    }

    #[test]
    fn a_file_with_no_tags_or_no_such_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare.flac");
        encode_flac(&tone(), &tone(), 44_100, &bare, &[], &[]).unwrap();
        assert_eq!(read(&bare), SourceTags::default());
        assert_eq!(read(&dir.path().join("missing.flac")), SourceTags::default());
    }
}
