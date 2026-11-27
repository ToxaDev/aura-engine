//! The link between a player that plays into Aura Player and Aura Player:
//! a header and a ring of samples in memory both processes map.
//!
//! The *client* is the player's side — Aura Player's ASIO driver, loaded in
//! foobar2000, JRiver, HQPlayer… — and puts in what the player sends. The
//! *server* is Aura Player, which made the memory and takes the samples out
//! for its chain. One client at a time.
//!
//! * Samples travel as f64, interleaved, exactly as the player gave them:
//!   every integer format a driver takes (16, 24, 32 bits) and float32 are
//!   whole in an f64. The ring counts samples, not frames, so its arithmetic
//!   does not change with the channels.
//! * A *segment* is one run of the client's stream in one format: rate,
//!   channels, the player's own sample type. The client opens one at start
//!   and closes it at stop; the server reads a segment to its end before it
//!   is told the next one's format, so a rate change falls between samples.
//! * The server's clock is the clock: the client runs ahead of the reader by
//!   at most `target` samples and waits for room otherwise, so the player
//!   plays at the rate the DAC takes the chain's output — no drift between
//!   two clocks, nothing cut or repeated, no resampler.
//! * `Mode` is what the server does with the stream: *Listening* — it reads;
//!   *Held* — it does not (pause: the client waits, the player stands still,
//!   nothing is lost); *Released* — it does not want the stream: the client
//!   keeps time by itself and drops what the player sends. A client that
//!   stops hearing the server's heartbeat behaves as if Released.
//! * While the server's reader takes the ring at an even pace it says so
//!   (`Server::set_even`), and the client may call its player by a clock of
//!   its own (`clock`), a reserve in the ring for the reader's hiccups —
//!   rather than whenever the reader makes room. And the client keeps an
//!   account of its calls in the header — late ones, waits for room, how
//!   long the player's own work took (`CallStats`, `LateCall`) — for the
//!   server's log: what lies between the player and the link.
//!
//! Only the client writes `write` and the segments; only the server writes
//! `read`, `mode` and the beat. The platform's part — the named memory, the
//! events, whether the process holding the link is alive — is `win` on
//! Windows; `HeapRegion` and `LocalSignals` put both ends in one process,
//! for tests.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub mod clock;
#[cfg(windows)]
pub mod win;

/// "AURALNK1": the header is the link's, and fully written.
pub const MAGIC: u64 = u64::from_le_bytes(*b"AURALNK1");
/// The layout below; a client of another version does not connect.
pub const VERSION: u32 = 1;
/// Segment slots: the server reads every segment in order while it listens,
/// so only a client starting and stopping this many times inside one ring of
/// unread samples could pass it.
pub const SEGMENTS: usize = 16;
/// The client's name: UTF-8, cut to this many bytes.
pub const NAME_BYTES: usize = 64;
/// The ring starts at this offset from the header.
pub const RING_OFFSET: usize = 4096;
/// A segment that is still open has this end.
const OPEN: u64 = u64::MAX;

/// What the server does with the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Mode {
    /// The server does not want the stream: the client drops it.
    Released = 0,
    /// The server reads it.
    Listening = 1,
    /// The server holds it: the client waits for room.
    Held = 2,
}

impl Mode {
    fn from_u32(v: u32) -> Mode {
        match v {
            1 => Mode::Listening,
            2 => Mode::Held,
            _ => Mode::Released,
        }
    }
}

/// One run of the stream in one format.
#[repr(C)]
pub struct Slot {
    /// Odd while the client writes the slot.
    seq: AtomicU32,
    id: AtomicU32,
    rate_bits: AtomicU64,
    channels: AtomicU32,
    sample_type: AtomicU32,
    /// Ring position (in samples) of its first sample, and after its last.
    start: AtomicU64,
    end: AtomicU64,
}

/// A segment's format and place, as read from its slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub id: u32,
    pub rate: f64,
    pub channels: u32,
    /// The player's own sample format, as the driver names it (an ASIO
    /// sample type): for the record and for the depth a BIT-PERFECT
    /// output opens at; the samples themselves are f64.
    pub sample_type: u32,
    pub start: u64,
}

/// The memory both ends map, before the ring.
#[repr(C)]
pub struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    _pad0: AtomicU32,
    ring_samples: AtomicU64,
    // The server's.
    server_pid: AtomicU32,
    mode: AtomicU32,
    beat: AtomicU64,
    target: AtomicU64,
    latency_frames: AtomicU32,
    _pad1: AtomicU32,
    read: AtomicU64,
    // The client's.
    client_pid: AtomicU32,
    client_gen: AtomicU32,
    running: AtomicU32,
    seg_next: AtomicU32,
    write: AtomicU64,
    name_seq: AtomicU32,
    _pad2: AtomicU32,
    name: [AtomicU64; NAME_BYTES / 8],
    slots: [Slot; SEGMENTS],
    // Past the first version's header: zero in a link an older server made,
    // left alone by an older client.
    /// The server's: its reader takes the ring at an even pace, so the
    /// client may call its player by a clock of its own (`clock`).
    even: AtomicU32,
    // The client's account of its calls (`CallStats`, `LateCall`), from
    // when it took the link.
    period_us: AtomicU32,
    calls: AtomicU64,
    late: AtomicU64,
    waited_us: AtomicU64,
    longest_us: AtomicU32,
    clocked: AtomicU32,
    clock_ppm: AtomicU32,
    _pad3: AtomicU32,
    late_calls: [LateSlot; LATE_SLOTS],
}

/// Late calls the header keeps, the newest.
pub const LATE_SLOTS: usize = 16;

/// One late call in the header: `index` the count of late calls before it.
#[repr(C)]
struct LateSlot {
    seq: AtomicU32,
    index: AtomicU32,
    gap_us: AtomicU32,
    waited_us: AtomicU32,
    before_us: AtomicU32,
    fill: AtomicU32,
}

/// A call of the player that came late — over one and a half of its
/// periods after the one before, the player playing — as the client saw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LateCall {
    /// From the call before to this one, µs.
    pub gap_us: u32,
    /// Of that, how long the client waited for room in the ring, µs.
    pub waited_us: u32,
    /// How long the player's call before took (its own work), µs.
    pub before_us: u32,
    /// Samples in the ring just before the call.
    pub fill: u32,
}

/// The client's account of its calls since it took the link (all zero from
/// an older client).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallStats {
    /// The player's period: its block at its rate, µs.
    pub period_us: u32,
    pub calls: u64,
    pub late: u64,
    /// All the time it waited for room while the server listened, µs.
    pub waited_us: u64,
    /// The longest call of the player, µs.
    pub longest_us: u32,
    /// It calls the player by its own clock (`clock`), trimmed by this much
    /// against the system's.
    pub clocked: bool,
    pub clock_ppm: i32,
}

const _: () = assert!(std::mem::size_of::<Header>() <= RING_OFFSET);

/// Bytes a link with a ring of `ring_samples` takes.
pub const fn region_bytes(ring_samples: usize) -> usize {
    RING_OFFSET + ring_samples * 8
}

/// Memory both ends map: writable, 8-byte aligned, zeroed when made.
///
/// # Safety
/// `as_ptr` points at `len` bytes that stay mapped as long as the region
/// lives, aligned to 8.
pub unsafe trait Region: Send + Sync {
    fn as_ptr(&self) -> *mut u8;
    fn len(&self) -> usize;
}

/// Wake-ups between the ends: `data` the client gives (samples put in, a
/// segment opened or closed), `room` the server gives (samples taken, the
/// mode changed). Auto-reset: a wait takes the one signal given before it.
pub trait Signals: Send + Sync {
    fn notify_data(&self);
    fn wait_data(&self, timeout: Duration) -> bool;
    fn notify_room(&self);
    fn wait_room(&self, timeout: Duration) -> bool;
}

/// Why a client did not connect.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    /// No server: Aura Player is not running (or not listening for players).
    NoServer,
    /// The server is of another version of the link.
    Version(u32),
    /// Another player holds the link.
    Busy,
    /// The operating system refused (its error code).
    Os(u32),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NoServer => write!(f, "Aura Player isn't running"),
            LinkError::Version(v) => write!(f, "Aura Player speaks another version of the link ({v})"),
            LinkError::Busy => write!(f, "another player is using Aura Player's input"),
            LinkError::Os(e) => write!(f, "the system refused the link (error {e})"),
        }
    }
}

impl std::error::Error for LinkError {}

/// The header and ring over a region.
struct Link {
    region: Box<dyn Region>,
    signals: Box<dyn Signals>,
    ring_samples: u64,
}

impl Link {
    fn header(&self) -> &Header {
        // SAFETY: the region is at least RING_OFFSET bytes, 8-aligned, and
        // every field of the header is an atomic: any bytes are a valid
        // header, and both processes only touch it through atomics.
        unsafe { &*(self.region.as_ptr() as *const Header) }
    }

    fn slot(&self, i: u64) -> &AtomicU64 {
        let i = (i & (self.ring_samples - 1)) as usize;
        // SAFETY: i < ring_samples, and the ring is ring_samples atomics
        // after RING_OFFSET inside the region (checked when the link was
        // made or opened).
        unsafe { &*(self.region.as_ptr().add(RING_OFFSET) as *const AtomicU64).add(i) }
    }

    fn read_slot(&self, id: u32) -> Option<(Segment, u64)> {
        let s = &self.header().slots[id as usize % SEGMENTS];
        for _ in 0..64 {
            let q0 = s.seq.load(Acquire);
            if q0 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let seg = Segment {
                id: s.id.load(Acquire),
                rate: f64::from_bits(s.rate_bits.load(Acquire)),
                channels: s.channels.load(Acquire),
                sample_type: s.sample_type.load(Acquire),
                start: s.start.load(Acquire),
            };
            let end = s.end.load(Acquire);
            if s.seq.load(Acquire) == q0 {
                return (seg.id == id).then_some((seg, end));
            }
        }
        None
    }
}

/// Aura Player's end: makes the link and reads the stream.
pub struct Server {
    link: Link,
    /// The next sample to read.
    pos: u64,
    /// The segment being read, and whether its format has been handed out.
    cur: Option<Segment>,
    told: bool,
}

/// What a read gave.
#[derive(Debug, PartialEq)]
pub enum Read {
    /// This many samples were put at the start of the buffer.
    Samples(usize),
    /// The stream goes on in this format; nothing was read. Handed out before
    /// a segment's first sample.
    Format(Segment),
    /// Nothing to read now.
    Empty,
}

/// Who holds the link, as the server sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientInfo {
    pub pid: u32,
    pub gen: u32,
    pub name: String,
    pub running: bool,
}

impl Server {
    /// Makes the link in `region` (zeroed, `region_bytes(ring_samples)` long
    /// or more) with a ring of `ring_samples` (a power of two). The client
    /// sees it only once it is whole: the magic is written last.
    pub fn create(region: Box<dyn Region>, signals: Box<dyn Signals>, ring_samples: usize, pid: u32) -> Server {
        assert!(ring_samples.is_power_of_two(), "the ring is a power of two");
        assert!(region.len() >= region_bytes(ring_samples), "the region holds the ring");
        let link = Link { region, signals, ring_samples: ring_samples as u64 };
        let h = link.header();
        h.version.store(VERSION, Relaxed);
        h.ring_samples.store(ring_samples as u64, Relaxed);
        h.server_pid.store(pid, Relaxed);
        h.mode.store(Mode::Released as u32, Relaxed);
        h.target.store((ring_samples / 4) as u64, Relaxed);
        h.read.store(h.write.load(Relaxed), Relaxed);
        h.magic.store(MAGIC, Release);
        let pos = h.write.load(Acquire);
        Server { link, pos, cur: None, told: false }
    }

    fn h(&self) -> &Header {
        self.link.header()
    }

    /// Says the server is alive: call it at least every few hundred ms.
    pub fn beat(&self) {
        self.h().beat.fetch_add(1, Release);
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u32(self.h().mode.load(Acquire))
    }

    /// Listening after Released starts at the newest sample, in the newest
    /// segment: what the player sent while nobody listened is gone. After
    /// Held it goes on where it stopped.
    pub fn set_mode(&mut self, m: Mode) {
        let was = self.mode();
        if m == Mode::Listening && was == Mode::Released {
            self.skip_to_newest();
        }
        self.h().mode.store(m as u32, Release);
        self.link.signals.notify_room();
    }

    /// To the newest sample: the next read finds the segment it is in, an
    /// open one, if the client is running.
    fn skip_to_newest(&mut self) {
        let h = self.link.header();
        self.pos = h.write.load(Acquire);
        self.cur = None;
        self.told = false;
        // Samples skipped are taken: the client may write over them.
        h.read.store(self.pos, Release);
    }

    /// The oldest segment still in a slot that is not over before `pos`.
    fn segment_from(&self, pos: u64) -> Option<Segment> {
        let newest = self.h().seg_next.load(Acquire);
        let oldest = newest.saturating_sub(SEGMENTS as u32 - 1).max(1);
        (oldest..=newest).find_map(|id| match self.link.read_slot(id) {
            Some((seg, end)) if end == OPEN || end > pos => Some(seg),
            _ => None,
        })
    }

    /// How far the client may run ahead of the reader, in samples (kept
    /// inside the ring by the client: a block always fits).
    pub fn set_target(&self, samples: u64) {
        self.h().target.store(samples.min(self.link.ring_samples), Release);
        self.link.signals.notify_room();
    }

    /// The server's own delay from a sample read to its sound, in frames:
    /// the client reports it to the player (its output latency).
    pub fn set_latency_frames(&self, frames: u32) {
        self.h().latency_frames.store(frames, Release);
    }

    /// Whether the reader takes the ring at an even pace: then the client
    /// may call its player by a clock of its own (`clock`), and keep a
    /// reserve in the ring for the reader's hiccups.
    pub fn set_even(&self, on: bool) {
        if self.h().even.swap(on as u32, AcqRel) != on as u32 {
            self.link.signals.notify_room();
        }
    }

    /// The client's account of its calls.
    pub fn call_stats(&self) -> CallStats {
        let h = self.h();
        CallStats {
            period_us: h.period_us.load(Acquire),
            calls: h.calls.load(Acquire),
            late: h.late.load(Acquire),
            waited_us: h.waited_us.load(Acquire),
            longest_us: h.longest_us.load(Acquire),
            clocked: h.clocked.load(Acquire) == 1,
            clock_ppm: h.clock_ppm.load(Acquire) as i32,
        }
    }

    /// The late calls counted past `*seen` (how many were read before),
    /// oldest first, into `out`; `*seen` moves on to the count. Gives how
    /// many of them the header no longer holds. A count below `*seen` is a
    /// new client's: from its first.
    pub fn late_calls(&self, seen: &mut u64, out: &mut Vec<LateCall>) -> u64 {
        let h = self.h();
        let n = h.late.load(Acquire);
        if n < *seen {
            *seen = 0;
        }
        let from = (*seen).max(n.saturating_sub(LATE_SLOTS as u64));
        let mut lost = from - *seen;
        for k in from..n {
            match read_late(&h.late_calls[k as usize % LATE_SLOTS], k as u32) {
                Some(c) => out.push(c),
                None => lost += 1,
            }
        }
        *seen = n;
        lost
    }

    /// Who holds the link, if anyone.
    pub fn client(&self) -> Option<ClientInfo> {
        let h = self.h();
        let pid = h.client_pid.load(Acquire);
        if pid == 0 {
            return None;
        }
        Some(ClientInfo {
            pid,
            gen: h.client_gen.load(Acquire),
            name: read_name(h),
            running: h.running.load(Acquire) == 1,
        })
    }

    /// Samples ready to read in the current segment.
    pub fn available(&self) -> u64 {
        let h = self.h();
        let w = h.write.load(Acquire);
        let limit = match self.cur {
            Some(seg) => self.link.read_slot(seg.id).map_or(w, |(_, end)| end.min(w)),
            None => w,
        };
        limit.saturating_sub(self.pos)
    }

    /// Reads what is there into `out`, up to the end of the current segment;
    /// at a segment's start hands out its format first. Reads only while
    /// listening.
    pub fn read(&mut self, out: &mut [f64]) -> Read {
        if self.mode() != Mode::Listening {
            return Read::Empty;
        }
        let h = self.link.header();
        // `write` before the segment's end: a client that closed a segment
        // and wrote the next one's samples published the end first.
        let w = h.write.load(Acquire);
        loop {
            let Some(seg) = self.cur else {
                // No segment yet: the oldest one not over before here.
                let Some(seg) = self.segment_from(self.pos) else { return Read::Empty };
                self.cur = Some(seg);
                self.told = false;
                if seg.start > self.pos {
                    self.pos = seg.start;
                }
                continue;
            };
            if !self.told {
                self.told = true;
                return Read::Format(seg);
            }
            let end = match self.link.read_slot(seg.id) {
                Some((_, end)) => end,
                // The slot was taken by a newer segment: this one's end
                // passed long ago.
                None => self.pos,
            };
            let limit = end.min(w);
            if self.pos < limit {
                let n = ((limit - self.pos) as usize).min(out.len());
                for (i, o) in out[..n].iter_mut().enumerate() {
                    *o = f64::from_bits(self.link.slot(self.pos + i as u64).load(Relaxed));
                }
                self.pos += n as u64;
                h.read.store(self.pos, Release);
                self.link.signals.notify_room();
                return Read::Samples(n);
            }
            if end == OPEN || self.pos < end {
                return Read::Empty;
            }
            // The segment is read to its end: the next one, when it starts.
            let next = h.seg_next.load(Acquire);
            if next <= seg.id {
                return Read::Empty;
            }
            match self.link.read_slot(seg.id + 1) {
                Some((s, _)) => {
                    self.cur = Some(s);
                    self.told = false;
                }
                None => {
                    // More segments came than there are slots: on with the newest.
                    self.skip_to_newest();
                    return Read::Empty;
                }
            }
        }
    }

    /// Waits for the client to give something (samples, a segment).
    pub fn wait_data(&self, timeout: Duration) -> bool {
        self.link.signals.wait_data(timeout)
    }

    /// The segment being read.
    pub fn segment(&self) -> Option<Segment> {
        self.cur
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The client hears it at once instead of a second later.
        self.h().mode.store(Mode::Released as u32, Release);
        self.link.signals.notify_room();
    }
}

/// Late call `index` from its slot: None if the slot holds another by now.
fn read_late(s: &LateSlot, index: u32) -> Option<LateCall> {
    for _ in 0..64 {
        let q0 = s.seq.load(Acquire);
        if q0 & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        let c = LateCall {
            gap_us: s.gap_us.load(Acquire),
            waited_us: s.waited_us.load(Acquire),
            before_us: s.before_us.load(Acquire),
            fill: s.fill.load(Acquire),
        };
        let at = s.index.load(Acquire);
        if s.seq.load(Acquire) == q0 {
            return (at == index).then_some(c);
        }
    }
    None
}

fn read_name(h: &Header) -> String {
    for _ in 0..64 {
        let q0 = h.name_seq.load(Acquire);
        if q0 & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        let mut b = [0u8; NAME_BYTES];
        for (i, w) in h.name.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.load(Acquire).to_le_bytes());
        }
        if h.name_seq.load(Acquire) == q0 {
            let n = b.iter().position(|&c| c == 0).unwrap_or(NAME_BYTES);
            return String::from_utf8_lossy(&b[..n]).into_owned();
        }
    }
    String::new()
}

/// A player's end: puts the stream in.
pub struct Client {
    link: Link,
    gen: u32,
    seg: Option<u32>,
    /// The beat last seen, and when it last changed (here, on this
    /// process's clock: the server's clock is not ours).
    beat: u64,
    beat_at: Instant,
}

impl Client {
    /// Connects to the server's link in `region` as process `pid`. A link
    /// another process holds is `Busy` while `alive` says that process is
    /// (one that died holding it is taken over); this process may take its
    /// own link again.
    pub fn open(
        region: Box<dyn Region>,
        signals: Box<dyn Signals>,
        pid: u32,
        alive: impl Fn(u32) -> bool,
    ) -> Result<Client, LinkError> {
        if region.len() < RING_OFFSET {
            return Err(LinkError::NoServer);
        }
        let probe = Link { region, signals, ring_samples: 1 };
        let h = probe.header();
        if h.magic.load(Acquire) != MAGIC {
            return Err(LinkError::NoServer);
        }
        let v = h.version.load(Acquire);
        if v != VERSION {
            return Err(LinkError::Version(v));
        }
        let ring = h.ring_samples.load(Acquire);
        if !ring.is_power_of_two() || probe.region.len() < region_bytes(ring as usize) {
            return Err(LinkError::Version(v));
        }
        let link = Link { ring_samples: ring, ..probe };
        let h = link.header();
        let mut held = h.client_pid.load(Acquire);
        loop {
            if held != 0 && held != pid && alive(held) {
                return Err(LinkError::Busy);
            }
            match h.client_pid.compare_exchange(held, pid, AcqRel, Acquire) {
                Ok(_) => break,
                Err(now) => held = now,
            }
        }
        // The account of the calls starts afresh, before the server sees the
        // new generation.
        for a in [&h.period_us, &h.longest_us, &h.clocked, &h.clock_ppm] {
            a.store(0, Release);
        }
        for a in [&h.calls, &h.late, &h.waited_us] {
            a.store(0, Release);
        }
        let gen = h.client_gen.fetch_add(1, AcqRel) + 1;
        let beat = h.beat.load(Acquire);
        Ok(Client { link, gen, seg: None, beat, beat_at: Instant::now() })
    }

    fn h(&self) -> &Header {
        self.link.header()
    }

    /// The generation this client took the link in.
    pub fn gen(&self) -> u32 {
        self.gen
    }

    /// The player's name, for Aura Player to show.
    pub fn set_name(&self, name: &str) {
        let h = self.h();
        let mut b = [0u8; NAME_BYTES];
        let mut n = name.len().min(NAME_BYTES);
        while !name.is_char_boundary(n) {
            n -= 1;
        }
        b[..n].copy_from_slice(&name.as_bytes()[..n]);
        h.name_seq.fetch_add(1, AcqRel);
        for (i, w) in h.name.iter().enumerate() {
            w.store(u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()), Release);
        }
        h.name_seq.fetch_add(1, AcqRel);
    }

    /// The server's mode — Released when its beat has not moved for
    /// `alive` (it died, or hangs).
    pub fn mode(&mut self, alive: Duration) -> Mode {
        let b = self.h().beat.load(Acquire);
        if b != self.beat {
            self.beat = b;
            self.beat_at = Instant::now();
        } else if self.beat_at.elapsed() > alive {
            return Mode::Released;
        }
        Mode::from_u32(self.h().mode.load(Acquire))
    }

    /// The server's delay to its sound, in frames.
    pub fn latency_frames(&self) -> u32 {
        self.h().latency_frames.load(Acquire)
    }

    /// Starts a segment in this format (closing the one before, if open).
    pub fn begin(&mut self, rate: f64, channels: u32, sample_type: u32) -> u32 {
        self.end();
        let h = self.link.header();
        let id = h.seg_next.load(Acquire) + 1;
        let s = &h.slots[id as usize % SEGMENTS];
        s.seq.fetch_add(1, AcqRel);
        s.id.store(id, Release);
        s.rate_bits.store(rate.to_bits(), Release);
        s.channels.store(channels, Release);
        s.sample_type.store(sample_type, Release);
        s.start.store(h.write.load(Acquire), Release);
        s.end.store(OPEN, Release);
        s.seq.fetch_add(1, AcqRel);
        h.seg_next.store(id, Release);
        h.running.store(1, Release);
        self.seg = Some(id);
        self.link.signals.notify_data();
        id
    }

    /// Closes the open segment: the server reads it to here.
    pub fn end(&mut self) {
        let Some(id) = self.seg.take() else { return };
        let h = self.link.header();
        let s = &h.slots[id as usize % SEGMENTS];
        if s.id.load(Acquire) == id {
            s.end.store(h.write.load(Acquire), Release);
        }
        h.running.store(0, Release);
        self.link.signals.notify_data();
    }

    /// Whether `samples` more fit under the server's target. A block always
    /// fits an empty ring.
    pub fn room(&self, samples: u64) -> bool {
        let h = self.h();
        let used = h.write.load(Acquire) - h.read.load(Acquire);
        let target = h.target.load(Acquire).min(self.link.ring_samples);
        used == 0 && samples <= self.link.ring_samples || used + samples <= target
    }

    /// Puts `n` samples in: `sample(i)` gives the i-th, interleaved. The
    /// caller made sure they fit (`room`).
    pub fn write_with(&self, n: usize, mut sample: impl FnMut(usize) -> f64) {
        let h = self.h();
        let w = h.write.load(Acquire);
        debug_assert!(w + n as u64 - h.read.load(Acquire) <= self.link.ring_samples);
        for i in 0..n {
            self.link.slot(w + i as u64).store(sample(i).to_bits(), Relaxed);
        }
        h.write.store(w + n as u64, Release);
        self.link.signals.notify_data();
    }

    /// Waits for the server to take samples or change its mode.
    pub fn wait_room(&self, timeout: Duration) -> bool {
        self.link.signals.wait_room(timeout)
    }

    /// Samples in the ring the server has not taken.
    pub fn used(&self) -> u64 {
        let h = self.h();
        h.write.load(Acquire) - h.read.load(Acquire)
    }

    /// How far the server lets the client run ahead of its reader, samples
    /// (the room rule's: `room`), and the ring.
    pub fn target(&self) -> u64 {
        self.h().target.load(Acquire).min(self.link.ring_samples)
    }

    pub fn ring_samples(&self) -> u64 {
        self.link.ring_samples
    }

    /// The server's reader takes the ring at an even pace: the player may
    /// be called by the client's own clock (`clock`).
    pub fn even(&self) -> bool {
        self.h().even.load(Acquire) == 1
    }

    /// The player's period, for the account of its calls.
    pub fn set_period(&self, period: Duration) {
        self.h().period_us.store(period.as_micros().min(u32::MAX as u128) as u32, Release);
    }

    /// One call of the player, for the account: of the time since the call
    /// before, `waited_us` went on waiting for room; the call took
    /// `took_us`; and it came late, if it did.
    pub fn count_call(&self, waited_us: u64, took_us: u32, late: Option<LateCall>) {
        let h = self.h();
        if waited_us > 0 {
            h.waited_us.fetch_add(waited_us, AcqRel);
        }
        h.longest_us.fetch_max(took_us, AcqRel);
        if let Some(c) = late {
            let k = h.late.load(Acquire);
            let s = &h.late_calls[k as usize % LATE_SLOTS];
            s.seq.fetch_add(1, AcqRel);
            s.index.store(k as u32, Release);
            s.gap_us.store(c.gap_us, Release);
            s.waited_us.store(c.waited_us, Release);
            s.before_us.store(c.before_us, Release);
            s.fill.store(c.fill, Release);
            s.seq.fetch_add(1, AcqRel);
            h.late.store(k + 1, Release);
        }
        h.calls.fetch_add(1, AcqRel);
    }

    /// Whether the player is called by the client's own clock, trimmed by
    /// `ppm` against the system's.
    pub fn set_clock(&self, clocked: bool, ppm: i32) {
        let h = self.h();
        h.clocked.store(clocked as u32, Release);
        h.clock_ppm.store(ppm as u32, Release);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.end();
        let h = self.link.header();
        // Only if no newer client took the link meanwhile.
        if h.client_gen.load(Acquire) == self.gen {
            h.client_pid.store(0, Release);
        }
        self.link.signals.notify_data();
    }
}

/// A region on the heap: both ends in one process (tests, and the tools
/// that check a driver without a server).
pub struct HeapRegion(Box<[AtomicU64]>);

impl HeapRegion {
    pub fn new(bytes: usize) -> Arc<HeapRegion> {
        Arc::new(HeapRegion((0..bytes.div_ceil(8)).map(|_| AtomicU64::new(0)).collect()))
    }
}

// SAFETY: the words live as long as the region and are 8-aligned.
unsafe impl Region for HeapRegion {
    fn as_ptr(&self) -> *mut u8 {
        self.0.as_ptr() as *mut u8
    }
    fn len(&self) -> usize {
        self.0.len() * 8
    }
}

// SAFETY: as the region it shares.
unsafe impl Region for Arc<HeapRegion> {
    fn as_ptr(&self) -> *mut u8 {
        (**self).as_ptr()
    }
    fn len(&self) -> usize {
        (**self).len()
    }
}

/// An auto-reset event in one process.
#[derive(Default)]
struct Event {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Event {
    fn notify(&self) {
        *self.set.lock().unwrap() = true;
        self.cv.notify_one();
    }
    fn wait(&self, timeout: Duration) -> bool {
        let g = self.set.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |s| !*s).unwrap();
        std::mem::replace(&mut *g, false)
    }
}

/// Both ends' wake-ups in one process.
#[derive(Clone, Default)]
pub struct LocalSignals(Arc<(Event, Event)>);

impl Signals for LocalSignals {
    fn notify_data(&self) {
        self.0 .0.notify()
    }
    fn wait_data(&self, timeout: Duration) -> bool {
        self.0 .0.wait(timeout)
    }
    fn notify_room(&self) {
        self.0 .1.notify()
    }
    fn wait_room(&self, timeout: Duration) -> bool {
        self.0 .1.wait(timeout)
    }
}

#[cfg(test)]
mod tests;
