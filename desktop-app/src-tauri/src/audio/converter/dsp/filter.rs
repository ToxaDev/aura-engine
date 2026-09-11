//! Resolves the path of a pre-computed FIR filter blob from
//! `fir-optimizer/output/`.
//!
//! Naming convention (set by `fir-optimizer/optimize.py --all-ratios`):
//!
//!     fir_<TAG>_<TARGET_HZ>_<phase>.npy
//!
//! where
//!     TAG       ∈ {"1M", "5M", "10M", "30M"}
//!     TARGET_HZ = output sample rate in Hz, e.g. 88200, 352800
//!     phase     ∈ {"linear_phase", "minimum_phase"}
//!
//! Examples:
//!   fir_30M_352800_linear_phase.npy   (44.1 kHz × 8)
//!   fir_1M_88200_minimum_phase.npy    (44.1 kHz × 2)
//!   fir_5M_768000_linear_phase.npy    (48 kHz × 16)
//!
//! Backward-compat fallback: an older single-rate naming is kept for
//! systems that still have the legacy blobs but only at the FS8 design
//! point (44.1 → 352.8 kHz or 48 → 384 kHz). Any other ratio MUST have
//! the ratio-specific file — otherwise the runtime would silently apply
//! the wrong cutoff (the bug fixed in commit `ca1af01`).
//!
//!     fir_<TAG>_<phase>.npy           (legacy, FS8 only)

/// Compact tap-count label as it appears in blob filenames, or `None` when
/// the requested count is below the smallest designed filter.
pub(crate) fn taps_label(taps: usize) -> Option<&'static str> {
    Some(match taps {
        t if t >= 25_000_000 => "30M",
        t if t >= 7_500_000 => "10M",
        t if t >= 2_500_000 => "5M",
        t if t >= 500_000 => "1M",
        _ => return None,
    })
}

/// Directories searched for filter blobs, in priority order.
fn search_dirs() -> [Option<std::path::PathBuf>; 6] {
    [
        // Explicit override for blobs stored outside the repo layout.
        std::env::var_os("AURA_FILTER_DIR").map(std::path::PathBuf::from),
        // Portable layout: the filter folder sits next to the exe.
        //   <pkg>/aura-engine.exe
        //   <pkg>/fir-optimizer/output/*.npy
        // Resolved from the exe rather than the working directory, so a
        // shortcut or a launch from another folder still finds the blobs.
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("fir-optimizer").join("output"))),
        // Portable layout, short form: <pkg>/filters/*.npy
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("filters"))),
        // Repo root relative to the exe:
        // <root>/desktop-app/src-tauri/target/release/aura-engine.exe
        //   → ../../../../fir-optimizer/output
        std::env::current_exe().ok().and_then(|p| {
            p.parent().map(|d| {
                d.join("..")
                    .join("..")
                    .join("..")
                    .join("..")
                    .join("fir-optimizer")
                    .join("output")
            })
        }),
        // One level shallower, for layouts where fir-optimizer sits next
        // to src-tauri instead of the repo root.
        std::env::current_exe().ok().and_then(|p| {
            p.parent().map(|d| {
                d.join("..")
                    .join("..")
                    .join("..")
                    .join("fir-optimizer")
                    .join("output")
            })
        }),
        // Current working directory (running from a repo-root shell).
        Some(std::path::Path::new("fir-optimizer").join("output")),
    ]
}

/// Name of the blob a given combination needs, or `None` below the smallest
/// designed filter.
pub fn blob_name(taps: usize, target_rate_hz: u32, phase_type: &str) -> Option<String> {
    Some(format!(
        "fir_{}_{}_{}.npy",
        taps_label(taps)?,
        target_rate_hz,
        phase_type
    ))
}

/// The release asset that contains a given blob, and a direct download link.
///
/// Filters ship as per-tap-count packs rather than as individual files, so a
/// user who is missing one needs the pack name, not just the filename.
pub fn filter_pack_for(taps: usize, target_rate_hz: u32) -> Option<(String, String)> {
    let label = taps_label(taps)?;
    let pack = if label == "30M" {
        // The 30M blobs are split by rate family — one pack for all of them
        // would be about 4 GB.
        if target_rate_hz % 44_100 == 0 {
            "aura-filters-30M-44k-family.zip".to_string()
        } else {
            "aura-filters-30M-48k-family.zip".to_string()
        }
    } else {
        format!("aura-filters-{}-all-rates.zip", label)
    };
    // Pinned to the release that actually carries the filter packs rather than
    // to `latest`: a later release without them attached would turn every one
    // of these links into a 404, including inside builds already in the wild.
    // Update this when the packs are re-uploaded to a newer release.
    const PACK_RELEASE_TAG: &str = "v1.0.0";
    let url = format!(
        "https://github.com/ToxaDev/aura-engine/releases/download/{}/{}",
        PACK_RELEASE_TAG, pack
    );
    Some((pack, url))
}

/// Where the portable layout expects filters to be dropped: the folder next to
/// the executable. Shown to the user so "extract it here" has a concrete
/// destination.
pub fn portable_filter_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fir-optimizer").join("output")))
}

/// Explains a lookup that came back empty, naming the exact file that was
/// wanted and every directory that was searched for it.
///
/// This is an error, never a warning: the converter refuses to substitute a
/// different filter or drop back to the plain resampler, because either would
/// quietly change what the listener hears while the UI still claims the
/// selected filter is in use.
pub fn missing_filter_error(taps: usize, target_rate_hz: u32, phase_type: &str) -> String {
    let phase_h = phase_type.replace('_', "-");
    // Below the smallest designed filter there is no filename to name, so the
    // two lines have to say different things in that case.
    let (wanted, wanted_for) = match taps_label(taps) {
        Some(label) => (
            format!("fir_{}_{}_{}.npy", label, target_rate_hz, phase_type),
            format!("{} taps at {} Hz output ({}).", label, target_rate_hz, phase_h),
        ),
        None => (
            format!("no filter is designed for {} taps — the smallest is 1M", taps),
            format!(
                "{} taps at {} Hz output ({}).",
                taps, target_rate_hz, phase_h
            ),
        ),
    };
    let mut searched = String::new();
    for dir in searched_dirs() {
        searched.push_str(&format!("\n    {}", dir));
    }
    format!(
        "Missing FIR filter — conversion stopped.\n\
         \n\
         Needed: {}\n\
         For: {}\n\
         \n\
         This build has no filter for that combination. Pick a tap count and FS \
         multiplier you have files for, or add the missing file — the converter \
         will not substitute a different filter, and will not fall back to a plain \
         resampler, because that would change what you hear while the UI still \
         showed the filter you selected.\n\
         \n\
         Searched:{}\n\
         \n\
         Filter packs: https://github.com/ToxaDev/aura-engine/releases\n\
         Or generate them: python fir-optimizer/optimize.py --all-ratios\n\
         AURA_FILTER_DIR overrides the search location.",
        wanted, wanted_for, searched
    )
}

/// Resolve the path of the pre-computed FIR blob for `taps`, output rate
/// `target_rate_hz`, and phase type `phase_type`. Returns `None` when no
/// suitable file exists — callers must treat that as a hard error and report
/// [`missing_filter_error`], never as a reason to process the file some other
/// way.
///
/// `phase_type` must be `"linear_phase"` or `"minimum_phase"`.
pub fn find_precomputed_filter(
    taps: usize,
    target_rate_hz: u32,
    phase_type: &str,
) -> Option<String> {
    let taps_label = taps_label(taps)?;

    let primary = format!("fir_{}_{}_{}.npy", taps_label, target_rate_hz, phase_type);
    let legacy = format!("fir_{}_{}.npy", taps_label, phase_type);
    // Legacy filters are designed for FS8 (8× upsample): 44.1k → 352.8k or
    // 48k → 384k. Using them at any other ratio mis-applies the cutoff
    // (see ca1af01 commit message and docs/13-...). So we only accept the
    // legacy file when target_rate is one of those two design points.
    let legacy_ok_for_rate = matches!(target_rate_hz, 352_800 | 384_000);

    for dir_opt in &search_dirs() {
        if let Some(ref dir) = dir_opt {
            // 1. Prefer the ratio-specific file
            let p = dir.join(&primary);
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
            // 2. Fall back to legacy ONLY when the requested rate matches
            //    the FS8 design point of the legacy blobs.
            if legacy_ok_for_rate {
                let p = dir.join(&legacy);
                if p.exists() {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }

    None
}

/// The tap counts the interface offers, smallest first. These are the exact
/// values the tap slider sends, so each one maps onto its own [`taps_label`].
pub const TAP_LADDER: [usize; 4] = [1_000_000, 5_000_000, 10_000_000, 30_000_000];

/// Every output rate the FS ladder can reach: the 44.1 and 48 kHz source
/// families at FS2/FS4/FS8/FS16.
pub const TARGET_RATES: [u32; 8] = [
    88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// One cell of the filter matrix that this installation actually has on disk.
pub struct Present {
    pub taps: usize,
    pub target_rate_hz: u32,
    pub linear: bool,
    pub minimum: bool,
}

/// Every (tap count, output rate) the installed blobs can serve.
///
/// Deliberately implemented by asking [`find_precomputed_filter`] rather than
/// by listing the directories: whatever the resolver accepts — including the
/// legacy single-rate names at their FS8 design point — is exactly what gets
/// reported. A separate directory walk would be a second, subtly different
/// definition of "available", and the two would drift apart at the first
/// naming change, leaving the interface offering a setting that then fails.
pub fn inventory() -> Vec<Present> {
    let mut out = Vec::new();
    for taps in TAP_LADDER {
        for rate in TARGET_RATES {
            let linear = find_precomputed_filter(taps, rate, "linear_phase").is_some();
            let minimum = find_precomputed_filter(taps, rate, "minimum_phase").is_some();
            if linear || minimum {
                out.push(Present {
                    taps,
                    target_rate_hz: rate,
                    linear,
                    minimum,
                });
            }
        }
    }
    out
}

/// The release assets that together cover one tap count at every rate — one
/// entry for most, two for 30M, whose blobs are split by rate family.
pub fn filter_packs_for_taps(taps: usize) -> Vec<(String, String)> {
    let mut packs: Vec<(String, String)> = Vec::new();
    // 352 800 Hz is in the 44.1 kHz family, 384 000 Hz in the 48 kHz one, so
    // probing both covers the split without hardcoding where it falls.
    for rate in [352_800u32, 384_000u32] {
        if let Some(pack) = filter_pack_for(taps, rate) {
            if !packs.iter().any(|(name, _)| *name == pack.0) {
                packs.push(pack);
            }
        }
    }
    packs
}

/// The directories that were searched, with `..` folded away so the list can
/// be shown to a user as somewhere to actually put files.
///
/// Several search paths are built by walking up from the executable, so they
/// arrive full of `..` components. Resolving them lexically rather than
/// canonically is deliberate: most of these directories do not exist, which
/// is precisely why we are printing them, and a path full of `..` tells a
/// reader nothing about where to actually put a file.
pub fn searched_dirs() -> Vec<String> {
    search_dirs()
        .iter()
        .flatten()
        .map(|dir| {
            let mut tidy = std::path::PathBuf::new();
            for comp in dir.components() {
                match comp {
                    std::path::Component::ParentDir => {
                        tidy.pop();
                    }
                    other => tidy.push(other.as_os_str()),
                }
            }
            tidy.display().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portable package ships the blobs beside the executable:
    ///
    ///     <pkg>/aura-engine.exe
    ///     <pkg>/fir-optimizer/output/*.npy
    ///
    /// Resolution must not depend on the working directory. It used to, so
    /// launching from a shortcut silently skipped the post-FIR stage and fell
    /// back to the plain sinc resampler — the one failure mode a listener
    /// cannot see.
    ///
    /// The probe uses an output rate that exists in no real filter pack, so
    /// neither `AURA_FILTER_DIR` nor a repo checkout can satisfy the lookup by
    /// accident and make this pass for the wrong reason.
    #[test]
    fn resolves_blobs_placed_next_to_the_executable() {
        let exe = std::env::current_exe().expect("current_exe");
        let dir = exe
            .parent()
            .expect("exe parent")
            .join("fir-optimizer")
            .join("output");
        std::fs::create_dir_all(&dir).expect("create portable filter dir");

        let name = "fir_30M_111111_linear_phase.npy";
        let blob = dir.join(name);
        std::fs::write(&blob, b"").expect("write probe blob");

        let found = find_precomputed_filter(30_000_000, 111_111, "linear_phase");
        std::fs::remove_file(&blob).ok();

        let found = found.expect("portable layout must resolve without AURA_FILTER_DIR");
        assert!(found.ends_with(name), "unexpected path: {found}");
    }

    /// A missing blob must resolve to `None` so the caller can fail the file,
    /// rather than reach for a mismatched filter.
    #[test]
    fn missing_blob_resolves_to_none() {
        assert!(find_precomputed_filter(30_000_000, 222_222, "linear_phase").is_none());
    }

    /// The download offered to a user with a missing filter has to point at
    /// the pack that actually contains it. The 30M blobs are split by rate
    /// family, so picking the wrong half sends them to a 1.9 GB download that
    /// does not help.
    #[test]
    fn download_pack_matches_the_missing_blob() {
        let (pack, url) = filter_pack_for(30_000_000, 352_800).expect("30M/44.1k pack");
        assert_eq!(pack, "aura-filters-30M-44k-family.zip");
        assert!(url.ends_with(&pack), "url must point at the pack: {url}");
        // Must address a concrete release. `releases/latest` would break every
        // link the moment a release without the packs attached is published —
        // including inside builds already handed out.
        assert!(
            url.contains("/releases/download/v"),
            "url must name the release that carries the packs: {url}"
        );

        let (pack, _) = filter_pack_for(30_000_000, 384_000).expect("30M/48k pack");
        assert_eq!(pack, "aura-filters-30M-48k-family.zip");

        // 705.6 kHz is still the 44.1 family; 768 kHz is still the 48 family.
        assert_eq!(
            filter_pack_for(30_000_000, 705_600).unwrap().0,
            "aura-filters-30M-44k-family.zip"
        );
        assert_eq!(
            filter_pack_for(30_000_000, 768_000).unwrap().0,
            "aura-filters-30M-48k-family.zip"
        );

        // Smaller tap counts ship as one pack covering every rate.
        assert_eq!(
            filter_pack_for(10_000_000, 352_800).unwrap().0,
            "aura-filters-10M-all-rates.zip"
        );
        assert_eq!(
            filter_pack_for(1_000_000, 96_000).unwrap().0,
            "aura-filters-1M-all-rates.zip"
        );
    }

    /// The name shown to the user must be the name the resolver looks for,
    /// otherwise they extract the right pack and still see the same error.
    #[test]
    fn advertised_blob_name_matches_the_resolver() {
        let name = blob_name(30_000_000, 352_800, "linear_phase").expect("name");
        assert_eq!(name, "fir_30M_352800_linear_phase.npy");

        let exe = std::env::current_exe().expect("current_exe");
        let dir = exe
            .parent()
            .expect("exe parent")
            .join("fir-optimizer")
            .join("output");
        std::fs::create_dir_all(&dir).expect("create portable filter dir");
        let probe = blob_name(30_000_000, 111_111, "minimum_phase").expect("probe name");
        let blob = dir.join(&probe);
        std::fs::write(&blob, b"").expect("write probe blob");
        let found = find_precomputed_filter(30_000_000, 111_111, "minimum_phase");
        std::fs::remove_file(&blob).ok();
        assert!(
            found.expect("resolver must find the advertised name").ends_with(&probe),
            "resolver looks for a different filename than the one advertised"
        );
    }

    /// The failure message is the only thing the user sees when a filter is
    /// missing, so it has to name the exact file and where it was looked for.
    #[test]
    fn missing_filter_error_names_the_file_and_the_search_path() {
        let msg = missing_filter_error(30_000_000, 352_800, "linear_phase");
        assert!(
            msg.contains("fir_30M_352800_linear_phase.npy"),
            "must name the exact file: {msg}"
        );
        assert!(
            msg.contains("fir-optimizer"),
            "must list where it searched: {msg}"
        );
        assert!(
            msg.contains("AURA_FILTER_DIR"),
            "must mention the override: {msg}"
        );
    }

    /// Each rung of the tap ladder must resolve to its own blob label. If two
    /// rungs ever collapsed onto one label, the slider would offer two
    /// positions backed by the same file — the user would move it, watch the
    /// interface say "10M Taps", and get the 5M filter.
    #[test]
    fn every_tap_ladder_rung_has_its_own_label() {
        let mut labels: Vec<&str> = Vec::new();
        for taps in TAP_LADDER {
            let label = taps_label(taps).expect("every ladder rung must be a designed size");
            assert!(
                !labels.contains(&label),
                "{} taps collapses onto the same label as an earlier rung: {}",
                taps,
                label
            );
            labels.push(label);
        }
    }

    /// The inventory is what the interface shapes itself around, so it must
    /// agree with the resolver cell for cell. A disagreement in either
    /// direction is a user-visible bug: an offered setting that then fails, or
    /// a working setting the user is told they do not have.
    #[test]
    fn inventory_agrees_with_the_resolver_cell_for_cell() {
        let inv = inventory();
        for taps in TAP_LADDER {
            for rate in TARGET_RATES {
                let linear = find_precomputed_filter(taps, rate, "linear_phase").is_some();
                let minimum = find_precomputed_filter(taps, rate, "minimum_phase").is_some();
                let entry = inv
                    .iter()
                    .find(|e| e.taps == taps && e.target_rate_hz == rate);
                match entry {
                    Some(e) => {
                        assert_eq!(e.linear, linear, "{taps}@{rate} linear");
                        assert_eq!(e.minimum, minimum, "{taps}@{rate} minimum");
                        assert!(
                            e.linear || e.minimum,
                            "{taps}@{rate} listed with neither phase present"
                        );
                    }
                    None => assert!(
                        !linear && !minimum,
                        "{taps}@{rate} is resolvable but missing from the inventory"
                    ),
                }
            }
        }
    }

    /// A user missing a whole tap count needs every pack that covers it — for
    /// 30M that is two, because its blobs are split by rate family. Offering
    /// only one would leave half their library unconvertible after a 1.9 GB
    /// download.
    #[test]
    fn tap_count_packs_cover_both_rate_families() {
        let packs = filter_packs_for_taps(30_000_000);
        let names: Vec<&str> = packs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "aura-filters-30M-44k-family.zip",
                "aura-filters-30M-48k-family.zip"
            ]
        );

        // Every other size ships as one pack covering all rates, and must be
        // offered once rather than twice.
        for taps in [1_000_000, 5_000_000, 10_000_000] {
            let packs = filter_packs_for_taps(taps);
            assert_eq!(packs.len(), 1, "{taps} should need exactly one pack");
        }
    }

    /// The search list shown to the user must be the list actually consulted,
    /// and must be readable — no `..` segments left in it.
    #[test]
    fn searched_dirs_are_tidy_and_complete() {
        let dirs = searched_dirs();
        assert_eq!(dirs.len(), search_dirs().iter().flatten().count());
        for dir in &dirs {
            assert!(!dir.contains(".."), "unresolved parent segment: {dir}");
        }
    }
}
