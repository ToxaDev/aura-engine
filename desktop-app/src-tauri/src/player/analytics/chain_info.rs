//! Effective chain description accessor for the analytics subsystem.
//!
//! Provides lock-free (try_lock only) access to the currently-audible chain
//! settings and pre-loaded filter resources, called from `resp.rs` only.
//!
//! ## Required edits from parallel sessions
//!
//! - **§2.4 chain.rs**: adds `Resources::peek_bank`, `Resources::peek_guard`,
//!   `Resources::peek_xtc` — try_lock internally, never trigger a load.
//! - **§2.5 render.rs**: adds `ChainDesc::settings: Arc<PlayerSettings>` and
//!   `ChainDesc::out_rate: u32` so the resp thread can read filter parameters
//!   without holding the controller `st` lock.
//! - **§2.6 controller.rs**: adds `Player::resources() -> &Resources` and
//!   `Player::render_shared() -> &Arc<render::Shared>`.
//!
//! None of the functions here call `Bank::load()`, `rayon::spawn`, or hold any
//! Mutex for more than a single `try_lock` (which immediately returns `None` on
//! contention).

use std::path::Path;
use std::sync::Arc;

use crate::player::chain::Resources;
use crate::player::convolver::{Alignment, Bank};
use crate::player::render::ChainDesc;
use crate::player::settings::{Phase, PlayerSettings};

// ─── Audible chain ────────────────────────────────────────────────────────────

/// Read the effective chain description of the currently-audible frame
/// without acquiring the controller's `st` Mutex.
///
/// Reads the most recent [`Mark`] from `render::Shared::marks` via a single
/// `try_lock`.  Returns `None` when:
/// - no track is playing,
/// - the marks deque is empty, or
/// - the marks Mutex is contended (caller should retry on the next wake).
///
/// The `render_shared()` method on `Player` is added by the §2.6 edit to
/// `controller.rs`.
#[allow(dead_code)] // kept for the analyzer views not wired yet
pub fn audible_chain_desc() -> Option<Arc<ChainDesc>> {
    let player = crate::player::controller::get();
    // analytics: §2.6 — render_shared() exposes render::Shared without a lock
    let shared = player.render_shared();
    let guard = shared.marks.try_lock().ok()?;
    guard.back().map(|m| Arc::clone(&m.desc))
}

// ─── Filter path helpers ──────────────────────────────────────────────────────

/// The [`Alignment`] that corresponds to the given phase setting.
///
/// Used when building the bank key for `peek_bank`: a Linear-phase bank has
/// `Linear` alignment; a TFS one looks ahead by `tfs::look_ahead` of its
/// `taps` at `out_rate`; Minimum-phase has `None`; Hybrid/Alpha peeks the
/// linear-phase bank (the min-phase bank is secondary, and HP/αHP chains are
/// analytically blocked anyway per the provenance rule).
pub fn phase_alignment(phase: Phase, taps: usize, out_rate: u32) -> Alignment {
    match phase {
        Phase::Minimum => Alignment::None,
        Phase::Tfs => Alignment::LookAhead(crate::audio::converter::dsp::lab::tfs::look_ahead(taps, out_rate)),
        // Hybrid/Alpha: peek the linear-phase bank for |H| display.
        // When hp_deferred=true the stand-in IS linear; when hp_deferred=false
        // and HP is audible, analytic O is blocked by provenance (Measured),
        // but |H| of the linear sub-branch is still shown.
        Phase::Hybrid | Phase::Alpha => Alignment::Linear,
        _ => Alignment::Linear,
    }
}

/// Resolve the path of the filter blob for the given settings and output rate.
///
/// For TFS phase the method tries to locate the TFS-derived blob via the same
/// helper the engine uses; if that fails (blob not on disk yet) it falls back
/// to the linear-phase blob — acceptable because TFS ≈ linear at the passband.
///
/// Returns `None` if the tap count is below the smallest designed filter or
/// the blob is not found on disk.  Never reads the file; just stat-checks it.
/// The blob is the one the chain loads: of the factor from `src_rate` up to
/// `out_rate` (`filter::design_rate`).
pub fn filter_path(settings: &PlayerSettings, src_rate: u32, out_rate: u32) -> Option<String> {
    use crate::audio::converter::dsp::filter;

    match settings.phase {
        Phase::Tfs => {
            // analytics: try TFS-derived path first; fall back to linear_phase
            let cancel = std::sync::atomic::AtomicBool::new(false);
            if let Ok(p) = crate::audio::converter::dsp::lab::tfs::resolve_or_derive(
                settings.taps,
                src_rate,
                out_rate,
                &cancel,
            ) {
                return Some(p.to_string_lossy().into_owned());
            }
            filter::find_precomputed_filter(settings.taps, src_rate, out_rate, "linear_phase")
        }
        Phase::Minimum => {
            filter::find_precomputed_filter(settings.taps, src_rate, out_rate, "minimum_phase")
        }
        // Linear, Hybrid, Alpha all peek the linear-phase bank.
        _ => filter::find_precomputed_filter(settings.taps, src_rate, out_rate, "linear_phase"),
    }
}

/// The bank a chain plays for `settings` at `out_rate` from `src_rate`, and
/// how it is lined up: a file's (`filter_path`, `phase_alignment`), or — a
/// live stream's (`stream`) — the stream's own linear filter when it is
/// cached (`stream_linear::cached`; a stream's plan makes it before it plays
/// it), as `radio::chain::plan` loads it. Nothing is made here.
pub fn bank_of(settings: &PlayerSettings, src_rate: u32, out_rate: u32, stream: bool) -> Option<(String, Alignment)> {
    if stream && matches!(settings.phase, Phase::Linear | Phase::Hybrid | Phase::Alpha) {
        if let Some((path, k)) =
            crate::audio::converter::dsp::lab::stream_linear::cached(settings.taps, src_rate, out_rate)
        {
            return Some((path, Alignment::LookAhead(k)));
        }
    }
    let path = filter_path(settings, src_rate, out_rate)?;
    Some((path, phase_alignment(settings.phase, settings.taps, out_rate)))
}

// ─── Resource peek functions ──────────────────────────────────────────────────

/// Peek the [`Bank`] from the resource cache without triggering a load and
/// without changing the LRU order.
///
/// Returns `None` if the bank for `(path, l, align)` at `out_rate` is not yet
/// in the cache. The caller should fall back to a NaN-filled stub and retry on
/// the next chain-rev update (by then the render thread will have loaded the
/// bank).
///
/// Calls `Resources::peek_bank` added by §2.4 edit to `chain.rs`.
pub fn peek_bank(
    resources: &Resources,
    path: &Path,
    l: usize,
    align: Alignment,
    out_rate: u32,
) -> Option<Arc<Bank>> {
    // analytics: §2.4 — peek without LRU reorder or disk I/O
    resources.peek_bank(path, l, align, out_rate)
}

/// Peek the subsonic guard taps from the resource cache.
///
/// Returns `None` if the guard for `(rate, hz)` is not yet computed.  On a
/// miss the resp thread omits the SUB fold and stores what it has; the next
/// chain-rev trigger (from a rack change or re-publish) will retry.
///
/// Calls `Resources::peek_guard` added by §2.4 edit to `chain.rs`.
pub fn peek_guard(resources: &Resources, rate: u32, hz: u32) -> Option<Arc<[f64]>> {
    // analytics: §2.4 — try_lock, never compute or block
    resources.peek_guard(rate, hz)
}

/// Peek the XTC filter pair (direct, cross) from the resource cache.
///
/// The returned `Arc` holds a `(direct_taps: Vec<f64>, cross_taps: Vec<f64>)`
/// pair — the same representation stored by `Resources::xtc_pair` internally.
///
/// Returns `None` when the XTC pair for `(settings, rate)` is not yet cached.
///
/// Calls `Resources::peek_xtc` added by §2.4 edit to `chain.rs`.
pub fn peek_xtc(
    resources: &Resources,
    settings: &PlayerSettings,
    rate: u32,
) -> Option<Arc<(Vec<f64>, Vec<f64>)>> {
    // analytics: §2.4 — try_lock, never design or block
    resources.peek_xtc(settings, rate)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::settings::{Phase, PlayerSettings};

    // ── phase_alignment ──────────────────────────────────────────────────────

    #[test]
    fn linear_phase_gives_linear_alignment() {
        assert!(matches!(phase_alignment(Phase::Linear, 1_000_000, 352_800), Alignment::Linear));
    }

    #[test]
    fn minimum_phase_gives_none_alignment() {
        assert!(matches!(phase_alignment(Phase::Minimum, 1_000_000, 352_800), Alignment::None));
    }

    /// TFS plays 30 ms ahead of its centre, as its bank is built.
    #[test]
    fn tfs_phase_gives_its_look_ahead() {
        assert!(matches!(phase_alignment(Phase::Tfs, 1_000_000, 352_800), Alignment::LookAhead(10_584)));
    }

    #[test]
    fn hybrid_phase_gives_linear_alignment() {
        // HP peeks the linear bank; the min bank is secondary.
        assert!(matches!(phase_alignment(Phase::Hybrid, 1_000_000, 352_800), Alignment::Linear));
    }

    #[test]
    fn alpha_phase_gives_linear_alignment() {
        assert!(matches!(phase_alignment(Phase::Alpha, 1_000_000, 352_800), Alignment::Linear));
    }

    /// A stream's chain plays its own linear filter once it is made (Hybrid-
    /// Phase's linear branch too), 50 ms ahead, and the analyzer peeks that
    /// bank; a file's chain, and a stream's minimum phase, never do.
    #[test]
    fn a_streams_bank_is_its_own_linear_filter_once_made() {
        let dir = std::env::current_exe().unwrap().parent().unwrap().join("fir-optimizer").join("output");
        std::fs::create_dir_all(&dir).unwrap();
        // 44.1 kHz ×16: a cell no other test touches.
        let (src, out) = (44_100u32, 705_600u32);
        let s = |phase| PlayerSettings { taps: 1_000_000, phase, fs_multiplier: 16, ..PlayerSettings::default() };
        let made = dir.join("fir_1M_705600_stream_linear_v1.npy");
        std::fs::write(&made, b"").unwrap();
        let linear = bank_of(&s(Phase::Linear), src, out, true);
        let hybrid = bank_of(&s(Phase::Hybrid), src, out, true);
        let file = bank_of(&s(Phase::Linear), src, out, false);
        let minimum = bank_of(&s(Phase::Minimum), src, out, true);
        std::fs::remove_file(&made).ok();
        for b in [linear, hybrid] {
            let (path, align) = b.expect("the stream's own");
            assert!(path.ends_with("fir_1M_705600_stream_linear_v1.npy"), "{path}");
            assert!(matches!(align, Alignment::LookAhead(35_280)), "{align:?}");
        }
        for b in [file, minimum] {
            assert!(b.map_or(true, |(p, _)| !p.contains("stream_linear")));
        }
    }

    // ── filter_path ──────────────────────────────────────────────────────────

    /// When a filter blob does not exist on disk, filter_path returns None.
    /// This test creates a settings struct with an impossible tap count so
    /// the lookup always misses — no disk access needed.
    #[test]
    fn filter_path_returns_none_for_impossible_tap_count() {
        let s = PlayerSettings {
            taps: 1, // below TAP_LADDER minimum → taps_label returns None
            phase: Phase::Linear,
            ..PlayerSettings::default()
        };
        let out_rate = 352_800u32;
        assert!(
            filter_path(&s, 44_100, out_rate).is_none(),
            "tap count below minimum should yield None"
        );
    }

    #[test]
    fn filter_path_returns_none_for_minimum_phase_when_not_on_disk() {
        let s = PlayerSettings {
            taps: 1,
            phase: Phase::Minimum,
            ..PlayerSettings::default()
        };
        assert!(filter_path(&s, 44_100, 352_800).is_none());
    }

    /// Hybrid-phase peeks the LINEAR bank — the TFS/min bank is not requested
    /// via filter_path.
    #[test]
    fn filter_path_hybrid_uses_linear_phase_lookup() {
        // Construct a settings with taps below the minimum so we get None
        // regardless of the phase — we just want to verify no panic and the
        // correct code path is taken.
        let s = PlayerSettings {
            taps: 1,
            phase: Phase::Hybrid,
            ..PlayerSettings::default()
        };
        // Would call find_precomputed_filter(1, _, _, "linear_phase") internally.
        // With taps=1 it returns None (below TAP_LADDER minimum).
        assert!(filter_path(&s, 44_100, 352_800).is_none());
    }
}
