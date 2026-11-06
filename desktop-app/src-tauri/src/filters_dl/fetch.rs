//! Downloading one filter file: into `<name>.part`, resumed with a Range
//! request after an interruption, hashed as it arrives, and renamed to its
//! name only once its size and SHA-256 match the signed list.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use super::manifest::Entry;

/// A response body as it arrives.
pub type Body = BoxStream<'static, Result<Vec<u8>, String>>;

/// What a source answered for a file asked from byte `from`.
pub struct Opened {
    /// `Some(start)`: a partial answer (206) starting at `start`; `None`: the
    /// whole file from its first byte (200).
    pub partial_from: Option<u64>,
    pub body: Body,
}

pub enum OpenError {
    /// 416: the range is past the end of the file the server has.
    RangeNotSatisfiable,
    Failed(String),
}

/// Where the files come from: the release over HTTPS, or a test's bytes.
pub trait Source: Send + Sync {
    fn open<'a>(&'a self, name: &'a str, from: u64) -> BoxFuture<'a, Result<Opened, OpenError>>;
}

/// The `filters` release over HTTP.
pub struct HttpSource {
    pub client: reqwest::Client,
    /// Ends in `/`.
    pub base_url: String,
}

/// The start of a `Content-Range: bytes <start>-<end>/<total>` header.
fn content_range_start(v: &str) -> Option<u64> {
    v.trim().strip_prefix("bytes ")?.split('-').next()?.trim().parse().ok()
}

impl Source for HttpSource {
    fn open<'a>(&'a self, name: &'a str, from: u64) -> BoxFuture<'a, Result<Opened, OpenError>> {
        Box::pin(async move {
            let mut req = self.client.get(format!("{}{}", self.base_url, name));
            if from > 0 {
                req = req.header(reqwest::header::RANGE, format!("bytes={from}-"));
            }
            let resp = crate::updater::within(crate::updater::RESPONSE_WAIT, req.send())
                .await
                .map_err(OpenError::Failed)?
                .map_err(|e| OpenError::Failed(format!("{name}: {e}")))?;
            let partial_from = match resp.status().as_u16() {
                200 => None,
                206 => Some(
                    resp.headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(content_range_start)
                        .ok_or_else(|| OpenError::Failed(format!("{name}: a partial answer without its range")))?,
                ),
                416 => return Err(OpenError::RangeNotSatisfiable),
                s => return Err(OpenError::Failed(format!("{name}: HTTP {s}"))),
            };
            let body = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string())).boxed();
            Ok(Opened { partial_from, body })
        })
    }
}

/// A small file read whole, refused past `cap` bytes.
pub async fn fetch_small(source: &dyn Source, name: &str, cap: u64, idle: Duration) -> Result<Vec<u8>, String> {
    let mut opened = match source.open(name, 0).await {
        Ok(o) => o,
        Err(OpenError::RangeNotSatisfiable) => return Err(format!("{name}: HTTP 416")),
        Err(OpenError::Failed(e)) => return Err(e),
    };
    let mut out = Vec::new();
    while let Some(chunk) = crate::updater::within(idle, opened.body.next()).await? {
        let chunk = chunk.map_err(|e| format!("{name}: {e}"))?;
        if (out.len() + chunk.len()) as u64 > cap {
            return Err(format!("{name} is larger than {cap} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[derive(Debug, PartialEq)]
pub enum FetchError {
    /// Stopped by the user; the `.part` stays for the next try.
    Cancelled,
    Failed(String),
}

/// Timing of a download; tests shorten it.
#[derive(Clone, Copy)]
pub struct Timing {
    /// Without a byte for this long, the connection is given up.
    pub idle: Duration,
    /// Tries after the first one when a connection breaks, each resuming
    /// where the last stopped.
    pub retries: u32,
    /// Pause before a retry.
    pub pause: Duration,
}

impl Timing {
    pub const REAL: Timing = Timing {
        idle: crate::updater::DOWNLOAD_IDLE,
        retries: 3,
        pause: Duration::from_secs(3),
    };
}

/// How one try ended short of the file.
enum TryEnd {
    Cancelled,
    /// The connection broke: try again from what the `.part` holds.
    Broken(String),
    /// Nothing a retry would change.
    Fatal(String),
}

/// `dir/<name>.part`.
pub fn part_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.part"))
}

/// Download `e` into `dir`, resuming any `.part` it finds. `progress` is told
/// how many bytes of the file are in hand, as they arrive. On success the file
/// is at `dir/<name>`, flushed to the disk, with the listed size and SHA-256;
/// on a mismatch the `.part` is deleted.
pub async fn fetch_file(
    source: &dyn Source,
    dir: &Path,
    e: &Entry,
    cancel: &AtomicBool,
    timing: Timing,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), FetchError> {
    let mut last = String::new();
    for attempt in 0..=timing.retries {
        if attempt > 0 && !timing.pause.is_zero() {
            tokio::time::sleep(timing.pause).await;
        }
        match try_once(source, dir, e, cancel, timing.idle, progress).await {
            Ok(()) => return Ok(()),
            Err(TryEnd::Cancelled) => return Err(FetchError::Cancelled),
            Err(TryEnd::Fatal(why)) => return Err(FetchError::Failed(why)),
            Err(TryEnd::Broken(why)) => {
                crate::aelog!("[FILTERS] {}: {} (try {} of {})", e.name, why, attempt + 1, timing.retries + 1);
                last = why;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
    }
    Err(FetchError::Failed(format!("{}: {}", e.name, last)))
}

/// Hash the first `len` bytes of `path` into `hasher`.
fn rehash(path: &Path, len: u64, hasher: &mut Sha256) -> std::io::Result<()> {
    let mut f = std::fs::File::open(path)?.take(len);
    let mut buf = vec![0u8; 1 << 20];
    let mut seen = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        seen += n as u64;
    }
    if seen != len {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the part file shrank"));
    }
    Ok(())
}

async fn try_once(
    source: &dyn Source,
    dir: &Path,
    e: &Entry,
    cancel: &AtomicBool,
    idle: Duration,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), TryEnd> {
    let part = part_path(dir, &e.name);
    let fatal = |what: &str, err: &dyn std::fmt::Display| TryEnd::Fatal(format!("{} {}: {}", what, part.display(), err));
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > e.size {
        std::fs::remove_file(&part).map_err(|err| fatal("cannot remove", &err))?;
        have = 0;
    }
    let mut hasher = Sha256::new();
    if have > 0 && rehash(&part, have, &mut hasher).is_err() {
        // Unreadable as it is: start over.
        hasher = Sha256::new();
        have = 0;
    }
    progress(have);

    if have < e.size {
        if cancel.load(Ordering::Relaxed) {
            return Err(TryEnd::Cancelled);
        }
        let opened = match source.open(&e.name, have).await {
            Ok(o) => o,
            Err(OpenError::RangeNotSatisfiable) => {
                // The server's file is not longer than the part: the part is
                // not a beginning of it. Start over.
                let _ = std::fs::remove_file(&part);
                return Err(TryEnd::Broken("the server refused to resume".to_string()));
            }
            Err(OpenError::Failed(why)) => return Err(TryEnd::Broken(why)),
        };
        let append = match opened.partial_from {
            Some(start) if start == have => true,
            Some(start) => {
                // A range other than the one asked for: not usable here.
                let _ = std::fs::remove_file(&part);
                return Err(TryEnd::Broken(format!("the server resumed at {start}, not at {have}")));
            }
            // The whole file: the part is overwritten from its start.
            None => false,
        };
        if !append && have > 0 {
            hasher = Sha256::new();
            have = 0;
            progress(0);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&part)
            .map_err(|err| fatal("cannot write", &err))?;
        file.set_len(have).map_err(|err| fatal("cannot write", &err))?;
        let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
        out.seek(SeekFrom::Start(have)).map_err(|err| fatal("cannot write", &err))?;
        let mut body = opened.body;
        let end = loop {
            if cancel.load(Ordering::Relaxed) {
                break Some(TryEnd::Cancelled);
            }
            let chunk = match crate::updater::within(idle, body.next()).await {
                Err(why) => break Some(TryEnd::Broken(why)),
                Ok(None) => break None,
                Ok(Some(Err(why))) => break Some(TryEnd::Broken(why)),
                Ok(Some(Ok(c))) => c,
            };
            if have + chunk.len() as u64 > e.size {
                drop(out);
                let _ = std::fs::remove_file(&part);
                return Err(TryEnd::Fatal(format!("{} is larger than the list says", e.name)));
            }
            out.write_all(&chunk).map_err(|err| fatal("cannot write", &err))?;
            hasher.update(&chunk);
            have += chunk.len() as u64;
            progress(have);
        };
        // What arrived stays in the part, whatever stopped the loop.
        let file = out.into_inner().map_err(|err| fatal("cannot write", &err.error()))?;
        if let Some(end) = end {
            return Err(end);
        }
        if have < e.size {
            return Err(TryEnd::Broken("the connection closed early".to_string()));
        }
        file.sync_all().map_err(|err| fatal("cannot flush", &err))?;
    }

    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != e.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(TryEnd::Fatal(format!("{} arrived damaged (its checksum does not match)", e.name)));
    }
    std::fs::rename(&part, dir.join(&e.name)).map_err(|err| fatal("cannot rename", &err))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// How the fake server behaves on the next request.
    #[derive(Clone, Copy, PartialEq, Debug)]
    pub(crate) enum Mode {
        Honest,
        /// Ignores Range and sends the whole file (200).
        IgnoresRange,
        /// Answers a range with a different start.
        WrongStart,
        /// Sends `n` bytes, then the connection breaks.
        BreaksAfter(usize),
        /// Sends `n` bytes, then goes silent.
        StallsAfter(usize),
        /// 416.
        NotSatisfiable,
    }

    pub(crate) struct Fake {
        pub files: std::collections::HashMap<String, Vec<u8>>,
        /// One mode per request, `Honest` once they run out.
        pub modes: Mutex<Vec<Mode>>,
        /// `from` of every request.
        pub asked: Mutex<Vec<u64>>,
        /// Chunk size of the bodies.
        pub chunk: usize,
        /// Set when the body has sent this many bytes (a cancel from outside).
        pub cancel_at: Mutex<Option<(usize, Arc<AtomicBool>)>>,
    }

    impl Fake {
        pub(crate) fn new(files: &[(&str, Vec<u8>)]) -> Fake {
            Fake {
                files: files.iter().map(|(n, d)| (n.to_string(), d.clone())).collect(),
                modes: Mutex::new(Vec::new()),
                asked: Mutex::new(Vec::new()),
                chunk: 7,
                cancel_at: Mutex::new(None),
            }
        }
        pub(crate) fn then(self, modes: &[Mode]) -> Fake {
            *self.modes.lock().unwrap() = modes.to_vec();
            self
        }
    }

    impl Source for Fake {
        fn open<'a>(&'a self, name: &'a str, from: u64) -> BoxFuture<'a, Result<Opened, OpenError>> {
            Box::pin(async move {
                self.asked.lock().unwrap().push(from);
                let mode = {
                    let mut m = self.modes.lock().unwrap();
                    if m.is_empty() { Mode::Honest } else { m.remove(0) }
                };
                let data = self.files.get(name).ok_or_else(|| OpenError::Failed(format!("{name}: HTTP 404")))?;
                let (partial_from, start) = match mode {
                    _ if from == 0 => (None, 0),
                    Mode::IgnoresRange => (None, 0),
                    Mode::WrongStart => (Some(from - 1), from as usize - 1),
                    Mode::NotSatisfiable => return Err(OpenError::RangeNotSatisfiable),
                    _ if from as usize >= data.len() => return Err(OpenError::RangeNotSatisfiable),
                    _ => (Some(from), from as usize),
                };
                let mut bytes = data[start..].to_vec();
                let tail: Option<Result<Vec<u8>, String>> = match mode {
                    Mode::BreaksAfter(n) => {
                        bytes.truncate(n);
                        Some(Err("connection reset".to_string()))
                    }
                    Mode::StallsAfter(n) => {
                        bytes.truncate(n);
                        None
                    }
                    _ => None,
                };
                let stall = matches!(mode, Mode::StallsAfter(_));
                let cancel = self.cancel_at.lock().unwrap().take();
                let mut items: Vec<Result<Vec<u8>, String>> = Vec::new();
                let mut sent = 0usize;
                let mut chunks = bytes.chunks(self.chunk.max(1)).map(|c| c.to_vec()).collect::<Vec<_>>().into_iter();
                for c in chunks.by_ref() {
                    sent += c.len();
                    items.push(Ok(c));
                    if let Some((at, _)) = &cancel {
                        if sent >= *at {
                            break;
                        }
                    }
                }
                let rest: Vec<Result<Vec<u8>, String>> = chunks.map(Ok).collect();
                items.extend(tail);
                let first = futures_util::stream::iter(items);
                let body: Body = if let Some((_, flag)) = cancel {
                    // The flag goes up once the first part has been taken.
                    let raise = futures_util::stream::once(async move {
                        flag.store(true, Ordering::Relaxed);
                        Ok(Vec::new())
                    });
                    first.chain(raise).chain(futures_util::stream::iter(rest)).boxed()
                } else if stall {
                    first.chain(futures_util::stream::pending()).boxed()
                } else {
                    first.boxed()
                };
                Ok(Opened { partial_from, body })
            })
        }
    }

    pub(crate) fn sha_hex(d: &[u8]) -> String {
        Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    pub(crate) fn entry(name: &str, d: &[u8]) -> Entry {
        Entry { name: name.into(), size: d.len() as u64, sha256: sha_hex(d), taps: 5_000, rate: 88_200, minimum: false }
    }

    pub(crate) const FAST: Timing = Timing { idle: Duration::from_millis(50), retries: 3, pause: Duration::ZERO };

    const NAME: &str = "fir_5k_88200_linear_phase.npy";

    async fn run(fake: &Fake, dir: &Path, e: &Entry, cancel: &AtomicBool) -> (Result<(), FetchError>, Vec<u64>) {
        let mut seen = Vec::new();
        let r = fetch_file(fake, dir, e, cancel, FAST, &mut |n| seen.push(n)).await;
        (r, seen)
    }

    #[tokio::test]
    async fn a_fresh_download_lands_whole_and_the_part_is_gone() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let fake = Fake::new(&[(NAME, d.clone())]);
        let (r, seen) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
        assert!(!part_path(t.path(), NAME).exists());
        assert_eq!(*seen.last().unwrap(), 100);
    }

    #[tokio::test]
    async fn a_broken_connection_resumes_from_the_part() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let fake = Fake::new(&[(NAME, d.clone())]).then(&[Mode::BreaksAfter(30), Mode::StallsAfter(20)]);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
        assert_eq!(*fake.asked.lock().unwrap(), vec![0, 30, 50]);
    }

    #[tokio::test]
    async fn a_part_from_before_is_rehashed_and_resumed() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        std::fs::write(part_path(t.path(), NAME), &d[..60]).unwrap();
        let fake = Fake::new(&[(NAME, d.clone())]);
        let (r, seen) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(seen[0], 60);
        assert_eq!(*fake.asked.lock().unwrap(), vec![60]);
        assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_restarts_the_file() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        std::fs::write(part_path(t.path(), NAME), &d[..60]).unwrap();
        let fake = Fake::new(&[(NAME, d.clone())]).then(&[Mode::IgnoresRange]);
        let (r, seen) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(seen[1], 0);
        assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
    }

    #[tokio::test]
    async fn a_wrong_range_or_416_starts_over() {
        for mode in [Mode::WrongStart, Mode::NotSatisfiable] {
            let t = tempfile::tempdir().unwrap();
            let d = data(100);
            std::fs::write(part_path(t.path(), NAME), &d[..60]).unwrap();
            let fake = Fake::new(&[(NAME, d.clone())]).then(&[mode]);
            let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
            assert_eq!(r, Ok(()), "{mode:?}");
            assert_eq!(*fake.asked.lock().unwrap(), vec![60, 0], "{mode:?}");
            assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
        }
    }

    #[tokio::test]
    async fn a_damaged_file_is_deleted_and_never_renamed() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let mut e = entry(NAME, &d);
        e.sha256 = sha_hex(b"something else");
        let fake = Fake::new(&[(NAME, d.clone())]);
        let (r, _) = run(&fake, t.path(), &e, &AtomicBool::new(false)).await;
        assert!(matches!(r, Err(FetchError::Failed(ref w)) if w.contains("damaged")));
        assert!(!t.path().join(NAME).exists());
        assert!(!part_path(t.path(), NAME).exists());
    }

    #[tokio::test]
    async fn a_file_longer_than_listed_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let fake = Fake::new(&[(NAME, d.clone())]);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d[..50]), &AtomicBool::new(false)).await;
        assert!(matches!(r, Err(FetchError::Failed(ref w)) if w.contains("larger")));
        assert!(!part_path(t.path(), NAME).exists());
    }

    #[tokio::test]
    async fn a_part_longer_than_the_file_is_dropped() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        std::fs::write(part_path(t.path(), NAME), data(120)).unwrap();
        let fake = Fake::new(&[(NAME, d.clone())]);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(*fake.asked.lock().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn a_whole_part_is_checked_without_asking_the_server() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        std::fs::write(part_path(t.path(), NAME), &d).unwrap();
        let fake = Fake::new(&[]);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert_eq!(r, Ok(()));
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_keeps_the_part_and_the_next_run_finishes_it() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let cancel = Arc::new(AtomicBool::new(false));
        let fake = Fake::new(&[(NAME, d.clone())]);
        *fake.cancel_at.lock().unwrap() = Some((35, cancel.clone()));
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &cancel).await;
        assert_eq!(r, Err(FetchError::Cancelled));
        assert_eq!(std::fs::read(part_path(t.path(), NAME)).unwrap(), d[..35]);
        assert!(!t.path().join(NAME).exists());

        cancel.store(false, Ordering::Relaxed);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &cancel).await;
        assert_eq!(r, Ok(()));
        assert_eq!(*fake.asked.lock().unwrap(), vec![0, 35]);
        assert_eq!(std::fs::read(t.path().join(NAME)).unwrap(), d);
    }

    #[tokio::test]
    async fn a_server_that_keeps_breaking_gives_up_after_the_retries() {
        let t = tempfile::tempdir().unwrap();
        let d = data(100);
        let fake = Fake::new(&[(NAME, d.clone())]).then(&[Mode::StallsAfter(0); 4]);
        let (r, _) = run(&fake, t.path(), &entry(NAME, &d), &AtomicBool::new(false)).await;
        assert!(matches!(r, Err(FetchError::Failed(ref w)) if w.contains("sent nothing")));
        assert_eq!(fake.asked.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn small_files_are_capped() {
        let fake = Fake::new(&[("a", data(10))]);
        assert_eq!(fetch_small(&fake, "a", 10, FAST.idle).await.unwrap(), data(10));
        assert!(fetch_small(&fake, "a", 9, FAST.idle).await.unwrap_err().contains("larger"));
        assert!(fetch_small(&fake, "b", 9, FAST.idle).await.unwrap_err().contains("404"));
    }

    #[test]
    fn content_range_is_read() {
        assert_eq!(content_range_start("bytes 60-99/100"), Some(60));
        assert_eq!(content_range_start("bytes */100"), None);
        assert_eq!(content_range_start("items 1-2/3"), None);
    }
}
