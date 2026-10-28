//! The input's link: Aura Player's end of the cable another player's ASIO
//! driver plays into (`aura_link`). One server for as long as the app runs,
//! made at its start; a thread of its own reads it into the input's session.
//!
//! * The mode is the controller's: the input not listened to — Released (the
//!   player plays on as into an unplugged cable); the input playing —
//!   Listening; paused here — Held (the player stands still, nothing lost).
//! * The pace: the reader takes from the link only what the session's live
//!   source has room for (`Feed::room`): until the chain is built, up to
//!   what it waits for; then evenly, by the reader's own clock trimmed to
//!   the chain's (`super::pace`). The player waits for room in the link, so
//!   it plays at the rate the device takes the music, and is called once a
//!   buffer's time — not the chain's whole portion at once; read as fast as
//!   it sends, it would decode whole tracks ahead.
//! * Formats: the first segment's, and one at another rate, are handed to
//!   the controller (`Events::format`), which starts a session for it and
//!   attaches its feed; nothing is read meanwhile. A segment at the same
//!   rate goes on in the same session, across a gap (`Feed::segment`).
//! * A player that starts playing while the input is not listened to may
//!   take the player over (`Events::started`; the input's auto switch).
//! * Once the chain is built the reader tells the driver it is even
//!   (`Server::set_even`): the driver then calls the player by its own clock,
//!   a reserve in the link for this reader's hiccups. The driver's account
//!   of its calls, and this reader's own hold-ups, go to the log (`calls`).
//!
//! The loop is the same over memory in this process (tests) as over the
//! named memory a driver opens (`aura_link::win`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aura_link::{ClientInfo, Mode, Read, Segment, Server};
use serde_json::{json, Value};

use super::calls::CallWatch;
use super::feed::Feed;

/// The heartbeat: the driver counts the server gone after a second without.
const BEAT: Duration = Duration::from_millis(100);
/// The longest wait for the player before the loop looks again.
const POLL: Duration = Duration::from_millis(20);
/// The wait while the source is full, or the even pace takes nothing yet:
/// the reader's tick, which the player's calls follow.
const PACE: Duration = Duration::from_millis(2);
/// Samples read at once at most.
const CHUNK: usize = 16_384;
/// What the player may run ahead of the reader in the link, seconds
/// (`AURA_INPUT_RING_S`): the link is always full (the reader takes only
/// what the source has room for), so this is delay to the sound. A block
/// always fits an empty link (`aura_link::Client::room`): a player with
/// longer blocks hands them over one at a time.
const RING_S: f64 = 0.05;

/// What the reader tells the player.
pub trait Events: Send + Sync + 'static {
    /// The stream goes on in a format no session plays (the first, or
    /// another rate): a session for it, whose feed is attached
    /// (`Link::attach`). Nothing is read until then.
    fn format(&self, seg: Segment);
    /// A player began playing while the input was not listened to.
    fn started(&self);
}

struct State {
    want: Mode,
    /// Bumped by every listen and release: the server goes through
    /// Released, so the stream is taken up from its newest sample and its
    /// format told again.
    epoch: u64,
    client: Option<ClientInfo>,
    /// The segment being read.
    fmt: Option<Segment>,
    /// Its format was handed out for a session (`Events::format`): nothing
    /// is read until the session's feed is attached.
    asked: bool,
}

/// The server and what the controller and the reader share.
pub struct Link {
    st: Mutex<State>,
    feed: Mutex<Option<Feed>>,
    quit: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Why the input has no server, when it should have one ("link").
    error: Option<&'static str>,
    /// Whether a player starting takes the player over (`super::auto`).
    auto: fn() -> bool,
}

/// A segment's rate in whole Hz.
pub fn rate_of(seg: &Segment) -> u32 {
    seg.rate.round().max(0.0) as u32
}

impl Link {
    fn new(error: Option<&'static str>, auto: fn() -> bool) -> Link {
        Link {
            st: Mutex::new(State { want: Mode::Released, epoch: 0, client: None, fmt: None, asked: false }),
            feed: Mutex::new(None),
            quit: AtomicBool::new(false),
            reader: Mutex::new(None),
            error,
            auto,
        }
    }

    /// A link over `server`, read by a thread of its own.
    pub fn spawn(server: Server, events: Arc<dyn Events>, auto: fn() -> bool) -> Arc<Link> {
        let link = Arc::new(Link::new(None, auto));
        let l = link.clone();
        let h = std::thread::Builder::new()
            .name("aura-input".into())
            .spawn(move || run(&l, server, &*events))
            .expect("spawn the input's thread");
        *link.reader.lock().unwrap() = Some(h);
        link
    }

    /// The input with no server: the system would not make it.
    pub fn broken(auto: fn() -> bool) -> Arc<Link> {
        Arc::new(Link::new(Some("link"), auto))
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn feed(&self) -> MutexGuard<'_, Option<Feed>> {
        self.feed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The reader ends, and the server with it (the driver hears it at once).
    pub fn stop(&self) {
        self.quit.store(true, Ordering::Release);
        let h = self.reader.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    /// The input is listened to from now: its stream from the newest sample.
    pub fn listen(&self) {
        let mut st = self.st();
        if st.want == Mode::Released {
            st.want = Mode::Listening;
            st.epoch += 1;
            st.fmt = None;
            st.asked = false;
        }
    }

    /// Paused here (the player stands still), or playing again.
    pub fn hold(&self, on: bool) {
        let mut st = self.st();
        if st.want != Mode::Released {
            st.want = if on { Mode::Held } else { Mode::Listening };
        }
    }

    /// Not listened to: the player plays on as into an unplugged cable.
    pub fn release(&self) {
        {
            let mut st = self.st();
            if st.want == Mode::Released {
                return;
            }
            st.want = Mode::Released;
            st.epoch += 1;
            st.fmt = None;
            st.asked = false;
        }
        *self.feed() = None;
    }

    pub fn listening(&self) -> bool {
        self.st().want != Mode::Released
    }

    /// The session for the format handed out is there: the reader feeds it.
    /// A session started again (another rack, another device) takes the
    /// stream over from the one before, at the same place.
    pub fn attach(&self, feed: Feed) {
        let mut st = self.st();
        if st.want == Mode::Released {
            return;
        }
        st.asked = false;
        *self.feed() = Some(feed);
    }

    /// The segment being read, while the input is listened to.
    pub fn format(&self) -> Option<Segment> {
        let st = self.st();
        st.fmt.filter(|_| st.want != Mode::Released)
    }

    /// A player is connected and playing.
    pub fn client_running(&self) -> bool {
        self.st().client.as_ref().is_some_and(|c| c.running)
    }

    /// The connected player's name ("" when none).
    pub fn client_name(&self) -> String {
        self.st().client.as_ref().map_or_else(String::new, |c| c.name.clone())
    }

    /// The input's part of the status (less the driver's state): who is
    /// connected, what comes in, what the player does with it.
    pub fn status(&self) -> Value {
        let st = self.st();
        let listening = st.want != Mode::Released;
        json!({
            "client": st.client.as_ref().map(|c| json!({ "name": c.name, "running": c.running })),
            "format": st.fmt.filter(|_| listening).map(|f| json!({
                "rate": f.rate,
                "channels": f.channels,
                "sampleType": f.sample_type,
            })),
            "listening": listening,
            "held": st.want == Mode::Held,
            "auto": (self.auto)(),
            "error": self.error,
        })
    }
}

/// What a step of the loop leaves to do.
enum Step {
    /// Read on at once.
    Go,
    /// Wait for the player (or for the controller).
    Wait,
    /// The source is full: wait for the chain to read.
    Pace,
    /// A format for a session of its own.
    Format(Segment),
}

fn run(link: &Link, mut server: Server, events: &dyn Events) {
    raise_priority();
    let mut buf = vec![0.0f64; CHUNK];
    let mut beat_at: Option<Instant> = None;
    let mut seen_epoch = 0;
    let mut was_running: Option<bool> = None;
    let mut watch = CallWatch::new();
    // How long the loop meant to wait after its last step.
    let mut meant: Option<Duration> = None;
    crate::aelog!("[INPUT] listening for players on the link");
    while !link.quit.load(Ordering::Acquire) {
        if beat_at.is_none_or(|t| t.elapsed() >= BEAT) {
            server.beat();
            beat_at = Some(Instant::now());
        }
        let client = server.client();
        let running = client.as_ref().is_some_and(|c| c.running);
        let (want, epoch, per_s) = {
            let mut st = link.st();
            if st.client.as_ref().map(|c| (c.pid, c.gen)) != client.as_ref().map(|c| (c.pid, c.gen)) {
                match &client {
                    Some(c) => crate::aelog!("[INPUT] {} connected (pid {})", c.name, c.pid),
                    None => crate::aelog!("[INPUT] the player let go of the link"),
                }
            }
            st.client = client.clone();
            (st.want, st.epoch, st.fmt.map_or(0.0, |f| f.rate * f.channels as f64))
        };
        if epoch != seen_epoch {
            // Through Released: what was sent before is gone, and the
            // format is told again.
            seen_epoch = epoch;
            if server.mode() != Mode::Released {
                server.set_mode(Mode::Released);
            }
        }
        if server.mode() != want {
            server.set_mode(want);
        }
        // The auto switch: a player that starts while nobody listens.
        if was_running == Some(false) && running && want == Mode::Released && (link.auto)() {
            crate::aelog!("[INPUT] a player started playing: the input takes the player over");
            events.started();
        }
        was_running = Some(running);
        if want != Mode::Listening {
            watch.round(Instant::now(), None);
            server.wait_data(POLL);
            continue;
        }
        let now = Instant::now();
        watch.round(now, meant);
        if let Some(c) = &client {
            watch.look(&server, c.gen, c.running, per_s, now);
        }
        meant = match step(link, &mut server, &mut buf, running) {
            Step::Go => Some(Duration::ZERO),
            Step::Wait => {
                server.wait_data(POLL);
                Some(POLL)
            }
            Step::Pace => {
                std::thread::sleep(PACE);
                Some(PACE)
            }
            Step::Format(seg) => {
                crate::aelog!("[INPUT] the player plays {} Hz, {} ch: a session for it", seg.rate, seg.channels);
                events.format(seg);
                None
            }
        };
    }
    crate::aelog!("[INPUT] the link closes");
}

/// The reader is the player's clock (`super::pace`): above the pools the
/// chain computes on, as the render thread is, or a busy chain delays the
/// player's calls.
fn raise_priority() {
    crate::audio::os_thread::set_priority(crate::audio::os_thread::HIGHEST);
}

/// How far the player may run ahead of the reader in the link at this
/// segment's rate.
fn target_for(seg: &Segment) -> u64 {
    let s = std::env::var("AURA_INPUT_RING_S").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(RING_S);
    let ch = seg.channels.max(1) as u64;
    ((s.max(0.0) * seg.rate) as u64).max(1) * ch
}

fn step(link: &Link, server: &mut Server, buf: &mut [f64], running: bool) -> Step {
    let mut fg = link.feed();
    if fg.as_ref().is_some_and(|f| f.stopped()) {
        *fg = None;
    }
    let Some(feed) = fg.as_mut() else {
        drop(fg);
        server.set_even(false);
        // No session: only a format is looked for (an empty buffer takes no
        // sample) — the first one, or after the session ended: then the
        // player waits until the input is let go or listened to again.
        if link.st().fmt.is_some() {
            return Step::Wait;
        }
        return match server.read(&mut buf[..0]) {
            Read::Format(seg) => {
                server.set_target(target_for(&seg));
                let mut st = link.st();
                st.fmt = Some(seg);
                st.asked = true;
                Step::Format(seg)
            }
            _ => Step::Wait,
        };
    };
    feed.note_link(server.available());
    let room = feed.room();
    server.set_even(feed.paced());
    if room <= 0 {
        return Step::Pace;
    }
    server.set_latency_frames(feed.delay_frames().clamp(0, u32::MAX as i64) as u32);
    let ch = feed.channels().max(1);
    let n = ((room as usize).saturating_mul(ch).min(buf.len()) / ch * ch).max(ch);
    match server.read(&mut buf[..n]) {
        Read::Samples(k) => {
            feed.push(&buf[..k]);
            feed.took(k / ch);
            Step::Go
        }
        Read::Format(seg) => {
            server.set_target(target_for(&seg));
            let mut st = link.st();
            st.fmt = Some(seg);
            if rate_of(&seg) == feed.rate() {
                feed.segment(seg.channels);
                crate::aelog!("[INPUT] the player plays on at {} Hz ({} ch)", seg.rate, seg.channels);
                Step::Go
            } else {
                st.asked = true;
                *fg = None;
                Step::Format(seg)
            }
        }
        Read::Empty => {
            // The player stopped: its segment is over.
            if !running && feed.open() {
                feed.end();
                crate::aelog!("[INPUT] the player stopped");
            }
            Step::Wait
        }
    }
}

static LINK: OnceLock<Arc<Link>> = OnceLock::new();

/// The app's link, once made (`start`); none on a system without one.
pub fn get() -> Option<Arc<Link>> {
    LINK.get().cloned()
}

/// Makes the app's link (once): the server under `AURA_LINK_BASE`, or the
/// driver's own name.
pub fn start(events: Arc<dyn Events>, auto: fn() -> bool) {
    #[cfg(windows)]
    {
        let _ = LINK.get_or_init(|| {
            let base = std::env::var("AURA_LINK_BASE").unwrap_or_else(|_| aura_link::win::BASE.to_string());
            match aura_link::win::create_server(&base, aura_link::win::RING_SAMPLES) {
                Ok(server) => Link::spawn(server, events, auto),
                Err(e) => {
                    crate::aelog!("[INPUT] the link could not be made ({}): {}", base, e);
                    Link::broken(auto)
                }
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (events, auto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::radio::Session;
    use crate::player::settings::PlayerSettings;
    use aura_link::{region_bytes, Client, HeapRegion, LocalSignals};
    use std::sync::atomic::AtomicU64;

    const RING: usize = 1 << 18;
    const INT32: u32 = 18;

    fn pair() -> (Server, Client) {
        let region = HeapRegion::new(region_bytes(RING));
        let sig = LocalSignals::default();
        let server = Server::create(Box::new(region.clone()), Box::new(sig.clone()), RING, 1);
        let client = Client::open(Box::new(region), Box::new(sig), 2, |_| true).unwrap();
        (server, client)
    }

    /// The controller in a test: each format asked for gets a session of
    /// its own, its feed attached as the controller attaches it.
    #[derive(Default)]
    struct Player {
        link: Mutex<Option<Arc<Link>>>,
        sessions: Mutex<Vec<Arc<Session>>>,
        formats: Mutex<Vec<f64>>,
        started: AtomicU64,
    }

    impl Events for Player {
        fn format(&self, seg: Segment) {
            self.formats.lock().unwrap().push(seg.rate);
            let s = Arc::new(Session::start_input(&plain(), rate_of(&seg), seg.channels, "test").unwrap());
            if let Some(old) = self.sessions.lock().unwrap().last() {
                old.stop();
            }
            self.sessions.lock().unwrap().push(s.clone());
            let link = self.link.lock().unwrap().clone().unwrap();
            link.attach(Feed::new(s.shared.clone(), seg.channels).unwrap());
        }
        fn started(&self) {
            self.started.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn on() -> bool {
        true
    }

    /// A rack without source stages: what the player sends is in the
    /// source at once.
    fn plain() -> PlayerSettings {
        PlayerSettings {
            isp: false,
            subsonic_hz: 0,
            apodizing: 0,
            adaptive_apodizer: false,
            declip: false,
            adaptive_headroom: false,
            ..Default::default()
        }
    }

    fn link_with(server: Server) -> (Arc<Link>, Arc<Player>) {
        let p = Arc::new(Player::default());
        let link = Link::spawn(server, p.clone(), on);
        *p.link.lock().unwrap() = Some(link.clone());
        (link, p)
    }

    /// Writes as fast as the link lets it, as the driver does (while the
    /// server listens or holds: when a block fits): `frames` frames of a
    /// count (left) and its negative (right).
    fn play(client: &mut Client, from: u64, frames: u64, block: u64, quit: &AtomicBool) -> u64 {
        let mut n = from;
        while n < from + frames && !quit.load(Ordering::Acquire) {
            if client.mode(Duration::from_secs(5)) == Mode::Released || !client.room(block * 2) {
                client.wait_room(Duration::from_millis(5));
                continue;
            }
            let k = block.min(from + frames - n);
            client.write_with((k * 2) as usize, |i| {
                let f = (n + i as u64 / 2) as f64 / 2_147_483_648.0;
                if i % 2 == 0 { f } else { -f }
            });
            n += k;
        }
        n - from
    }

    fn until(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed().as_secs_f64() < secs, "{what}: not within {secs} s");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The player in the test plays as fast as it can; the chain reads in
    /// real time. The reader must hold the player back to the chain's pace:
    /// what it sent stays within the source's ceiling (and the link's
    /// target) of what the chain has read — not seconds of music ahead.
    #[test]
    fn the_player_is_held_to_the_pace_the_chain_reads_at() {
        let (server, mut client) = pair();
        let (link, p) = link_with(server);
        link.listen();
        let rate = 48_000u64;
        client.begin(rate as f64, 2, INT32);
        let quit = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (q, s) = (quit.clone(), sent.clone());
        let host = std::thread::spawn(move || {
            while !q.load(Ordering::Acquire) {
                let n = play(&mut client, s.load(Ordering::Acquire), 1024, 1024, &q);
                s.fetch_add(n, Ordering::AcqRel);
            }
            client
        });
        until("a session", 5.0, || !p.sessions.lock().unwrap().is_empty());
        let live = p.sessions.lock().unwrap()[0].shared.live.get().unwrap().clone();
        // The chain: the margin in hand before it starts, then a block of
        // 10 ms every 10 ms.
        until("the margin", 5.0, || live.end() >= rate as i64 / 10);
        let t0 = Instant::now();
        let mut read = 0i64;
        while t0.elapsed() < Duration::from_millis(1500) {
            std::thread::sleep(Duration::from_millis(10));
            let due = (t0.elapsed().as_secs_f64() * rate as f64) as i64;
            if due > read {
                read = due;
                live.ensure(read);
            }
        }
        let sent_now = sent.load(Ordering::Acquire) as i64;
        quit.store(true, Ordering::Release);
        let _client = host.join().unwrap();
        // The ceiling (the 0.1 s margin, nothing planned, and 0.05 s), the
        // link's target and a block: a fifth of a second, not seconds.
        let ceiling = ((crate::player::radio::INPUT_MARGIN_S + super::super::feed::AHEAD_S) * rate as f64) as i64;
        let bound = read + ceiling + (target_for(&Segment { id: 0, rate: rate as f64, channels: 2, sample_type: 0, start: 0 }) / 2) as i64 + 1024;
        assert!(bound - read <= rate as i64 / 4, "the bound itself: {} frames", bound - read);
        assert!(sent_now <= bound, "the player sent {sent_now} frames in 1.5 s while the chain read {read}: ahead by {:.2} s", (sent_now - read) as f64 / rate as f64);
        assert!(sent_now >= read, "the player kept up: {sent_now} sent, {read} read");
        assert_eq!(live.stats().starves, 0, "the chain never ran dry");
        assert_eq!(*p.formats.lock().unwrap(), [48_000.0]);
        link.stop();
    }

    /// A player whose own source runs by its own clock (a bridge from a
    /// system device: VB-Audio's ASIO Bridge): what it hands over waits in a
    /// FIFO of `fifo` blocks filled in real time, and every call takes a
    /// block out. Calls that come in bunches run it dry; a wait longer than
    /// it holds runs it over.
    struct Bridge {
        rate: f64,
        block: f64,
        cap: f64,
        level: f64,
        at: Instant,
        dry: u32,
        over: u32,
    }

    impl Bridge {
        fn new(rate: f64, block: f64, fifo: f64) -> Bridge {
            Bridge { rate, block, cap: fifo * block, level: fifo * block / 2.0, at: Instant::now(), dry: 0, over: 0 }
        }

        fn call(&mut self, now: Instant) {
            self.level += (now - self.at).as_secs_f64() * self.rate;
            self.at = now;
            // A block too many is dropped; one short, silence played instead.
            if self.level > self.cap {
                self.over += 1;
                self.level -= self.block;
            }
            if self.level < self.block {
                self.dry += 1;
            } else {
                self.level -= self.block;
            }
        }
    }

    /// The chain takes its source in portions of a quarter of a second (its
    /// output stages' step, at any rate and on either processor); the player
    /// is called as the driver calls it — when a block fits the link. The
    /// calls must come once a block's time apart, not a portion's worth at
    /// once and then nothing for a quarter of a second: a bridge's FIFO of
    /// four blocks neither runs dry nor over, and the chain never starves.
    #[test]
    fn the_player_is_called_once_a_block_while_the_chain_reads_in_portions() {
        let (server, mut client) = pair();
        let (link, p) = link_with(server);
        link.listen();
        let (rate, block) = (48_000u64, 2_048u64);
        let period = block as f64 / rate as f64;
        client.begin(rate as f64, 2, INT32);
        let quit = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let (q, c) = (quit.clone(), calls.clone());
        let host = std::thread::spawn(move || {
            while !q.load(Ordering::Acquire) {
                if client.mode(Duration::from_secs(5)) == Mode::Released || !client.room(block * 2) {
                    client.wait_room(Duration::from_millis(20));
                    continue;
                }
                c.lock().unwrap().push(Instant::now());
                client.write_with((block * 2) as usize, |_| 0.0);
            }
            client
        });
        until("a session", 5.0, || !p.sessions.lock().unwrap().is_empty());
        let s = p.sessions.lock().unwrap()[0].clone();
        let live = s.shared.live.get().unwrap().clone();
        // What the chain waits for before it is built, then built.
        s.mark(|i, _| i.need_s = 0.3);
        until("what the chain waits for", 5.0, || live.end() >= (0.3 * rate as f64) as i64);
        s.mark(|i, ms| i.t_built_ms = Some(ms));
        // The device's clock: a portion every quarter of a second.
        let portion = rate as i64 / 4;
        let t0 = Instant::now();
        let settle = Duration::from_millis(1500);
        let mut asked = 0i64;
        let mut bridge: Option<(Bridge, usize)> = None;
        while t0.elapsed() < Duration::from_millis(4500) {
            let due = ((t0.elapsed().as_secs_f64() * 4.0).floor() as i64 + 1) * portion;
            if due > asked {
                asked = due;
                live.ensure(asked);
            }
            if bridge.is_none() && t0.elapsed() >= settle {
                bridge = Some((Bridge::new(rate as f64, block as f64, 4.0), calls.lock().unwrap().len()));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        quit.store(true, Ordering::Release);
        let _client = host.join().unwrap();
        let (mut bridge, from) = bridge.unwrap();
        let calls = calls.lock().unwrap()[from..].to_vec();
        for &t in &calls {
            bridge.call(t);
        }
        let iv: Vec<f64> = calls.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
        let n = iv.len() as f64;
        let mean = iv.iter().sum::<f64>() / n;
        let sd = (iv.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        let max = iv.iter().cloned().fold(0.0, f64::max);
        let late = iv.iter().filter(|&&x| x > 1.5 * period).count();
        let said = format!(
            "{} calls, every {:.1} ms (sd {:.1}, max {:.1}; {late} over 1.5 periods) for a period of {:.1} ms; the bridge ran dry {} and over {} times",
            calls.len(), mean * 1e3, sd * 1e3, max * 1e3, period * 1e3, bridge.dry, bridge.over
        );
        println!("{said}");
        assert!((mean / period - 1.0).abs() < 0.03, "the pace: {said}");
        assert!(sd < period / 4.0, "the calls are even: {said}");
        assert!(late as f64 <= n * 0.03, "hardly a call late: {said}");
        assert_eq!((bridge.dry, bridge.over), (0, 0), "{said}");
        assert_eq!(live.stats().starves, 0, "the chain never ran dry");
        link.stop();
    }

    /// The same rate again (the player stopped and played, or set it anew):
    /// the session goes on, across a gap. Another rate: a session of its
    /// own, and the one before gets nothing of it.
    #[test]
    fn a_segment_at_the_same_rate_is_a_gap_and_another_rate_a_new_session() {
        let (server, mut client) = pair();
        let (link, p) = link_with(server);
        link.listen();
        let quit = AtomicBool::new(false);
        client.begin(44_100.0, 2, INT32);
        play(&mut client, 0, 4_410, 441, &quit);
        until("a session", 5.0, || !p.sessions.lock().unwrap().is_empty());
        let first = p.sessions.lock().unwrap()[0].shared.live.get().unwrap().clone();
        until("the first segment", 5.0, || first.end() >= 4_000);
        client.end();
        until("the player's stop", 5.0, || first.stats().gaps == 1);
        assert!(!p.sessions.lock().unwrap()[0].shared.info.lock().unwrap().connected, "stopped: not connected");
        // The chain reads what is there: the source is kept thin.
        first.ensure(first.end());
        client.begin(44_100.0, 2, INT32);
        play(&mut client, 4_410, 4_410, 441, &quit);
        until("the second segment", 5.0, || first.end() >= 8_000);
        assert_eq!(first.stats().gaps, 1, "one gap");
        assert_eq!(p.sessions.lock().unwrap().len(), 1, "the same session");
        let end = first.end();
        client.begin(96_000.0, 2, INT32);
        play(&mut client, 0, 9_600, 960, &quit);
        until("a session at 96 kHz", 5.0, || p.sessions.lock().unwrap().len() == 2);
        let second = p.sessions.lock().unwrap()[1].shared.live.get().unwrap().clone();
        until("96 kHz in", 5.0, || second.end() >= 9_000);
        assert_eq!(second.rate(), 96_000);
        assert!(first.end() <= end + 4_410, "the 44.1 kHz session got none of the 96 kHz stream");
        assert_eq!(*p.formats.lock().unwrap(), [44_100.0, 96_000.0]);
        link.stop();
    }

    /// Held: the player stands still once the link is full, nothing read;
    /// listened to again, it goes on where it stopped. Released: the player
    /// is told so, and the session's feed is let go.
    #[test]
    fn held_stands_the_player_still_and_released_lets_it_go() {
        let (server, mut client) = pair();
        let (link, p) = link_with(server);
        link.listen();
        client.begin(48_000.0, 2, INT32);
        let quit = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (q, s) = (quit.clone(), sent.clone());
        let host = std::thread::spawn(move || {
            while !q.load(Ordering::Acquire) {
                if client.mode(Duration::from_secs(5)) == Mode::Released {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let n = play(&mut client, s.load(Ordering::Acquire), 480, 480, &q);
                s.fetch_add(n, Ordering::AcqRel);
            }
            client
        });
        until("a session", 5.0, || !p.sessions.lock().unwrap().is_empty());
        let live = p.sessions.lock().unwrap()[0].shared.live.get().unwrap().clone();
        until("some music", 5.0, || live.end() >= 5_000);
        link.hold(true);
        std::thread::sleep(Duration::from_millis(150));
        let (a, end) = (sent.load(Ordering::Acquire), live.end());
        // The chain reads meanwhile: room in the source, but held.
        live.ensure(end);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(sent.load(Ordering::Acquire), a, "the player stood still");
        assert_eq!(live.end(), end, "nothing read while held");
        link.hold(false);
        until("on again", 5.0, || live.end() > end);
        link.release();
        assert!(link.format().is_none() && !link.listening());
        quit.store(true, Ordering::Release);
        let mut client = host.join().unwrap();
        until("Released told", 5.0, || client.mode(Duration::from_secs(5)) == Mode::Released);
        assert!(link.feed().is_none(), "the feed let go");
        link.stop();
    }

    /// A player that starts while nobody listens: the auto switch is asked
    /// for once — not for one playing already when the app looked first,
    /// and not while the input is listened to.
    #[test]
    fn a_player_starting_while_nobody_listens_asks_for_the_input_once() {
        let (server, mut client) = pair();
        client.begin(44_100.0, 2, INT32);
        let (link, p) = link_with(server);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(p.started.load(Ordering::Acquire), 0, "playing before the app looked: no switch");
        client.end();
        std::thread::sleep(Duration::from_millis(100));
        client.begin(44_100.0, 2, INT32);
        until("the switch asked for", 5.0, || p.started.load(Ordering::Acquire) == 1);
        link.listen();
        client.end();
        std::thread::sleep(Duration::from_millis(100));
        client.begin(44_100.0, 2, INT32);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(p.started.load(Ordering::Acquire), 1, "listened to: no switch");
        let v = link.status();
        assert_eq!(v["listening"], true);
        assert_eq!(v["client"]["running"], true);
        assert_eq!(v["format"]["rate"], 44_100.0);
        assert_eq!(v["auto"], true);
        assert!(v["error"].is_null());
        link.stop();
    }

    #[test]
    fn a_broken_link_says_so() {
        let v = Link::broken(on).status();
        assert_eq!(v["error"], "link");
        assert_eq!(v["listening"], false);
        assert!(v["client"].is_null() && v["format"].is_null());
    }
}
