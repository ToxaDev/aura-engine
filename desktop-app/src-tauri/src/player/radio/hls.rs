//! HLS (.m3u8): a stream served as a playlist of short segments instead of
//! one endless connection. The network thread plays the client: the master
//! playlist's best variant the player decodes, the live media playlist read
//! again as it grows, its segments fetched in order — and what the decoder
//! reads is what an Icecast connection brings, the elementary audio (ADTS or
//! MPEG audio) in a `Pipe`: from MPEG-TS segments (`ts`), from packed audio
//! (ADTS or MP3 after an ID3 tag) or from fragmented MP4 (`mp4`).
//!
//! One pipe goes on for as long as the segments follow each other: a segment
//! that fails is fetched again by its number while the playlist still has
//! it, and the decoder never sees a break. A segment the playlist dropped
//! before it could be fetched, a discontinuity, numbers that went back (the
//! server started again): the pipe ends and the next one is a known gap
//! (`Connected::gap`: met with fades, not looked for in what was received).
//! A playlist that is refused (403, 404, 410 — a token in its address ran
//! out) is looked up again from the station's address: a new connection,
//! placed by the joiner like an Icecast reconnect.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::mp4::{self, Mp4Error};
use super::net::{Connected, Pipe};
use super::ts::{Es, TsDemux, TsError};
use super::RadioShared;

/// Why an fMP4 stream cannot go on, as the session ends.
fn mp4_end(e: Mp4Error) -> End {
    match e {
        Mp4Error::Encrypted(s) => End::Failed(super::ERR_ENCRYPTED, s),
        Mp4Error::Unsupported(s) => End::Failed(super::ERR_FORMAT, s),
    }
}

/// No byte for this long: the request is taken as broken.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A playlist larger than this is not a playlist (a long window of short
/// segments runs to a few hundred kB).
pub const PLAYLIST_MAX: usize = 4 << 20;
/// A segment larger than this is not an audio segment.
const SEGMENT_MAX: usize = 64 << 20;
/// Tries of one segment before the playlist is read again (which tells
/// whether it can still be had).
const SEGMENT_TRIES: usize = 3;
/// Segments from the end of a live playlist the stream starts at: what is
/// in hand at once (the spec's three target durations).
const START_BACK: usize = 3;
/// Pauses after failures in a row (the network's own, net.rs).
const BACKOFF_S: [u64; 5] = [1, 2, 4, 8, 10];
/// Failed tries in a row before a station that never played is given up.
const FIRST_TRIES: usize = 3;
/// A playlist that keeps lagging behind what was fetched for this many
/// readings has started again with lower numbers.
const BEHIND_READS: u32 = 4;
/// The client's pauses, in hundredths of their length: 100, but for the
/// tests that play a scripted server on the loopback (a live server is
/// always given the full pauses, the harness that plays one included).
static PAUSE_PCT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);

/// What the HLS side reports (`info.hls` in the status).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HlsInfo {
    /// The variant played (the master playlist's BANDWIDTH, bit/s, and CODECS).
    pub bandwidth: Option<u64>,
    pub codecs: String,
    /// "TS" or "packed audio".
    pub container: String,
    pub target_s: f64,
    /// The number of the next segment.
    pub seq: u64,
    pub segments: u64,
    /// The audio they brought, seconds (their EXTINF).
    pub seconds: f64,
    /// Segment fetches that failed and were tried again.
    pub retries: u64,
    /// Segments the playlist dropped before they could be fetched.
    pub lost: u64,
    pub discontinuities: u64,
    /// Playlists whose numbers went back (a server started again).
    pub rollbacks: u64,
    /// Looked up again from the station's address.
    pub restarts: u64,
    pub playlist_errors: u64,
    /// Transport stream packets missing (continuity counters).
    pub lost_packets: u64,
}

// ── Playlists ───────────────────────────────────────────────────────────────

/// A variant of a master playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub codecs: String,
    /// Its audio group (EXT-X-MEDIA TYPE=AUDIO), when the audio comes apart.
    pub audio: Option<String>,
}

/// An audio rendition of a master playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    pub group: String,
    pub uri: String,
    pub default: bool,
}

/// One segment of a media playlist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Segment {
    pub seq: u64,
    pub uri: String,
    pub duration_s: f64,
    /// "Artist - Title" from the EXTINF line's attributes.
    pub title: Option<String>,
    /// An EXT-X-DISCONTINUITY stands before it.
    pub discontinuity: bool,
    /// Bytes `start .. start + len` of its resource (EXT-X-BYTERANGE).
    pub range: Option<(u64, u64)>,
    /// The fMP4 initialization section in force (EXT-X-MAP): address, range.
    pub map: Option<(String, Option<(u64, u64)>)>,
    /// The encryption in force (EXT-X-KEY with a METHOD other than NONE).
    pub key: Option<String>,
}

/// A media playlist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Media {
    pub target_s: f64,
    /// EXT-X-MEDIA-SEQUENCE: the first segment's number.
    pub seq: u64,
    pub segments: Vec<Segment>,
    pub endlist: bool,
}

impl Media {
    /// The last segment's number (None: no segments).
    fn last_seq(&self) -> Option<u64> {
        self.segments.last().map(|s| s.seq)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master { variants: Vec<Variant>, audio: Vec<Rendition> },
    Media(Media),
}

/// The attributes of a tag: `KEY=value,KEY="a, quoted value"`.
fn attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_ascii_uppercase();
        rest = &rest[eq + 1..];
        let value = if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            let v = r[..end].to_string();
            rest = &r[(end + 1).min(r.len())..];
            v
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let v = rest[..end].trim().to_string();
            rest = &rest[end..];
            v
        };
        rest = rest.trim_start().trim_start_matches(',').trim_start();
        out.push((key, value));
    }
    out
}

fn attr<'a>(a: &'a [(String, String)], key: &str) -> Option<&'a str> {
    a.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `n[@o]` of a byte range; `o` defaults to `follow` (where the last one ended).
fn byte_range(s: &str, follow: Option<u64>) -> Option<(u64, u64)> {
    let (n, o) = match s.trim().split_once('@') {
        Some((n, o)) => (n.trim().parse().ok()?, o.trim().parse().ok()?),
        None => (s.trim().parse().ok()?, follow.unwrap_or(0)),
    };
    Some((o, n))
}

/// "Artist - Title" from attributes some servers put on EXTINF
/// (`#EXTINF:10,title="…",artist="…"`); plain text there is ignored —
/// for many it is "no desc".
fn extinf_title(rest: &str) -> Option<String> {
    if !rest.contains('=') {
        return None;
    }
    let a = attrs(rest);
    let get = |k: &str| attr(&a, k).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    join_title(get("ARTIST"), get("TITLE"))
}

fn join_title(artist: Option<String>, title: Option<String>) -> Option<String> {
    match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{} - {}", a, t)),
        (None, Some(t)) => Some(t),
        (Some(a), None) => Some(a),
        (None, None) => None,
    }
}

/// Read a playlist: a master playlist (variants, audio renditions) or a
/// media playlist (segments numbered from EXT-X-MEDIA-SEQUENCE).
pub fn parse(text: &str) -> Result<Playlist, String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    if !lines.peek().is_some_and(|l| l.starts_with("#EXTM3U")) {
        return Err("not an HLS playlist".into());
    }
    let lines: Vec<&str> = lines.collect();
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let (mut variants, mut audio) = (Vec::new(), Vec::new());
        let mut pending: Option<Vec<(String, String)>> = None;
        for l in &lines {
            if let Some(a) = l.strip_prefix("#EXT-X-STREAM-INF:") {
                pending = Some(attrs(a));
            } else if let Some(a) = l.strip_prefix("#EXT-X-MEDIA:") {
                let a = attrs(a);
                if attr(&a, "TYPE") == Some("AUDIO") {
                    if let (Some(g), Some(u)) = (attr(&a, "GROUP-ID"), attr(&a, "URI")) {
                        audio.push(Rendition { group: g.to_string(), uri: u.to_string(), default: attr(&a, "DEFAULT") == Some("YES") });
                    }
                }
            } else if !l.starts_with('#') {
                if let Some(a) = pending.take() {
                    variants.push(Variant {
                        uri: l.to_string(),
                        bandwidth: attr(&a, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                        codecs: attr(&a, "CODECS").unwrap_or("").to_string(),
                        audio: attr(&a, "AUDIO").map(str::to_string),
                    });
                }
            }
        }
        return Ok(Playlist::Master { variants, audio });
    }
    let mut m = Media::default();
    let mut seg = Segment::default();
    let mut follow: Option<u64> = None;
    let mut map: Option<(String, Option<(u64, u64)>)> = None;
    let mut key: Option<String> = None;
    let mut count = 0u64;
    for l in &lines {
        if let Some(v) = l.strip_prefix("#EXT-X-TARGETDURATION:") {
            m.target_s = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            m.seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("#EXTINF:") {
            let (d, rest) = v.split_once(',').unwrap_or((v, ""));
            seg.duration_s = d.split_whitespace().next().and_then(|d| d.parse().ok()).unwrap_or(0.0);
            seg.title = extinf_title(rest);
        } else if *l == "#EXT-X-DISCONTINUITY" {
            seg.discontinuity = true;
        } else if let Some(v) = l.strip_prefix("#EXT-X-BYTERANGE:") {
            seg.range = byte_range(v, follow);
        } else if let Some(v) = l.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(v);
            map = attr(&a, "URI").map(|u| (u.to_string(), attr(&a, "BYTERANGE").and_then(|r| byte_range(r, None))));
        } else if let Some(v) = l.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(v);
            key = attr(&a, "METHOD").filter(|m| !m.eq_ignore_ascii_case("NONE")).map(str::to_string);
        } else if l.starts_with("#EXT-X-ENDLIST") {
            m.endlist = true;
        } else if !l.starts_with('#') {
            seg.seq = m.seq + count;
            seg.uri = l.to_string();
            seg.map = map.clone();
            seg.key = key.clone();
            follow = seg.range.map(|(o, n)| o + n);
            m.segments.push(std::mem::take(&mut seg));
            count += 1;
        }
    }
    Ok(Playlist::Media(m))
}

/// What the player makes of a CODECS list: the best audio it decodes in it
/// (3 AAC-LC, 2 MP3, 1 HE-AAC v1/v2, 0 none) and whether video comes along.
/// An empty list says nothing: taken as playable, ranked last.
fn codec_rank(codecs: &str) -> (u8, bool) {
    if codecs.trim().is_empty() {
        return (1, false);
    }
    let (mut rank, mut video) = (0u8, false);
    for c in codecs.split(',').map(|c| c.trim().to_ascii_lowercase()) {
        let r = match c.as_str() {
            "mp4a.40.2" => 3,
            "mp4a.40.34" | "mp4a.69" | "mp4a.6b" | "mp3" => 2,
            "mp4a.40.5" | "mp4a.40.29" => 1,
            _ => {
                if ["avc", "hvc", "hev", "av01", "vp0", "mp4v", "dvh"].iter().any(|v| c.starts_with(v)) {
                    video = true;
                }
                0
            }
        };
        rank = rank.max(r);
    }
    (rank, video)
}

/// The variant to play: the highest bandwidth among those whose audio the
/// player decodes, AAC-LC before MP3 before HE-AAC at the same bandwidth.
/// One that brings video along only when there is no other: the one that
/// brings the least.
pub fn choose(variants: &[Variant]) -> Option<&Variant> {
    let playable: Vec<(&Variant, u8, bool)> = variants
        .iter()
        .map(|v| {
            let (r, video) = codec_rank(&v.codecs);
            (v, r, video)
        })
        .filter(|(_, r, _)| *r > 0)
        .collect();
    let audio_only = playable.iter().filter(|(_, _, video)| !video).max_by_key(|(v, r, _)| (v.bandwidth, *r)).map(|(v, ..)| *v);
    audio_only.or_else(|| playable.iter().min_by_key(|(v, ..)| v.bandwidth).map(|(v, ..)| *v))
}

/// Where a variant's audio is: its audio group's rendition (the default one,
/// else the first) when the group has one with an address, else the variant.
fn audio_uri<'a>(v: &'a Variant, audio: &'a [Rendition]) -> &'a str {
    let Some(g) = v.audio.as_deref() else { return &v.uri };
    let group: Vec<&'a Rendition> = audio.iter().filter(|r| r.group == g).collect();
    group.iter().copied().find(|r| r.default).or_else(|| group.first().copied()).map_or(&v.uri, |r| &r.uri)
}

/// An address in a playlist, from the playlist's own (after redirects).
pub fn resolve(base: &str, uri: &str) -> Result<String, String> {
    let b = reqwest::Url::parse(base).map_err(|e| format!("playlist address {}: {}", base, e))?;
    b.join(uri.trim()).map(|u| u.to_string()).map_err(|e| format!("segment address {}: {}", uri, e))
}

/// Where the stream goes on in a playlist just read, `next` being the number
/// of the segment it needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Next {
    /// From this index of the playlist on (possibly past its end: nothing new).
    From(usize),
    /// The segment needed is gone: from this index, after a gap.
    Lost(usize),
    /// The numbers went back (the server started again): from this index,
    /// after a gap.
    Rollback(usize),
}

/// Where the stream goes on in `m`: `behind` counts the readings in a row
/// that lag behind what was fetched (a cache's old copy, or a server that
/// started again — told apart by how long it lasts and by how far back).
pub fn next_in(m: &Media, next: u64, behind: &mut u32) -> Next {
    let (Some(first), Some(last)) = (m.segments.first().map(|s| s.seq), m.last_seq()) else {
        return Next::From(0);
    };
    if next < first {
        *behind = 0;
        return Next::Lost(0);
    }
    if next <= last + 1 {
        *behind = 0;
        return Next::From((next - first) as usize);
    }
    // The playlist ends before the segment needed.
    *behind += 1;
    let far = next - (last + 1) > m.segments.len() as u64 + 2;
    if far || *behind >= BEHIND_READS {
        *behind = 0;
        return Next::Rollback(0);
    }
    Next::From(m.segments.len())
}

// ── ID3 ─────────────────────────────────────────────────────────────────────

fn syncsafe(b: &[u8]) -> usize {
    b.iter().take(4).fold(0usize, |a, &x| (a << 7) | usize::from(x & 0x7F))
}

/// The text of an ID3 text frame (its encoding byte first), up to its first NUL.
fn id3_text(b: &[u8]) -> Option<String> {
    let (&enc, t) = b.split_first()?;
    let s = match enc {
        0 => t.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect::<String>(),
        1 | 2 => {
            let (be, body) = match t {
                [0xFE, 0xFF, rest @ ..] => (true, rest),
                [0xFF, 0xFE, rest @ ..] => (false, rest),
                _ => (enc == 2, t),
            };
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => {
            let end = t.iter().position(|&c| c == 0).unwrap_or(t.len());
            String::from_utf8_lossy(&t[..end]).into_owned()
        }
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// "Artist - Title" out of one ID3v2 tag's frames (`body`: after its header).
fn id3_frames_title(body: &[u8], ver: u8) -> Option<String> {
    let (mut artist, mut title) = (None, None);
    let (id_len, head) = if ver == 2 { (3, 6) } else { (4, 10) };
    let mut at = 0;
    while at + head <= body.len() {
        let id = &body[at..at + id_len];
        if id[0] == 0 {
            break;
        }
        let size = match ver {
            2 => (usize::from(body[at + 3]) << 16) | (usize::from(body[at + 4]) << 8) | usize::from(body[at + 5]),
            3 => u32::from_be_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]) as usize,
            _ => syncsafe(&body[at + 4..at + 8]),
        };
        let data = &body[(at + head).min(body.len())..(at + head + size).min(body.len())];
        match id {
            b"TIT2" | b"TT2" => title = id3_text(data),
            b"TPE1" | b"TP1" => artist = id3_text(data),
            _ => {}
        }
        at += head + size;
    }
    join_title(artist, title)
}

/// The ID3v2 tags at the start of `b`: the title they carry and the bytes
/// they take (0 when `b` does not start with one).
pub fn id3_head(b: &[u8]) -> (Option<String>, usize) {
    let (mut title, mut at) = (None, 0);
    while b.len() >= at + 10 && &b[at..at + 3] == b"ID3" {
        let (ver, flags) = (b[at + 3], b[at + 5]);
        let size = syncsafe(&b[at + 6..at + 10]);
        let end = (at + 10 + size + if flags & 0x10 != 0 { 10 } else { 0 }).min(b.len());
        let mut body = &b[(at + 10).min(end)..(at + 10 + size).min(b.len())];
        if flags & 0x40 != 0 && body.len() >= 4 {
            // An extended header: its size (v2.4 counts itself, v2.3 does not).
            let n = if ver >= 4 { syncsafe(&body[..4]) } else { 4 + u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize };
            body = &body[n.min(body.len())..];
        }
        if let Some(t) = id3_frames_title(body, ver) {
            title = Some(t);
        }
        at = end;
    }
    (title, at)
}

/// What a segment's bytes are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Ts,
    /// Packed audio: this elementary stream after `usize` bytes of ID3 tags.
    Packed(Es, usize),
    /// Fragmented MP4 (CMAF).
    Mp4,
    Unknown,
}

pub fn segment_kind(b: &[u8]) -> Kind {
    if !b.is_empty() && b[0] == 0x47 && (b.len() <= 188 || b[188] == 0x47) {
        return Kind::Ts;
    }
    if b.len() >= 8 && matches!(&b[4..8], b"ftyp" | b"styp" | b"moof" | b"sidx" | b"moov" | b"emsg" | b"prft") {
        return Kind::Mp4;
    }
    let (_, at) = id3_head(b);
    match b.get(at..at + 2) {
        Some([0xFF, x]) if x & 0xF6 == 0xF0 => Kind::Packed(Es::Adts, at),
        Some([0xFF, x]) if x & 0xE0 == 0xE0 => Kind::Packed(Es::Mpeg, at),
        _ => Kind::Unknown,
    }
}

// ── The client ──────────────────────────────────────────────────────────────

/// How an HLS session ended.
pub enum End {
    Stopped,
    /// It cannot go on: the kind (ERR_*) and why.
    Failed(&'static str, String),
    /// A finished playlist (EXT-X-ENDLIST) played to its end.
    Ended,
}

enum Fetch {
    Http(u16),
    Net(String),
}

impl std::fmt::Display for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fetch::Http(c) => write!(f, "HTTP {}", c),
            Fetch::Net(e) => f.write_str(e),
        }
    }
}

/// GET `url` (bytes `range`, when given): the body and the address it came
/// from after redirects.
async fn fetch(client: &reqwest::Client, url: &str, range: Option<(u64, u64)>, max: usize) -> Result<(Vec<u8>, String), Fetch> {
    let mut req = client.get(url);
    if let Some((o, n)) = range {
        req = req.header("Range", format!("bytes={}-{}", o, o + n.max(1) - 1));
    }
    let mut resp = tokio::time::timeout(READ_TIMEOUT, req.send())
        .await
        .map_err(|_| Fetch::Net("no answer".into()))?
        .map_err(|e| Fetch::Net(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(Fetch::Http(resp.status().as_u16()));
    }
    let at = resp.url().to_string();
    read_body(&mut resp, max).await.map(|b| (b, at))
}

async fn read_body(resp: &mut reqwest::Response, max: usize) -> Result<Vec<u8>, Fetch> {
    let mut body = Vec::new();
    while let Some(c) = tokio::time::timeout(READ_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| Fetch::Net(format!("no data for {} s", READ_TIMEOUT.as_secs())))?
        .map_err(|e| Fetch::Net(e.to_string()))?
    {
        body.extend_from_slice(&c);
        if body.len() > max {
            return Err(Fetch::Net("too large".into()));
        }
    }
    Ok(body)
}

/// The decoder's side of the stream as the client makes it.
struct Out {
    pipe: Arc<Pipe>,
    es: Es,
    dump: Option<std::fs::File>,
}

struct Client<'a> {
    shared: &'a Arc<RadioShared>,
    to_decoder: &'a Sender<Connected>,
    n: &'a mut u32,
    out: Option<Out>,
    /// The transport stream's demultiplexer, across the segments of a pipe.
    demux: Option<TsDemux>,
    /// The fMP4 initialization section in force: its address and range.
    init: Option<((String, Option<(u64, u64)>), mp4::Init)>,
    /// The next pipe follows a known break.
    gap_next: bool,
    next: Option<u64>,
    behind: u32,
    media_url: String,
    info: HlsInfo,
    t_try: Instant,
}

impl Client<'_> {
    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    async fn pause(&self, secs: f64) {
        let scale = f64::from(PAUSE_PCT.load(Ordering::Relaxed)) / 100.0;
        let until = Instant::now() + Duration::from_secs_f64(secs.max(0.0) * scale);
        loop {
            let now = Instant::now();
            if now >= until || self.stopped() {
                return;
            }
            tokio::time::sleep((until - now).min(Duration::from_millis(100))).await;
        }
    }

    fn publish(&self) {
        self.shared.info.lock().unwrap().hls = Some(self.info.clone());
    }

    /// The pipe ends here; `gap`: the next one follows a known break.
    fn close(&mut self, gap: bool) {
        if let Some(o) = self.out.take() {
            o.pipe.finish();
            self.shared.info.lock().unwrap().connected = false;
        }
        self.demux = None;
        self.gap_next |= gap;
    }

    /// From the station's address again: the pipe ends, and the next starts a
    /// few segments back from the edge, as a new Icecast connection does —
    /// the joiner finds what it repeats and goes on without a seam, or meets
    /// it with a gap when audio was lost meanwhile.
    fn restart(&mut self) {
        self.close(false);
        self.next = None;
        self.behind = 0;
        self.info.restarts += 1;
    }

    /// The pipe for elementary stream `es`: the one open, or a new one handed
    /// to the decoder (another stream kind ends the old one, after a gap).
    fn pipe_for(&mut self, es: Es, container: &str) -> Option<Arc<Pipe>> {
        if let Some(o) = &self.out {
            if o.es == es {
                return Some(o.pipe.clone());
            }
            self.close(true);
        }
        *self.n += 1;
        let n = *self.n;
        let pipe = Pipe::new();
        let ct = es.content_type();
        self.info.container = container.to_string();
        {
            let mut i = self.shared.info.lock().unwrap();
            i.connected = true;
            i.connects = n;
            i.stream_url = self.media_url.clone();
            i.content_type = ct.to_string();
            i.connect_ms.push(self.t_try.elapsed().as_millis() as u64);
            if i.t_connected_ms.is_none() {
                i.t_connected_ms = Some(self.shared.t0.elapsed().as_millis() as u64);
            }
        }
        crate::aelog!(
            "[RADIO] HLS connection #{}: {} {} from segment {}{}",
            n,
            container,
            ct,
            self.next.unwrap_or(0),
            if self.gap_next { ", after a gap" } else { "" }
        );
        let gap = std::mem::take(&mut self.gap_next);
        let c = Connected { pipe: pipe.clone(), content_type: ct.to_string(), url: self.media_url.clone(), n, gap };
        if self.to_decoder.send(c).is_err() {
            return None;
        }
        self.out = Some(Out { pipe: pipe.clone(), es, dump: self.shared.dump_file(n, ct) });
        Some(pipe)
    }

    /// Hand a segment's audio to the decoder. Err: the stream cannot go on.
    fn take(&mut self, seg: &Segment, body: &[u8]) -> Result<(), End> {
        let mut title = seg.title.clone();
        let (es, audio, container): (Es, Vec<u8>, &str) = match segment_kind(body) {
            Kind::Ts => {
                let d = self.demux.get_or_insert_with(TsDemux::new);
                d.segment_starts();
                let mut audio = Vec::with_capacity(body.len());
                let mut kind = d.es();
                let mut tags: Vec<Vec<u8>> = Vec::new();
                let (lost_before, resynced_before) = (d.lost, d.resynced);
                let r = d.push(
                    body,
                    &mut |e, b| {
                        kind = Some(e);
                        audio.extend_from_slice(b);
                    },
                    &mut |tag| tags.push(tag.to_vec()),
                );
                // A segment's timed metadata ends with it.
                d.flush(&mut |tag| tags.push(tag.to_vec()));
                for tag in &tags {
                    if let (Some(t), _) = id3_head(tag) {
                        title = Some(t);
                    }
                }
                self.info.lost_packets += d.lost - lost_before;
                if d.resynced > resynced_before {
                    crate::aelog!("[RADIO] HLS segment {}: {} bytes skipped to find its packets", seg.seq, d.resynced - resynced_before);
                }
                match r {
                    Err(TsError::Encrypted(e)) => return Err(End::Failed(super::ERR_ENCRYPTED, e)),
                    Err(TsError::Unsupported(e)) => return Err(End::Failed(super::ERR_FORMAT, e)),
                    Ok(()) => {}
                }
                match kind {
                    Some(k) => (k, audio, "TS"),
                    // No audio in this segment yet (its tables only): nothing to hand on.
                    None => return Ok(()),
                }
            }
            Kind::Packed(es, at) => {
                if let (Some(t), _) = id3_head(body) {
                    title = Some(t);
                }
                (es, body[at..].to_vec(), "packed audio")
            }
            Kind::Mp4 => {
                // The playlist's initialization section (EXT-X-MAP), or the
                // segment's own movie box.
                let own;
                let init = match &self.init {
                    Some((_, i)) => i,
                    None => {
                        own = mp4::parse_init(body).map_err(mp4_end)?;
                        &own
                    }
                };
                let mut audio = Vec::with_capacity(body.len());
                mp4::samples(init, body, &mut audio).map_err(mp4_end)?;
                (init.es, audio, "fMP4")
            }
            Kind::Unknown => {
                return Err(End::Failed(super::ERR_FORMAT, "unsupported: HLS segments this player cannot read".into()))
            }
        };
        let Some(pipe) = self.pipe_for(es, container) else { return Err(End::Stopped) };
        if let Some(t) = title {
            self.shared.set_title(&t, "hls");
        }
        pipe.write(&audio);
        if let Some(f) = self.out.as_mut().and_then(|o| o.dump.as_mut()) {
            use std::io::Write;
            let _ = f.write_all(&audio);
        }
        let mut i = self.shared.info.lock().unwrap();
        i.bytes += audio.len() as u64;
        if i.t_first_byte_ms.is_none() {
            i.t_first_byte_ms = Some(self.shared.t0.elapsed().as_millis() as u64);
        }
        Ok(())
    }

    /// The media playlist's address from the station's (`first`: its answer
    /// and the address it came from, when they are in hand), and the media
    /// playlist when that is what it was.
    async fn variant(&mut self, client: &reqwest::Client, origin: &str, first: Option<(Vec<u8>, String)>) -> Result<(String, Option<Media>), (bool, String)> {
        let (body, at) = match first {
            Some(f) => f,
            None => fetch(client, origin, None, PLAYLIST_MAX).await.map_err(|e| (false, format!("playlist: {}", e)))?,
        };
        match parse(&String::from_utf8_lossy(&body)).map_err(|e| (true, e))? {
            Playlist::Media(m) => Ok((at, Some(m))),
            Playlist::Master { variants, audio } => {
                let v = choose(&variants).ok_or_else(|| {
                    let codecs: Vec<&str> = variants.iter().map(|v| v.codecs.as_str()).collect();
                    (true, format!("unsupported: no HLS variant this player decodes ({})", codecs.join("; ")))
                })?;
                let uri = resolve(&at, audio_uri(v, &audio)).map_err(|e| (true, e))?;
                self.info.bandwidth = Some(v.bandwidth);
                self.info.codecs = v.codecs.clone();
                crate::aelog!(
                    "[RADIO] HLS: {} variants; playing {} bit/s {}{}",
                    variants.len(),
                    v.bandwidth,
                    if v.codecs.is_empty() { "(codecs not given)" } else { &v.codecs },
                    if v.audio.is_some() && uri != resolve(&at, &v.uri).unwrap_or_default() { " (its audio group)" } else { "" }
                );
                Ok((uri, None))
            }
        }
    }

    /// The fMP4 initialization section `map` names, fetched once. Err(Ok):
    /// the stream cannot go on; Err(Err): it failed this time.
    async fn init_for(&mut self, client: &reqwest::Client, map: &(String, Option<(u64, u64)>)) -> Result<(), Result<End, String>> {
        let url = resolve(&self.media_url, &map.0).map_err(Err)?;
        let key = (url, map.1);
        if self.init.as_ref().is_some_and(|(k, _)| *k == key) {
            return Ok(());
        }
        let (b, _) = fetch(client, &key.0, map.1, SEGMENT_MAX).await.map_err(|e| Err(format!("initialization section: {}", e)))?;
        let init = mp4::parse_init(&b).map_err(|e| Ok(mp4_end(e)))?;
        if self.init.as_ref().is_some_and(|(_, old)| *old != init) {
            // Another configuration of the audio: a new connection, after a gap.
            self.close(true);
        }
        self.init = Some((key, init));
        Ok(())
    }

    /// One segment, tried a few times. Err: still failing (the playlist is
    /// read again to tell whether it can still be had).
    async fn segment(&mut self, client: &reqwest::Client, seg: &Segment) -> Result<Vec<u8>, String> {
        let url = resolve(&self.media_url, &seg.uri)?;
        let mut last = String::new();
        for k in 0..SEGMENT_TRIES {
            if self.stopped() {
                return Err("stopped".into());
            }
            match fetch(client, &url, seg.range, SEGMENT_MAX).await {
                Ok((b, _)) => return Ok(b),
                Err(e) => {
                    last = e.to_string();
                    self.info.retries += 1;
                    crate::aelog!("[RADIO] HLS segment {} failed ({}){}", seg.seq, last, if k + 1 < SEGMENT_TRIES { ": again" } else { "" });
                    if k + 1 < SEGMENT_TRIES {
                        self.pause(1.0).await;
                    }
                }
            }
        }
        Err(last)
    }
}

/// Whether a playlist's text is HLS (some servers send one with the type and
/// address of a plain .m3u).
pub fn is_hls_text(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U")
        && ["#EXT-X-TARGETDURATION", "#EXT-X-STREAM-INF", "#EXT-X-MEDIA-SEQUENCE"].iter().any(|t| text.contains(t))
}

/// Play an HLS stream from the station's address (`origin`); `first`: the
/// playlist it answered with and the address that came from after
/// redirects, when they are in hand. Runs until the session stops, the
/// stream cannot go on, or a finished playlist has played.
pub async fn run(
    shared: &Arc<RadioShared>,
    to_decoder: &Sender<Connected>,
    client: &reqwest::Client,
    first: Option<(Vec<u8>, String)>,
    origin: &str,
    n: &mut u32,
) -> End {
    let mut c = Client {
        shared,
        to_decoder,
        n,
        out: None,
        demux: None,
        init: None,
        gap_next: false,
        next: None,
        behind: 0,
        media_url: String::new(),
        info: HlsInfo::default(),
        t_try: Instant::now(),
    };
    let end = run_client(&mut c, client, first, origin).await;
    c.close(false);
    c.publish();
    end
}

async fn run_client(c: &mut Client<'_>, client: &reqwest::Client, first: Option<(Vec<u8>, String)>, origin: &str) -> End {
    let mut first = first;
    let mut failures = 0usize;
    let mut seg_failures = 0usize;
    let mut played = false;
    'variant: loop {
        if c.stopped() {
            return End::Stopped;
        }
        c.t_try = Instant::now();
        let mut media = match c.variant(client, origin, first.take()).await {
            Ok((url, m)) => {
                c.media_url = url;
                m
            }
            Err((true, e)) => return End::Failed(super::ERR_FORMAT, e),
            Err((false, e)) => {
                failures += 1;
                crate::aelog!("[RADIO] HLS: {}", e);
                c.shared.info.lock().unwrap().last_error = Some(e.clone());
                if !played && failures >= FIRST_TRIES {
                    return End::Failed(super::ERR_UNREACHABLE, e);
                }
                c.pause(BACKOFF_S[(failures - 1).min(BACKOFF_S.len() - 1)] as f64).await;
                continue 'variant;
            }
        };
        loop {
            if c.stopped() {
                return End::Stopped;
            }
            // The experiment's drop hook (AURA_RADIO_DROP_AT_S): a new
            // connection from the station's address, as after a broken one.
            if c.shared.drop_due() {
                crate::aelog!("[RADIO] HLS: dropped on purpose (AURA_RADIO_DROP_AT_S): from the station's address again");
                c.restart();
                continue 'variant;
            }
            let t_load = Instant::now();
            let m = match media.take() {
                Some(m) => m,
                None => match fetch(client, &c.media_url, None, PLAYLIST_MAX).await {
                    Ok((b, _)) => match parse(&String::from_utf8_lossy(&b)) {
                        Ok(Playlist::Media(m)) => m,
                        Ok(Playlist::Master { .. }) => return End::Failed(super::ERR_FORMAT, "unsupported: an HLS variant that is a master playlist".into()),
                        Err(e) => {
                            c.info.playlist_errors += 1;
                            crate::aelog!("[RADIO] HLS playlist unreadable ({}): again", e);
                            c.pause(BACKOFF_S[0] as f64).await;
                            continue;
                        }
                    },
                    Err(Fetch::Http(code @ (403 | 404 | 410))) => {
                        // An address with a token that ran out, or a variant
                        // that is gone: from the station's address again.
                        crate::aelog!("[RADIO] HLS playlist refused (HTTP {}): from the station's address again", code);
                        c.info.playlist_errors += 1;
                        c.restart();
                        failures += 1;
                        c.pause(BACKOFF_S[(failures - 1).min(BACKOFF_S.len() - 1)] as f64).await;
                        continue 'variant;
                    }
                    Err(e) => {
                        failures += 1;
                        c.info.playlist_errors += 1;
                        crate::aelog!("[RADIO] HLS playlist failed ({}): again", e);
                        c.shared.info.lock().unwrap().last_error = Some(format!("playlist: {}", e));
                        c.publish();
                        if !played && failures >= FIRST_TRIES {
                            return End::Failed(super::ERR_UNREACHABLE, format!("playlist: {}", e));
                        }
                        c.pause(BACKOFF_S[(failures - 1).min(BACKOFF_S.len() - 1)] as f64).await;
                        continue;
                    }
                },
            };
            failures = 0;
            c.info.target_s = m.target_s;
            if let Some(k) = m.segments.iter().find_map(|s| s.key.clone()) {
                return End::Failed(super::ERR_ENCRYPTED, format!("the HLS stream is encrypted ({})", k));
            }
            let from = match c.next {
                // The start: a few segments back from a live playlist's end
                // (what is in hand at once); a finished one from its start.
                None if m.endlist => 0,
                None => m.segments.len().saturating_sub(START_BACK),
                Some(next) => match next_in(&m, next, &mut c.behind) {
                    Next::From(i) => i,
                    Next::Lost(i) => {
                        let first_seq = m.segments.first().map_or(next, |s| s.seq);
                        c.info.lost += first_seq - next;
                        crate::aelog!(
                            "[RADIO] HLS: segments {}..{} left the playlist before they could be fetched: a gap",
                            next,
                            first_seq - 1
                        );
                        c.close(true);
                        i
                    }
                    Next::Rollback(i) => {
                        c.info.rollbacks += 1;
                        crate::aelog!(
                            "[RADIO] HLS: the playlist's numbers went back (next {}, it ends at {:?}): the server started again — a gap",
                            next,
                            m.last_seq()
                        );
                        c.close(true);
                        i
                    }
                },
            };
            let new = m.segments.len().saturating_sub(from);
            let mut broke = false;
            for seg in m.segments.iter().skip(from) {
                if c.stopped() {
                    return End::Stopped;
                }
                if seg.discontinuity && c.out.is_some() {
                    c.info.discontinuities += 1;
                    crate::aelog!("[RADIO] HLS: a discontinuity before segment {}: a gap", seg.seq);
                    c.close(true);
                }
                if c.next.is_none() {
                    c.next = Some(seg.seq);
                }
                if let Some(map) = &seg.map {
                    match c.init_for(client, map).await {
                        Ok(()) => {}
                        Err(Ok(end)) => return end,
                        Err(Err(e)) => {
                            crate::aelog!("[RADIO] HLS: {}: again", e);
                            c.info.retries += 1;
                            broke = true;
                            break;
                        }
                    }
                }
                match c.segment(client, seg).await {
                    Ok(body) => {
                        if let Err(e) = c.take(seg, &body) {
                            return e;
                        }
                        c.next = Some(seg.seq + 1);
                        c.info.segments += 1;
                        c.info.seconds += seg.duration_s;
                        c.info.seq = seg.seq + 1;
                        played = true;
                    }
                    Err(e) => {
                        if c.stopped() {
                            return End::Stopped;
                        }
                        c.shared.info.lock().unwrap().last_error = Some(format!("segment {}: {}", seg.seq, e));
                        // A station whose segments never come (a region it
                        // keeps them from): given up like one that never answers.
                        seg_failures += 1;
                        if !played && seg_failures >= FIRST_TRIES {
                            return End::Failed(super::ERR_UNREACHABLE, format!("segment: {}", e));
                        }
                        broke = true;
                        break;
                    }
                }
            }
            if !broke {
                seg_failures = 0;
            }
            c.publish();
            if m.endlist && !broke && c.next.map_or(true, |s| m.last_seq().map_or(true, |l| s > l)) {
                crate::aelog!("[RADIO] HLS: the playlist is finished and has played");
                return End::Ended;
            }
            // Read again a target duration after this reading began; half
            // that when it brought nothing new (the spec's pace).
            let target = if m.target_s > 0.0 { m.target_s } else { 4.0 };
            let wait = if broke { 1.0 } else if new > 0 { target } else { target / 2.0 };
            let left = wait - t_load.elapsed().as_secs_f64();
            c.pause(left).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIP: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:2335526\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-START:TIME-OFFSET=0\n#EXT-X-PROGRAM-DATE-TIME:2026-10-02T14:59:00Z\n#EXTINF:4.000,\n\
        /accs3/fip/prod1transcoder2/fip_aac_hifi_4_2335526_1790953124.ts\n#EXTINF:4.000,\n\
        /accs3/fip/prod1transcoder2/fip_aac_hifi_4_2335527_1790953128.ts\n#EXTINF:4.000,\n\
        /accs3/fip/prod1transcoder2/fip_aac_hifi_4_2335528_1790953132.ts\n";

    fn media(text: &str) -> Media {
        match parse(text).unwrap() {
            Playlist::Media(m) => m,
            p => panic!("not a media playlist: {p:?}"),
        }
    }

    /// A live media playlist: its target duration, the segments numbered
    /// from its media sequence, their addresses resolved from the
    /// playlist's own (path from the host's root, relative, absolute).
    #[test]
    fn a_media_playlist_numbers_its_segments_and_their_addresses_resolve() {
        let m = media(FIP);
        assert_eq!(m.target_s, 4.0);
        assert_eq!(m.segments.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![2335526, 2335527, 2335528]);
        assert!(!m.endlist && m.segments.iter().all(|s| s.duration_s == 4.0 && s.title.is_none() && !s.discontinuity));
        let base = "https://stream.radiofrance.fr/fip/fip_hifi.m3u8?id=x";
        assert_eq!(
            resolve(base, &m.segments[0].uri).unwrap(),
            "https://stream.radiofrance.fr/accs3/fip/prod1transcoder2/fip_aac_hifi_4_2335526_1790953124.ts"
        );
        let wowza = "http://host.example/app/st.stream/chunklist_w81.m3u8";
        assert_eq!(resolve(wowza, "media_w81_334007.aac").unwrap(), "http://host.example/app/st.stream/media_w81_334007.aac");
        assert_eq!(resolve(wowza, "https://cdn.example/a.aac?t=1").unwrap(), "https://cdn.example/a.aac?t=1");
        assert_eq!(resolve("https://h.example/a/b/list.m3u8", "../c/1.ts").unwrap(), "https://h.example/a/c/1.ts");
    }

    /// The tags a client has to honour: a discontinuity before a segment,
    /// byte ranges (one following on from the last), an fMP4 map, a key
    /// (NONE is none), the end of a finished playlist, EXTINF titles in
    /// attributes (and plain text that is not one).
    #[test]
    fn the_tags_of_a_media_playlist_are_read() {
        let m = media(
            "\u{feff}#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:10,title=\"Song, One\",artist=\"The Band\"\n#EXT-X-BYTERANGE:1000@0\nall.aac\n\
             #EXTINF:10, no desc\n#EXT-X-BYTERANGE:500\nall.aac\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"700@0\"\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:9.98,title=\"\",artist=\" \"\nc.m4s\n#EXT-X-ENDLIST\n",
        );
        assert_eq!(m.seq, 7);
        assert!(m.endlist);
        let s = &m.segments;
        assert_eq!((s[0].seq, s[1].seq, s[2].seq), (7, 8, 9));
        assert_eq!(s[0].title.as_deref(), Some("The Band - Song, One"));
        assert_eq!((s[1].title.clone(), s[2].title.clone()), (None, None));
        assert_eq!((s[0].range, s[1].range), (Some((0, 1000)), Some((1000, 500))));
        assert!(!s[0].discontinuity && !s[1].discontinuity && s[2].discontinuity);
        assert_eq!((s[0].map.clone(), s[2].map.clone()), (None, Some(("init.mp4".to_string(), Some((0, 700))))));
        assert_eq!((s[0].key.clone(), s[2].key.clone()), (None, Some("AES-128".to_string())));
        assert_eq!(s[2].duration_s, 9.98);
        assert!(parse("<html>").is_err());
        // Told from a plain .m3u by what it says.
        assert!(is_hls_text(FIP));
        assert!(!is_hls_text("#EXTM3U\n#EXTINF:-1,Station\nhttp://c.example:8000/live\n"));
    }

    /// A master playlist: the highest bandwidth the player decodes, AAC-LC
    /// before MP3 before HE-AAC at the same bandwidth, video only when there
    /// is nothing else (the least of it), an audio group's rendition when
    /// the variant points to one.
    #[test]
    fn the_variant_played_is_the_best_the_player_decodes() {
        let text = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=48000,CODECS=\"mp4a.40.2\"\nlc48.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"mp4a.40.5\"\nhe192.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"mp4a.40.2\"\nlc192.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=320000,CODECS=\"ac-3\"\nac3.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.640029,mp4a.40.2\",AUDIO=\"aud\"\nvideo.m3u8\n";
        let Playlist::Master { variants, audio } = parse(text).unwrap() else { panic!("a master playlist") };
        assert_eq!(variants.len(), 5);
        let v = choose(&variants).unwrap();
        assert_eq!((v.uri.as_str(), v.bandwidth), ("lc192.m3u8", 192000));
        assert_eq!(audio_uri(v, &audio), "lc192.m3u8");
        // HE beats a lower LC; MP3 beats HE at the same bandwidth.
        let vs = |l: &[(&str, u64, &str)]| l.iter().map(|(u, b, c)| Variant { uri: u.to_string(), bandwidth: *b, codecs: c.to_string(), audio: None }).collect::<Vec<_>>();
        assert_eq!(choose(&vs(&[("lc", 48000, "mp4a.40.2"), ("he", 64000, "mp4a.40.5")])).unwrap().uri, "he");
        assert_eq!(choose(&vs(&[("he", 128000, "mp4a.40.29"), ("mp3", 128000, "mp4a.40.34")])).unwrap().uri, "mp3");
        assert_eq!(choose(&vs(&[("x", 128000, ""), ("lc", 128000, "mp4a.40.2")])).unwrap().uri, "lc");
        assert!(choose(&vs(&[("ac3", 128000, "ac-3"), ("opus", 96000, "opus")])).is_none());
        // Only video: the least of it, and its audio group's rendition.
        let only_video: Vec<Variant> = variants.iter().filter(|v| v.codecs.contains("avc1")).cloned().collect();
        let v = choose(&only_video).unwrap();
        assert_eq!(audio_uri(v, &audio), "audio/en.m3u8");
    }

    fn seqs(first: u64, n: usize) -> Media {
        Media {
            target_s: 4.0,
            seq: first,
            segments: (0..n as u64).map(|k| Segment { seq: first + k, uri: format!("{}.ts", first + k), ..Segment::default() }).collect(),
            endlist: false,
        }
    }

    /// Where the stream goes on in a playlist read again: from the segment
    /// needed, without repeats or skips; nothing new while the playlist has
    /// not grown; after a gap when the segment needed has gone; numbers
    /// that went back: a lagging copy is waited out, a server that started
    /// again (far back, or for long) is a gap.
    #[test]
    fn the_stream_goes_on_from_the_segment_it_needs() {
        let mut behind = 0;
        assert_eq!(next_in(&seqs(100, 6), 103, &mut behind), Next::From(3));
        assert_eq!(next_in(&seqs(100, 6), 106, &mut behind), Next::From(6), "nothing new yet");
        assert_eq!(next_in(&seqs(101, 6), 106, &mut behind), Next::From(5));
        assert_eq!(next_in(&seqs(110, 6), 106, &mut behind), Next::Lost(0), "four segments gone");
        // A lagging copy: waited out a few readings, then taken as a restart.
        for _ in 1..BEHIND_READS {
            assert_eq!(next_in(&seqs(98, 6), 106, &mut behind), Next::From(6));
        }
        assert_eq!(next_in(&seqs(98, 6), 106, &mut behind), Next::Rollback(0));
        assert_eq!(behind, 0);
        // Far back at once: a server that started again.
        assert_eq!(next_in(&seqs(1, 3), 5000, &mut behind), Next::Rollback(0));
        // A copy that caught up resets the count.
        assert_eq!(next_in(&seqs(98, 6), 106, &mut behind), Next::From(6));
        assert_eq!(next_in(&seqs(101, 6), 106, &mut behind), Next::From(5));
        assert_eq!(behind, 0);
        assert_eq!(next_in(&Media::default(), 5, &mut behind), Next::From(0));
    }

    fn id3_v4(frames: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, data) in &frames {
            body.extend_from_slice(id.as_bytes());
            let n = data.len();
            body.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F, 0, 0]);
            body.extend_from_slice(data);
        }
        let n = body.len();
        let mut t = b"ID3\x04\x00\x00".to_vec();
        t.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
        t.extend_from_slice(&body);
        t
    }

    /// Packed audio: the ID3 tag before the audio (a timestamp, and the
    /// title where there is one, in each of ID3's text encodings), then
    /// ADTS or MPEG audio; TS and fMP4 are told by their first bytes.
    #[test]
    fn packed_audio_is_found_after_its_id3_tags_and_their_title_read() {
        let mut priv_ts = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
        priv_ts.extend_from_slice(&[0, 0, 0, 0, 0, 0xA6, 0x10, 0x79]);
        let tag = id3_v4(vec![("PRIV", priv_ts.clone())]);
        assert_eq!(tag.len(), 10 + 10 + priv_ts.len());
        let mut seg = tag.clone();
        seg.extend_from_slice(&[0xFF, 0xF1, 0x4C, 0x80, 0x39, 0xFF, 0xFC]);
        assert_eq!(segment_kind(&seg), Kind::Packed(Es::Adts, tag.len()));
        assert_eq!(id3_head(&seg).0, None);
        // Titles: UTF-8, Latin-1, UTF-16 with a BOM.
        let tit2_utf16: Vec<u8> = [vec![1u8, 0xFF, 0xFE], "Ça va".encode_utf16().flat_map(|u| u.to_le_bytes()).collect(), vec![0, 0]].concat();
        let tag = id3_v4(vec![("TPE1", b"\x03Bj\xc3\xb6rk\0".to_vec()), ("TIT2", tit2_utf16)]);
        let mut seg = tag.clone();
        seg.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        assert_eq!(segment_kind(&seg), Kind::Packed(Es::Mpeg, tag.len()));
        assert_eq!(id3_head(&seg).0.as_deref(), Some("Björk - Ça va"));
        let tag = id3_v4(vec![("TIT2", b"\x00Caf\xe9".to_vec())]);
        assert_eq!(id3_head(&tag).0.as_deref(), Some("Café"));
        // Not packed audio.
        let mut ts = vec![0u8; 376];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(segment_kind(&ts), Kind::Ts);
        assert_eq!(segment_kind(b"\0\0\0\x18styp msdh"), Kind::Mp4);
        assert_eq!(segment_kind(b"<html>"), Kind::Unknown);
    }

    // ── The client against a scripted server ────────────────────────────

    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    /// An HTTP server on the loopback, one request per connection: `answer`
    /// gives each path's status and body. The clients that play it pause a
    /// fiftieth of their pauses.
    fn serve(answer: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> String {
        PAUSE_PCT.store(2, Ordering::Relaxed);
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let base = format!("http://{}", l.local_addr().expect("its address"));
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (code, body) = answer(&path);
                let head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", code, body.len());
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        base
    }

    /// Segment `mark` as a transport stream: its tables, and one PES whose
    /// audio is the mark (4 bytes) and filler, 300 bytes in all.
    fn ts_segment(mark: u64) -> Vec<u8> {
        let mut m = crate::player::radio::ts::tests::Muxer::new();
        m.pat(0x1000);
        m.pmt(0x1000, &[(0x0F, 0x100, &[])]);
        let mut audio = (mark as u32).to_be_bytes().to_vec();
        audio.resize(300, 0xA5);
        m.pes(0x100, 0xC0, &audio, 0);
        m.out
    }

    /// The client against a live server whose window moves one segment on
    /// each time its playlist is read. It starts three segments back from
    /// the edge and goes on in one pipe, without repeats or skips; a segment
    /// that fails twice is fetched again in the same pipe. A discontinuity,
    /// a segment the window dropped before it could be had, numbers that
    /// went back: a new pipe after a known gap, from where the stream can go
    /// on. A refused playlist: from the station's address again, three
    /// segments back, in a pipe the joiner places (no known gap).
    #[test]
    fn the_client_plays_a_live_playlist_through_its_troubles() {
        let state = Arc::new(Mutex::new((0u64, HashMap::<String, u32>::new())));
        let st = state.clone();
        let base = serve(move |path| {
            let mut g = st.lock().unwrap();
            if path == "/live.m3u8" {
                let k = g.0;
                g.0 += 1;
                if k == 18 {
                    return (403, Vec::new());
                }
                // From the 24th reading on, the server has started again:
                // low numbers, other audio.
                let (edge, dir) = if k >= 24 { (3 + k - 20, "r") } else { (3 + k, "s") };
                let mut text = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{}\n", edge - 3);
                for s in edge - 3..=edge {
                    if s == 9 && dir == "s" {
                        text += "#EXT-X-DISCONTINUITY\n";
                    }
                    text += &format!("#EXTINF:1.0,\n/{}/{}.ts\n", dir, s);
                }
                return (200, text.into_bytes());
            }
            let tries = {
                let t = g.1.entry(path.to_string()).or_insert(0);
                *t += 1;
                *t
            };
            let (dir, n) = path.trim_start_matches('/').trim_end_matches(".ts").split_once('/').unwrap_or(("", "0"));
            let n: u64 = n.parse().unwrap_or(0);
            match (dir, n) {
                ("s", 6) if tries <= 2 => (503, Vec::new()),
                ("s", 13) => (404, Vec::new()),
                ("r", n) => (200, ts_segment(1000 + n)),
                (_, n) => (200, ts_segment(n)),
            }
        });
        let url = format!("{}/live.m3u8", base);
        let shared = Arc::new(RadioShared::new(&url, &crate::player::settings::PlayerSettings::default()));
        let (tx, rx) = std::sync::mpsc::channel::<Connected>();
        let (sh, u) = (shared.clone(), url.clone());
        let client = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
            rt.block_on(async {
                let client = reqwest::Client::new();
                let mut n = 0u32;
                let end = run(&sh, &tx, &client, None, &u, &mut n).await;
                (matches!(end, End::Stopped), n)
            })
        });
        let watchdog = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            watchdog.stop.store(true, Ordering::Release);
        });
        // The pipes in order: whether each follows a known gap, and the
        // marks of the segments it brought.
        let mut pipes: Vec<(bool, Vec<u64>)> = Vec::new();
        while let Ok(c) = rx.recv_timeout(Duration::from_secs(30)) {
            let mut r = c.pipe.reader();
            let (mut es, mut marks, mut buf) = (Vec::new(), Vec::new(), [0u8; 4096]);
            loop {
                let k = r.read(&mut buf).expect("the pipe");
                if k == 0 {
                    break;
                }
                es.extend_from_slice(&buf[..k]);
                while es.len() >= 300 {
                    let seg: Vec<u8> = es.drain(..300).collect();
                    let mark = u64::from(u32::from_be_bytes([seg[0], seg[1], seg[2], seg[3]]));
                    marks.push(mark);
                    if mark >= 1010 {
                        shared.stop.store(true, Ordering::Release);
                    }
                }
            }
            assert!(es.is_empty(), "whole segments");
            pipes.push((c.gap, marks));
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
        }
        let (stopped, n) = client.join().expect("the client");
        assert!(stopped, "stopped, not failed");
        assert_eq!(n as usize, pipes.len());
        assert_eq!(pipes.iter().map(|p| p.0).collect::<Vec<_>>(), vec![false, true, true, false, true]);
        assert_eq!(pipes[0].1, vec![1, 2, 3, 4, 5, 6, 7, 8], "three back from the edge, on without a seam");
        assert_eq!(pipes[1].1, vec![9, 10, 11, 12], "after the discontinuity");
        assert_eq!(pipes[2].1, vec![14, 15, 16, 17, 18, 19, 20], "after the segment that never came");
        assert_eq!(pipes[3].1, vec![20, 21, 22, 23, 24, 25, 26], "the station's address again: three back");
        assert_eq!(pipes[4].1[..7], [1004, 1005, 1006, 1007, 1008, 1009, 1010], "the server that started again");
        let i = shared.info.lock().unwrap().hls.clone().expect("the client's account");
        assert_eq!((i.lost, i.discontinuities, i.rollbacks, i.restarts, i.playlist_errors), (1, 1, 1, 1, 1));
        assert_eq!(i.retries, 2 + 4 * SEGMENT_TRIES as u64);
        assert_eq!(i.container, "TS");
    }

    /// An fMP4 stream (EXT-X-MAP): its initialization section fetched once,
    /// its segments' audio handed on as ADTS frames.
    #[test]
    fn fmp4_segments_come_out_as_adts() {
        use crate::player::radio::mp4::tests as m;
        let frames = |n: u64| -> Vec<Vec<u8>> { (0..3).map(|k| vec![(n * 3 + k) as u8; 20]).collect() };
        let inits = Arc::new(Mutex::new(0u32));
        let count = inits.clone();
        let base = serve(move |path| match path {
            "/live.m3u8" => (
                200,
                "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                 #EXTINF:1,\nf/5.m4s\n#EXTINF:1,\nf/6.m4s\n#EXTINF:1,\nf/7.m4s\n"
                    .into(),
            ),
            "/init.mp4" => {
                *count.lock().unwrap() += 1;
                (200, m::init(1, 0x40, &[0x11, 0x90], b"mp4a"))
            }
            p => {
                let n: u64 = p.trim_start_matches("/f/").trim_end_matches(".m4s").parse().unwrap_or(0);
                (200, m::segment(1, &frames(n)))
            }
        });
        let url = format!("{}/live.m3u8", base);
        let shared = Arc::new(RadioShared::new(&url, &crate::player::settings::PlayerSettings::default()));
        let (tx, rx) = std::sync::mpsc::channel::<Connected>();
        let (sh, u) = (shared.clone(), url.clone());
        let client = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
            rt.block_on(async {
                run(&sh, &tx, &reqwest::Client::new(), None, &u, &mut 0u32).await;
            })
        });
        let c = rx.recv_timeout(Duration::from_secs(30)).expect("a connection");
        assert_eq!(c.content_type, "audio/aac");
        let want: Vec<u8> = (5..8u64).flat_map(frames).flat_map(|s| [mp4::adts_header((1, 3, 2), s.len()).to_vec(), s].concat()).collect();
        let mut got = vec![0u8; want.len()];
        c.pipe.reader().read_exact(&mut got).expect("the audio");
        shared.stop.store(true, Ordering::Release);
        client.join().expect("the client");
        assert_eq!(got, want);
        assert_eq!(*inits.lock().unwrap(), 1, "the initialization section fetched once");
        assert_eq!(shared.info.lock().unwrap().hls.clone().expect("the account").container, "fMP4");
    }
}
