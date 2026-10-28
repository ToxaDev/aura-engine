//! The stamp every render carries: which version made it, which generation
//! of the sound it is, and the version of each stage that ran.
//!
//! A render is the source as this app hears it. When a release changes what
//! a render holds, a file made before that is no longer what the source would
//! sound like now: the list shows the source as not converted and the player
//! plays the source, until the file is converted again. What decides it is
//! the generation of the sound ([`AURA_RENDER`]) and the stages' versions, not
//! the app's version number: a release that only touches the interface keeps
//! the library as it is. The app's version is in the file for people.
//!
//! The stamp is plain tags — VORBIS_COMMENT in a FLAC, ID3v2 `TXXX` frames in
//! a DSF — read here from the headers alone, without decoding a sample.

use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The generation of the sound a render holds. Raise it when a release
/// changes what every render writes; a stage of its own raises its own
/// version instead (`*STAGE_VERSION` beside each stage).
// 2 (1.8.0): noise shaping above 48 kHz output and the multistage upsampler as the only one change every PCM render.
pub const AURA_RENDER: u32 = 2;

/// The app that wrote the render, as the app shows it.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The first version that stamps its renders: a render without a stamp was
/// made before it.
const FIRST_STAMPED: &str = "1.7.0";

pub const KEY_APP_VERSION: &str = "AURA_APP_VERSION";
pub const KEY_RENDER: &str = "AURA_RENDER";
pub const KEY_STAGES: &str = "AURA_STAGES";
pub const KEY_SETTINGS: &str = "AURA_SETTINGS";

/// The resampler, always in a render.
pub const STAGE_FIR: &str = "FIR";
/// The DSD modulator, in a DSF render.
pub const STAGE_DSD: &str = "DSD";

/// The current version of a stage, by the token it has in the chain (and in
/// the file name). None for a token this build does not know.
pub fn stage_version(token: &str) -> Option<u32> {
    use crate::audio::converter::dsp::lab;
    use crate::audio::converter::{apodize, dsd_out, dsp, pipeline::hybrid_mixer};
    Some(match token {
        STAGE_FIR => dsp::filter::FIR_STAGE_VERSION,
        STAGE_DSD => dsd_out::STAGE_VERSION,
        "DC" => lab::declip::STAGE_VERSION,
        "ISP" => lab::isp::STAGE_VERSION,
        "SUB10" | "SUB15" | "SUB20" => apodize::SUBSONIC_STAGE_VERSION,
        "AHR" => lab::headroom::STAGE_VERSION,
        "AA" => apodize::AA_STAGE_VERSION,
        "PFR" => dsp::polyphase::PFR_STAGE_VERSION,
        "HP" => hybrid_mixer::HP_STAGE_VERSION,
        "aHP" => hybrid_mixer::AHP_STAGE_VERSION,
        "TFS" => lab::tfs::STAGE_VERSION,
        "XTC" => lab::xtc::STAGE_VERSION,
        _ => return None,
    })
}

/// The VORBIS_COMMENT vendor string of a render.
pub fn vendor() -> String {
    format!("AuraEngine {}", APP_VERSION)
}

/// The stamp's tags for a render whose chain ran `lab_chain` (the tokens of
/// the file name), into DSF when `dsd`. `rack_sig` is the page's settings
/// signature, kept as its hash.
pub fn stamp_tags(lab_chain: &[&str], dsd: bool, rack_sig: Option<&str>) -> Vec<(String, String)> {
    let mut stages = vec![STAGE_FIR];
    if dsd {
        stages.push(STAGE_DSD);
    }
    let mut chain: Vec<&str> = Vec::new();
    for want in crate::audio::converter::dsp::lab::CHAIN_ORDER {
        if lab_chain.contains(&want) {
            chain.push(want);
        }
    }
    let unlisted: Vec<&str> = lab_chain.iter().filter(|t| !chain.contains(t)).copied().collect();
    chain.extend(unlisted);
    stages.extend(chain);
    let stages = stages
        .iter()
        .map(|t| format!("{}:{}", t, stage_version(t).unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(",");
    let mut out = vec![
        (KEY_APP_VERSION.to_string(), APP_VERSION.to_string()),
        (KEY_RENDER.to_string(), AURA_RENDER.to_string()),
        (KEY_STAGES.to_string(), stages),
    ];
    if let Some(sig) = rack_sig.filter(|s| !s.is_empty()) {
        let hash = Sha256::digest(sig.as_bytes());
        out.push((KEY_SETTINGS.to_string(), hash.iter().map(|b| format!("{:02x}", b)).collect()));
    }
    out
}

/// Whether a render still is what its source sounds like in this build.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Valid,
    /// No stamp: made before the first stamped version (or not by this app).
    NoStamp,
    /// An older generation of the sound, or an older version of a stage
    /// that ran; with the version that made it.
    Older { made_by: String },
}

impl Verdict {
    /// The row's tip for a render that does not count.
    pub fn tip(&self) -> Option<String> {
        match self {
            Verdict::Valid => None,
            Verdict::NoStamp => Some(format!("Made before {} — convert again", FIRST_STAMPED)),
            Verdict::Older { made_by } => Some(format!("Made by {} — convert again", made_by)),
        }
    }
}

fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.trim())
}

/// The rule, on the tags a render carries (None: it has none). A stage this
/// build does not know, or a newer version of one, does not count against a
/// render: it was made by a newer build.
pub fn verdict(tags: Option<&[(String, String)]>) -> Verdict {
    let Some(tags) = tags else { return Verdict::NoStamp };
    let Some(render) = tag(tags, KEY_RENDER).and_then(|v| v.parse::<u32>().ok()) else {
        return Verdict::NoStamp;
    };
    let older = || Verdict::Older {
        made_by: tag(tags, KEY_APP_VERSION).filter(|v| !v.is_empty()).unwrap_or("an older version").to_string(),
    };
    if render < AURA_RENDER {
        return older();
    }
    for item in tag(tags, KEY_STAGES).unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (token, version) = item.split_once(':').unwrap_or((item, "0"));
        let version = version.trim().parse::<u32>().unwrap_or(0);
        if stage_version(token.trim()).is_some_and(|now| version < now) {
            return older();
        }
    }
    Verdict::Valid
}

/// The verdict on the file at `path`; a file that cannot be read is no
/// render to play.
pub fn check(path: &Path) -> Verdict {
    match read_render_stamp(path) {
        Ok(tags) => verdict(tags.as_deref()),
        Err(_) => Verdict::NoStamp,
    }
}

/// True when the player may play the render at `path` in place of its source.
pub fn playable(path: &Path) -> bool {
    check(path) == Verdict::Valid
}

/// A metadata block or tag larger than this is not one of ours.
const MAX_META: u64 = 16 << 20;

/// The key/value tags of a FLAC (VORBIS_COMMENT) or a DSF (ID3v2 `TXXX`)
/// from its headers only. Ok(None): a file of either kind without such tags,
/// or of another kind. Err: a header that is cut short or broken.
pub fn read_render_stamp(path: &Path) -> Result<Option<Vec<(String, String)>>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_err() {
        return Ok(None);
    }
    match &magic {
        b"fLaC" => read_flac_tags(&mut f, len),
        b"DSD " => read_dsf_tags(&mut f, len),
        _ => Ok(None),
    }
}

fn read_flac_tags<R: Read + Seek>(f: &mut R, len: u64) -> Result<Option<Vec<(String, String)>>, String> {
    let mut at = 4u64;
    loop {
        let mut head = [0u8; 4];
        f.read_exact(&mut head).map_err(|_| "FLAC: metadata cut short".to_string())?;
        let last = head[0] & 0x80 != 0;
        let kind = head[0] & 0x7F;
        let size = u32::from_be_bytes([0, head[1], head[2], head[3]]) as u64;
        at += 4;
        if kind == 127 || at + size > len {
            return Err("FLAC: broken metadata block header".to_string());
        }
        if kind == 4 {
            if size > MAX_META {
                return Err("FLAC: VORBIS_COMMENT too large".to_string());
            }
            let mut body = vec![0u8; size as usize];
            f.read_exact(&mut body).map_err(|_| "FLAC: VORBIS_COMMENT cut short".to_string())?;
            return parse_vorbis_comment(&body).map(Some);
        }
        if last {
            return Ok(None);
        }
        f.seek(SeekFrom::Current(size as i64)).map_err(|e| e.to_string())?;
        at += size;
    }
}

fn parse_vorbis_comment(b: &[u8]) -> Result<Vec<(String, String)>, String> {
    let bad = || "FLAC: broken VORBIS_COMMENT".to_string();
    let u32_at = |i: usize| -> Option<usize> {
        b.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let vlen = u32_at(0).ok_or_else(bad)?;
    let mut i = 4usize.checked_add(vlen).ok_or_else(bad)?;
    let count = u32_at(i).ok_or_else(bad)?;
    i += 4;
    let mut out = Vec::new();
    for _ in 0..count {
        let n = u32_at(i).ok_or_else(bad)?;
        i += 4;
        let entry = b.get(i..i.checked_add(n).ok_or_else(bad)?).ok_or_else(bad)?;
        i += n;
        let entry = String::from_utf8_lossy(entry);
        if let Some((k, v)) = entry.split_once('=') {
            out.push((k.to_string(), v.to_string()));
        }
    }
    Ok(out)
}

fn read_dsf_tags<R: Read + Seek>(f: &mut R, len: u64) -> Result<Option<Vec<(String, String)>>, String> {
    // The DSD chunk: id, its size (28), the file's size, the metadata's offset.
    let mut head = [0u8; 24];
    f.read_exact(&mut head).map_err(|_| "DSF: header cut short".to_string())?;
    let meta_at = u64::from_le_bytes(head[16..24].try_into().unwrap());
    if meta_at == 0 {
        return Ok(None);
    }
    if meta_at + 10 > len {
        return Err("DSF: metadata offset past the end".to_string());
    }
    f.seek(SeekFrom::Start(meta_at)).map_err(|e| e.to_string())?;
    let mut id3 = [0u8; 10];
    f.read_exact(&mut id3).map_err(|_| "DSF: ID3v2 header cut short".to_string())?;
    if &id3[0..3] != b"ID3" || id3[6..10].iter().any(|b| b & 0x80 != 0) {
        return Err("DSF: broken ID3v2 header".to_string());
    }
    let major = id3[3];
    let size = syncsafe(&id3[6..10]) as u64;
    if size > MAX_META || meta_at + 10 + size > len {
        return Err("DSF: ID3v2 tag past the end".to_string());
    }
    let mut body = vec![0u8; size as usize];
    f.read_exact(&mut body).map_err(|_| "DSF: ID3v2 tag cut short".to_string())?;
    Ok(Some(parse_id3_txxx(&body, major)))
}

fn syncsafe(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |a, x| (a << 7) | (*x as u32 & 0x7F))
}

/// The `TXXX` frames of an ID3v2.3/2.4 tag body as (description, value).
fn parse_id3_txxx(b: &[u8], major: u8) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 10 <= b.len() && b[i] != 0 {
        let id = &b[i..i + 4];
        let size = if major >= 4 { syncsafe(&b[i + 4..i + 8]) } else { u32::from_be_bytes(b[i + 4..i + 8].try_into().unwrap()) }
            as usize;
        let Some(body) = b.get(i + 10..i + 10 + size) else { break };
        i += 10 + size;
        if id != b"TXXX" || body.is_empty() {
            continue;
        }
        let texts = split_text(body[0], &body[1..]);
        if texts.len() >= 2 {
            out.push((texts[0].clone(), texts[1].clone()));
        }
    }
    out
}

/// The NUL-separated strings of an ID3v2 text body in its encoding.
fn split_text(enc: u8, b: &[u8]) -> Vec<String> {
    if enc == 1 || enc == 2 {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| if enc == 2 { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
            .collect();
        let mut out = Vec::new();
        for part in units.split(|u| *u == 0) {
            // A BOM per string; FE FF read as little-endian is 0xFFFE.
            let (big, part) = match part.first() {
                Some(0xFEFF) => (false, &part[1..]),
                Some(0xFFFE) => (true, &part[1..]),
                _ => (false, part),
            };
            let part: Vec<u16> = if big && enc == 1 { part.iter().map(|u| u.swap_bytes()).collect() } else { part.to_vec() };
            out.push(String::from_utf16_lossy(&part));
        }
        out
    } else {
        b.split(|x| *x == 0)
            .map(|p| if enc == 3 { String::from_utf8_lossy(p).into_owned() } else { p.iter().map(|c| *c as char).collect() })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_tags() -> Vec<(String, String)> {
        stamp_tags(&["ISP", "DC"], false, Some("sig"))
    }

    #[test]
    fn every_stage_has_a_version() {
        // The table of stage versions stays whole: every token the chain can
        // carry, the resampler and the modulator have one.
        for t in crate::audio::converter::dsp::lab::CHAIN_ORDER {
            assert!(stage_version(t).is_some(), "no version for stage {t}");
        }
        for t in [STAGE_FIR, STAGE_DSD] {
            assert!(stage_version(t).is_some(), "no version for stage {t}");
        }
        assert!(AURA_RENDER >= 1);
    }

    #[test]
    fn the_stamp_names_the_version_the_generation_and_the_stages() {
        let t = now_tags();
        assert_eq!(t[0], (KEY_APP_VERSION.to_string(), APP_VERSION.to_string()));
        assert_eq!(t[1], (KEY_RENDER.to_string(), AURA_RENDER.to_string()));
        // FIR first, then the chain in its order (DC before ISP).
        assert_eq!(t[2], (KEY_STAGES.to_string(), "FIR:1,DC:1,ISP:1".to_string()));
        assert_eq!(t[3].0, KEY_SETTINGS);
        assert_eq!(t[3].1.len(), 64);
        let dsd = stamp_tags(&[], true, None);
        assert_eq!(dsd.len(), 3);
        assert_eq!(dsd[2].1, "FIR:1,DSD:1");
        assert_eq!(vendor(), format!("AuraEngine {}", APP_VERSION));
    }

    #[test]
    fn the_rule_takes_generation_and_stages_not_the_app_version() {
        assert_eq!(verdict(Some(&now_tags())), Verdict::Valid);
        assert_eq!(verdict(None), Verdict::NoStamp);
        assert_eq!(verdict(Some(&[("AURAENGINE_LAB".into(), "1".into())])), Verdict::NoStamp);
        // Another app version with the same sound: still valid.
        let mut t = now_tags();
        t[0].1 = "0.0.1".into();
        assert_eq!(verdict(Some(&t)), Verdict::Valid);
        // An older generation.
        let mut t = now_tags();
        t[1].1 = (AURA_RENDER - 1).to_string();
        assert_eq!(verdict(Some(&t)), Verdict::Older { made_by: APP_VERSION.into() });
        // A 1.7.x render (generation 1) is not current from 1.8.0 on.
        let mut t = now_tags();
        t[0].1 = "1.7.1".into();
        t[1].1 = "1".into();
        assert_eq!(verdict(Some(&t)), Verdict::Older { made_by: "1.7.1".into() });
        // An older version of a stage that ran.
        let mut t = now_tags();
        t[2].1 = "FIR:1,DC:0,ISP:1".into();
        t[0].1 = "1.6.9".into();
        let v = verdict(Some(&t));
        assert_eq!(v, Verdict::Older { made_by: "1.6.9".into() });
        assert_eq!(v.tip().unwrap(), "Made by 1.6.9 — convert again");
        assert_eq!(Verdict::NoStamp.tip().unwrap(), "Made before 1.7.0 — convert again");
        // A newer build: a newer stage version or a stage this build lacks.
        let mut t = now_tags();
        t[1].1 = (AURA_RENDER + 1).to_string();
        t[2].1 = "FIR:9,NEW:3".into();
        assert_eq!(verdict(Some(&t)), Verdict::Valid);
    }

    fn flac_with(blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut b = b"fLaC".to_vec();
        for (n, (kind, body)) in blocks.iter().enumerate() {
            let last = if n + 1 == blocks.len() { 0x80 } else { 0 };
            b.push(last | kind);
            b.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            b.extend_from_slice(body);
        }
        // A frame's sync code after the metadata: never read.
        b.extend_from_slice(&[0xFF, 0xF8, 0, 0]);
        b
    }

    fn vorbis(vendor: &str, tags: &[(String, String)]) -> Vec<u8> {
        let mut b = (vendor.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(vendor.as_bytes());
        b.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for (k, v) in tags {
            let e = format!("{k}={v}");
            b.extend_from_slice(&(e.len() as u32).to_le_bytes());
            b.extend_from_slice(e.as_bytes());
        }
        b
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn reads_the_stamp_from_the_headers_alone() {
        let dir = tempfile::tempdir().unwrap();
        let tags = now_tags();
        // STREAMINFO, PADDING, then VORBIS_COMMENT.
        let ours = write(&dir, "a.flac", &flac_with(&[(0, vec![0; 34]), (1, vec![0; 100]), (4, vorbis("AuraEngine 1.7.0", &tags))]));
        assert_eq!(read_render_stamp(&ours).unwrap().unwrap(), tags);
        assert!(playable(&ours));
        // Our render from before the stamp: STREAMINFO only.
        let old = write(&dir, "old.flac", &flac_with(&[(0, vec![0; 34])]));
        assert_eq!(read_render_stamp(&old).unwrap(), None);
        assert_eq!(check(&old), Verdict::NoStamp);
        // An old lab render: tags, no stamp.
        let lab = write(&dir, "lab.flac", &flac_with(&[(0, vec![0; 34]), (4, vorbis("AuraEngine", &[("AURAENGINE_LAB".into(), "1".into())]))]));
        assert_eq!(check(&lab), Verdict::NoStamp);
        // A foreign FLAC with ordinary tags.
        let foreign = write(&dir, "f.flac", &flac_with(&[(0, vec![0; 34]), (4, vorbis("reference libFLAC 1.4.3", &[("TITLE".into(), "x".into())]))]));
        assert_eq!(read_render_stamp(&foreign).unwrap().unwrap(), vec![("TITLE".to_string(), "x".to_string())]);
        assert!(!playable(&foreign));
        // A broken block header: a size past the end of the file.
        let mut broken = flac_with(&[(0, vec![0; 34])]);
        broken[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        let broken = write(&dir, "b.flac", &broken);
        assert!(read_render_stamp(&broken).is_err());
        assert!(!playable(&broken));
        // A cut VORBIS_COMMENT body.
        let mut cut = vorbis("v", &tags);
        cut.truncate(cut.len() - 3);
        let cut = write(&dir, "c.flac", &flac_with(&[(4, cut)]));
        assert!(read_render_stamp(&cut).is_err());
        // Not a FLAC or DSF; no file.
        let mp3 = write(&dir, "s.mp3", b"ID3\x03\0\0\0\0\0\0");
        assert_eq!(read_render_stamp(&mp3).unwrap(), None);
        assert!(!playable(&dir.path().join("none.flac")));
    }

    #[test]
    fn reads_the_stamp_of_a_dsf() {
        let dir = tempfile::tempdir().unwrap();
        let tags = stamp_tags(&["XTC"], true, None);
        let id3 = crate::audio::dsd::dsf_write::id3v2_tag(&[("TPE1", "Ärtist"), ("TIT2", "Тitle")], &tags);
        let mut b = b"DSD ".to_vec();
        b.extend_from_slice(&28u64.to_le_bytes());
        let total = 28 + 64 + id3.len() as u64;
        b.extend_from_slice(&total.to_le_bytes());
        b.extend_from_slice(&(28u64 + 64).to_le_bytes());
        b.extend_from_slice(&[0u8; 64]); // stands for the fmt and data chunks
        b.extend_from_slice(&id3);
        let p = write(&dir, "a.dsf", &b);
        assert_eq!(read_render_stamp(&p).unwrap().unwrap(), tags);
        assert!(playable(&p));
        // No metadata chunk: made before the stamp.
        let mut none = b[..28 + 64].to_vec();
        none[20..28].copy_from_slice(&0u64.to_le_bytes());
        let none = write(&dir, "n.dsf", &none);
        assert_eq!(check(&none), Verdict::NoStamp);
        // A tag size past the end.
        let mut cut = b.clone();
        cut.truncate(b.len() - 5);
        let cut = write(&dir, "c.dsf", &cut);
        assert!(read_render_stamp(&cut).is_err());
    }

    #[test]
    fn reads_utf8_and_latin1_txxx() {
        // ID3v2.4 with syncsafe frame sizes, UTF-8 and Latin-1 bodies.
        let mut frames = Vec::new();
        for (enc, body) in [(3u8, "AURA_RENDER\01".as_bytes().to_vec()), (0u8, b"K\0v\xe9".to_vec())] {
            let mut f = vec![enc];
            f.extend_from_slice(&body);
            frames.extend_from_slice(b"TXXX");
            frames.extend_from_slice(&[0, 0, 0, f.len() as u8]);
            frames.extend_from_slice(&[0, 0]);
            frames.extend_from_slice(&f);
        }
        frames.extend_from_slice(&[0; 8]); // padding
        let t = parse_id3_txxx(&frames, 4);
        assert_eq!(t, vec![("AURA_RENDER".to_string(), "1".to_string()), ("K".to_string(), "vé".to_string())]);
    }
}
