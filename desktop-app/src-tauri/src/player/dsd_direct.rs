//! DSD files played as they are: BIT-PERFECT with a DSF or DSDIFF file sends
//! the file's one-bit data to the DAC over DoP at the file's own rate,
//! untouched — no volume, no fade, no dither.
//!
//! The data travels the player's usual road. A chain's stage streams the
//! file's bytes (`DsdStage`, never the whole file in memory, never as f64
//! planes) and writes every DoP frame's two bytes per channel into the
//! timeline as one exact f64 word (`word`) at the DoP carrier rate; the
//! output thread turns the words back into the same bytes (`bytes`) and
//! packs them with their markers (`OutputMode::DsdDirect`). Seek, position
//! and the gapless hand-over then work in carrier frames as for any stream.
//! The word is the 16 bits XOR `DSD_SILENCE` in both bytes, so 0.0 — what the
//! timeline reads where nothing was written, and past a track's end — is
//! DSD silence, never a full-scale pattern.
//!
//! A sample-wise operation on words (a fade, a crossfade, the volume) would
//! make noise of them, so every transition of such a stream is DSD silence:
//! the render thread splices without crossfades and the output sends a
//! period of 0x69 in place of its ramps (`Stage::is_raw_dsd`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::audio::dsd::dsf::DsdRead;
use crate::audio::dsd::{DsdRate, DSD_SILENCE};

use super::convolver::SourceBuf;
use super::stages::Stage;

/// The two silence bytes, as one 16-bit value.
const SILENCE_WORD: u16 = u16::from_be_bytes([DSD_SILENCE, DSD_SILENCE]);

/// Two DSD bytes of one channel (the earlier first) as a timeline sample:
/// k / 32768 with k = their 16 bits XOR 0x6969 read as i16 — exact in f64,
/// and DSD silence is 0.0.
#[inline]
pub fn word(first: u8, second: u8) -> f64 {
    ((u16::from_be_bytes([first, second]) ^ SILENCE_WORD) as i16) as f64 / 32_768.0
}

/// A timeline sample back to its two DSD bytes (`word`'s inverse). A value
/// that is no word — none should reach here — reads as its nearest one.
#[inline]
pub fn bytes(x: f64) -> [u8; 2] {
    let k = (x * 32_768.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
    ((k as u16) ^ SILENCE_WORD).to_be_bytes()
}

/// The DoP frames `bytes` bytes per channel make: two bytes a frame, the
/// last one half silence when they are odd.
pub fn frames_of(bytes: u64) -> u64 {
    bytes.div_ceil(2)
}

/// The highest DSD rate DoP carries here: DSD256's 705.6 / 768 kHz carrier
/// is the fastest a WASAPI device is opened at. Faster files play as PCM.
pub const MAX_MULTIPLE: u32 = 256;

/// A DSD file a Direct variant can play as it is.
pub struct DsdFile {
    pub rate: DsdRate,
    /// One-bit samples per channel.
    samples: u64,
    /// The file opened once; each chain streams it through its own handle
    /// (`DsdRead::reopen`).
    reader: Mutex<Box<dyn DsdRead>>,
    /// Its PCM decoded behind the music (`decode_behind`), for a variant
    /// that started without it.
    pcm: Mutex<Pcm>,
}

/// Where a file's PCM decoded behind the music stands.
enum Pcm {
    /// Not asked for: the variant decoded it before playing.
    Idle,
    Decoding,
    Ready(Arc<SourceBuf>),
    /// The decode failed (said in the log).
    Failed,
}

/// One file's PCM is decoded behind the music at a time: skipping through
/// a list leaves no pile of decodes running at once, and one whose variant
/// has gone meanwhile is not started at all.
static BEHIND_TURN: Mutex<()> = Mutex::new(());

/// The threads a decode behind the music takes: a quarter of the cores, four
/// at most — the music already plays, it need not be fast.
fn behind_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores / 4).clamp(1, 4)
}

/// A decode thread behind the music runs below normal priority.
fn below_normal() {
    super::source_stages::set_thread_priority(-1);
}

impl DsdFile {
    /// `path` when it is a DSD file, None for any other.
    pub fn open(path: &Path) -> Result<Option<DsdFile>, String> {
        if crate::audio::dsd::dsf::sniff(path).is_none() {
            return Ok(None);
        }
        let reader = crate::audio::dsd::dsf::open(path)?;
        Ok(Some(Self::from_reader(reader)))
    }

    pub fn from_reader(reader: Box<dyn DsdRead>) -> DsdFile {
        let info = reader.info();
        DsdFile {
            rate: info.rate,
            samples: info.samples,
            reader: Mutex::new(reader),
            pcm: Mutex::new(Pcm::Idle),
        }
    }

    /// The rate its PCM decodes to: 88.2 kHz, 96 kHz for the 48 kHz family.
    pub fn pcm_rate(&self) -> u32 {
        crate::audio::dsd::decimate::pcm_rate(self.rate)
    }

    /// Frames a DSD-as-is chain of it plays: two bytes a frame per channel.
    pub fn frames(&self) -> u64 {
        frames_of(self.samples.div_ceil(8))
    }

    /// Its PCM decoded on a thread of its own, behind the music: on a few
    /// cores, below normal priority, one file at a time (`BEHIND_TURN`).
    /// `pcm` / `pcm_now` give it once it is there, and `done` is called with
    /// it on that thread. Asked for once; later calls do nothing.
    pub fn decode_behind(self: &Arc<Self>, done: impl FnOnce(Arc<SourceBuf>) + Send + 'static) {
        self.decode_behind_with(|r| crate::audio::dsd::decimate::decode_all_with(r, behind_threads(), below_normal), done);
    }

    /// `decode_behind` with the decoder given (tests: a slow one).
    pub fn decode_behind_with<D>(self: &Arc<Self>, decode: D, done: impl FnOnce(Arc<SourceBuf>) + Send + 'static)
    where
        D: FnOnce(&mut dyn DsdRead) -> Result<(Vec<f64>, Vec<f64>, u32), String> + Send + 'static,
    {
        {
            let mut g = self.pcm.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(*g, Pcm::Idle) {
                return;
            }
            *g = Pcm::Decoding;
        }
        let file = Arc::downgrade(self);
        let label = self.label();
        let spawned = std::thread::Builder::new().name("aura-dsd-pcm".into()).spawn(move || {
            below_normal();
            let _turn = BEHIND_TURN.lock().unwrap_or_else(|e| e.into_inner());
            // Gone while it waited for its turn: nobody needs its PCM.
            let Some(f) = file.upgrade() else { return };
            let reader = f.reader.lock().unwrap_or_else(|e| e.into_inner()).reopen();
            // Not held while decoding, so a file left meanwhile can go.
            drop(f);
            let t0 = std::time::Instant::now();
            let got = reader.and_then(|mut r| decode(&mut *r)).and_then(|(l, r, rate)| {
                if l.is_empty() {
                    Err("No audio samples decoded".to_string())
                } else {
                    Ok(Arc::new(SourceBuf { l, r, rate }))
                }
            });
            match &got {
                Ok(_) => crate::aelog!("[PLAYER] {label} as it is: its PCM decoded behind the music in {:.1} s", t0.elapsed().as_secs_f64()),
                Err(e) => crate::aelog!("[PLAYER] {label} as it is: its PCM could not be decoded: {e}"),
            }
            let Some(f) = file.upgrade() else { return };
            f.settle(got);
            // The PCM kept: decoded here, or by `pcm_now` meanwhile.
            let src = f.pcm();
            drop(f);
            if let Some(src) = src {
                done(src);
            }
        });
        if let Err(e) = spawned {
            self.settle(Err(format!("no thread to decode its PCM: {e}")));
        }
    }

    /// The PCM a decode brought, or why there is none. One already there
    /// stays: the decode behind the music finding `pcm_now`'s.
    fn settle(&self, got: Result<Arc<SourceBuf>, String>) {
        let mut g = self.pcm.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*g, Pcm::Ready(_)) {
            return;
        }
        *g = match got {
            Ok(s) => Pcm::Ready(s),
            Err(_) => Pcm::Failed,
        };
    }

    /// Its PCM decoded behind the music, once there.
    pub fn pcm(&self) -> Option<Arc<SourceBuf>> {
        match &*self.pcm.lock().unwrap_or_else(|e| e.into_inner()) {
            Pcm::Ready(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Its PCM now: the one decoded behind the music if it is there, else
    /// decoded here at once on every core, as before playing — a device
    /// that refuses DoP cannot wait for the slow decode behind the music
    /// (a minute of DSD256 takes it ~24 s). That decode then finds it done.
    pub fn pcm_now(&self) -> Result<Arc<SourceBuf>, String> {
        if let Some(s) = self.pcm() {
            return Ok(s);
        }
        let mut reader = self.reader.lock().unwrap_or_else(|e| e.into_inner()).reopen()?;
        let (l, r, rate) = crate::audio::dsd::decimate::decode_all(&mut *reader)?;
        if l.is_empty() {
            return Err("No audio samples decoded".to_string());
        }
        self.settle(Ok(Arc::new(SourceBuf { l, r, rate })));
        self.pcm().ok_or_else(|| "its PCM could not be kept".to_string())
    }

    /// "DSD64", "DSD256 (48k)".
    pub fn label(&self) -> String {
        label(self.rate)
    }

    /// Whether DoP can carry this file at all (its rate up to DSD256).
    pub fn carried(&self) -> bool {
        self.rate.multiple <= MAX_MULTIPLE
    }
}

/// The DSD rate whose DoP carrier is `carrier` (176 400 → DSD64), None for
/// a rate that is no carrier.
pub fn rate_of_carrier(carrier: u32) -> Option<DsdRate> {
    let bits = carrier.checked_mul(16)?;
    [44_100, 48_000].into_iter().find_map(|base| (bits % base == 0).then(|| DsdRate::new(base, bits / base)).flatten())
}

/// A DSD rate as the strip names it: "DSD64", "DSD256 (48k)".
pub fn label(r: DsdRate) -> String {
    if r.base == 44_100 {
        format!("DSD{}", r.multiple)
    } else {
        format!("DSD{} (48k)", r.multiple)
    }
}

/// A Direct chain's stage for a DSD file played as it is: the file's bytes
/// read as the stream goes, two a frame per channel, written as words. A
/// mono file's channel goes to both sides.
pub struct DsdStage {
    reader: Box<dyn DsdRead>,
    channels: usize,
    bytes: u64,
    /// Frame of the next `read`.
    pos: u64,
    /// The reader's byte position; u64::MAX before the first read.
    at: u64,
    buf: [Vec<u8>; 2],
    /// The file could not be read further: silence from there on.
    failed: bool,
}

impl DsdStage {
    /// The stage at frame `start` (a byte boundary: frame k starts at byte 2k).
    pub fn new(file: &DsdFile, start: u64) -> Result<DsdStage, String> {
        let reader = file.reader.lock().unwrap_or_else(|e| e.into_inner()).reopen()?;
        Ok(Self::from_reader(reader, start))
    }

    pub fn from_reader(reader: Box<dyn DsdRead>, start: u64) -> DsdStage {
        let info = reader.info();
        let (channels, bytes) = (info.channels.clamp(1, 2), info.bytes());
        DsdStage { reader, channels, bytes, pos: start.min(frames_of(bytes)), at: u64::MAX, buf: [Vec::new(), Vec::new()], failed: false }
    }

    /// Bytes `from..from + n` of every channel into `buf`; how many it got.
    fn fill(&mut self, from: u64, n: usize) -> usize {
        if self.at != from {
            if let Err(e) = self.reader.seek(from) {
                return self.fail(&e);
            }
            self.at = from;
        }
        for b in &mut self.buf {
            b.resize(n, DSD_SILENCE);
        }
        let mut got = 0;
        while got < n {
            let [l, r] = &mut self.buf;
            let mut views: Vec<&mut [u8]> = vec![&mut l[got..], &mut r[got..]];
            views.truncate(self.channels);
            match self.reader.read(&mut views) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) => {
                    self.at = u64::MAX;
                    self.fail(&e);
                    return got;
                }
            }
        }
        self.at = from + got as u64;
        got
    }

    fn fail(&mut self, e: &str) -> usize {
        crate::aelog!("[PLAYER] DSD as-is: {e} - silence from here");
        self.failed = true;
        0
    }
}

impl Stage for DsdStage {
    fn read(&mut self, out_l: &mut [f64], out_r: &mut [f64]) -> usize {
        let n = out_l.len();
        let total = frames_of(self.bytes);
        let mut inside = (total.saturating_sub(self.pos) as usize).min(n);
        if inside > 0 && !self.failed {
            let from = 2 * self.pos;
            let want = ((self.bytes - from) as usize).min(2 * inside);
            let got = self.fill(from, want);
            if got < want {
                inside = got.div_ceil(2);
            }
            // An odd last byte: the frame's second byte is silence.
            for b in &mut self.buf {
                b.truncate(got);
                b.resize(2 * inside, DSD_SILENCE);
            }
            let r = if self.channels == 2 { 1 } else { 0 };
            for i in 0..inside {
                out_l[i] = word(self.buf[0][2 * i], self.buf[0][2 * i + 1]);
                out_r[i] = word(self.buf[r][2 * i], self.buf[r][2 * i + 1]);
            }
        } else {
            inside = 0;
        }
        out_l[inside..].fill(0.0);
        out_r[inside..].fill(0.0);
        self.pos += n as u64;
        inside
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn total(&self) -> u64 {
        frames_of(self.bytes)
    }

    fn is_raw_dsd(&self) -> bool {
        true
    }
}

// ── The sanity guard ─────────────────────────────────────────────────────────

/// The guard's window: 1 ms of one-bit samples per channel (DSD64 2 822,
/// DSD256 11 290). Long enough that a noise-shaped modulator's quantisation
/// noise — nearly all of it far above the audio band — averages out of the
/// density, short enough that a stuck pattern is caught within a period.
pub const GUARD_WINDOW_S: f64 = 0.001;

/// The ones density a window must stay inside. DSD's 0 dB is 50 %
/// modulation (density 25–75 %); SACD allows peaks to +3 dB, 70.7 %
/// modulation (14.6–85.4 %). A window's density is the music's level over
/// that millisecond — never beyond its peak — plus the noise left after the
/// average (well under 1 % for any noise-shaped modulator), so a valid
/// stream stays inside 14–86 %; 12 / 88 % leaves two points more. A stuck
/// byte (0x00, 0xFF), PCM silence or a constant offset sit outside.
pub const GUARD_DENSITY_LOW: f64 = 0.12;
pub const GUARD_DENSITY_HIGH: f64 = 0.88;

/// PCM read as DSD: its high bytes are 0x00 or 0xFF (sign extension) while
/// the low ones vary, so the density can look like music's while whole
/// bytes of one bit are everywhere. Random bits at density p make such a
/// byte with probability p⁸ + (1 − p)⁸ (0.8 % at 50 %, 10 % at 75 %); a
/// noise-shaped modulator spreads its ones more evenly still, so it makes
/// fewer. More than twice the random share — and more than a quarter of the
/// window's bytes — is not DSD.
pub const GUARD_SOLID_BYTES: f64 = 0.25;

/// The share of one-bit-only bytes (0x00, 0xFF) a window at density `p`
/// may hold.
fn solid_limit(p: f64) -> f64 {
    (2.0 * (p.powi(8) + (1.0 - p).powi(8))).max(GUARD_SOLID_BYTES)
}

/// Watches the bytes a DSD-as-is stream sends, window by window per
/// channel; once they stop looking like DSD it stays tripped (the output
/// sends silence) until it is reset where the next track starts.
pub struct DsdGuard {
    window: usize,
    seen: usize,
    ones: [u32; 2],
    solid: [u32; 2],
    tripped: bool,
}

impl DsdGuard {
    pub fn new(rate: DsdRate) -> DsdGuard {
        let window = ((rate.bit_rate() as f64 * GUARD_WINDOW_S) / 8.0).round().max(1.0) as usize;
        DsdGuard { window, seen: 0, ones: [0; 2], solid: [0; 2], tripped: false }
    }

    pub fn tripped(&self) -> bool {
        self.tripped
    }

    /// Starts afresh, as new: a new track's bytes are judged on their own.
    pub fn reset(&mut self) {
        *self = DsdGuard { window: self.window, seen: 0, ones: [0; 2], solid: [0; 2], tripped: false };
    }

    /// A window's verdict: true when it is not DSD.
    fn bad(ones: u32, solid: u32, bytes: usize) -> bool {
        let density = ones as f64 / (8 * bytes) as f64;
        !(GUARD_DENSITY_LOW..=GUARD_DENSITY_HIGH).contains(&density) || solid as f64 / bytes as f64 > solid_limit(density)
    }

    /// The next bytes of both channels (the same length). Returns true when
    /// the guard has tripped, now or before.
    pub fn check(&mut self, l: &[u8], r: &[u8]) -> bool {
        if self.tripped {
            return true;
        }
        for (&a, &b) in l.iter().zip(r) {
            for (c, x) in [a, b].into_iter().enumerate() {
                self.ones[c] += x.count_ones();
                self.solid[c] += (x == 0 || x == 0xFF) as u32;
            }
            self.seen += 1;
            if self.seen == self.window {
                if (0..2).any(|c| Self::bad(self.ones[c], self.solid[c], self.window)) {
                    self.tripped = true;
                    return true;
                }
                self.seen = 0;
                self.ones = [0; 2];
                self.solid = [0; 2];
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsd::decimate::test_signal::modulate;
    use crate::audio::dsd::dsf::test_files::{dsf, noise, write};
    use crate::audio::dsd::dff::test_files::dff;
    use std::sync::Arc;

    #[test]
    fn words_carry_every_byte_pair_exactly_and_zero_is_silence() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let w = word(a, b);
                assert_eq!(bytes(w), [a, b]);
                assert!(w.abs() <= 1.0);
            }
        }
        assert_eq!(word(DSD_SILENCE, DSD_SILENCE), 0.0);
        assert_eq!(bytes(0.0), [DSD_SILENCE, DSD_SILENCE]);
    }

    /// A stage's words back to bytes, channel by channel.
    fn unword(l: &[f64], r: &[f64]) -> (Vec<u8>, Vec<u8>) {
        (l.iter().flat_map(|&x| bytes(x)).collect(), r.iter().flat_map(|&x| bytes(x)).collect())
    }

    #[test]
    fn a_dsf_and_a_dff_file_stream_their_bytes_as_they_are_from_any_frame() {
        let n = 10_001; // odd: the last frame is half silence
        let (l, r) = (noise(n, 1), noise(n, 2));
        let samples = n as u64 * 8;
        let files = [
            ("a.dsf", dsf(&[l.clone(), r.clone()], 2_822_400, samples, 4096, false, None)),
            ("b.dff", dff(&[l.clone(), r.clone()], 2_822_400, b"DSD ", None, None)),
        ];
        for (name, data) in files {
            let (_dir, path) = write(&data, name);
            let f = DsdFile::open(&path).unwrap().expect("a DSD file");
            assert_eq!((f.rate, DsdStage::new(&f, 0).unwrap().total()), (DsdRate::DSD64, 5_001));
            for start in [0u64, 1, 2_047, 4_999, 5_001] {
                let mut s = DsdStage::new(&f, start).unwrap();
                let (mut ol, mut or) = (vec![1.0; 6_000], vec![1.0; 6_000]);
                let mut inside = 0;
                for k in 0..3 {
                    let part = 2_000;
                    inside += s.read(&mut ol[k * part..(k + 1) * part], &mut or[k * part..(k + 1) * part]);
                }
                let want = (5_001 - start) as usize;
                assert_eq!(inside, want.min(6_000), "{name} from {start}");
                let (bl, br) = unword(&ol[..want], &or[..want]);
                let from = 2 * start as usize;
                let mut el = l.get(from..).unwrap_or_default().to_vec();
                let mut er = r.get(from..).unwrap_or_default().to_vec();
                if want > 0 {
                    el.push(DSD_SILENCE);
                    er.push(DSD_SILENCE);
                }
                assert_eq!(bl, el, "{name} from {start}: left");
                assert_eq!(br, er, "{name} from {start}: right");
                assert!(ol[want..].iter().chain(&or[want..]).all(|&x| x == 0.0), "past the end: silence");
                assert_eq!(s.position(), start + 6_000);
            }
        }
    }

    #[test]
    fn a_mono_file_plays_on_both_sides() {
        let one = noise(4_000, 5);
        let (_dir, path) = write(&dsf(&[one.clone()], 2_822_400, 32_000, 4096, false, None), "m.dsf");
        let f = DsdFile::open(&path).unwrap().unwrap();
        let mut s = DsdStage::new(&f, 0).unwrap();
        let (mut l, mut r) = (vec![0.0; 2_000], vec![0.0; 2_000]);
        assert_eq!(s.read(&mut l, &mut r), 2_000);
        assert_eq!(l, r);
        assert_eq!(unword(&l, &r).0, one);
    }

    #[test]
    fn a_stage_holds_a_buffer_not_the_file() {
        // 1 MB per channel streamed in reads of 4096 frames: the stage's
        // buffers stay at one read's bytes.
        let n = 1 << 20;
        let (_dir, path) = write(&dsf(&[noise(n, 3), noise(n, 4)], 2_822_400, n as u64 * 8, 4096, false, None), "big.dsf");
        let f = DsdFile::open(&path).unwrap().unwrap();
        let mut s = DsdStage::new(&f, 0).unwrap();
        let (mut l, mut r) = (vec![0.0; 4096], vec![0.0; 4096]);
        while s.read(&mut l, &mut r) > 0 {}
        assert!(s.buf.iter().all(|b| b.capacity() <= 2 * 4096 + 64), "{:?}", s.buf.iter().map(|b| b.capacity()).collect::<Vec<_>>());
    }

    /// BIT-PERFECT with a DSD file: the variant plays it at its DoP carrier
    /// from the file itself — its PCM (for the analyzer and a device without
    /// DoP) at the decimator's rate, never f64 planes at the carrier — and
    /// its chain streams the bytes; the PCM twin starts at the same place.
    /// A DSD512 file plays as its PCM.
    #[test]
    fn a_dsd_file_s_direct_variant_streams_the_file_and_keeps_only_its_pcm() {
        use crate::player::chain::{build_chain, pcm_twin, plays_raw, prepare_variant, Resources};
        use crate::player::settings::{Mode, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        let n = 88_200; // a quarter second of DSD64
        let (l, r) = (sine(0.5, 1_000.0, DsdRate::DSD64, 0.25), sine(-0.5, 1_000.0, DsdRate::DSD64, 0.25));
        assert_eq!(l.len(), n);
        let (_dir, path) = write(&dsf(&[l.clone(), r.clone()], 2_822_400, n as u64 * 8, 4096, false, None), "t.dsf");
        let s = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
        let v = Arc::new(prepare_variant(&path, &s, false, &AtomicBool::new(false)).unwrap());
        assert!(plays_raw(&v));
        assert_eq!((v.out_rate, v.l, v.src.rate), (176_400, 2, 88_200));
        assert_eq!(v.src.len(), n / 4, "the PCM at 88.2 kHz, not planes at the carrier");
        let cache = tempfile::tempdir().unwrap();
        let res = Resources::new(cache.path().to_path_buf());
        let mut c = build_chain(&res, 1, "t", &v, &s, 1_000, None, None, 0).unwrap();
        assert!(c.stage.is_raw_dsd() && c.direct);
        assert_eq!((c.out_rate, c.l, c.start), (176_400, 2, 1_000));
        let (twin, pv) = pcm_twin(&c).expect("a twin");
        assert!(!twin.stage.is_raw_dsd() && !plays_raw(&pv));
        assert_eq!((twin.out_rate, twin.l, twin.start, twin.stage.position()), (88_200, 1, 500, 500));
        let (mut ol, mut or) = (vec![0.0; 64], vec![0.0; 64]);
        assert_eq!(c.stage.read(&mut ol, &mut or), 64);
        let (bl, br) = unword(&ol, &or);
        assert!(bl == l[2_000..2_128] && br == r[2_000..2_128]);

        let big = DsdRate::new(44_100, 512).unwrap();
        let (_dir5, p5) = write(&dsf(&[noise(4_096, 1)], big.bit_rate(), 4_096 * 8, 4096, false, None), "t5.dsf");
        let v5 = prepare_variant(&p5, &s, false, &AtomicBool::new(false)).unwrap();
        assert!(!plays_raw(&v5) && v5.dsd.as_ref().is_some_and(|f| !f.carried()));
        assert_eq!(v5.out_rate, v5.src.rate);
    }

    /// BIT-PERFECT with a DSD file DoP carries: its variant comes at once,
    /// without its PCM, and its chains stream the file from any frame (a
    /// seek) while the PCM is decoded behind the music — here by a decoder
    /// held until let go. The PCM twin does not wait for it: its PCM is
    /// decoded at once and kept for the file, and it plays from where the
    /// chain stood; the decode behind the music then hands that same PCM on.
    /// The PCM is the one decoded before playing, bit for bit, and so is the
    /// player's decode behind the music (one thread, below normal priority).
    #[test]
    fn a_dsd_file_plays_at_once_while_its_pcm_is_decoded_behind_the_music() {
        use crate::player::chain::{build_chain, pcm_pending, pcm_twin, plays_raw, prepare_direct_dsd, prepare_variant, twin_with_pcm, with_pcm, Resources};
        use crate::player::settings::{Mode, PlayerSettings};
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let n = 88_200;
        let (l, r) = (sine(0.5, 1_000.0, DsdRate::DSD64, 0.25), sine(-0.5, 1_000.0, DsdRate::DSD64, 0.25));
        let (_dir, path) = write(&dsf(&[l.clone(), r.clone()], 2_822_400, n as u64 * 8, 4096, false, None), "t.dsf");
        let s = PlayerSettings { mode: Mode::Direct, ..PlayerSettings::default() };
        let before = prepare_variant(&path, &s, false, &AtomicBool::new(false)).unwrap();
        let v = Arc::new(prepare_direct_dsd(&path).unwrap().expect("a DSD file DoP carries"));
        assert!(pcm_pending(&v) && plays_raw(&v) && !pcm_pending(&before));
        assert_eq!((v.out_rate, v.l, v.src.rate), (before.out_rate, before.l, before.src.rate));

        let (go, held) = channel::<()>();
        let (done_tx, done) = channel();
        let f = v.dsd.clone().unwrap();
        f.decode_behind_with(
            move |rd| {
                held.recv().ok();
                crate::audio::dsd::decimate::decode_all_with(rd, 1, below_normal)
            },
            move |src| {
                done_tx.send(src).ok();
            },
        );
        let cache = tempfile::tempdir().unwrap();
        let res = Resources::new(cache.path().to_path_buf());
        let mut c = build_chain(&res, 1, "t", &v, &s, 1_000, None, None, 0).unwrap();
        assert!(c.stage.is_raw_dsd());
        assert_eq!((c.stage.total(), c.start), (f.frames(), 1_000));
        let (mut ol, mut or) = (vec![0.0; 64], vec![0.0; 64]);
        assert_eq!(c.stage.read(&mut ol, &mut or), 64);
        let (bl, br) = unword(&ol, &or);
        assert!(bl == l[2_000..2_128] && br == r[2_000..2_128], "the file's bytes before its PCM exists");
        assert!(f.pcm().is_none(), "the PCM is still being decoded");

        // The twin cannot wait for the slow decode behind the music: its
        // PCM is decoded at once (on every core), still held behind.
        let (t2, p2) = pcm_twin(&c).expect("a twin");
        let (mut twin, tv) = twin_with_pcm(t2, p2).expect("the twin with its PCM");
        assert!(f.pcm().is_some(), "the file's PCM, kept for everyone");
        assert!(!pcm_pending(&tv) && !plays_raw(&tv) && tv.out_rate == 88_200);
        assert_eq!((twin.out_rate, twin.start, twin.stage.position()), (88_200, 500, 532));
        let (mut tl, mut tr) = (vec![0.0; 100], vec![0.0; 100]);
        assert_eq!(twin.stage.read(&mut tl, &mut tr), 100);
        let same = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits());
        assert!(same(&tl, &before.src.l[532..632]) && same(&tr, &before.src.r[532..632]));

        // Let go, the decode behind the music finds it done and hands it on.
        go.send(()).ok();
        let src = done.recv_timeout(Duration::from_secs(30)).expect("the decode is done");
        assert!(Arc::ptr_eq(&src, &f.pcm().unwrap()), "the PCM kept, not a second copy");
        assert!(same(&src.l, &before.src.l) && same(&src.r, &before.src.r) && src.rate == before.src.rate, "the PCM as before, bit for bit");
        // The player's decode behind the music (one thread here) makes the same samples.
        let mut rd = crate::audio::dsd::dsf::open(&path).unwrap();
        let (bl2, br2, rate2) = crate::audio::dsd::decimate::decode_all_with(&mut *rd, 1, below_normal).unwrap();
        assert!(same(&bl2, &before.src.l) && same(&br2, &before.src.r) && rate2 == before.src.rate);
        let w = with_pcm(&v, src);
        assert!(!pcm_pending(&w) && plays_raw(&w));
        assert_eq!((w.out_rate, w.l), (v.out_rate, v.l));
        // Asked for once.
        f.decode_behind_with(|_| panic!("a second decode"), |_| panic!("done twice"));
        assert!(f.pcm().is_some());
    }

    /// How long a decode behind the music takes (the player's threads and
    /// priority): a minute of DSD64 and of DSD256. A measurement, not a
    /// check: `cargo test --bins behind_the_music_takes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_decode_behind_the_music_takes() {
        for rate in [DsdRate::DSD64, DsdRate::DSD256] {
            let n = (rate.bit_rate() / 8 * 60) as usize;
            let (_dir, path) = write(&dsf(&[noise(n, 1), noise(n, 2)], rate.bit_rate(), n as u64 * 8, 4096, false, None), "m.dsf");
            let f = Arc::new(DsdFile::open(&path).unwrap().unwrap());
            let (tx, rx) = std::sync::mpsc::channel();
            let t0 = std::time::Instant::now();
            f.decode_behind(move |_| {
                tx.send(()).ok();
            });
            rx.recv_timeout(std::time::Duration::from_secs(600)).expect("decoded");
            println!("DSD{}: a minute decoded behind the music in {:.2} s on {} threads", rate.multiple, t0.elapsed().as_secs_f64(), behind_threads());
        }
    }

    fn sine(level: f64, hz: f64, rate: DsdRate, seconds: f64) -> Vec<u8> {
        let fs = rate.bit_rate() as f64;
        let bits = ((fs * seconds) as usize) / 8 * 8;
        modulate(|t| level * (2.0 * std::f64::consts::PI * hz * t).sin(), fs, bits)
    }

    fn trips(rate: DsdRate, l: &[u8], r: &[u8]) -> bool {
        let mut g = DsdGuard::new(rate);
        // In periods of 10 ms, as the output hands it over.
        let p = (rate.dop_carrier() / 100) as usize * 2;
        l.chunks(p).zip(r.chunks(p)).any(|(a, b)| g.check(a, b))
    }

    #[test]
    fn the_guard_passes_music_up_to_the_sacd_peak_and_silence() {
        for rate in [DsdRate::DSD64, DsdRate::DSD128, DsdRate::DSD256] {
            // 0 dB (50 % modulation) and the SACD peak (70 %), low and
            // high tones, and DSD silence.
            for (level, hz) in [(1.0, 20.0), (1.0, 1_000.0), (1.0, 15_000.0), (1.4, 20.0), (1.4, 5_000.0), (0.0, 1.0)] {
                let s = sine(level, hz, rate, 0.25);
                assert!(!trips(rate, &s, &s), "DSD{} {level} at {hz} Hz", rate.multiple);
            }
            let idle = vec![DSD_SILENCE; (rate.bit_rate() / 8 / 10) as usize];
            assert!(!trips(rate, &idle, &idle));
        }
    }

    #[test]
    fn the_guard_trips_on_stuck_bytes_and_pcm_read_as_dsd() {
        let rate = DsdRate::DSD64;
        let n = (rate.bit_rate() / 8 / 20) as usize; // 50 ms
        let good = sine(1.0, 440.0, rate, 0.05);
        for stuck in [0x00u8, 0xFF] {
            assert!(trips(rate, &good, &vec![stuck; n]), "{stuck:#x} on one channel");
        }
        // A quiet 16- and a 24-bit PCM sine's bytes read as DSD (big-endian
        // words, as a DoP frame's two bytes; a wrong decoder's output).
        let pcm16: Vec<u8> = (0..n / 2)
            .flat_map(|i| ((200.0 * (i as f64 * 0.01).sin()) as i16).to_be_bytes())
            .collect();
        assert!(trips(rate, &pcm16, &pcm16), "16-bit PCM");
        let pcm24: Vec<u8> = (0..n / 3)
            .flat_map(|i| {
                let v = (30_000.0 * (i as f64 * 0.003).sin()) as i32;
                [(v >> 16) as u8, (v >> 8) as u8, v as u8]
            })
            .collect();
        assert!(trips(rate, &pcm24, &pcm24), "24-bit PCM");
        // Once tripped it stays so, whatever follows — until it is reset
        // (the next track), after which good data passes and bad trips it
        // again.
        let mut g = DsdGuard::new(rate);
        assert!(g.check(&vec![0u8; n], &vec![0u8; n]));
        assert!(g.check(&good, &good) && g.tripped());
        g.reset();
        assert!(!g.tripped() && !g.check(&good, &good));
        assert!(g.check(&vec![0xFFu8; n], &good));
    }
}
