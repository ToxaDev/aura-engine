//! The input on the transport: what another player plays into Aura Player
//! ASIO (`player::input`), played as a stream is (`radio_job`) — a session
//! of the radio's, its chain planned, built and put on the air the same way,
//! with the input's source and no drift corridor (`radio::StreamSrc`).
//!
//! * Listen (the input's view, or a player starting with the auto switch
//!   on): what plays fades out, and the link listens. The session comes with
//!   the player's first format (`Job::InputFormat`); one at another rate is
//!   a session of its own, the device held between them as between two
//!   stations.
//! * ⏸ holds the link (the player stands still), ▶ listens again. Stop, a
//!   track or a station let it go: the player plays on as into an unplugged
//!   cable.
//! * A rack, a device or BIT-PERFECT that has the session start again
//!   starts it again from the input (`Job::InputRestart`), at the format
//!   being read.

use super::*;
use crate::player::input::{self, feed::Feed, link};
use crate::player::radio::{Session, StreamSrc};
use aura_link::Segment;

/// The device held in silence for a player whose format has not come (it
/// stopped as it was listened to): the player stops after this.
const HOLD_FOR_INPUT: Duration = Duration::from_secs(2);

/// The player's end of the input's link: what its reader asks for becomes
/// a job of the worker's.
struct ToPlayer;

impl link::Events for ToPlayer {
    fn format(&self, seg: Segment) {
        get().send_internal(Job::InputFormat { seg });
    }
    fn started(&self) {
        // As the listener's own choice: a pre-roll under way gives way to it.
        get().send_job(Job::InputListen);
    }
}

/// Makes the input's link, at the app's start.
pub fn start_input() {
    input::start(Arc::new(ToPlayer));
}

/// The job that starts a session again from its source.
pub(super) fn again(src: &StreamSrc) -> Job {
    match src {
        StreamSrc::Url(url) => Job::Radio { url: url.clone() },
        StreamSrc::Input => Job::InputRestart,
    }
}

impl Player {
    /// Listen to the input (its view's Listen).
    pub fn input_listen(&self) {
        self.send_job(Job::InputListen);
    }

    /// A player starting takes the player over, or not.
    pub fn input_set_auto(&self, on: bool) {
        input::set_auto(on);
    }

    /// The input is the source: listened to, with its session or before it.
    pub(super) fn input_listening(&self) -> bool {
        link::get().is_some_and(|l| l.listening())
    }

    /// Start a session again from `src`, here on the worker.
    pub(super) fn stream_again(&self, src: StreamSrc) {
        match src {
            StreamSrc::Url(url) => self.radio_start(url),
            StreamSrc::Input => self.input_restart(),
        }
    }

    pub(super) fn input_listen_now(&self) {
        let Some(l) = link::get() else { return };
        if l.listening() {
            return;
        }
        crate::aelog!("[UI] input: listen");
        // What plays fades out. A player playing sends its format at once:
        // the device is held for it in silence, as for a station; else the
        // player stops until one plays.
        if l.client_running() && self.radio_hold_air() {
            *self.input_wait.lock().unwrap() = Some(Instant::now());
        } else {
            self.fade_out();
            self.stop_now();
        }
        {
            let mut st = self.st.lock().unwrap();
            st.current = None;
            st.error = None;
        }
        l.listen();
    }

    /// The input is let go (Stop, a track, a station): its player plays on
    /// as into an unplugged cable.
    pub(super) fn input_release(&self) {
        *self.input_wait.lock().unwrap() = None;
        if let Some(l) = link::get().filter(|l| l.listening()) {
            crate::aelog!("[INPUT] let go: the player plays on, unheard");
            l.release();
        }
    }

    /// ⏸ (the player stands still) or ▶ on the input.
    pub(super) fn input_hold(&self, on: bool) {
        if let Some(l) = link::get() {
            l.hold(on);
        }
    }

    /// The player plays `seg`, which no session plays: a session for it.
    pub(super) fn input_format(&self, seg: Segment) {
        if self.input_listening() {
            self.input_start(seg);
        }
    }

    /// The session again, at the format being read (another rack, device,
    /// mode); none is being read: nothing plays until the player plays.
    pub(super) fn input_restart(&self) {
        match link::get().and_then(|l| l.format()) {
            Some(seg) => self.input_start(seg),
            None => {
                self.fade_out();
                self.stop_now();
            }
        }
    }

    fn input_start(&self, seg: Segment) {
        let Some(l) = link::get() else { return };
        let rate = link::rate_of(&seg);
        crate::aelog!("[INPUT] a session at {} Hz, {} ch", rate, seg.channels);
        // As a station's start: what plays (the session before, at another
        // rate) fades out and the device stays open for the new one.
        if !self.radio_hold_air() {
            self.stop_now();
        }
        *self.input_wait.lock().unwrap() = None;
        let gen = self.radio_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let settings = {
            let mut st = self.st.lock().unwrap();
            st.error = None;
            st.current = None;
            st.pending = Some(("Connecting".into(), Instant::now()));
            st.settings.clone()
        };
        let started = Session::start_input(&settings, rate, seg.channels, &l.client_name())
            .and_then(|s| Feed::new(s.shared.clone(), seg.channels).map(|f| (Arc::new(s), f)));
        let (session, feed) = match started {
            Ok(x) => x,
            Err(e) => {
                self.set_pending(None);
                self.radio_fail_from(&StreamSrc::Input, crate::player::radio::ERR_OTHER, e);
                return;
            }
        };
        *self.radio.lock().unwrap() = Some((gen, session.clone()));
        // A new session plays (one started again under a pause: the player
        // goes on, or the chain would wait for it).
        l.hold(false);
        l.attach(feed);
        self.radio_launch(gen, session, settings);
    }

    /// The tick's: the device held for a player that has not sent its
    /// format. True while it waits (nothing for the track housekeeping to
    /// follow); after HOLD_FOR_INPUT the player stops.
    pub(super) fn input_holding(&self) -> bool {
        let mut w = self.input_wait.lock().unwrap();
        let Some(since) = *w else { return false };
        if self.radio_active() {
            *w = None;
            return false;
        }
        if since.elapsed() > HOLD_FOR_INPUT {
            *w = None;
            drop(w);
            crate::aelog!("[INPUT] no format from the player: the device is let go");
            self.stop_now();
        }
        true
    }
}
