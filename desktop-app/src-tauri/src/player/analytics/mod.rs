//! Live analytics: measurements of the source, the source after its stages,
//! and the output the listener actually hears.
//!
//! Every number carries its provenance (analytic, forecast, measured,
//! unchanged). The algorithms here are pure: planar f64 in, numbers out, at
//! any sample rate the player produces — nothing in this module touches the
//! render thread, the output thread or the engine.

pub mod kweight;
pub mod loudness;
pub mod lra;
pub mod peaks;
pub mod dr;
pub mod stats;
pub mod spectra;
pub mod response;
pub mod provenance;
// analytics: chain description accessor and filter peek helpers (owner C)
pub mod chain_info;
// analytics: chain frequency response and analytic O spectrum (owner C)
pub mod resp;

// analytics: live consumer thread — owner D
pub mod live;

// The live analyzer on a stream: S from its shadow beside O
pub mod stream;
// A stream's session, songs and bandwidth for the analyzer
pub mod stream_totals;

// analytics: wire frame encoders (AAN1 / AAN2 / AAN3 / AAWT / AAST)
pub mod proto;

// analytics: subject state machine — owner A
pub mod subject;

// analytics: global hub + subject registry — owner A
pub mod hub;

// analytics: whole-track analysis thread — owner A
pub mod track;

// The O pass: the whole track through the chain heard (on the track thread)
pub mod opass;

// analytics: non-blocking HTTP route dispatch — owner A
pub mod routes;

// The zoomed spectrogram: finer tiles for the stretch the page shows
pub mod zoom;

#[cfg(test)]
pub mod refdump;

// ── audible_frame — the ONE place this is computed ────────────────────────────

#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
use crate::player::timeline::Timeline;
#[cfg(test)]
use crate::player::output::OutputShared;

/// The output timeline frame that is currently audible by the listener.
///
/// Formula: `read_pos() − device_latency_frames` (saturating).
/// This is the **sole authoritative location** for the audible-frame
/// computation; every module that needs it (live.rs playhead, live S/B
/// meters, overlay cursors) must call this function — never copy the formula.
///
/// # Parameters
/// * `timeline`  — the shared output timeline.
/// * `out`       — the shared output state; `device_latency_frames` is written
///                 once at device-open time and is safe to read with Relaxed.
///
/// # Returns
/// The absolute output-timeline frame that the listener is hearing right now,
/// or 0 when `read_pos < device_latency_frames`.
// analytics: audible_frame — single source of truth (COORDINATION.md §"Слышимый кадр")
#[cfg(test)]
pub fn audible_frame(timeline: &Timeline, out: &OutputShared) -> u64 {
    let read_pos = timeline.read_pos();
    let latency  = out.device_latency_frames.load(Ordering::Relaxed);
    read_pos.saturating_sub(latency)
}

#[cfg(test)]
mod tests_audible_frame {
    use super::*;
    use std::sync::{atomic::Ordering, Arc};
    use crate::player::timeline::Timeline;
    use crate::player::output::OutputShared;

    /// Write n silent frames to the timeline.
    fn write_n(tl: &Timeline, n: usize) {
        let zeros = vec![0.0_f64; n];
        tl.append(&zeros, &zeros);
    }

    /// Advance timeline read_pos by consuming n frames.
    fn read_n(tl: &Timeline, n: usize) {
        let mut buf = vec![0.0_f64; n * 2];
        tl.read_into(&mut buf, n);
    }

    #[test]
    fn audible_frame_zero_latency() {
        let tl = Arc::new(Timeline::new(44_100, 2.0));
        let os = OutputShared::new();
        os.device_latency_frames.store(0, Ordering::Relaxed);
        write_n(&tl, 1000);
        read_n(&tl, 500);
        assert_eq!(audible_frame(&tl, &os), 500);
    }

    #[test]
    fn audible_frame_subtracts_latency() {
        let tl = Arc::new(Timeline::new(44_100, 2.0));
        let os = OutputShared::new();
        os.device_latency_frames.store(100, Ordering::Relaxed);
        write_n(&tl, 1000);
        read_n(&tl, 500);
        // read_pos = 500, latency = 100 → audible = 400
        assert_eq!(audible_frame(&tl, &os), 400);
    }

    #[test]
    fn audible_frame_saturates_at_zero() {
        let tl = Arc::new(Timeline::new(44_100, 2.0));
        let os = OutputShared::new();
        // latency > read_pos → saturates to 0
        os.device_latency_frames.store(9999, Ordering::Relaxed);
        write_n(&tl, 200);
        read_n(&tl, 50);
        assert_eq!(audible_frame(&tl, &os), 0);
    }
}
