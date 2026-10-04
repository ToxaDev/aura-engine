//! The network side: one HTTP(S) connection to the stream at a time, its
//! ICY metadata cut out before the decoder sees a byte, and a new connection
//! after a pause when one breaks. An HLS stream (.m3u8) is played by the HLS
//! client (`hls`), which hands the decoder the same kind of bytes.
//!
//! Runs on its own thread with a single-threaded tokio runtime (reqwest's
//! async client: a read that hangs is cut by a timeout, which the blocking
//! client of this reqwest cannot do). The audio bytes go into a `Pipe` per
//! connection, which the decoder thread reads as a plain `Read`.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::RadioShared;

/// No byte for this long: the connection is taken as broken.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Pauses before the next connection: grows with each failure in a row.
const BACKOFF_S: [u64; 5] = [1, 2, 4, 8, 10];
/// A playlist larger than this is not a playlist.
const PLAYLIST_MAX: usize = 64 * 1024;
/// Failed tries in a row before a station that never answered is given up.
/// Once it has played, the stream is tried again for as long as it is on.
const FIRST_TRIES: usize = 3;

/// Radio Paradise's list of what plays on a channel (`chan` 0 Main, 1
/// Mellow, 2 Rock, 3 Global): artist, title, album, year, and `time`, the
/// seconds until it changes.
const RP_NOW_PLAYING: &str = "https://api.radioparadise.com/api/now_playing?chan=";

const USER_AGENT: &str = concat!("AuraEngine/", env!("CARGO_PKG_VERSION"));

/// The bytes of one connection, from the network thread to the decoder.
pub struct Pipe {
    st: Mutex<(VecDeque<u8>, bool)>,
    cv: Condvar,
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe { st: Mutex::new((VecDeque::new(), false)), cv: Condvar::new() })
    }

    pub fn write(&self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.0.extend(b);
        drop(st);
        self.cv.notify_all();
    }

    /// No more bytes will come: the reader gets end-of-file after the rest.
    pub fn finish(&self) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.cv.notify_all();
    }

    pub fn reader(self: &Arc<Pipe>) -> PipeReader {
        PipeReader(self.clone())
    }
}

/// The decoder's end of a `Pipe`: blocks until bytes come or the
/// connection is over.
pub struct PipeReader(Arc<Pipe>);

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut st = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !st.0.is_empty() {
                let n = out.len().min(st.0.len());
                for (o, b) in out.iter_mut().zip(st.0.drain(..n)) {
                    *o = b;
                }
                return Ok(n);
            }
            if st.1 {
                return Ok(0);
            }
            st = self.0.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A connection is up: its bytes, what the server said about them, and its
/// number (1 = the first).
pub struct Connected {
    pub pipe: Arc<Pipe>,
    pub content_type: String,
    pub url: String,
    pub n: u32,
    /// It follows a known break (HLS: segments lost, a discontinuity, a
    /// server that started again): met as a gap, not looked for in what was
    /// received. An Icecast reconnection does not know: the joiner looks.
    pub gap: bool,
}

/// The ICY interleave: `metaint` audio bytes, a length byte (×16), that much
/// metadata, and again.
pub struct Icy {
    metaint: usize,
    /// Audio bytes until the next length byte.
    left: usize,
    /// Metadata bytes still to come in the current block.
    meta_left: usize,
    meta: Vec<u8>,
}

impl Icy {
    pub fn new(metaint: usize) -> Icy {
        Icy { metaint, left: metaint, meta_left: 0, meta: Vec::new() }
    }

    /// Split `data`: audio to `audio`, each complete metadata block to `meta`.
    pub fn feed(&mut self, mut data: &[u8], audio: &mut impl FnMut(&[u8]), meta: &mut impl FnMut(&str)) {
        if self.metaint == 0 {
            audio(data);
            return;
        }
        while !data.is_empty() {
            if self.meta_left > 0 {
                let n = self.meta_left.min(data.len());
                self.meta.extend_from_slice(&data[..n]);
                self.meta_left -= n;
                data = &data[n..];
                if self.meta_left == 0 {
                    let text = String::from_utf8_lossy(&self.meta).trim_end_matches('\0').to_string();
                    meta(&text);
                    self.meta.clear();
                    self.left = self.metaint;
                }
                continue;
            }
            if self.left == 0 {
                let n = data[0] as usize * 16;
                data = &data[1..];
                if n == 0 {
                    self.left = self.metaint;
                } else {
                    self.meta_left = n;
                }
                continue;
            }
            let n = self.left.min(data.len());
            audio(&data[..n]);
            self.left -= n;
            data = &data[n..];
        }
    }
}

/// `StreamTitle='…';` out of an ICY metadata block.
pub fn stream_title(meta: &str) -> Option<String> {
    let start = meta.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &meta[start..];
    let end = rest.find("';").unwrap_or(rest.trim_end_matches('\'').len());
    Some(rest[..end].trim().to_string())
}

/// The first stream address in a .pls or .m3u playlist.
pub fn playlist_entry(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let url = if let Some((k, v)) = line.split_once('=') {
            if k.trim().to_ascii_lowercase().starts_with("file") { v.trim() } else { continue }
        } else if line.starts_with('#') {
            continue;
        } else {
            line
        };
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(url.to_string());
        }
    }
    None
}

fn is_playlist(url: &str, content_type: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let ct = content_type.to_ascii_lowercase();
    path.ends_with(".pls")
        || path.ends_with(".m3u")
        || ct.contains("scpls")
        || (ct.contains("mpegurl") && !path.ends_with(".m3u8"))
}

fn is_hls(url: &str, content_type: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    path.ends_with(".m3u8") || content_type.to_ascii_lowercase().contains("vnd.apple.mpegurl")
}

/// Radio Paradise's channel for one of its stream addresses (0 Main, 1
/// Mellow, 2 Rock, 3 Global); none for another station.
pub fn rp_channel(url: &str) -> Option<u32> {
    let rest = url.split_once("://")?.1;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    if host != "stream.radioparadise.com" {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    Some(if path.starts_with("mellow") {
        1
    } else if path.starts_with("rock") {
        2
    } else if path.starts_with("global") || path.starts_with("world") || path.starts_with("eclectic") {
        3
    } else {
        0
    })
}

/// "Artist - Title", "Album · Year" and the seconds until the next song out
/// of Radio Paradise's now_playing answer.
fn rp_parse(v: &serde_json::Value) -> Option<(String, Option<String>, f64)> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    let text = match (s("artist"), s("title")) {
        (Some(a), Some(t)) => format!("{} - {}", a, t),
        (None, Some(t)) => t,
        (Some(a), None) => a,
        (None, None) => return None,
    };
    let album = match (s("album"), s("year")) {
        (Some(a), Some(y)) => Some(format!("{} · {}", a, y)),
        (a, y) => a.or(y),
    };
    let time = v
        .get("time")
        .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|t| t.trim().parse().ok())))
        .unwrap_or(30.0);
    Some((text, album, time))
}

/// What Radio Paradise says plays on the session's channel, for its streams
/// that carry no titles of their own (no ICY metadata): asked again when
/// the song is due to change, at least every two minutes.
async fn rp_now_playing(shared: Arc<RadioShared>, client: reqwest::Client) {
    let Some(chan) = rp_channel(&shared.url) else { return };
    while !shared.stop.load(Ordering::Acquire) {
        let mut next_s = 30.0;
        if shared.info.lock().unwrap().metaint == 0 {
            let asked = tokio::time::timeout(READ_TIMEOUT, async {
                let r = client.get(format!("{}{}", RP_NOW_PLAYING, chan)).send().await?.error_for_status()?;
                r.json::<serde_json::Value>().await
            })
            .await;
            match asked {
                Ok(Ok(v)) => match rp_parse(&v) {
                    Some((text, album, time)) => {
                        shared.rp_now_playing(&text, album, time);
                        next_s = (time + 2.0).clamp(5.0, 120.0);
                    }
                    None => crate::aelog!("[RADIO] Radio Paradise now playing: no title in the answer"),
                },
                Ok(Err(e)) => crate::aelog!("[RADIO] Radio Paradise now playing: {}", e),
                Err(_) => crate::aelog!("[RADIO] Radio Paradise now playing: no answer"),
            }
        }
        let until = Instant::now() + Duration::from_secs_f64(next_s);
        while Instant::now() < until && !shared.stop.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// Start the network thread: connections one after another until the
/// session stops, each handed to the decoder as a `Connected`; beside them,
/// for Radio Paradise, its list of what plays.
pub fn spawn(shared: Arc<RadioShared>, to_decoder: Sender<Connected>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("aura-radio-net".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    shared.fail(format!("network runtime: {}", e));
                    return;
                }
            };
            let client = match reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    shared.fail(format!("HTTP client: {}", e));
                    return;
                }
            };
            rt.block_on(async {
                tokio::join!(run(shared.clone(), to_decoder, client.clone()), rp_now_playing(shared, client));
            });
        })
        .expect("spawn the radio network thread")
}

async fn run(shared: Arc<RadioShared>, to_decoder: Sender<Connected>, client: reqwest::Client) {
    let mut url = shared.url.clone();
    let mut failures = 0usize;
    let mut n = 0u32;
    while !shared.stop.load(Ordering::Acquire) {
        let t_try = Instant::now();
        match connect(&client, &url).await {
            Ok(resp) => {
                let ct = header(&resp, "content-type");
                let hls = is_hls(&url, &ct);
                if hls || is_playlist(&url, &ct) {
                    let at = resp.url().to_string();
                    let text = read_playlist(resp, if hls { super::hls::PLAYLIST_MAX } else { PLAYLIST_MAX }).await;
                    // A playlist of segments, by its address or type or by what
                    // it says (some servers send one as a plain .m3u): the HLS
                    // client plays it from here on, for the whole session.
                    if hls || text.as_deref().is_ok_and(super::hls::is_hls_text) {
                        let first = text.ok().map(|t| (t.into_bytes(), at));
                        match super::hls::run(&shared, &to_decoder, &client, first, &url, &mut n).await {
                            super::hls::End::Stopped => {}
                            super::hls::End::Failed(kind, e) => shared.fail_kind(kind, e),
                            super::hls::End::Ended => shared.fail_kind(super::ERR_OTHER, "the stream has ended".into()),
                        }
                        return;
                    }
                    match text.and_then(|t| playlist_entry(&t).ok_or_else(|| "no stream address in the playlist".to_string())) {
                        Ok(u) => {
                            crate::aelog!("[RADIO] playlist {} → {}", url, u);
                            url = u;
                            continue;
                        }
                        Err(e) => {
                            shared.fail_kind(super::ERR_UNREACHABLE, format!("playlist: {}", e));
                            return;
                        }
                    }
                }
                n += 1;
                shared.info.lock().unwrap().connected = true;
                let ms = t_try.elapsed().as_millis() as u64;
                let metaint: usize = header(&resp, "icy-metaint").parse().unwrap_or(0);
                {
                    let mut i = shared.info.lock().unwrap();
                    i.connects = n;
                    i.stream_url = url.clone();
                    i.content_type = ct.clone();
                    i.icy_name = header(&resp, "icy-name");
                    i.icy_br = header(&resp, "icy-br");
                    i.audio_info = header(&resp, "ice-audio-info");
                    i.metaint = metaint;
                    i.connect_ms.push(ms);
                    if i.t_connected_ms.is_none() {
                        i.t_connected_ms = Some(shared.t0.elapsed().as_millis() as u64);
                    }
                }
                crate::aelog!(
                    "[RADIO] connected #{} in {} ms: {} ({}; icy-br {}; metaint {}) {}",
                    n, ms, url, ct, header(&resp, "icy-br"), metaint, header(&resp, "icy-name")
                );
                let pipe = Pipe::new();
                if to_decoder.send(Connected { pipe: pipe.clone(), content_type: ct, url: url.clone(), n, gap: false }).is_err() {
                    return;
                }
                let how = stream_body(&shared, resp, &pipe, metaint, n).await;
                pipe.finish();
                shared.info.lock().unwrap().connected = false;
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                crate::aelog!("[RADIO] connection #{} ended: {}", n, how);
                shared.info.lock().unwrap().last_error = Some(how);
                // A connection that played a while resets the pause.
                failures = if t_try.elapsed() > Duration::from_secs(30) { 0 } else { failures + 1 };
            }
            Err(e) => {
                crate::aelog!("[RADIO] connect failed: {}", e);
                shared.info.lock().unwrap().last_error = Some(e.clone());
                failures += 1;
                // A station that never answered: given up after a few tries.
                if n == 0 && failures >= FIRST_TRIES {
                    shared.fail_kind(super::ERR_UNREACHABLE, e);
                    return;
                }
            }
        }
        let pause = BACKOFF_S[(failures.max(1) - 1).min(BACKOFF_S.len() - 1)];
        let until = Instant::now() + Duration::from_secs(pause);
        while Instant::now() < until && !shared.stop.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn connect(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let resp = client
        .get(url)
        .header("Icy-MetaData", "1")
        .send()
        .await
        .map_err(|e| format!("{}", e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(resp)
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

async fn read_playlist(mut resp: reqwest::Response, max: usize) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = tokio::time::timeout(READ_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| "playlist: no data".to_string())?
        .map_err(|e| e.to_string())?
    {
        body.extend_from_slice(&chunk);
        if body.len() > max {
            return Err("playlist too large".into());
        }
    }
    Ok(String::from_utf8_lossy(&body).to_string())
}

/// Copy one connection's audio bytes into `pipe` until it ends; how it ended.
async fn stream_body(shared: &RadioShared, mut resp: reqwest::Response, pipe: &Pipe, metaint: usize, n: u32) -> String {
    let mut icy = Icy::new(metaint);
    let mut dump = shared.dump_file(n, &header(&resp, "content-type"));
    let mut first = true;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return "stopped".into();
        }
        // The drop hook for the experiment: cut this connection after so
        // many seconds of the session (AURA_RADIO_DROP_AT_S).
        if shared.drop_due() {
            return "dropped on purpose (AURA_RADIO_DROP_AT_S)".into();
        }
        // The stall hook: stop reading for a while, the connection open
        // (AURA_RADIO_STALL_AT_S).
        if let Some(s) = shared.stall_due() {
            tokio::time::sleep(Duration::from_secs_f64(s)).await;
        }
        let chunk = match tokio::time::timeout(READ_TIMEOUT, resp.chunk()).await {
            Err(_) => return format!("no data for {} s", READ_TIMEOUT.as_secs()),
            Ok(Err(e)) => return format!("read error: {}", e),
            Ok(Ok(None)) => return "the server ended the stream".into(),
            Ok(Ok(Some(c))) => c,
        };
        if first {
            first = false;
            let mut i = shared.info.lock().unwrap();
            if i.t_first_byte_ms.is_none() {
                i.t_first_byte_ms = Some(shared.t0.elapsed().as_millis() as u64);
            }
        }
        let mut audio_bytes = 0usize;
        icy.feed(
            &chunk,
            &mut |a: &[u8]| {
                pipe.write(a);
                audio_bytes += a.len();
                if let Some(f) = dump.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(a);
                }
            },
            &mut |m: &str| {
                if let Some(t) = stream_title(m) {
                    shared.set_title(&t, "icy");
                }
            },
        );
        shared.info.lock().unwrap().bytes += audio_bytes as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icy_metadata_is_cut_out_of_the_audio() {
        // metaint 4: AAAA [len 1] 16 bytes of meta, AAAA [len 0], AA…
        let mut data = b"abcd".to_vec();
        data.push(1);
        let mut meta = b"StreamTitle='X';".to_vec();
        meta.resize(16, 0);
        data.extend_from_slice(&meta);
        data.extend_from_slice(b"efgh");
        data.push(0);
        data.extend_from_slice(b"ij");
        // Fed in every split, the result is the same.
        for split in 1..data.len() {
            let mut icy = Icy::new(4);
            let mut audio = Vec::new();
            let mut titles = Vec::new();
            for part in data.chunks(split) {
                icy.feed(part, &mut |a: &[u8]| audio.extend_from_slice(a), &mut |m: &str| titles.push(stream_title(m)));
            }
            assert_eq!(audio, b"abcdefghij", "split {}", split);
            assert_eq!(titles, vec![Some("X".to_string())], "split {}", split);
        }
    }

    #[test]
    fn stream_titles_keep_their_apostrophes() {
        assert_eq!(stream_title("StreamTitle='Guns N' Roses - Don't Cry';StreamUrl='';").as_deref(), Some("Guns N' Roses - Don't Cry"));
        assert_eq!(stream_title("StreamUrl='x';"), None);
    }

    #[test]
    fn playlists_give_their_first_stream() {
        let pls = "[playlist]\nNumberOfEntries=2\nFile1=https://a.example/stream\nTitle1=A\nFile2=http://b.example/\n";
        assert_eq!(playlist_entry(pls).as_deref(), Some("https://a.example/stream"));
        let m3u = "#EXTM3U\n#EXTINF:-1,Station\nhttp://c.example:8000/live\n";
        assert_eq!(playlist_entry(m3u).as_deref(), Some("http://c.example:8000/live"));
        assert!(is_playlist("http://x/listen.pls", ""));
        assert!(is_playlist("http://x/a", "audio/x-mpegurl"));
        assert!(!is_playlist("http://x/a.m3u8", "application/vnd.apple.mpegurl"));
        assert!(is_hls("http://x/a.m3u8?t=1", ""));
    }

    #[test]
    fn radio_paradise_streams_name_their_channel() {
        assert_eq!(rp_channel("https://stream.radioparadise.com/flac"), Some(0));
        assert_eq!(rp_channel("https://stream.radioparadise.com/aac-320?x=1"), Some(0));
        assert_eq!(rp_channel("https://stream.radioparadise.com/mellow-flac"), Some(1));
        assert_eq!(rp_channel("http://stream.radioparadise.com:80/rock-flac"), Some(2));
        assert_eq!(rp_channel("https://stream.radioparadise.com/global-flac"), Some(3));
        assert_eq!(rp_channel("https://icecast.radiofrance.fr/fip-hifi.aac"), None);
        assert_eq!(rp_channel("not an address"), None);
    }

    #[test]
    fn radio_paradise_answers_give_the_title_the_album_and_when_it_changes() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"time":51,"artist":"Mark Pritchard & Thom Yorke","title":"The Spirit","album":"Tall Tales","year":"2025","cover":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            rp_parse(&v),
            Some(("Mark Pritchard & Thom Yorke - The Spirit".to_string(), Some("Tall Tales · 2025".to_string()), 51.0))
        );
        let v: serde_json::Value = serde_json::from_str(r#"{"time":"12","title":"Only a title"}"#).unwrap();
        assert_eq!(rp_parse(&v), Some(("Only a title".to_string(), None, 12.0)));
        assert_eq!(rp_parse(&serde_json::json!({"time": 5})), None);
    }

    #[test]
    fn a_pipe_reads_what_was_written_then_ends() {
        let p = Pipe::new();
        let mut r = p.reader();
        p.write(b"hello");
        p.finish();
        let mut buf = [0u8; 3];
        assert_eq!(r.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"hel");
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }
}
