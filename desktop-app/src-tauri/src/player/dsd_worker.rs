//! DSD output's modulators on their own threads. A modulator is sequential —
//! each bit depends on the one before — so it cannot use more than one core a
//! channel, and the heavier kinds (look-ahead) take a large part of one: run
//! on the audio thread, one late period is a click or a drop-out of DoP (the
//! DAC leaves DSD mode). So each channel's modulator runs on a worker thread
//! of its own, `lead` frames ahead of the device.
//!
//! The audio thread keeps all its PCM work (the timeline, the volume, the
//! ramps, the guard, the test mute) and hands each period's PCM to the ring
//! (`Pipe::push`); the workers modulate it into one-bit bytes; the audio thread
//! takes the bytes of the frames it pushed `lead` frames earlier
//! (`Pipe::pop`) and packs DoP. Every period pushes and pops the same number
//! of frames, so the ring holds `lead` frames in flight at all times and the
//! sound is heard `lead` frames (plus the modulator's own `latency`) after the
//! audio thread read it: the position and the analyzer take both off
//! (`latency_frames`). The ring starts with `lead` frames of DSD silence.
//!
//! The ring is frame-indexed: frame `f` sits in slot `f mod capacity`, its
//! PCM (one f64 a channel), its bytes (`ratio / 8` a channel) and its tag —
//! music, silence (nothing to modulate; the modulator starts from silence
//! again before the next music, as `reset` says) or muted (a test run:
//! modulated, DSD silence sent). Every slot is an atomic, so nothing on the
//! audio thread locks, allocates or waits.
//!
//! A worker that falls behind (the bytes of a frame not ready when it is due,
//! or the frames ready ahead below a quarter of the lead) does not glitch: the
//! audio thread sends DSD silence for a short gap and tells both workers to go
//! on from the frame after it with the next lighter modulator of the chain
//! (`lighter`), started from silence; at the lightest it goes on with the same
//! one. Never back up within the stream.

use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::Duration;

use crate::audio::dsd::{DsdEncoder, DsdError, DsdRate, ModulatorKind, NoiseCurve, Search, CATALOG, DSD_SILENCE};

/// A frame of the ring: music to modulate and send.
const MUSIC: u8 = 0;
/// Nothing to modulate: DSD silence goes out, the modulator starts from
/// silence again before the next music.
const SILENCE: u8 = 1;
/// A test run: modulated as music, DSD silence sent.
const MUTED: u8 = 2;

/// What a pushed period is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    Music,
    Muted,
}

/// How far ahead of the device the workers modulate, by the modulator's
/// search. It has to outlast the longest stall a worker survives without a
/// gap: a few scheduler quanta (~15 ms each) and what it takes to catch up
/// after one, which grows as 1 / (1 − load) — a plain loop (~10 % of a core
/// at DSD256) recovers at once, a look-ahead near half a core needs as long
/// again, the deeper one near a whole core several times as long. Volume,
/// pause and seek are heard this much later.
pub(crate) fn lead_s(kind: ModulatorKind) -> f64 {
    match kind.info().search {
        Search::None => 0.08,
        Search::LookAhead => 0.15,
        Search::LookAheadPlus | Search::Trellis => 0.25,
    }
}

/// The next lighter real-time modulator for `rate`: the search dropped first
/// (LA+ → LA → none, the same order and curve), then the order (9 → 7 → 5),
/// the curve kept. A step the catalog lacks (or that is not built for the
/// rate) is passed over. None at the fifth order without a search.
pub(crate) fn lighter(kind: ModulatorKind, rate: DsdRate) -> Option<ModulatorKind> {
    let i = kind.info();
    let (mut order, curve, mut search): (u8, NoiseCurve, Search) = (i.order, i.curve, i.search);
    loop {
        (order, search) = match search {
            Search::LookAheadPlus => (order, Search::LookAhead),
            Search::LookAhead => (order, Search::None),
            Search::None if order > 5 => (order - 2, Search::None),
            _ => return None,
        };
        if let Some(j) = CATALOG.iter().find(|j| j.order == order && j.curve == curve && j.search == search) {
            if j.realtime && j.kind.supports(rate) {
                return Some(j.kind);
            }
        }
    }
}

/// One channel's part of the ring.
struct Lane {
    /// The PCM of each slot (f64 bits).
    pcm: Box<[AtomicU64]>,
    /// The one-bit bytes of each slot, `bpf` of them.
    bytes: Box<[AtomicU8]>,
    /// Frames before this are modulated (under the command `ack` names).
    encoded: AtomicU64,
    /// The last command (`Ring::epoch`) this lane took.
    ack: AtomicU64,
    /// The modulators' overloads, all the lane ran.
    overloads: AtomicU64,
    /// The running modulator's `latency()`.
    latency: AtomicUsize,
}

struct Ring {
    mask: usize,
    /// Bytes a frame, a channel (`ratio / 8`).
    bpf: usize,
    tags: Box<[AtomicU8]>,
    /// Frames pushed so far.
    written: AtomicU64,
    /// The audio thread's command to both workers: from frame `resume` on,
    /// the chain's modulator `level`, started from silence. Published by
    /// `epoch` (Release) after both.
    epoch: AtomicU64,
    resume: AtomicU64,
    level: AtomicUsize,
    stop: AtomicBool,
    lanes: [Lane; 2],
}

impl Ring {
    fn slot(&self, f: u64) -> usize {
        f as usize & self.mask
    }
}

/// What `pop` sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Popped {
    /// Some frame went out as music (not DSD silence).
    pub music: bool,
    /// Frames that were due and not modulated in time (sent as DSD silence).
    pub missed: usize,
}

/// What `pop` did about a worker behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lag {
    /// On to a lighter modulator.
    SteppedDown,
    /// Already the lightest: on with it after the gap.
    Resynced,
}

/// The audio thread's end of the ring.
pub(crate) struct Pipe {
    ring: Arc<Ring>,
    threads: [Thread; 2],
    lead: usize,
    /// The modulator asked for, then each lighter one this PC can build.
    chain: Vec<ModulatorKind>,
    level: usize,
    epoch: u64,
    /// Frames before this go out as DSD silence (the gap of a command).
    gap_until: u64,
    /// The gap's length: the frames a command gives the workers to start.
    gap: usize,
    /// Judging the margin: off after a command until the workers are half a
    /// lead ahead again.
    watching: bool,
    written: u64,
    played: u64,
    step_downs: u64,
    gaps: u64,
    /// Tests: `pop` waits for the workers, so a test sees no gap from a busy
    /// machine.
    #[cfg(test)]
    wait: bool,
}

#[cfg(test)]
thread_local! {
    /// Tests: the lead in frames (None: the kind's) and whether `pop` waits.
    /// Set on the thread that builds the pipe.
    pub(crate) static TEST_PIPE: std::cell::Cell<(Option<usize>, bool)> = const { std::cell::Cell::new((Some(0), true)) };
}

/// The most frames a worker modulates at once: short enough that a command
/// (a step down) is taken well within its gap, however slow the modulator.
const CHUNK: usize = 1024;

impl Pipe {
    /// The two workers for `kind` at `rate`, the input `input_rate`, periods
    /// of at most `buf_frames`; `make` builds a modulator (`new_encoder`).
    /// Fails if `kind` itself cannot be built or a thread not started.
    pub(crate) fn new(
        kind: ModulatorKind,
        input_rate: u32,
        rate: DsdRate,
        buf_frames: usize,
        make: &dyn Fn(ModulatorKind) -> Result<Box<dyn DsdEncoder>, DsdError>,
    ) -> Result<Pipe, String> {
        // Each lane gets its own chain of modulators, built here, so a step
        // down needs nothing new on the workers.
        let mut chain = vec![kind];
        let mut encs: [Vec<Box<dyn DsdEncoder>>; 2] = [vec![make(kind).map_err(|e| e.to_string())?], vec![make(kind).map_err(|e| e.to_string())?]];
        let mut k = kind;
        while let Some(next) = lighter(k, rate) {
            k = next;
            if let (Ok(a), Ok(b)) = (make(next), make(next)) {
                chain.push(next);
                encs[0].push(a);
                encs[1].push(b);
            }
        }
        let ratio = encs[0][0].ratio();
        let bpf = ratio / 8;
        let buf = buf_frames.max(1);
        let lead = (lead_s(kind) * input_rate as f64).ceil() as usize;
        let lead = lead.max(4 * buf);
        #[cfg(test)]
        let (lead, wait) = match TEST_PIPE.with(|c| c.get()) {
            (Some(l), w) => (l, w),
            (None, w) => (lead, w),
        };
        let cap = (lead + 4 * buf + input_rate as usize / 20 + CHUNK).next_power_of_two();
        let lane = |latency: usize| Lane {
            pcm: (0..cap).map(|_| AtomicU64::new(0)).collect(),
            bytes: (0..cap * bpf).map(|_| AtomicU8::new(DSD_SILENCE)).collect(),
            encoded: AtomicU64::new(0),
            ack: AtomicU64::new(0),
            overloads: AtomicU64::new(0),
            latency: AtomicUsize::new(latency),
        };
        let latency = encs[0][0].latency();
        let ring = Arc::new(Ring {
            mask: cap - 1,
            bpf,
            tags: (0..cap).map(|i| AtomicU8::new(if i < lead { SILENCE } else { MUSIC })).collect(),
            written: AtomicU64::new(lead as u64),
            epoch: AtomicU64::new(0),
            resume: AtomicU64::new(0),
            level: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            lanes: [lane(latency), lane(latency)],
        });
        let [el, er] = encs;
        let spawn = |ch: usize, encs: Vec<Box<dyn DsdEncoder>>| {
            let ring = ring.clone();
            thread::Builder::new()
                .name(format!("aura-dsd-{}", ["left", "right"][ch]))
                .spawn(move || worker(&ring, ch, encs, buf))
                .map(|h| h.thread().clone())
                .map_err(|e| format!("DSD: the modulator thread did not start: {e}"))
        };
        let left = spawn(0, el)?;
        let right = match spawn(1, er) {
            Ok(t) => t,
            Err(e) => {
                ring.stop.store(true, Ordering::Release);
                left.unpark();
                return Err(e);
            }
        };
        Ok(Pipe {
            ring,
            threads: [left, right],
            lead,
            chain,
            level: 0,
            epoch: 0,
            gap_until: 0,
            gap: (0.03 * input_rate as f64) as usize,
            watching: false,
            written: lead as u64,
            played: 0,
            step_downs: 0,
            gaps: 0,
            #[cfg(test)]
            wait,
        })
    }

    /// Frames in flight: the sound leaves `lead` frames after it was pushed.
    pub(crate) fn lead(&self) -> usize {
        self.lead
    }

    /// The delay from a push to the music heard, in input frames: the lead and
    /// the running modulator's `latency` (its first bytes after a reset are
    /// DSD silence filler, the music comes after them).
    pub(crate) fn delay(&self) -> usize {
        self.lead + self.ring.lanes[0].latency.load(Ordering::Relaxed)
    }

    /// The modulator the workers were last told to run.
    pub(crate) fn kind(&self) -> ModulatorKind {
        self.chain[self.level]
    }

    pub(crate) fn step_downs(&self) -> u64 {
        self.step_downs
    }

    /// Gaps of DSD silence sent for a worker behind (steps down and resyncs).
    pub(crate) fn gaps(&self) -> u64 {
        self.gaps
    }

    pub(crate) fn overloads(&self) -> u64 {
        self.ring.lanes.iter().map(|l| l.overloads.load(Ordering::Relaxed)).sum()
    }

    /// One period of PCM (each channel `l.len()` frames) for the workers.
    pub(crate) fn push(&mut self, l: &[f64], r: &[f64], tag: Tag) {
        let t = if tag == Tag::Music { MUSIC } else { MUTED };
        let ring = &*self.ring;
        let [a, b] = &ring.lanes;
        for (i, (x, y)) in l.iter().zip(r).enumerate() {
            let s = ring.slot(self.written + i as u64);
            a.pcm[s].store(x.to_bits(), Ordering::Relaxed);
            b.pcm[s].store(y.to_bits(), Ordering::Relaxed);
            ring.tags[s].store(t, Ordering::Relaxed);
        }
        self.publish(l.len());
    }

    /// `frames` with nothing to modulate (DSD silence goes out for them).
    pub(crate) fn push_silence(&mut self, frames: usize) {
        for i in 0..frames {
            self.ring.tags[self.ring.slot(self.written + i as u64)].store(SILENCE, Ordering::Relaxed);
        }
        self.publish(frames);
    }

    fn publish(&mut self, frames: usize) {
        self.written += frames as u64;
        self.ring.written.store(self.written, Ordering::Release);
        for t in &self.threads {
            t.unpark();
        }
    }

    /// Frames before this are modulated on both lanes (under the last
    /// command); the gap's end while a command is not taken yet.
    fn ready_to(&self) -> u64 {
        let ring = &*self.ring;
        if ring.lanes.iter().all(|l| l.ack.load(Ordering::Acquire) == self.epoch) {
            ring.lanes.iter().map(|l| l.encoded.load(Ordering::Acquire)).min().unwrap_or(0).max(self.gap_until)
        } else {
            self.gap_until
        }
    }

    /// The bytes of the next `frames` (each channel `frames * ratio / 8`):
    /// the modulators' for the music they finished, DSD silence for the rest.
    /// Then the workers' margin is judged, and a lag dealt with (`Lag`).
    pub(crate) fn pop(&mut self, frames: usize, out_l: &mut [u8], out_r: &mut [u8]) -> (Popped, Option<Lag>) {
        let p = self.played;
        #[cfg(test)]
        if self.wait {
            let t0 = std::time::Instant::now();
            while self.ready_to() < p + frames as u64 {
                assert!(t0.elapsed() < Duration::from_secs(20), "the DSD workers never caught up");
                thread::yield_now();
            }
        }
        let ready_to = self.ready_to();
        let ring = &*self.ring;
        let bpf = ring.bpf;
        let [a, b] = &ring.lanes;
        let mut got = Popped::default();
        for i in 0..frames {
            let f = p + i as u64;
            let s = ring.slot(f);
            let tag = ring.tags[s].load(Ordering::Relaxed);
            let (dl, dr) = (&mut out_l[i * bpf..(i + 1) * bpf], &mut out_r[i * bpf..(i + 1) * bpf]);
            let due = tag != SILENCE && f >= self.gap_until;
            if due && f < ready_to && tag == MUSIC {
                for k in 0..bpf {
                    dl[k] = a.bytes[s * bpf + k].load(Ordering::Relaxed);
                    dr[k] = b.bytes[s * bpf + k].load(Ordering::Relaxed);
                }
                got.music = true;
            } else {
                dl.fill(DSD_SILENCE);
                dr.fill(DSD_SILENCE);
                if due && f >= ready_to {
                    got.missed += 1;
                }
            }
        }
        self.played += frames as u64;
        let lag = self.watch(got.missed, ready_to, frames);
        (got, lag)
    }

    /// The workers' margin after a period: behind → a gap and a command.
    fn watch(&mut self, missed: usize, ready_to: u64, frames: usize) -> Option<Lag> {
        #[cfg(test)]
        if self.wait {
            return None; // no device clock: pushed as fast as the workers go
        }
        let ring = &*self.ring;
        if ring.lanes.iter().any(|l| l.ack.load(Ordering::Acquire) != self.epoch) {
            return None; // the last command is not taken yet
        }
        let margin = ready_to.saturating_sub(self.played) as usize;
        if self.lead > 0 && !self.watching && margin > self.lead / 2 {
            self.watching = true;
        }
        let behind = missed > 0 || (self.watching && margin < self.lead / 4);
        if !behind {
            return None;
        }
        let next = (self.level + 1).min(self.chain.len() - 1);
        let lag = if next > self.level { Lag::SteppedDown } else { Lag::Resynced };
        let resume = self.played + self.gap.max(3 * frames) as u64;
        self.epoch += 1;
        self.level = next;
        self.gap_until = resume;
        self.watching = false;
        ring.resume.store(resume, Ordering::Relaxed);
        ring.level.store(next, Ordering::Relaxed);
        ring.epoch.store(self.epoch, Ordering::Release);
        for t in &self.threads {
            t.unpark();
        }
        if lag == Lag::SteppedDown {
            self.step_downs += 1;
        }
        self.gaps += 1;
        Some(lag)
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // The workers end on their own; nobody waits for them here.
        self.ring.stop.store(true, Ordering::Release);
        for t in &self.threads {
            t.unpark();
        }
    }
}

/// A channel's worker: modulates what the audio thread pushed, in order, as
/// soon as it is there.
fn worker(ring: &Ring, ch: usize, mut encs: Vec<Box<dyn DsdEncoder>>, buf: usize) {
    crate::player::source_stages::set_thread_priority(2);
    let lane = &ring.lanes[ch];
    let cap = ring.mask as u64 + 1;
    let bpf = ring.bpf;
    // What a worker this far behind may still read: the audio thread may be
    // writing a period over the oldest slots.
    let room = cap - 2 * buf as u64;
    let mut level = 0;
    let mut base_overloads = 0;
    let mut epoch = 0;
    let mut e: u64 = 0;
    let mut stale = true;
    let mut pcm = vec![0.0f64; CHUNK];
    let mut tags = vec![0u8; CHUNK];
    let mut out = vec![0u8; CHUNK * bpf];
    loop {
        if ring.stop.load(Ordering::Acquire) {
            return;
        }
        let ep = ring.epoch.load(Ordering::Acquire);
        if ep != epoch {
            epoch = ep;
            let lv = ring.level.load(Ordering::Relaxed).min(encs.len() - 1);
            if lv != level {
                base_overloads += encs[level].overloads();
                level = lv;
                lane.latency.store(encs[level].latency(), Ordering::Relaxed);
            }
            encs[level].reset();
            stale = false;
            e = ring.resume.load(Ordering::Relaxed);
            lane.encoded.store(e, Ordering::Relaxed);
            lane.ack.store(ep, Ordering::Release);
        }
        let w = ring.written.load(Ordering::Acquire);
        if e >= w {
            thread::park_timeout(Duration::from_millis(5));
            continue;
        }
        if w - e > room {
            // Far behind: those frames were due long ago (the lead is far
            // shorter than the room) and went out as DSD silence.
            e = w - room;
            stale = true;
        }
        let n = (w - e).min(CHUNK as u64) as usize;
        for i in 0..n {
            let s = ring.slot(e + i as u64);
            tags[i] = ring.tags[s].load(Ordering::Relaxed);
            pcm[i] = f64::from_bits(lane.pcm[s].load(Ordering::Relaxed));
        }
        // Written over while read: start again from what is left.
        fence(Ordering::Acquire);
        if ring.written.load(Ordering::Relaxed) + buf as u64 > e + cap {
            continue;
        }
        let enc = &mut encs[level];
        let mut i = 0;
        while i < n {
            let t = tags[i];
            let mut j = i + 1;
            while j < n && tags[j] == t {
                j += 1;
            }
            if t == SILENCE {
                stale = true;
            } else {
                if stale {
                    enc.reset();
                    stale = false;
                }
                enc.process(&pcm[i..j], &mut out[i * bpf..j * bpf]);
                for k in i..j {
                    let s = ring.slot(e + k as u64) * bpf;
                    for q in 0..bpf {
                        lane.bytes[s + q].store(out[k * bpf + q], Ordering::Relaxed);
                    }
                }
            }
            i = j;
        }
        lane.overloads.store(base_overloads + enc.overloads(), Ordering::Relaxed);
        e += n as u64;
        lane.encoded.store(e, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    /// A stand-in modulator: each output byte a hash of its input sample and
    /// the samples before it (so a reset or a lost sample shows), and as slow
    /// as asked — `spin` of busy time per input sample, read live.
    struct Fake {
        state: u64,
        spin: Arc<AtomicU32>,
        latency: usize,
        resets: Arc<AtomicU32>,
    }

    impl DsdEncoder for Fake {
        fn ratio(&self) -> usize {
            16
        }
        fn process(&mut self, pcm: &[f64], out: &mut [u8]) {
            let ns = self.spin.load(Ordering::Relaxed) as u128 * pcm.len() as u128;
            if ns > 0 {
                let t0 = Instant::now();
                while t0.elapsed().as_nanos() < ns {
                    std::hint::spin_loop();
                }
            }
            for (x, o) in pcm.iter().zip(out.chunks_mut(2)) {
                self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(x.to_bits() | 1);
                o[0] = (self.state >> 56) as u8;
                o[1] = (self.state >> 48) as u8;
            }
        }
        fn reset(&mut self) {
            self.state = 0;
            self.resets.fetch_add(1, Ordering::Relaxed);
        }
        fn overloads(&self) -> u64 {
            0
        }
        fn latency(&self) -> usize {
            self.latency
        }
    }

    const RATE: u32 = 176_400;
    const P: usize = 1_764; // 10 ms

    fn fake_maker(spin: Arc<AtomicU32>) -> impl Fn(ModulatorKind) -> Result<Box<dyn DsdEncoder>, DsdError> {
        move |kind| {
            // Each kind its own latency, so a step down shows in `delay`.
            let latency = if kind.info().search == Search::None { 0 } else { 2 };
            Ok(Box::new(Fake { state: 0, spin: spin.clone(), latency, resets: Arc::new(AtomicU32::new(0)) }) as Box<dyn DsdEncoder>)
        }
    }

    fn signal(n: usize, seed: f64) -> Vec<f64> {
        (0..n).map(|i| ((i as f64) * 0.013 + seed).sin() * 0.3).collect()
    }

    fn with_test_pipe<T>(lead: Option<usize>, wait: bool, f: impl FnOnce() -> T) -> T {
        let old = TEST_PIPE.with(|c| c.replace((lead, wait)));
        let t = f();
        TEST_PIPE.with(|c| c.set(old));
        t
    }

    #[test]
    fn the_step_down_chain_drops_the_search_then_the_order_and_keeps_the_curve() {
        use ModulatorKind as K;
        let r256 = DsdRate::DSD256;
        let r512 = DsdRate::new(44_100, 512).unwrap();
        let chain = |k: K, r: DsdRate| {
            let mut v = vec![k];
            while let Some(n) = lighter(*v.last().unwrap(), r) {
                v.push(n);
            }
            v
        };
        assert_eq!(chain(K::Sdm7LaPlus, r256), [K::Sdm7LaPlus, K::Sdm7La, K::Sdm7, K::Sdm5]);
        assert_eq!(chain(K::Sdm7WideLa, r256), [K::Sdm7WideLa, K::Sdm7Wide, K::Sdm5Wide]);
        assert_eq!(chain(K::Sdm9La, r512), [K::Sdm9La, K::Sdm9, K::Sdm7]); // plain SDM5 stops at DSD256
        assert_eq!(chain(K::Sdm5La, r256), [K::Sdm5La, K::Sdm5]);
        assert_eq!(chain(K::Sdm5, r256), [K::Sdm5]);
        assert_eq!(chain(K::Sdm5Wide, r256), [K::Sdm5Wide]);
        // Every real-time kind steps down to a real-time kind of the rate.
        for i in CATALOG.iter().filter(|i| i.realtime) {
            let mut k = i.kind;
            while let Some(n) = lighter(k, r512) {
                assert!(n.info().realtime && n.supports(r512));
                assert_eq!(n.info().curve, i.curve);
                k = n;
            }
        }
    }

    /// The bytes after the lead are the inline modulator's for the same PCM,
    /// whatever the workers' chunks; the first `lead` frames DSD silence.
    #[test]
    fn the_workers_bytes_are_the_inline_modulators_after_the_lead() {
        let spin = Arc::new(AtomicU32::new(0));
        let make = fake_maker(spin.clone());
        let lead = 7 * P + 123;
        let mut pipe = with_test_pipe(Some(lead), true, || Pipe::new(ModulatorKind::Sdm5, RATE, DsdRate::DSD64, P, &make).unwrap());
        assert_eq!(pipe.delay(), lead);
        let (l, r) = (signal(40 * P, 0.0), signal(40 * P, 1.0));
        let (mut gl, mut gr) = (Vec::new(), Vec::new());
        let (mut bl, mut br) = (vec![0u8; 2 * P], vec![0u8; 2 * P]);
        for k in 0..40 {
            pipe.push(&l[k * P..(k + 1) * P], &r[k * P..(k + 1) * P], Tag::Music);
            let (got, lag) = pipe.pop(P, &mut bl, &mut br);
            assert_eq!((got.missed, lag), (0, None));
            gl.extend_from_slice(&bl);
            gr.extend_from_slice(&br);
        }
        let reference = |pcm: &[f64]| {
            let mut e = make(ModulatorKind::Sdm5).unwrap();
            let mut out = vec![0u8; pcm.len() * 2];
            e.reset();
            e.process(pcm, &mut out);
            out
        };
        assert!(gl[..2 * lead].iter().all(|&b| b == DSD_SILENCE));
        let n = 40 * P - lead;
        assert!(gl[2 * lead..] == reference(&l[..n])[..], "left");
        assert!(gr[2 * lead..] == reference(&r[..n])[..], "right");
        assert_eq!((pipe.step_downs(), pipe.gaps()), (0, 0));
    }

    /// Silence frames (a pause, a seek's hold) go out as DSD silence and the
    /// modulator starts from silence after them: the music after a pause is
    /// the inline modulator's from a reset. Muted frames are modulated and
    /// not sent.
    #[test]
    fn silence_resets_the_modulator_and_muted_frames_are_not_sent() {
        let make = fake_maker(Arc::new(AtomicU32::new(0)));
        let lead = 3 * P;
        let mut pipe = with_test_pipe(Some(lead), true, || Pipe::new(ModulatorKind::Sdm5, RATE, DsdRate::DSD64, P, &make).unwrap());
        let (l, r) = (signal(20 * P, 0.0), signal(20 * P, 2.0));
        let mut sent = Vec::new();
        let (mut bl, mut br) = (vec![0u8; 2 * P], vec![0u8; 2 * P]);
        let plan = ["m", "m", "s", "s", "m", "m", "m", "u", "u", "m", "s", "s", "s", "s"];
        let mut fed = 0;
        for what in plan {
            match what {
                "s" => pipe.push_silence(P),
                w => {
                    let tag = if w == "u" { Tag::Muted } else { Tag::Music };
                    pipe.push(&l[fed..fed + P], &r[fed..fed + P], tag);
                    fed += P;
                }
            }
            let (got, _) = pipe.pop(P, &mut bl, &mut br);
            assert_eq!(got.missed, 0);
            sent.push(bl.clone());
        }
        let run = |from: usize, frames: usize| {
            let mut e = make(ModulatorKind::Sdm5).unwrap();
            let mut out = vec![0u8; frames * 2];
            e.process(&l[from..from + frames], &mut out);
            out
        };
        let sil = vec![DSD_SILENCE; 2 * P];
        // Output k is push k − 3.
        assert!(sent[..3].iter().all(|b| *b == sil));
        assert_eq!(sent[3..5].concat(), run(0, 2 * P));
        assert!(sent[5..7].iter().all(|b| *b == sil));
        // After the pause: from silence, and the muted periods ran through it.
        let after = run(2 * P, 6 * P);
        assert_eq!(sent[7..10].concat(), after[..6 * P]);
        assert!(sent[10..12].iter().all(|b| *b == sil), "muted");
        assert_eq!(sent[12], after[10 * P..]);
        assert_eq!(sent[13], sil);
    }

    /// A worker too slow for the rate: the audio thread never waits, sends
    /// DSD silence for what is late, steps down to the lighter modulator after
    /// a gap no longer than planned, and the lighter one keeps up.
    #[test]
    fn a_worker_behind_steps_down_after_a_short_gap_and_the_lighter_keeps_up() {
        // The look-ahead stand-in is 2.5 × too slow for real time: a period of
        // 10 ms takes 25 ms. The plain one is free.
        let spin_la = Arc::new(AtomicU32::new(0));
        let spin_plain = Arc::new(AtomicU32::new(0));
        let (sl, sp) = (spin_la.clone(), spin_plain.clone());
        let make = move |kind: ModulatorKind| -> Result<Box<dyn DsdEncoder>, DsdError> {
            let la = kind.info().search != Search::None;
            let spin = if la { sl.clone() } else { sp.clone() };
            Ok(Box::new(Fake { state: 0, spin, latency: if la { 2 } else { 0 }, resets: Arc::new(AtomicU32::new(0)) }))
        };
        spin_la.store((25_000_000 / P) as u32, Ordering::Relaxed);
        let lead = 6 * P;
        let mut pipe = with_test_pipe(Some(lead), false, || Pipe::new(ModulatorKind::Sdm5La, RATE, DsdRate::DSD64, P, &make).unwrap());
        assert_eq!(pipe.chain, [ModulatorKind::Sdm5La, ModulatorKind::Sdm5]);
        assert_eq!(pipe.delay(), lead + 2);
        let l = signal(400 * P, 0.0);
        let (mut bl, mut br) = (vec![0u8; 2 * P], vec![0u8; 2 * P]);
        let mut silent_after_music = 0usize;
        let mut longest_gap = 0usize;
        let mut music_seen = false;
        let mut lagged = None;
        let mut slow_periods = 0;
        let t0 = Instant::now();
        for k in 0..300 {
            // The device's clock: one period every 10 ms.
            while t0.elapsed() < Duration::from_millis(10 * k as u64) {
                thread::sleep(Duration::from_micros(200));
            }
            let t = Instant::now();
            pipe.push(&l[k * P..(k + 1) * P], &l[k * P..(k + 1) * P], Tag::Music);
            let (got, lag) = pipe.pop(P, &mut bl, &mut br);
            // Never waits for the slow worker (25 ms a period); a busy test
            // machine may hold the thread up now and then.
            if t.elapsed() > Duration::from_millis(10) {
                slow_periods += 1;
            }
            if lag.is_some() && lagged.is_none() {
                lagged = Some((k, lag.unwrap()));
            }
            if got.music {
                if music_seen && silent_after_music > 0 && lagged.is_some() {
                    longest_gap = longest_gap.max(silent_after_music);
                }
                music_seen = true;
                silent_after_music = 0;
            } else if music_seen {
                silent_after_music += 1;
            }
        }
        assert!(slow_periods <= 3, "the audio thread waited in {slow_periods} periods");
        let (k, lag) = lagged.expect("never stepped down");
        assert_eq!(lag, Lag::SteppedDown, "at period {k}");
        assert_eq!(pipe.kind(), ModulatorKind::Sdm5);
        assert_eq!(pipe.step_downs(), 1, "stepped down once, then kept up");
        assert_eq!(pipe.delay(), lead, "the plain loop has no latency");
        assert!(silent_after_music == 0, "the lighter one kept up to the end");
        // The gap: the 30 ms (3 periods) planned, plus a period's rounding.
        assert!(longest_gap <= 4, "a gap of {longest_gap} periods");
    }

    /// The audio thread pushes and pops without allocating: a counting
    /// allocator sees nothing from it over many periods.
    #[test]
    fn push_and_pop_do_not_allocate() {
        let make = fake_maker(Arc::new(AtomicU32::new(0)));
        let mut pipe = with_test_pipe(Some(4 * P), true, || Pipe::new(ModulatorKind::Sdm5, RATE, DsdRate::DSD64, P, &make).unwrap());
        let l = signal(P, 0.0);
        let (mut bl, mut br) = (vec![0u8; 2 * P], vec![0u8; 2 * P]);
        let before = alloc_count::this_thread();
        for k in 0..50 {
            if k % 7 == 3 {
                pipe.push_silence(P);
            } else {
                pipe.push(&l, &l, Tag::Music);
            }
            pipe.pop(P, &mut bl, &mut br);
        }
        assert_eq!(alloc_count::this_thread(), before);
    }

    /// The test binary's allocator: the system's, counting per thread.
    #[global_allocator]
    static COUNTING: alloc_count::Counting = alloc_count::Counting;

    /// Per-thread allocation counts.
    pub(crate) mod alloc_count {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static N: Cell<u64> = const { Cell::new(0) };
        }

        pub struct Counting;

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, l: Layout) -> *mut u8 {
                let _ = N.try_with(|n| n.set(n.get() + 1));
                System.alloc(l)
            }
            // The system's own: zeroed memory from the OS is not filled
            // again (the default, alloc and fill, cost a test 27 ms where the
            // app pays 0.01 for two 4-minute buffers).
            unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
                let _ = N.try_with(|n| n.set(n.get() + 1));
                System.alloc_zeroed(l)
            }
            unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
                System.dealloc(p, l)
            }
            unsafe fn realloc(&self, p: *mut u8, l: Layout, s: usize) -> *mut u8 {
                let _ = N.try_with(|n| n.set(n.get() + 1));
                System.realloc(p, l, s)
            }
        }

        pub fn this_thread() -> u64 {
            N.with(|n| n.get())
        }
    }
}
