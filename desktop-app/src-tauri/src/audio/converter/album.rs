//! One level per album.
//!
//! The true-peak ceiling is one gain per file, cut-only, and how deep it cuts
//! depends on what the chain did to that file: how high Declip rebuilt the
//! peaks, what the phase engine and XTC added, and whether the ISP output
//! limiter could hold the overs locally or refused and left the whole file to
//! the ceiling. Converted track by track, an album comes out with its balance
//! changed. Measured on a 12-track CD (t.A.T.u., *200 по встречной*): cuts of
//! 0 to −6.2 dB, the balance against the disc off by up to 4.9 dB; one gain for
//! the whole album — the deepest of the per-track cuts — kept it within 0.14 dB.
//! Discussions #5 (Californication, DR4) lost 11.9 dB on one track and 3.2 on
//! another.
//!
//! A group is the queued files that share a folder and an ALBUM tag. A file
//! without the tag, or the only queued track of its album, keeps its own gain
//! exactly as before: singles in a folder of years must never tie together.
//!
//! The gain has to be known before a track's output stages run, and each
//! track's cut is only known once its chain has run. So the manager runs the
//! source stages of every track in the group first, estimates what the output
//! stage will find on each render (`estimate_peak`), and takes the smallest of
//! the per-track gains (`own_gain`). Each track then takes that one gain in
//! front of its output limiter. The estimate comes from a short render of the
//! same chain when the application has installed one (`install_peak_probe`);
//! what it misses by, the limiter holds locally at the ceiling, so the balance
//! does not move with it.

use crate::audio::converter::dsp::true_peak::{measure_true_peak, target_lin_for};
use crate::audio::converter::types::{ConvertSettings, PreparedAudio};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What the output stages will find on a track's finished render, estimated
/// before the render exists.
#[derive(Clone, Copy, Debug)]
pub struct PeakEstimate {
    /// True peak of the render, linear, before any gain.
    pub tp_lin: f64,
    /// Whether the ISP output limiter may hold the overs locally at the
    /// ceiling (its own rule: no cluster deeper than 6 dB, at most 5 % of the
    /// file under gain reduction).
    pub local: bool,
    /// True when a short render of the chain was measured; false for the
    /// source's own 4× peaks, which miss what the phase engine and XTC add.
    pub rendered: bool,
}

/// A short render of a track's chain, measured: the application's peak probe.
/// It is handed the prepared source and may take its buffers for the time it
/// runs, but must give them back unchanged. None when it cannot run for this
/// file (no integer ratio, a filter missing), and the source's peaks are used.
pub type PeakProbe = fn(&Path, &mut PreparedAudio, &ConvertSettings) -> Option<PeakEstimate>;

static PROBE: OnceLock<PeakProbe> = OnceLock::new();

/// Called once at start-up by the application, which has the renderer. The
/// engine on its own (the headless converter) has none and falls back to the
/// source's 4× peaks.
pub fn install_peak_probe(probe: PeakProbe) {
    let _ = PROBE.set(probe);
}

/// The output peak of one prepared track: the installed probe's, or the
/// source's own 4× peaks and the limiter's decision on them.
pub fn estimate_peak(src: &Path, prep: &mut PreparedAudio, settings: &ConvertSettings) -> PeakEstimate {
    if let Some(probe) = PROBE.get() {
        if let Some(e) = probe(src, prep, settings) {
            return e;
        }
    }
    let tp_lin = measure_true_peak(&prep.audio_l, &prep.audio_r);
    let local = !settings.lab.isp
        || crate::audio::converter::dsp::lab::isp::output_limit_is_local(
            &prep.audio_l,
            &prep.audio_r,
            target_lin_for(prep.true_peak_target_dbtp),
            prep.sample_rate,
        );
    PeakEstimate { tp_lin, local, rendered: false }
}

/// The gain the output stage takes a track down by when it is converted on
/// its own, by the converter's own rule: nothing when the peak is under the
/// ceiling; nothing when the ISP output limiter holds the overs locally;
/// otherwise the whole track down to the ceiling.
pub fn own_gain(est: &PeakEstimate, target_dbtp: f64, isp: bool) -> f64 {
    let t = target_lin_for(target_dbtp);
    if !(est.tp_lin > t) {
        return 1.0;
    }
    let over_db = 20.0 * (est.tp_lin / t).log10();
    if isp && est.local && over_db <= 6.0 {
        1.0
    } else {
        t / est.tp_lin
    }
}

/// The album level one track takes.
#[derive(Clone, Debug)]
pub struct AlbumLevel {
    /// The one gain every track of the group takes, linear, at most 1.
    pub gain: f64,
    /// What this track would have taken converted alone (the estimate).
    pub own: f64,
    /// The track whose cut set the level.
    pub set_by: String,
    /// The ALBUM tag.
    pub album: String,
    /// Queued tracks in the group.
    pub tracks: usize,
    /// Tracks in the same folder with the same tag that are not in the queue.
    pub not_queued: usize,
}

/// What album level did for one file: None when it is off for the batch.
#[derive(Clone, Debug)]
pub enum AlbumRow {
    /// The file is one of a group and takes its level.
    Level(AlbumLevel),
    /// The file keeps its own gain, and why.
    Alone(String),
}

impl AlbumLevel {
    pub fn gain_db(&self) -> f64 {
        20.0 * self.gain.log10()
    }

    /// The gain actually put in front of the output stages: none at all when
    /// no track of the album needs a cut, so such an album converts exactly
    /// as its tracks would alone.
    pub fn applied(&self) -> f64 {
        if self.gain < 1.0 {
            self.gain
        } else {
            1.0
        }
    }

    /// The AURA_ALBUM_GAIN tag value.
    pub fn tag_value(&self) -> String {
        format!(
            "{:+.2}dB;own={:+.2}dB;set_by={};tracks={}",
            self.gain_db(),
            20.0 * self.own.log10(),
            self.set_by.replace(';', ","),
            self.tracks
        )
    }

    /// The sentence the queue row and the lab report carry.
    pub fn summary(&self) -> String {
        let mut s = if self.gain < 1.0 {
            format!(
                "album level {:+.2} dB for the {} queued tracks of «{}», set by «{}»; this track alone would have taken {:+.2} dB",
                self.gain_db(),
                self.tracks,
                self.album,
                self.set_by,
                20.0 * self.own.log10()
            )
        } else {
            format!(
                "no track of the {} queued from «{}» needs a cut — each keeps its level",
                self.tracks, self.album
            )
        };
        if self.not_queued > 0 {
            s.push_str(&format!(
                "; the folder holds {} more track{} with this album tag, not in this batch",
                self.not_queued,
                if self.not_queued == 1 { "" } else { "s" }
            ));
        }
        s
    }
}

/// What the status line adds for a file done at its album's level: empty
/// when no album gain was applied.
pub fn status_note(row: Option<&AlbumRow>) -> String {
    match row {
        Some(AlbumRow::Level(a)) if a.gain < 1.0 => {
            format!(", album level {:+.2} dB set by «{}»", a.gain_db(), a.set_by)
        }
        _ => String::new(),
    }
}

/// The ALBUM tag of a file, read from the container header without decoding
/// (the same probe `decode::probe_input_frames` makes). None when the file
/// has none, or it is empty.
pub fn album_tag(path: &Path) -> Option<String> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::{MetadataOptions, StandardTag};

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    // Every revision the reader holds, ID3v2's album before ID3v1's (whose
    // 30 bytes cut a longer one) — the walk decode_file makes.
    let mut album = crate::audio::tag_text::Field::default();
    let mut metadata = format.metadata();
    while let Some(revision) = metadata.current() {
        let reader = revision.info.short_name;
        for tag in &revision.media.tags {
            if let Some(StandardTag::Album(v)) = &tag.std {
                album.offer(reader, v.trim());
            }
        }
        if !metadata.is_latest() {
            metadata.pop();
        } else {
            break;
        }
    }
    // This is the key tracks are grouped by, so the same raw album must give
    // the same key in every file: read as Windows-1251 only when it is so
    // beyond doubt by itself — not by the file's name or its other tags.
    let own = crate::audio::tag_text::file_reads_1251(album.id3());
    let (v, _) = album.read(own, "");
    (!v.is_empty()).then_some(v)
}

/// The folder a file is grouped by: its parent directory, as the Adaptive
/// Apodizer's album pool keys it.
pub fn folder_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// One album in the queue: two or more queued files in one folder with one
/// ALBUM tag, in queue order.
#[derive(Clone, Debug, PartialEq)]
pub struct AlbumGroup {
    pub folder: String,
    pub album: String,
    pub members: Vec<usize>,
}

/// The queue cut into albums. For each file, the group it belongs to, or
/// None when it keeps its own gain — no tag, or the only queued track of its
/// album. `tags[i]` is the ALBUM tag of `paths[i]`.
pub fn group_queue(paths: &[String], tags: &[Option<String>]) -> (Vec<Option<usize>>, Vec<AlbumGroup>) {
    let mut groups: Vec<AlbumGroup> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let Some(album) = tags.get(i).and_then(|t| t.as_ref()) else { continue };
        let folder = folder_of(p);
        match groups.iter_mut().find(|g| g.folder == folder && g.album == *album) {
            Some(g) => g.members.push(i),
            None => groups.push(AlbumGroup { folder, album: album.clone(), members: vec![i] }),
        }
    }
    groups.retain(|g| g.members.len() >= 2);
    let mut of = vec![None; paths.len()];
    for (gi, g) in groups.iter().enumerate() {
        for &m in &g.members {
            of[m] = Some(gi);
        }
    }
    (of, groups)
}

/// Audio files the decoder may be handed, by extension — for counting an
/// album's tracks that were left out of the queue.
const AUDIO_EXTENSIONS: [&str; 13] = [
    "flac", "wav", "wave", "mp3", "ogg", "oga", "opus", "m4a", "aac", "aif", "aiff", "caf", "wv",
];

/// A folder with more audio files than this is not read for its album tags:
/// counting an album's left-out tracks is one sentence in the log, and a
/// folder of a few thousand singles would make it cost a minute.
const MAX_FOLDER_SCAN: usize = 200;

/// The ALBUM tag of every audio file in `folder`, by file name, from the
/// container headers. The converter's own outputs (`… [AE · …].flac`) are
/// left out. None when the folder cannot be read or holds more than
/// `MAX_FOLDER_SCAN` audio files.
pub fn folder_album_tags(folder: &str) -> Option<Vec<(std::ffi::OsString, Option<String>)>> {
    let files: Vec<PathBuf> = std::fs::read_dir(folder)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| AUDIO_EXTENSIONS.contains(&x.to_ascii_lowercase().as_str()))
        })
        .filter(|p| !p.file_name().map_or(false, |n| n.to_string_lossy().contains("[AE \u{b7} ")))
        .collect();
    if files.len() > MAX_FOLDER_SCAN {
        return None;
    }
    Some(
        files
            .iter()
            .filter_map(|p| Some((p.file_name()?.to_os_string(), album_tag(p))))
            .collect(),
    )
}

/// How many files of `folder` (its `folder_album_tags`) carry `album` without
/// being in `queued`. Compared by name among the queued files of this folder:
/// the queue's spelling of the folder need not be read_dir's.
pub fn count_not_queued(
    in_folder: &[(std::ffi::OsString, Option<String>)],
    folder: &str,
    album: &str,
    queued: &[String],
) -> usize {
    let queued: Vec<std::ffi::OsString> = queued
        .iter()
        .filter(|q| folder_of(q) == folder)
        .filter_map(|q| PathBuf::from(q).file_name().map(|n| n.to_os_string()))
        .collect();
    in_folder
        .iter()
        .filter(|(name, tag)| tag.as_deref() == Some(album) && !queued.iter().any(|q| q == name))
        .count()
}

/// Why a file keeps its own gain, for its queue row. `not_queued` is None
/// when the folder was not read for its tags (`folder_album_tags`).
pub fn alone_reason(tag: Option<&str>, not_queued: Option<usize>) -> String {
    match (tag, not_queued) {
        (None, _) => "no album tag: this file keeps its own level".to_string(),
        (Some(a), Some(n)) if n > 0 => format!(
            "the only queued track of «{}» — the folder holds {} more with this tag; queue them together for one album level",
            a, n
        ),
        (Some(a), Some(_)) => format!("the only track of «{}» in its folder: it keeps its own level", a),
        (Some(a), None) => format!("the only queued track of «{}»: it keeps its own level", a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn a_group_is_one_folder_and_one_tag_with_two_tracks_or_more() {
        let paths = vec![
            p(r"C:\m\A\01.flac"),
            p(r"C:\m\A\02.flac"),
            p(r"C:\m\B\01.flac"), // same tag, other folder: its own album
            p(r"C:\m\A\bonus.flac"), // same folder, other tag
            p(r"C:\m\A\03.flac"),
            p(r"C:\m\1999\single.flac"), // no tag
            p(r"C:\m\1999\other.flac"),  // no tag, same folder: never tied
        ];
        let tags = vec![
            Some("X".to_string()),
            Some("X".to_string()),
            Some("X".to_string()),
            Some("Y".to_string()),
            Some("X".to_string()),
            None,
            None,
        ];
        let (of, groups) = group_queue(&paths, &tags);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![0, 1, 4]);
        assert_eq!(of, vec![Some(0), Some(0), None, None, Some(0), None, None]);
    }

    #[test]
    fn one_raw_album_is_one_key_whatever_the_file_says_besides() {
        use crate::audio::tag_text::test_mp3::{id3_text_frame, make_mp3};
        // ALBUM «Мы» in Windows-1251, two letters: in one file beside a title
        // that is Windows-1251 beyond doubt («Геометрия») and a Cyrillic
        // name, in the other beside "Intro". Both must fall in one album.
        let album: &[u8] = b"\xCC\xFB";
        let dir = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for (name, title) in [("01 Мы.mp3", &b"\xC3\xE5\xEE\xEC\xE5\xF2\xF0\xE8\xFF"[..]), ("02 Intro.mp3", &b"Intro"[..])] {
            let path = dir.path().join(name);
            let mp3 = make_mp3(&[id3_text_frame(b"TIT2", title), id3_text_frame(b"TALB", album)], None);
            std::fs::write(&path, mp3).expect("write");
            paths.push(path.to_string_lossy().to_string());
        }
        let tags: Vec<Option<String>> = paths.iter().map(|p| album_tag(Path::new(p))).collect();
        assert_eq!(tags[0], tags[1]);
        assert!(tags[0].is_some());
        let (_, groups) = group_queue(&paths, &tags);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![0, 1]);
    }

    #[test]
    fn the_own_gain_is_the_converters_rule() {
        let t = -0.5;
        let tl = target_lin_for(t);
        // Under the ceiling: nothing.
        let e = PeakEstimate { tp_lin: tl * 0.9, local: false, rendered: true };
        assert_eq!(own_gain(&e, t, true), 1.0);
        // Over, local, ISP on: held by the limiter, nothing off the track.
        let e = PeakEstimate { tp_lin: tl * 1.5, local: true, rendered: true };
        assert_eq!(own_gain(&e, t, true), 1.0);
        // The same with ISP off: the whole track comes down.
        assert!((own_gain(&e, t, false) - 1.0 / 1.5).abs() < 1e-12);
        // Over and not local: the whole track comes down.
        let e = PeakEstimate { tp_lin: tl * 2.0, local: false, rendered: true };
        assert!((own_gain(&e, t, true) - 0.5).abs() < 1e-12);
        // More than 6 dB over is never local, whatever the flag says.
        let e = PeakEstimate { tp_lin: tl * 2.5, local: true, rendered: true };
        assert!((own_gain(&e, t, true) - 0.4).abs() < 1e-12);
    }

    #[test]
    fn a_file_on_its_own_says_why() {
        assert!(alone_reason(None, None).starts_with("no album tag"));
        assert!(alone_reason(Some("X"), Some(11)).contains("the folder holds 11 more with this tag"));
        assert!(alone_reason(Some("X"), Some(0)).contains("the only track of «X» in its folder"));
        assert!(alone_reason(Some("X"), None).starts_with("the only queued track of «X»"));
    }

    #[test]
    fn left_out_tracks_are_counted_by_name_in_their_own_folder() {
        use std::ffi::OsString;
        let folder = r"C:\m\A";
        let in_folder = vec![
            (OsString::from("01.flac"), Some("X".to_string())),
            (OsString::from("02.flac"), Some("X".to_string())),
            (OsString::from("03.flac"), Some("X".to_string())),
            (OsString::from("bonus.flac"), Some("Y".to_string())),
            (OsString::from("untagged.flac"), None),
        ];
        // 01 is queued from this folder; 02 is queued, but from another one.
        let queued = vec![p(r"C:\m\A\01.flac"), p(r"C:\m\B\02.flac")];
        assert_eq!(count_not_queued(&in_folder, folder, "X", &queued), 2);
        assert_eq!(count_not_queued(&in_folder, folder, "Y", &queued), 1);
        assert_eq!(count_not_queued(&in_folder, folder, "Z", &queued), 0);
    }

    #[test]
    fn an_album_that_needs_no_cut_applies_nothing() {
        let a = AlbumLevel {
            gain: 1.0,
            own: 1.0,
            set_by: String::new(),
            album: "X".into(),
            tracks: 3,
            not_queued: 0,
        };
        assert_eq!(a.applied(), 1.0);
        let b = AlbumLevel { gain: 0.5, ..a };
        assert_eq!(b.applied(), 0.5);
        assert!(b.tag_value().starts_with("-6.02dB;own=+0.00dB;set_by=;tracks=3"));
    }
}
