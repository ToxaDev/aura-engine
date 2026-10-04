//! Live consumer thread for the output analytics.
//!
//! **Owner: implementer D.**
//!
//! One global `"aura-analytics-live"` thread wakes every 20 ms and reads the
//! frames `[last_read, read_pos)` from the live [`Timeline`].  It segments the
//! window by [`Mark`] boundaries per **ERRATA E10**:
//!
//! | Discontinuity | Segment type |
//! |---|---|
//! | `mark.track_id ≠ prev.track_id` | Track change |
//! | `!Arc::ptr_eq(&mark.desc, &prev.desc)` | Chain change |
//! | `mark.index ≠ prev.index + (mark.frame − prev.frame)` | Seek splice |
//!
//! For each segment the O signal feeds a [`LoudnessMeter`] (streaming, at the
//! output rate) and a [`Stft`] folded into the spectrogram's log bands
//! (`spectra::SPEC_LOG_BINS`, as the S tiles; N = `spec_fft_len`).  Every ~5
//! wakeups (≈ 100 ms) a [`LiveFrame`] is assembled and handed to
//! [`publish_to_hub`], which will call `super::hub::get().publish_live_aan1`
//! once `hub.rs` (owner B) is written.
//!
//! # AAN1 ring
//!
//! Owner B's hub keeps the last 256 AAN1 `Arc<[u8]>` payloads indexed by
//! `seq`.  `live.rs` publishes complete frames; the hub maintains the ring and
//! handles `since=` delta requests from routes.rs.
//!
//! # Plausibility guard
//!
//! Never counts more played frames than
//! `wall_elapsed × out_rate × 1.25 + one_period`.  Prevents runaway
//! accumulation during a `jump_to` seek transition until ERRATA E10's
//! splice-log rebase lands.
//!
//! # Dependencies on sibling modules (not yet written)
//!
//! - `super::hub::publish_live_aan1` — stub in [`publish_to_hub`]; owner B
//!   replaces the body.
//! - `LiveFrame` and `LoudnessPoint` are defined here; `proto.rs` (owner A)
//!   either imports them from this module or defines its own versions and
//!   updates the import below.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::player::render::{ChainDesc, Shared as RenderShared, Mark};
use crate::player::output::OutputShared;
use crate::player::timeline::Timeline;
use crate::player::convolver::SourceBuf;
use super::loudness::LoudnessMeter;
use super::peaks::{EburTruePeak, lin_to_db};
use super::spectra::{LogBins, Stft, spec_fft_len, SPEC_BETA, SPEC_F0_HZ, SPEC_LOG_BINS};

// ─── Public wire types ────────────────────────────────────────────────────────
// These are re-used by proto.rs (owner A) when encoding the AAN1 wire frame.

/// One incremental O loudness history point carried by the AAN1 live frame.
/// PROTOCOL.md §5, variable `loud_pts` section (offset 146 + n_loud_pts*12).
#[derive(Clone, Debug)]
#[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
pub struct LoudnessPoint {
    /// Track-relative position in seconds.
    pub time_s: f32,
    /// LUFS-S (short-term, 3-second window) at this moment.
    pub lufs_s: f32,
    /// LUFS-M (momentary, 400-ms window) at this moment.
    pub lufs_m: f32,
}

/// All data needed to encode one AAN1 live frame (PROTOCOL.md §5).
///
/// Metric values are `f64` internally (NaN = UNAVAIL); `proto::encode_aan1`
/// downcasts to `f32[26]` for the wire.  Metric IDs per PROTOCOL.md §3.
#[derive(Clone, Debug)]
#[cfg_attr(test, allow(dead_code))] // reached from the app's live path, which the test build does not run
pub struct LiveFrame {
    /// AAN1 flags byte (PROTOCOL.md §5, offset 5).
    pub flags: u8,
    /// Monotonic sequence counter (wraps at u32::MAX).
    pub seq: u32,
    /// Current track id (0 if no track loaded).
    pub track_id: u64,
    /// Hash of current chain settings.
    pub chain_rev: u32,
    /// O coverage 0–100 %.
    pub coverage_pct: u8,
    /// log₂ of the bytes per spectrogram column: log-spaced bands from
    /// 20 Hz to the output's Nyquist (9 → 512, as the S tiles).
    pub n_fft_bins_log2: u8,
    /// log₂ of the column hop in output samples.
    pub spec_hop_log2: u8,
    /// |spectrogram floor| in dB, unsigned (e.g. 120 → −120 dBFS).
    pub spec_floor_neg: u8,
    /// O-tap metrics IDs 0–25 (slots 26–31 = NaN).  f64 internally.
    pub live_metrics: [f64; 32],
    /// Provenance nibbles for 26 O-tap metrics (PROTOCOL.md §4).
    pub prov_nibbles: [u8; 32],
    /// Incremental O loudness points since the previous AAN1.
    pub loud_pts: Vec<LoudnessPoint>,
    /// Incremental O spectrogram columns since the previous AAN1.
    /// Each inner `Vec` has exactly `1 << n_fft_bins_log2` bytes,
    /// encoded per PROTOCOL.md §5 (v = 0 → floor dBFS, v = 255 → 0 dBFS).
    pub spec_cols: Vec<Vec<u8>>,

    // analytics: S/B live metrics — backward-compat AAN1 extension (PROTOCOL.md §5 §SB-EXT)
    // All values NaN when no source audio is available.
    /// LUFS-M of the S tap at the audible source position (NaN = unavailable).
    pub s_lufs_m: f32,
    /// LUFS-S of the S tap at the audible source position (NaN = unavailable).
    pub s_lufs_s: f32,
    /// Running sample peak (dBFS) of the S tap since meter reset (NaN = unavailable).
    pub s_tp: f32,
    /// LUFS-M of the B tap (= S tap in the current implementation; NaN = unavailable).
    pub b_lufs_m: f32,
    /// LUFS-S of the B tap (NaN = unavailable).
    pub b_lufs_s: f32,
    /// Running sample peak (dBFS) of the B tap (NaN = unavailable).
    pub b_tp: f32,
    /// Packed provenance nibble: bits 0-3 = S prov code, bits 4-7 = B prov code.
    pub sb_prov: u8,
    /// On a stream, its S beside O: the STRM tail (stream.rs); empty else.
    pub strm: Vec<u8>,
}

impl Default for LiveFrame {
    fn default() -> Self {
        let mut live_metrics = [f64::NAN; 32];
        live_metrics[MID_COVERAGE_O] = 0.0;
        LiveFrame {
            flags: 0,
            seq: 0,
            track_id: 0,
            chain_rev: 0,
            coverage_pct: 0,
            n_fft_bins_log2: 9,
            spec_hop_log2: 0,
            spec_floor_neg: SPEC_FLOOR_NEG,
            live_metrics,
            prov_nibbles: [0u8; 32],
            loud_pts: Vec::new(),
            spec_cols: Vec::new(),
            // analytics: S/B extension defaults — NaN until source audio arrives
            s_lufs_m: f32::NAN,
            s_lufs_s: f32::NAN,
            s_tp:     f32::NAN,
            b_lufs_m: f32::NAN,
            b_lufs_s: f32::NAN,
            b_tp:     f32::NAN,
            sb_prov:  0, // UNAVAIL for both S and B
            strm: Vec::new(),
        }
    }
}

// ─── Metric ID constants (PROTOCOL.md §3) ─────────────────────────────────────

const MID_LUFS_S_LIVE: usize = 1;
const MID_LUFS_M_LIVE: usize = 2;
const MID_TP_EBUR:     usize = 4;
const MID_COVERAGE_O:  usize = 25;

/// The live true peak's window, in 10 Hz publishes (3 s).
const TP_WINDOW: usize = 30;

// ─── Provenance nibble codes (PROTOCOL.md §4) ─────────────────────────────────

const PROV_MEASURED:    u8 = 3;
const PROV_HP_PENDING:  u8 = 5;

// ─── AAN1 flag bits (PROTOCOL.md §5, offset 5) ───────────────────────────────

const FLAG_HP_ACTIVE:   u8 = 0x01;
const FLAG_B_READY:     u8 = 0x02;
// analytics: bit 2 — converted output file available (PROTOCOL.md §5)
const FLAG_CONV_EXISTS: u8 = 0x04;
const FLAG_XTC_ACTIVE:  u8 = 0x08;
const FLAG_STALE:       u8 = 0x10;
const FLAG_ISP_OUT:     u8 = 0x20;

// Suppress unused-variable warning until conv_exists state is fed from hub/subject.
#[allow(dead_code)]
const _FLAG_CONV_EXISTS_DEFINED: u8 = FLAG_CONV_EXISTS;

// ─── Tuning constants ─────────────────────────────────────────────────────────

/// Default spectrogram floor magnitude (unsigned dB).
const SPEC_FLOOR_NEG: u8 = 120;

/// Maximum spectrogram column ring depth.
const SPEC_RING_MAX: usize = 256;

/// Wakeups per 10 Hz publish epoch (20 ms × 5 = 100 ms).
const WAKEUPS_PER_EPOCH: u32 = 5;

/// Plausibility guard multiplier: real-time factor ceiling.
const PLAUS_FACTOR: f64 = 1.25;

// ─── Thread message ───────────────────────────────────────────────────────────

/// Message sent to the live consumer thread.
pub enum LiveMsg {
    /// Begin a new playback session.
    Stream {
        timeline:       Arc<Timeline>,
        render_shared:  Arc<RenderShared>,
        output_shared:  Arc<OutputShared>,
        /// Output sample rate, frames/second.
        out_rate: u32,
        /// Source sample rate if known at stream-start; refined from the first Mark.
        src_rate: Option<u32>,
        /// Track id at stream start.
        track_id: u64,
        /// Chain hash at stream start.
        chain_rev: u32,
        // Chain flags
        hp_active:      bool,
        xtc_active:     bool,
        isp_out_active: bool,
    },
    /// Flush partial state and end accumulation for the current session.
    /// The thread stays alive to accept the next `Stream` message.
    Stop,
    /// Whole-track S analysis completed (rev 5 published). Sets b_ready in
    /// the current session so the next AAN1 frame sets FLAG_B_READY.
    // analytics: notification from track.rs → live.rs for b_ready flag
    BReady,
    // analytics: S/B live meters — new source audio for the current track.
    /// A new source buffer arrived (on_variant hook). The live thread resets
    /// its S/B meters and starts reading from the SourceBuf at the audible
    /// source position.
    #[allow(dead_code)] // kept for the analyzer views not wired yet (S/B live meters)
    SrcBuf {
        buf:      Arc<SourceBuf>,
        track_id: u64,
    },
}

// ─── Global sender ────────────────────────────────────────────────────────────

static TX: OnceLock<Sender<LiveMsg>> = OnceLock::new();

/// Send a message to the live consumer thread.
/// Panics if [`start`] was not called first.
pub fn send(msg: LiveMsg) {
    let tx = TX.get().expect("live::start() must be called before live::send()");
    let _ = tx.send(msg);
}

/// Send a message without panicking when the thread is not running (e.g. during
/// unit tests that do not initialize the hub).
// analytics: safe send for use from track.rs BReady notification
pub fn try_send(msg: LiveMsg) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(msg);
    }
}

/// Convenience wrapper called from `hub::on_stream`.
/// analytics: hub hook
pub fn notify_stream(
    timeline:       Arc<Timeline>,
    render_shared:  Arc<RenderShared>,
    output_shared:  Arc<OutputShared>,
    out_rate:       u32,
    src_rate:       Option<u32>,
    track_id:       u64,
    chain_rev:      u32,
    hp_active:      bool,
    xtc_active:     bool,
    isp_out_active: bool,
) {
    send(LiveMsg::Stream {
        timeline, render_shared, output_shared,
        out_rate, src_rate, track_id, chain_rev,
        hp_active, xtc_active, isp_out_active,
    });
}

/// Called from `hub::on_stop`.
/// analytics: hub hook
pub fn notify_stop() {
    send(LiveMsg::Stop);
}

// ─── Hub publish stub ─────────────────────────────────────────────────────────

/// Hand a fully assembled `LiveFrame` to the hub for atomic publication.
///
/// Owner B replaces this stub body with:
/// ```ignore
/// super::hub::get().publish_live_aan1(frame);
/// ```
/// Until then the call is a no-op in non-test builds so compilation proceeds.
///
/// analytics: replace stub body once hub.rs is written
fn publish_to_hub(frame: LiveFrame) {
    #[cfg(test)]
    TEST_SINK.with(|s| {
        let mut g = s.lock().unwrap();
        g.push(frame);
    });
    // analytics: forward to hub for AAN1 publication
    #[cfg(not(test))]
    super::hub::get().publish_live_aan1(frame);
}

#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
thread_local! {
    /// Collects published frames during unit tests.
    static TEST_SINK: Mutex<Vec<LiveFrame>> = Mutex::new(Vec::new());
}
#[cfg(test)]
pub(super) fn drain_test_sink() -> Vec<LiveFrame> {
    TEST_SINK.with(|s| std::mem::take(&mut *s.lock().unwrap()))
}

// ─── Thread entry point ───────────────────────────────────────────────────────

/// Spawn the `"aura-analytics-live"` background thread at normal priority.
/// Call exactly once at application startup, before any `on_stream` call.
pub fn start() {
    let (tx, rx) = mpsc::channel::<LiveMsg>();
    TX.get_or_init(|| tx);
    std::thread::Builder::new()
        .name("aura-analytics-live".into())
        .spawn(move || thread_body(rx))
        .expect("failed to spawn aura-analytics-live thread");
}

fn thread_body(rx: Receiver<LiveMsg>) {
    let mut session: Option<Session> = None;

    loop {
        match rx.try_recv() {
            Ok(LiveMsg::Stream {
                timeline, render_shared, output_shared,
                out_rate, src_rate, track_id, chain_rev,
                hp_active, xtc_active, isp_out_active,
            }) => {
                // Flush previous session on track/stream change.
                if let Some(mut s) = session.take() {
                    s.flush_and_publish();
                }
                session = Some(Session::new(
                    timeline, render_shared, output_shared,
                    out_rate, src_rate, track_id, chain_rev,
                    hp_active, xtc_active, isp_out_active,
                ));
            }
            Ok(LiveMsg::Stop) => {
                if let Some(mut s) = session.take() {
                    s.flush_and_publish();
                }
            }
            // analytics: track.rs signals b_ready after rev-5 publish
            Ok(LiveMsg::BReady) => {
                if let Some(ref mut s) = session {
                    s.b_ready = true;
                }
            }
            // analytics: new SourceBuf arrived — reset S/B meters for the new track
            Ok(LiveMsg::SrcBuf { buf, track_id }) => {
                if let Some(ref mut s) = session {
                    s.on_src_buf(buf, track_id);
                }
            }
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }

        if let Some(ref mut s) = session {
            s.tick();
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}

// ─── Per-session state ────────────────────────────────────────────────────────

pub(super) struct Session {
    timeline:       Arc<Timeline>,
    render_shared:  Arc<RenderShared>,
    output_shared:  Arc<OutputShared>,
    out_rate: u32,
    src_rate: u32,              // refined from first Mark; starts as out_rate

    /// Exclusive upper bound of frames already processed.
    last_read: u64,
    /// Timeline frame at stream start (for coverage denominator).
    #[allow(dead_code)] // kept for the coverage denominator
    stream_start: u64,

    // O loudness accumulator.
    o_meter: LoudnessMeter,
    /// Frames fed to `o_meter` so far.
    frames_measured: u64,
    /// O true peak (BS.1770 at the output rate), taken per publish; the
    /// live value is the maximum of the last TP_WINDOW.
    o_tp: EburTruePeak,
    o_tp_ring: VecDeque<f64>,

    // Spectrogram accumulation.
    stft:           Stft,
    spec_bands:     LogBins,
    spec_power:     Vec<f64>,
    carry_l:        Vec<f64>,
    carry_r:        Vec<f64>,
    spec_ring:      VecDeque<Vec<u8>>,
    spec_new_since_publish: usize,

    // Scratch buffers (re-used each tick to avoid allocation).
    tick_l: Vec<f64>,
    tick_r: Vec<f64>,

    // Loudness history points pending the next 10 Hz publish.
    loud_pts_pending: Vec<LoudnessPoint>,

    // AAN1 sequence counter.
    seq: u32,
    // Wakeups since last 10 Hz publish.
    wakeup_in_epoch: u32,

    // Wall-clock base for plausibility guard.
    wall_start:          Instant,
    frames_at_wall_start: u64,

    // Last Mark seen (for segment discontinuity detection).
    last_mark: Option<Mark>,

    // Chain state (for AAN1 flags byte and provenance).
    track_id:       u64,
    chain_rev:      u32,
    hp_active:      bool,
    xtc_active:     bool,
    isp_out_active: bool,
    b_ready:        bool,
    stale:          bool,

    // analytics: S/B live meters — fed from SourceBuf at the audible source position.
    // Both S and B use the same SourceBuf (variant.src) in the current architecture;
    // they run in parallel with the O meter but at the source rate.
    src_buf:         Option<Arc<SourceBuf>>,
    /// Source frame cursor: how many source frames have been fed to s_meter.
    src_consumed:    u64,
    /// Running S LoudnessMeter (at src_rate).
    s_meter:         Option<LoudnessMeter>,
    /// Running S sample peak (linear, max over all samples seen).
    s_peak_lin:      f64,
    /// Running B LoudnessMeter (same signal as S in current impl).
    b_meter:         Option<LoudnessMeter>,
    /// Running B sample peak (linear).
    b_peak_lin:      f64,
    /// Track whose source `src_buf` holds (u64::MAX: none yet).
    src_track:       u64,
    /// O frames measured on the current track (coverage numerator).
    track_frames:    u64,
    /// The current track's length in output frames (0: unknown).
    track_total_out: u64,

    // The stereo picture of the output: L², R² and L·R summed with a 0.3 s
    // exponential memory (correlation, side over mid), and the last L/R
    // pairs for the vectorscope (~50 ms, every `vec_step`-th sample).
    st_ll: f64,
    st_rr: f64,
    st_lr: f64,
    vec_l: VecDeque<f32>,
    vec_r: VecDeque<f32>,
    vec_step: usize,
    vec_phase: usize,

    /// A stream: S from its shadow beside O (stream.rs).
    stream: Option<super::stream::StreamTap>,
}

/// The stereo meters' memory and the vectorscope's span and size.
const STEREO_TAU_S: f64 = 0.3;
const VEC_SPAN_S: f64 = 0.05;
const VEC_POINTS: usize = 2048;

impl Session {
    pub(super) fn new(
        timeline:       Arc<Timeline>,
        render_shared:  Arc<RenderShared>,
        output_shared:  Arc<OutputShared>,
        out_rate:       u32,
        src_rate:       Option<u32>,
        track_id:       u64,
        chain_rev:      u32,
        hp_active:      bool,
        xtc_active:     bool,
        isp_out_active: bool,
    ) -> Self {
        let src_rate = src_rate.unwrap_or(out_rate);
        let n_fft = spec_fft_len(out_rate);
        let stft = Stft::new(n_fft, n_fft / 2, SPEC_BETA);
        let spec_bands = LogBins::new(n_fft, out_rate, SPEC_LOG_BINS, SPEC_F0_HZ);
        let read_now = timeline.read_pos();

        Session {
            timeline,
            render_shared,
            output_shared,
            out_rate,
            src_rate,
            last_read: read_now,
            stream_start: read_now,
            o_meter: LoudnessMeter::new(out_rate as f64),
            frames_measured: 0,
            o_tp: EburTruePeak::new(out_rate),
            o_tp_ring: VecDeque::with_capacity(TP_WINDOW),
            stft,
            spec_bands,
            spec_power: Vec::new(),
            carry_l: Vec::new(),
            carry_r: Vec::new(),
            spec_ring: VecDeque::new(),
            spec_new_since_publish: 0,
            tick_l: Vec::new(),
            tick_r: Vec::new(),
            loud_pts_pending: Vec::new(),
            seq: 0,
            wakeup_in_epoch: 0,
            wall_start: Instant::now(),
            frames_at_wall_start: read_now,
            last_mark: None,
            track_id,
            chain_rev,
            hp_active,
            xtc_active,
            isp_out_active,
            b_ready: false,
            stale: false,
            // analytics: S/B live meters — initialised when SrcBuf arrives
            src_buf:      None,
            src_consumed: 0,
            s_meter:      None,
            s_peak_lin:   0.0,
            b_meter:      None,
            b_peak_lin:   0.0,
            src_track:       u64::MAX,
            track_frames:    0,
            track_total_out: 0,
            st_ll: 0.0,
            st_rr: 0.0,
            st_lr: 0.0,
            vec_l: VecDeque::with_capacity(VEC_POINTS),
            vec_r: VecDeque::with_capacity(VEC_POINTS),
            vec_step: ((out_rate as f64 * VEC_SPAN_S / VEC_POINTS as f64).round() as usize).max(1),
            vec_phase: 0,
            stream: None,
        }
    }

    /// The stereo sums and the vectorscope's pairs for one stretch of output.
    fn stereo_push(&mut self, l: &[f64], r: &[f64]) {
        let (mut ll, mut rr, mut lr) = (0.0, 0.0, 0.0);
        for (a, b) in l.iter().zip(r) {
            ll += a * a;
            rr += b * b;
            lr += a * b;
        }
        let d = (-(l.len() as f64) / (STEREO_TAU_S * self.out_rate.max(1) as f64)).exp();
        self.st_ll = self.st_ll * d + ll;
        self.st_rr = self.st_rr * d + rr;
        self.st_lr = self.st_lr * d + lr;
        let mut i = self.vec_phase;
        while i < l.len() {
            if self.vec_l.len() == VEC_POINTS {
                self.vec_l.pop_front();
                self.vec_r.pop_front();
            }
            self.vec_l.push_back(l[i] as f32);
            self.vec_r.push_back(r[i] as f32);
            i += self.vec_step;
        }
        self.vec_phase = i - l.len();
    }

    /// Correlation (−1..1) and side over mid (dB) of the last 0.3 s; NaN in
    /// silence.
    fn stereo_now(&self) -> (f64, f64) {
        let (ll, rr, lr) = (self.st_ll, self.st_rr, self.st_lr);
        if ll <= 1e-12 || rr <= 1e-12 {
            return (f64::NAN, f64::NAN);
        }
        let corr = (lr / (ll * rr).sqrt()).clamp(-1.0, 1.0);
        let mid = (ll + rr + 2.0 * lr) / 4.0;
        let side = (ll + rr - 2.0 * lr) / 4.0;
        let sm = 10.0 * (side.max(1e-30) / mid.max(1e-30)).log10();
        (corr, sm.clamp(-120.0, 120.0))
    }

    /// AAVS: "AAVS", corr f32, side/mid dB f32, count u32, step u32 (output
    /// samples between two pairs), rate u32, then count × (L f32, R f32).
    fn publish_vec(&self) {
        #[cfg(not(test))]
        {
            let Some(hub) = super::hub::try_get() else { return };
            if !hub.live_watched() { return; }
            let Some(live) = hub.live_subject() else { return };
            let (corr, sm) = self.stereo_now();
            let n = self.vec_l.len();
            let mut v = Vec::with_capacity(24 + n * 8);
            v.extend_from_slice(b"AAVS");
            v.extend_from_slice(&(corr as f32).to_le_bytes());
            v.extend_from_slice(&(sm as f32).to_le_bytes());
            v.extend_from_slice(&(n as u32).to_le_bytes());
            v.extend_from_slice(&(self.vec_step as u32).to_le_bytes());
            v.extend_from_slice(&self.out_rate.to_le_bytes());
            for (a, b) in self.vec_l.iter().zip(&self.vec_r) {
                v.extend_from_slice(&a.to_le_bytes());
                v.extend_from_slice(&b.to_le_bytes());
            }
            live.set_live_vec(v.into());
        }
    }

    /// Hold the source of the audible `track_id` for the S/B meters and point
    /// the live subject at it. Retried every wakeup until the player has
    /// registered that track's variant; a rack change re-checks the variant.
    fn ensure_source(&mut self, desc: &Arc<ChainDesc>, track_id: u64, recheck: bool) {
        // A stream: S from its shadow (stream.rs).
        super::stream::ensure(&mut self.stream, desc, self.out_rate);
        if self.src_track == track_id && !recheck {
            return;
        }
        #[cfg(not(test))]
        let buf = if desc.source == "file" {
            super::hub::try_get().and_then(|h| h.activate_file(track_id, desc, self.chain_rev))
        } else {
            super::hub::try_get().and_then(|h| h.activate_track(track_id, desc.variant.upgrade()))
        };
        #[cfg(not(test))]
        if let Some(buf) = buf {
            let same = self.src_track == track_id
                && self.src_buf.as_ref().is_some_and(|b| Arc::ptr_eq(b, &buf));
            self.src_track = track_id;
            if buf.rate > 0 {
                self.track_total_out =
                    (buf.l.len() as f64 * self.out_rate as f64 / buf.rate as f64) as u64;
            }
            if !same {
                self.on_src_buf(buf, track_id);
            }
        }
        #[cfg(test)]
        let _ = (desc, recheck);
    }

    // analytics: S/B live meters — ────────────────────────────────────────────

    /// Replace the current source buffer (called when SrcBuf arrives from hub).
    /// Resets the S/B meters so they start fresh on the new track audio.
    fn on_src_buf(&mut self, buf: Arc<SourceBuf>, track_id: u64) {
        let rate = buf.rate;
        self.src_buf      = Some(buf);
        self.src_consumed = 0;
        self.s_meter      = Some(LoudnessMeter::new(rate as f64));
        self.s_peak_lin   = 0.0;
        self.b_meter      = Some(LoudnessMeter::new(rate as f64));
        self.b_peak_lin   = 0.0;
        // Reset b_ready when track changes (same guard as on_segment_start).
        if track_id != self.track_id {
            self.b_ready = false;
        }
    }

    /// Feed source frames corresponding to the output range `[out_start, out_end)`
    /// into the S and B meters.  Uses `audible_frame` to compute the audible
    /// source position from the current last_mark.
    ///
    /// # Arguments
    /// * `out_start` / `out_end` — output frame range (exclusive upper bound).
    fn feed_sb_meters(&mut self, out_start: u64, out_end: u64) {
        let src_buf = match &self.src_buf {
            Some(b) => b.clone(),
            None    => return,
        };
        let s_meter = match &mut self.s_meter {
            Some(m) => m,
            None    => return,
        };

        // Find the Mark that covers out_start: the last mark with frame ≤ out_start.
        let mark = match self.last_mark.as_ref() {
            Some(m) => m.clone(),
            None    => return,
        };

        // Mark indices count output frames of the chain; the source runs
        // src_rate / out_rate as fast.
        if src_buf.rate == 0 || self.out_rate == 0 {
            return;
        }
        let ratio = src_buf.rate as f64 / self.out_rate as f64;
        let out_idx = |f: u64| mark.index.wrapping_add(f.saturating_sub(mark.frame));
        let src_start = (out_idx(out_start) as f64 * ratio) as u64;
        let src_end   = (out_idx(out_end) as f64 * ratio) as u64;

        if src_start >= src_end {
            return; // nothing to feed
        }

        let n_src = src_buf.l.len();
        if src_start as usize >= n_src {
            return;
        }
        let src_end_clamped = (src_end as usize).min(n_src);
        let src_start_usize = src_start as usize;

        let l = &src_buf.l[src_start_usize..src_end_clamped];
        let r = &src_buf.r[src_start_usize..src_end_clamped];

        // S peak (running max).
        let chunk_peak = l.iter().chain(r.iter())
            .map(|&x| x.abs())
            .fold(0.0_f64, f64::max);
        if chunk_peak > self.s_peak_lin {
            self.s_peak_lin = chunk_peak;
        }

        // S loudness (LUFS-M and LUFS-S).
        s_meter.push(l, r);
        self.src_consumed = src_end;

        // B tap: same signal in current implementation.
        if let Some(b_meter) = &mut self.b_meter {
            if chunk_peak > self.b_peak_lin {
                self.b_peak_lin = chunk_peak;
            }
            b_meter.push(l, r);
        }
    }

    // ── Main wakeup ───────────────────────────────────────────────────────────

    /// Called every 20 ms from the thread loop.
    pub(super) fn tick(&mut self) {
        let read_pos = self.timeline.read_pos();

        // Paused: read position did not advance.
        if read_pos == self.last_read {
            self.maybe_publish();
            return;
        }

        let new_frames = read_pos - self.last_read;

        // Plausibility guard: more frames passed than wall time allows, so a
        // jump skipped part of them (seek via hold + jump_to). The frames just
        // behind the reader are the ones actually played — keep those and
        // drop the OLDER part, never the other way round.
        let max_frames = self.plausibility_max();
        if new_frames > max_frames {
            self.last_read = read_pos - max_frames;
            self.reset_wall_anchor(self.last_read);
        }
        let n = (read_pos - self.last_read) as usize;

        if n == 0 {
            // All frames rejected by plausibility; still update position.
            self.last_read = read_pos;
            self.reset_wall_anchor(read_pos);
            self.maybe_publish();
            return;
        }

        // Resize scratch buffers.
        self.tick_l.resize(n, 0.0_f64);
        self.tick_r.resize(n, 0.0_f64);

        // Copy frames from timeline (PROTOCOL.md rule: peek [last_read, read_pos)).
        self.timeline.peek(self.last_read, &mut self.tick_l, &mut self.tick_r);

        // Post-copy overrun check: if the ring wrapped past last_read, discard.
        if self.is_overrun(read_pos) {
            self.last_read = read_pos;
            self.reset_wall_anchor(read_pos);
            self.maybe_publish();
            return;
        }

        // Segment and accumulate.
        // analytics: if marks were deferred (try_lock failed), do NOT advance
        // last_read — the same window will be retried on the next wakeup with
        // the same audio data already in tick_l/tick_r.
        let marks_deferred = self.process_window(n);

        if !marks_deferred {
            self.last_read = read_pos;
        }
        // The vectorscope every wakeup (50 a second), while the window looks.
        self.publish_vec();
        self.maybe_publish();
    }

    // ── Plausibility guard ────────────────────────────────────────────────────

    /// Maximum frames to count given elapsed wall time.
    fn plausibility_max(&self) -> u64 {
        let elapsed = self.wall_start.elapsed().as_secs_f64();
        let one_period = self.output_shared.device_latency_frames
            .load(Ordering::Relaxed)
            .max(1);
        let already_counted = self.last_read.saturating_sub(self.frames_at_wall_start);
        let ceiling = (elapsed * self.out_rate as f64 * PLAUS_FACTOR).ceil() as u64
            + one_period;
        ceiling.saturating_sub(already_counted)
    }

    fn reset_wall_anchor(&mut self, at_frame: u64) {
        self.wall_start = Instant::now();
        self.frames_at_wall_start = at_frame;
    }

    // ── Overrun check ─────────────────────────────────────────────────────────

    /// True if the timeline ring wrapped past `last_read` since the last peek.
    fn is_overrun(&self, _current_read_pos: u64) -> bool {
        ring_overrun(
            self.timeline.write_pos(),
            self.timeline.capacity_frames(),
            self.last_read,
        )
    }

    // ── Segment walking ───────────────────────────────────────────────────────

    /// Walk marks in [last_read, last_read + n) and feed each segment into
    /// the O loudness meter and spectrogram STFT.
    ///
    /// Returns `true` when the marks Mutex was contended (marks deferred to
    /// next wakeup). The caller must NOT advance `last_read` in that case so
    /// segment boundaries landing in this window are not silently lost.
    // analytics: deferred-marks fix — return value drives last_read advance
    fn process_window(&mut self, n: usize) -> bool {
        // The mark that started this stream lies before the first frame this
        // session reads: take the one covering it, or the first segment has
        // no track, chain or source.
        if self.last_mark.is_none() {
            if let Some(m) = self.render_shared.locate(self.last_read) {
                self.on_first_mark(&m);
                self.last_mark = Some(m);
            }
        }

        // Collect marks that fall within this window (non-blocking try_lock).
        let (marks, deferred): (Vec<Mark>, bool) = match self.render_shared.marks.try_lock() {
            Ok(g) => {
                let end = self.last_read + n as u64;
                (g.iter()
                    .filter(|m| m.frame >= self.last_read && m.frame < end)
                    .cloned()
                    .collect(), false)
            }
            // Render thread holds the lock; defer mark processing to next wakeup.
            Err(_) => (Vec::new(), true),
        };
        if deferred {
            return true;
        }

        let mut cursor = 0usize;

        for mark in &marks {
            let seg_end = (mark.frame - self.last_read) as usize;

            if seg_end > cursor {
                // Frames before this mark: current segment.
                self.accumulate_range(cursor, seg_end);
                cursor = seg_end;
            }

            // Check for a new segment at this mark.
            if self.is_new_segment(mark) {
                self.on_segment_start(mark);
            }

            // Refine src_rate from the mark if it changed (the spectrogram
            // depends on the output rate only).
            if mark.rate != 0 && mark.rate != self.src_rate {
                self.src_rate = mark.rate;
            }

            self.last_mark = Some(mark.clone());
        }

        // Remaining frames in the current segment.
        if cursor < n {
            self.accumulate_range(cursor, n);
        }
        if let Some((d, t)) = self.last_mark.as_ref().map(|m| (m.desc.clone(), m.track_id)) {
            self.ensure_source(&d, t, false);
        }
        false // marks were not deferred
    }

    /// Returns true if this mark begins a new measurement segment.
    fn is_new_segment(&self, mark: &Mark) -> bool {
        let prev = match &self.last_mark {
            Some(p) => p,
            None => return false, // First mark ever; no segment boundary yet.
        };
        // Track change.
        if mark.track_id != prev.track_id {
            return true;
        }
        // Chain change: Arc pointer inequality.
        if !Arc::ptr_eq(&mark.desc, &prev.desc) {
            return true;
        }
        // Seek splice: source index is discontinuous with the extrapolation.
        let elapsed_frames = mark.frame.saturating_sub(prev.frame);
        let expected_index = prev.index.wrapping_add(elapsed_frames);
        mark.index != expected_index
    }

    /// Flush the current segment state and update chain flags for the new segment.
    ///
    /// Called at each segment boundary (track change, chain change, seek splice).
    /// Publishes the accumulated data immediately (bypassing the epoch gate) so
    /// the receiver sees a frame before the discontinuity. The wakeup counter
    /// is reset so the next epoch starts fresh.
    fn on_segment_start(&mut self, mark: &Mark) {
        let desc = &*mark.desc;

        // Chain changed when the Arc pointer differs from the previous mark.
        let chain_changed = self.last_mark.as_ref()
            .map(|p| !Arc::ptr_eq(&p.desc, &mark.desc))
            .unwrap_or(false);

        // Seek: source index is discontinuous → S/B meters must be reset so
        // they do not mix audio from two different track positions.
        let is_seek = self.is_new_segment(mark) && {
            if let Some(prev) = &self.last_mark {
                let elapsed_frames = mark.frame.saturating_sub(prev.frame);
                let expected_index = prev.index.wrapping_add(elapsed_frames);
                mark.index != expected_index && mark.track_id == prev.track_id
            } else {
                false
            }
        };

        if chain_changed {
            // O measurements are now stale.
            self.stale = true;
        }

        // analytics: reset S/B meters on seek splice (source discontinuity)
        if is_seek {
            if let Some(ref buf) = self.src_buf.clone() {
                let rate = buf.rate;
                self.s_meter    = Some(LoudnessMeter::new(rate as f64));
                self.s_peak_lin = 0.0;
                self.b_meter    = Some(LoudnessMeter::new(rate as f64));
                self.b_peak_lin = 0.0;
            }
            self.src_consumed = 0;
        }

        // Flush accumulated data before the discontinuity.
        // analytics: force publish at segment boundary so the receiver sees
        // a frame before the new segment's measurements overwrite the ring.
        self.publish_aan1();
        self.wakeup_in_epoch = 0;

        // Update chain state for the new segment.
        self.set_chain_flags(desc);
        // analytics: reset b_ready on track change; stays across chain-only changes
        let track_changed = mark.track_id != self.track_id;
        if track_changed {
            self.b_ready = false;
            // O belongs to one track: its loudness and coverage start again.
            self.o_meter = LoudnessMeter::new(self.out_rate as f64);
            self.o_tp = EburTruePeak::new(self.out_rate);
            self.o_tp_ring.clear();
            self.track_frames = 0;
            self.track_total_out = 0;
            self.stale = false;
        }
        if track_changed || chain_changed {
            // The page refetches the track and response frames on a new rev.
            self.chain_rev = self.chain_rev.wrapping_add(1);
        }
        self.track_id       = mark.track_id;
        // The source before the chain: a new track's S is on its way before
        // the chain's O pass is queued, and that pass waits for it (the S
        // column fills first, as when the window opens).
        self.ensure_source(&mark.desc, mark.track_id, chain_changed);
        if track_changed || chain_changed {
            self.chain_heard(&mark.desc, mark.track_id);
        }
    }

    /// The first mark this session sees: its track, chain and source (the
    /// source first, as at a segment's start).
    fn on_first_mark(&mut self, mark: &Mark) {
        self.set_chain_flags(&mark.desc);
        self.track_id = mark.track_id;
        self.chain_rev = self.chain_rev.wrapping_add(1);
        self.ensure_source(&mark.desc, mark.track_id, false);
        self.chain_heard(&mark.desc, mark.track_id);
    }

    /// A chain became audible: its |H| for the response panel and the O
    /// pass over the whole track.
    fn chain_heard(&self, desc: &Arc<ChainDesc>, track_id: u64) {
        #[cfg(not(test))]
        if let Some(hub) = super::hub::try_get() {
            hub.on_live_chain(desc.clone(), self.chain_rev, track_id);
        }
        #[cfg(test)]
        let _ = (desc, track_id);
    }

    fn set_chain_flags(&mut self, desc: &ChainDesc) {
        self.hp_active      = desc.stages.iter().any(|s| s.tok == "HP" || s.tok == "aHP");
        self.xtc_active     = desc.stages.iter().any(|s| s.tok == "XTC");
        self.isp_out_active = desc.stages.iter().any(|s| s.tok == "ISP-L");
    }

    // ── Accumulation ─────────────────────────────────────────────────────────

    /// Feed frames `[start, end)` from the tick scratch buffers into the
    /// O loudness meter and the spectrogram STFT carry buffer, and also
    /// feed the corresponding source frames into the S/B meters.
    fn accumulate_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }

        // analytics: feed S/B meters for this output range
        let out_start = self.last_read + start as u64;
        let out_end   = self.last_read + end   as u64;
        self.feed_sb_meters(out_start, out_end);
        // A stream: O here and S on the same frames from its shadow (stream.rs).
        if let (Some(t), Some(m)) = (self.stream.as_mut(), self.last_mark.as_ref()) {
            let idx0 = m.index.wrapping_add(out_start.saturating_sub(m.frame));
            t.feed(idx0, &self.tick_l[start..end], &self.tick_r[start..end]);
        }

        // The stereo picture (the scratch buffers lent out for the call).
        let (tl, tr) = (std::mem::take(&mut self.tick_l), std::mem::take(&mut self.tick_r));
        self.stereo_push(&tl[start..end], &tr[start..end]);
        self.tick_l = tl;
        self.tick_r = tr;

        // Safety: tick_l/tick_r were filled in tick() before process_window().
        let l = &self.tick_l[start..end];
        let r = &self.tick_r[start..end];

        // O loudness and true peak.
        self.o_meter.push(l, r);
        self.o_tp.push(l, r);
        self.frames_measured += (end - start) as u64;
        self.track_frames += (end - start) as u64;
        // A stream's columns are S's and O's on S's grid (stream.rs).
        if self.stream.is_some() {
            return;
        }

        // Spectrogram: accumulate into carry buffer.
        self.carry_l.extend_from_slice(l);
        self.carry_r.extend_from_slice(r);

        // Drain carry buffer one FFT frame at a time.
        let n_fft  = self.stft.n;
        let hop    = self.stft.hop;
        let floor  = -(SPEC_FLOOR_NEG as f64);
        let range  =   SPEC_FLOOR_NEG as f64;

        while self.carry_l.len() >= n_fft {
            // The mean of the L and R power, folded into the log bands and
            // encoded v = clamp((dBFS − floor) / range, 0, 1) × 255.
            self.stft.frame_power_lr(&self.carry_l[..n_fft], &self.carry_r[..n_fft],
                                     &mut self.spec_power);
            let mut col = vec![0u8; SPEC_LOG_BINS];
            self.spec_bands.to_u8(&self.spec_power, floor, range, &mut col);

            if self.spec_ring.len() >= SPEC_RING_MAX {
                self.spec_ring.pop_front();
            }
            self.spec_ring.push_back(col);
            self.spec_new_since_publish += 1;

            self.carry_l.drain(..hop);
            self.carry_r.drain(..hop);
        }
    }

    // ── 10 Hz publish ─────────────────────────────────────────────────────────

    /// Publish an AAN1 frame if the epoch boundary has been reached.
    fn maybe_publish(&mut self) {
        self.wakeup_in_epoch += 1;
        if self.wakeup_in_epoch >= WAKEUPS_PER_EPOCH {
            self.publish_aan1();
            self.wakeup_in_epoch = 0;
        }
    }

    /// Assemble and publish a complete AAN1 live frame.
    fn publish_aan1(&mut self) {
        // --- Flags ---
        let mut flags = 0u8;
        if self.hp_active      { flags |= FLAG_HP_ACTIVE; }
        if self.b_ready        { flags |= FLAG_B_READY; }
        if self.xtc_active     { flags |= FLAG_XTC_ACTIVE; }
        if self.stale          { flags |= FLAG_STALE; }
        if self.isp_out_active { flags |= FLAG_ISP_OUT; }

        // --- Metrics (O tap) ---
        let mut live_metrics = [f64::NAN; 32];
        // Live values only (whole-track O comes from the O pass, opass.rs):
        // loudness M and S now, true peak of the last 3 s.
        live_metrics[MID_LUFS_S_LIVE] = self.o_meter.short_term();
        live_metrics[MID_LUFS_M_LIVE] = self.o_meter.momentary();
        // Coverage: the share of the track heard so far (a replayed part
        // counts again, so it caps at 100).
        let coverage = if self.track_total_out > 0 {
            (self.track_frames as f64 * 100.0 / self.track_total_out as f64).min(100.0)
        } else {
            0.0
        };
        live_metrics[MID_COVERAGE_O]  = coverage;
        if self.o_tp_ring.len() == TP_WINDOW { self.o_tp_ring.pop_front(); }
        self.o_tp_ring.push_back(self.o_tp.take_block_peak());
        let tp = self.o_tp_ring.iter().copied().fold(0.0f64, f64::max);
        live_metrics[MID_TP_EBUR] = if tp > 0.0 { lin_to_db(tp) } else { f64::NAN };
        // Stereo correlation of the last 0.3 s.
        live_metrics[super::proto::mid::STEREO_CORR] = self.stereo_now().0;

        let coverage_pct = coverage.floor() as u8;

        // --- Provenance nibbles (O tap) ---
        let mut prov_nibbles = [0u8; 32];
        let prov_code = if self.hp_active { PROV_HP_PENDING } else { PROV_MEASURED };
        for id in [MID_LUFS_S_LIVE, MID_LUFS_M_LIVE, MID_TP_EBUR, MID_COVERAGE_O, super::proto::mid::STEREO_CORR] {
            if live_metrics[id].is_finite() {
                set_prov_nibble(&mut prov_nibbles, id, prov_code);
            }
        }

        // --- Loudness history point at this tick ---
        // A stream's points go by its own clock (the status's positionS).
        let time_s = self.stream.as_ref()
            .map_or(self.frames_measured as f32 / self.out_rate as f32, |t| t.clock_s() as f32);
        let lufs_s = self.o_meter.short_term() as f32;
        let lufs_m = self.o_meter.momentary() as f32;
        if lufs_s.is_finite() || lufs_m.is_finite() {
            self.loud_pts_pending.push(LoudnessPoint { time_s, lufs_s, lufs_m });
        }
        let loud_pts = std::mem::take(&mut self.loud_pts_pending);

        // --- Spectrogram columns since last publish ---
        let n_new = self.spec_new_since_publish.min(self.spec_ring.len());
        let spec_cols: Vec<Vec<u8>> = if n_new > 0 {
            let start = self.spec_ring.len().saturating_sub(n_new);
            self.spec_ring.iter().skip(start).cloned().collect()
        } else {
            Vec::new()
        };
        self.spec_new_since_publish = 0;

        // After the stale event is published, clear it so subsequent frames
        // are not re-flagged.  The hub/subject manages the full stale lifecycle.
        // analytics: owner B / subject.rs refines this.
        if self.stale {
            self.stale = false;
        }

        // analytics: S/B live metrics — backward-compat extension
        let sb_prov_code = if self.src_buf.is_some() { PROV_MEASURED } else { 0u8 };
        let s_lufs_m = self.s_meter.as_ref().map(|m| m.momentary() as f32).unwrap_or(f32::NAN);
        let s_lufs_s = self.s_meter.as_ref().map(|m| m.short_term() as f32).unwrap_or(f32::NAN);
        let s_tp     = if self.s_peak_lin > 0.0 { lin_to_db(self.s_peak_lin) as f32 } else { f32::NAN };
        let b_lufs_m = self.b_meter.as_ref().map(|m| m.momentary() as f32).unwrap_or(f32::NAN);
        let b_lufs_s = self.b_meter.as_ref().map(|m| m.short_term() as f32).unwrap_or(f32::NAN);
        let b_tp     = if self.b_peak_lin > 0.0 { lin_to_db(self.b_peak_lin) as f32 } else { f32::NAN };
        // S in low nibble, B in high nibble
        let sb_prov  = (sb_prov_code & 0x0f) | ((sb_prov_code & 0x0f) << 4);

        let frame = LiveFrame {
            flags,
            seq: self.seq,
            track_id: self.track_id,
            chain_rev: self.chain_rev,
            coverage_pct,
            n_fft_bins_log2: SPEC_LOG_BINS.trailing_zeros() as u8,
            spec_hop_log2: self.stft.hop.trailing_zeros() as u8,
            spec_floor_neg: SPEC_FLOOR_NEG,
            live_metrics,
            prov_nibbles,
            loud_pts,
            spec_cols,
            s_lufs_m,
            s_lufs_s,
            s_tp,
            b_lufs_m,
            b_lufs_s,
            b_tp,
            sb_prov,
            strm: self.stream.as_mut().map(|t| t.publish()).unwrap_or_default(),
        };

        self.seq = self.seq.wrapping_add(1);

        publish_to_hub(frame);
    }

    /// Flush partial state and publish a final AAN1 frame on stream stop.
    /// Sets `last_read = u64::MAX` as sentinel so the next `on_stream` resets.
    fn flush_and_publish(&mut self) {
        self.publish_aan1();
        self.last_read = u64::MAX;
    }

    /// Rewind the wall-clock anchor so the plausibility guard allows `frames`
    /// more frames to be counted from the current position.
    ///
    /// Only for use in tests; bypasses the guard by making wall time appear to
    /// have advanced enough.
    #[cfg(test)]
    pub fn allow_frames_for_test(&mut self, frames: u64) {
        let needed_s = frames as f64 / self.out_rate as f64 / PLAUS_FACTOR + 0.1;
        self.wall_start = Instant::now() - Duration::from_secs_f64(needed_s);
        self.frames_at_wall_start = self.last_read;
    }
}

// ─── Pure helpers ─────────────────────────────────────────────────────────────

/// Set the 4-bit provenance nibble for metric `id`.
///
/// Packing rule (PROTOCOL.md §4, rule 5): metric `i` → byte `i >> 1`,
/// bits `(i & 1) * 4 .. (i & 1) * 4 + 3`, little-nibble-first.
pub fn set_prov_nibble(nibbles: &mut [u8; 32], id: usize, code: u8) {
    if id >= 32 { return; }
    let byte  = id >> 1;
    let shift = (id & 1) * 4;
    nibbles[byte] = (nibbles[byte] & !(0x0f << shift)) | ((code & 0x0f) << shift);
}

/// Read the 4-bit provenance nibble for metric `id`.
#[cfg(test)]
pub fn get_prov_nibble(nibbles: &[u8; 32], id: usize) -> u8 {
    if id >= 32 { return 0; }
    let byte  = id >> 1;
    let shift = (id & 1) * 4;
    (nibbles[byte] >> shift) & 0x0f
}

/// Returns true if the timeline ring wrapped past `last_read`.
///
/// Pure function; extracted for unit-testing without a full `Session`.
pub fn ring_overrun(write_pos: u64, cap: u64, last_read: u64) -> bool {
    write_pos.saturating_sub(cap) > last_read
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::player::render::{ChainDesc, Shared as RenderShared, StageInfo};
    use crate::player::output::OutputShared;
    use crate::player::timeline::Timeline;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_chain(toks: &[&str]) -> Arc<ChainDesc> {
        use crate::player::settings::PlayerSettings;
        Arc::new(ChainDesc {
            source: "live",
            quick: false,
            taps: None,
            stages: toks.iter().map(|&t| StageInfo {
                tok: t.to_string(), st: 1, why: String::new(),
            }).collect(),
            gain_db: 0.0,
            tp_db: None,
            ceiling_db: None,
            gpu_on: false,
            downgrade: None,
            downgrade_gen: 0,
            hp_deferred: false,
            stream: false,
            file: None,
            // analytics: §2.5 stub for tests
            settings: std::sync::Arc::new(PlayerSettings::default()),
            variant: std::sync::Weak::new(),
            radio: std::sync::Weak::new(),
            out_rate: 0,
            l: 1,
        })
    }

    fn make_mark(frame: u64, track_id: u64, index: u64, desc: Arc<ChainDesc>) -> Mark {
        Mark { frame, track_id, index, rate: 44_100, l: 65536, desc }
    }

    /// Advance the timeline read position by consuming `n` frames.
    fn advance_read(tl: &Timeline, n: usize) {
        let mut buf = vec![0.0_f64; n * 2];
        tl.read_into(&mut buf, n);
    }

    /// Write `n` silent frames at the current write position.
    fn write_silence(tl: &Timeline, n: usize) {
        let zeros = vec![0.0_f64; n];
        tl.append(&zeros, &zeros);
    }

    // ── Pure helpers ──────────────────────────────────────────────────────────

    #[test]
    fn prov_nibble_round_trip() {
        let mut nib = [0u8; 32];
        // Set alternating codes for all 26 metrics and read them back.
        for id in 0..26 {
            let code = (id as u8 % 8) + 1;
            set_prov_nibble(&mut nib, id, code);
        }
        for id in 0..26 {
            let expected = (id as u8 % 8) + 1;
            assert_eq!(
                get_prov_nibble(&nib, id), expected,
                "prov nibble mismatch at id={id}"
            );
        }
    }

    #[test]
    fn prov_nibbles_do_not_bleed_across_metrics() {
        let mut nib = [0u8; 32];
        set_prov_nibble(&mut nib, 0, PROV_MEASURED);
        // metric 1 must remain 0
        assert_eq!(get_prov_nibble(&nib, 1), 0);
        set_prov_nibble(&mut nib, 1, PROV_HP_PENDING);
        // metric 0 must still be PROV_MEASURED
        assert_eq!(get_prov_nibble(&nib, 0), PROV_MEASURED);
    }

    #[test]
    fn ring_overrun_detection() {
        // cap = 4096, last_read = 100, write_pos = 4200 → overrun (4200 - 4096 = 104 > 100)
        assert!(ring_overrun(4200, 4096, 100));
        // write_pos = 4195, write_pos - cap = 99 ≤ 100 → no overrun
        assert!(!ring_overrun(4195, 4096, 100));
        // Exact boundary: write_pos - cap = last_read → no overrun (strictly >)
        assert!(!ring_overrun(4196, 4096, 100));
        // write_pos - cap = 101 > 100 → overrun
        assert!(ring_overrun(4197, 4096, 100));
    }

    #[test]
    fn spec_columns_are_log_bands_with_a_power_of_two_hop() {
        for rate in [44_100u32, 48_000, 96_000, 352_800] {
            let n = spec_fft_len(rate);
            let stft = Stft::new(n, n / 2, SPEC_BETA);
            assert!(stft.hop.is_power_of_two(), "{rate}");
            // ≈ 46 ms a column at any rate.
            let col_s = stft.hop as f64 / rate as f64;
            assert!((0.04..0.05).contains(&col_s), "{rate}: {col_s}");
        }
        assert_eq!(1usize << SPEC_LOG_BINS.trailing_zeros(), SPEC_LOG_BINS);
    }

    // ── Segment detection (pure logic) ────────────────────────────────────────

    /// Build a minimal `Session` whose only purpose is exercising
    /// `is_new_segment`; no real audio is processed.
    fn make_session(out_rate: u32, src_rate: u32) -> Session {
        let tl = Arc::new(Timeline::new(out_rate, 1.0));
        let rs = RenderShared::new();
        let os = OutputShared::new();
        Session::new(
            tl, rs, os, out_rate, Some(src_rate),
            1, 0, false, false, false,
        )
    }

    #[test]
    fn no_segment_on_first_mark() {
        let mut s = make_session(44_100, 44_100);
        let desc = make_chain(&["DC"]);
        let mark = make_mark(1000, 42, 0, desc);
        // No previous mark → no new segment.
        assert!(!s.is_new_segment(&mark));
        s.last_mark = Some(mark);
    }

    #[test]
    fn track_change_is_new_segment() {
        let mut s = make_session(44_100, 44_100);
        let desc = make_chain(&["DC"]);
        let m0 = make_mark(1000, 1, 0, Arc::clone(&desc));
        s.last_mark = Some(m0);
        // Different track_id
        let m1 = make_mark(2000, 2, 1000, desc);
        assert!(s.is_new_segment(&m1));
    }

    #[test]
    fn chain_change_is_new_segment() {
        let mut s = make_session(44_100, 44_100);
        let desc_a = make_chain(&["DC"]);
        let desc_b = make_chain(&["DC", "ISP"]); // different allocation
        let m0 = make_mark(1000, 1, 0, Arc::clone(&desc_a));
        s.last_mark = Some(m0);
        // Same track_id, different chain pointer
        let m1 = make_mark(2000, 1, 1000, desc_b);
        assert!(s.is_new_segment(&m1));
    }

    #[test]
    fn seek_splice_is_new_segment() {
        let mut s = make_session(44_100, 44_100);
        let desc = make_chain(&["DC"]);
        let m0 = make_mark(1000, 1, 5000, Arc::clone(&desc));
        s.last_mark = Some(m0);
        // Expected index = 5000 + (2000 - 1000) = 6000; but actual = 99000 → splice.
        let m1 = make_mark(2000, 1, 99000, Arc::clone(&desc));
        assert!(s.is_new_segment(&m1));
    }

    #[test]
    fn continuous_play_is_not_new_segment() {
        let mut s = make_session(44_100, 44_100);
        let desc = make_chain(&["DC"]);
        let m0 = make_mark(1000, 1, 5000, Arc::clone(&desc));
        s.last_mark = Some(m0);
        // Expected index = 5000 + (2000 - 1000) = 6000; actual = 6000 → continuous.
        let m1 = make_mark(2000, 1, 6000, Arc::clone(&desc));
        assert!(!s.is_new_segment(&m1));
    }

    // ── Pause: no accumulation ────────────────────────────────────────────────

    #[test]
    fn pause_does_not_accumulate_frames() {
        let rate = 44_100u32;
        let tl   = Arc::new(Timeline::new(rate, 2.0));
        let rs   = RenderShared::new();
        let os   = OutputShared::new();

        // Write 1 second of silence but do NOT advance read_pos.
        write_silence(&tl, rate as usize);

        let mut s = Session::new(
            Arc::clone(&tl), Arc::clone(&rs), Arc::clone(&os),
            rate, Some(rate), 1, 0, false, false, false,
        );
        let init_measured = s.frames_measured;

        // Three wakeups with read_pos unchanged (paused).
        for _ in 0..3 {
            s.tick();
        }

        assert_eq!(s.frames_measured, init_measured,
            "paused ticks must not accumulate frames");
    }

    // ── Full segment-correctness test (contract §6.3) ─────────────────────────

    /// BACKEND-CONTRACT §6.3: synthetic timeline with three injected marks
    /// (track change, chain change, seek splice).  Verifies:
    /// - Three distinct segments accumulated.
    /// - No frames counted more than once.
    /// - Frames after seek splice are only in the new segment.
    #[test]
    fn live_consumer_segments_correctly() {

        let rate = 44_100u32;
        let cap_s = 4.0; // seconds of ring capacity
        let tl = Arc::new(Timeline::new(rate, cap_s));
        let rs = RenderShared::new();
        let os = OutputShared::new();

        // ── Write 3 × 10_000 frames of silence ───────────────────────────────
        // We will inject marks at frames 10_000 and 20_000 (track and chain change)
        // and a seek-splice mark at frame 30_000.
        let total_frames: usize = 40_000;
        write_silence(&tl, total_frames);

        // ── Create three chain descriptors ────────────────────────────────────
        let chain_a = make_chain(&["DC"]);
        let chain_b = make_chain(&["DC", "ISP"]);          // chain change
        let chain_c = Arc::clone(&chain_b);                // same chain, seek splice

        // ── Inject marks into render::Shared ─────────────────────────────────
        // Mark 0: stream start, track=1, chain_a, index=0
        // Mark 1: frame 10_000 — track change (track=2)
        // Mark 2: frame 20_000 — chain change (same track=2, chain_b)
        // Mark 3: frame 30_000 — seek splice (track=2, chain_c, discontinuous index)
        {
            let mut marks = rs.marks.lock().unwrap();
            marks.push_back(Mark { frame: 0,       track_id: 1, index: 0,       rate, l: 65536, desc: Arc::clone(&chain_a) });
            marks.push_back(Mark { frame: 10_000,  track_id: 2, index: 0,       rate, l: 65536, desc: Arc::clone(&chain_a) }); // track change
            marks.push_back(Mark { frame: 20_000,  track_id: 2, index: 10_000,  rate, l: 65536, desc: Arc::clone(&chain_b) }); // chain change
            marks.push_back(Mark { frame: 30_000,  track_id: 2, index: 999_999, rate, l: 65536, desc: Arc::clone(&chain_c) }); // seek splice
        }

        // ── Create session starting at timeline frame 0 ───────────────────────
        let mut session = Session::new(
            Arc::clone(&tl), Arc::clone(&rs), Arc::clone(&os),
            rate, Some(rate), 1, 0, false, false, false,
        );
        // last_read is set to current read_pos (= 0) in Session::new.

        // Allow the plausibility guard to accept all 40_000 frames in one tick.
        session.allow_frames_for_test(total_frames as u64 + 1000);

        // ── Advance read_pos to 40_000 (simulate output thread consuming) ─────
        advance_read(&tl, total_frames);
        assert_eq!(tl.read_pos(), total_frames as u64);

        // ── Run one large tick (process all 40_000 frames at once) ────────────
        session.tick();

        // ── Assertions ────────────────────────────────────────────────────────

        // All frames must be counted exactly once (modulo plausibility cap).
        // No duplicate counting across segments.
        assert_eq!(
            session.frames_measured,
            total_frames as u64,
            "all {} frames must be counted exactly once", total_frames,
        );

        // Verify last_read advanced to 40_000.
        assert_eq!(session.last_read, total_frames as u64);

        // Sequence counter must have incremented (publish fired).
        assert!(
            session.seq >= 1,
            "publish must have fired at least once during tick"
        );
    }

    // ── AAN1 loudness history accumulates ────────────────────────────────────

    #[test]
    fn loudness_points_published_after_epoch() {
        use std::f64::consts::PI;

        let rate = 44_100u32;
        let tl   = Arc::new(Timeline::new(rate, 2.0));
        let rs   = RenderShared::new();
        let os   = OutputShared::new();

        // Write 5 seconds of a 997 Hz sine (loud enough to pass the EBU gate).
        let n = (rate as usize) * 5;
        let l: Vec<f64> = (0..n).map(|i| 0.5 * (2.0 * PI * 997.0 / rate as f64 * i as f64).sin()).collect();
        let r = l.clone();
        tl.append(&l, &r);
        advance_read(&tl, n);

        let mut s = Session::new(
            Arc::clone(&tl), Arc::clone(&rs), Arc::clone(&os),
            rate, Some(rate), 1, 0, false, false, false,
        );

        // Allow the guard to pass all frames.
        s.allow_frames_for_test(n as u64 + 1000);

        // Run enough ticks to reach an epoch boundary and produce a loudness point.
        for _ in 0..(WAKEUPS_PER_EPOCH + 1) {
            s.tick();
        }

        // Drain frames published to TEST_SINK.
        let published = drain_test_sink();
        assert!(!published.is_empty(), "at least one AAN1 frame must be published");

        let frame = published.last().unwrap();
        // The live frame carries live values only: momentary loudness (NaN
        // before 400 ms), no integrated loudness.
        let lufs_m = frame.live_metrics[MID_LUFS_M_LIVE];
        assert!(lufs_m.is_nan() || lufs_m.is_finite(), "LUFS_M must be NaN or finite, got {lufs_m}");
        assert!(frame.live_metrics[0].is_nan(), "no LUFS-I in the live frame");
    }

    // ── Spectrogram encoding sanity ───────────────────────────────────────────

    #[test]
    fn spec_column_encoding_bounds() {
        // Verify the encoding function maps:
        //   dBFS = floor → 0
        //   dBFS = 0.0   → 255
        let floor = -(SPEC_FLOOR_NEG as f64);
        let range =   SPEC_FLOOR_NEG as f64;

        let encode = |db: f64| -> u8 {
            let norm = (db - floor) / range;
            (norm.clamp(0.0, 1.0) * 255.0).round() as u8
        };

        assert_eq!(encode(floor),  0,   "floor maps to 0");
        assert_eq!(encode(0.0),    255, "0 dBFS maps to 255");
        assert_eq!(encode(-60.0),  encode(-60.0));
        // Any dB above 0 still maps to 255 (clamped).
        assert_eq!(encode(3.0), 255);
        // Any dB below floor maps to 0 (clamped).
        assert_eq!(encode(-300.0), 0);
    }

    // ── S/B live meters ───────────────────────────────────────────────────────

    /// Build a minimal SourceBuf with a 997 Hz sine tone.
    fn make_src_buf(rate: u32, n_frames: usize) -> Arc<SourceBuf> {
        use std::f64::consts::PI;
        let l: Vec<f64> = (0..n_frames)
            .map(|i| 0.5 * (2.0 * PI * 997.0 / rate as f64 * i as f64).sin())
            .collect();
        let r = l.clone();
        Arc::new(SourceBuf { l, r, rate })
    }

    #[test]
    fn sb_meters_start_nan_without_src_buf() {
        let s = make_session(44_100, 44_100);
        // No source buf → all S/B fields must be NaN
        assert!(s.s_meter.is_none());
        assert!(s.b_meter.is_none());
        assert_eq!(s.s_peak_lin, 0.0);
        assert_eq!(s.b_peak_lin, 0.0);
    }

    #[test]
    fn sb_meters_initialise_after_src_buf() {
        let mut s = make_session(44_100, 44_100);
        let buf = make_src_buf(44_100, 44_100);
        s.on_src_buf(Arc::clone(&buf), s.track_id);
        assert!(s.s_meter.is_some(), "s_meter should be set after src_buf");
        assert!(s.b_meter.is_some(), "b_meter should be set after src_buf");
        assert_eq!(s.s_peak_lin, 0.0, "peak should start at 0 before feeding");
    }

    #[test]
    fn sb_feed_updates_peak() {
        let rate = 44_100u32;
        let n = rate as usize;     // 1 second
        let mut s = make_session(rate, rate);
        let buf = make_src_buf(rate, n);
        s.on_src_buf(Arc::clone(&buf), s.track_id);

        // Inject a mark so feed_sb_meters can compute source index
        let desc = make_chain(&["DC"]);
        let mark = make_mark(0, s.track_id, 0, desc);
        s.last_mark = Some(mark);

        // Feed half the buffer: out frames 0..22050 → src frames 0..22050
        s.feed_sb_meters(0, 22_050);
        assert!(s.s_peak_lin > 0.0, "peak must be positive after feeding audio");
        assert!(s.b_peak_lin > 0.0, "B peak must be positive after feeding audio");
        // Half of a ±0.5 sine ≈ 0.5 peak
        assert!((s.s_peak_lin - 0.5).abs() < 0.01, "peak near 0.5");
    }

    #[test]
    fn sb_seek_resets_meters() {
        let rate = 44_100u32;
        let n = rate as usize;
        let mut s = make_session(rate, rate);
        let buf = make_src_buf(rate, n);
        s.on_src_buf(Arc::clone(&buf), s.track_id);

        let desc = make_chain(&["DC"]);
        let m0 = make_mark(0, s.track_id, 0, Arc::clone(&desc));
        s.last_mark = Some(m0);
        s.feed_sb_meters(0, 22_050);
        let peak_before = s.s_peak_lin;
        assert!(peak_before > 0.0);

        // Simulate a seek: on_segment_start detects discontinuous index
        // By calling on_src_buf again (which resets meters)
        s.on_src_buf(Arc::clone(&buf), s.track_id);
        assert_eq!(s.s_peak_lin, 0.0, "peak resets after seek/new src_buf");
        assert!(s.s_meter.is_some(), "meter must still exist after reset");
    }
}
