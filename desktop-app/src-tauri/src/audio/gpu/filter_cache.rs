//! Cross-file cache for the two expensive, purely-derived artefacts of GPU
//! convolver construction.
//!
//! Building a `GpuDspProcessor` recomputes both of these from scratch every
//! single time:
//!
//!   * the DS twiddle table — `N/2` f64 `sin`/`cos` evaluations plus hi/lo
//!     splits; depends on **nothing but the FFT size**;
//!   * the partitioned filter spectrum `H[ω]` — `K` rustfft transforms of
//!     length `N` in `Complex<f64>`, then packed to DS; depends on **nothing
//!     but the filter coefficients**.
//!
//! On the polyphase path a processor is constructed once per sub-filter, so
//! that is `L` constructions per pass and `2L` per file once Hybrid-Phase is
//! on — and every file in a batch that shares a source-rate family rebuilds
//! the *same* spectra again. Measured on a 30M-tap 44.1k→352.8k ×8 job
//! (RTX 4090, session log 2026-07-14): ~150 ms of rustfft and ~42 ms of
//! twiddle generation per sub-pass, ×16 sub-passes per file, on every one of
//! 671 files.
//!
//! Correctness argument: both cached values are pure functions of their key,
//! and are handed out immutably. A hit therefore produces byte-for-byte the
//! same bytes the uncached path would have uploaded — the convolution is
//! bit-identical whether an entry was cached or recomputed.
//!
//! Sizing: the spectrum blob is `K × N × 4` f32 = 128 MB for the common
//! K=2 / N=4M case. The access pattern is strictly cyclic (phase 0..L, then
//! min-phase 0..L, repeat next file), and LRU is pathological on cyclic
//! access — it evicts precisely the entry needed next. So the policy here is
//! *fill to the cap, then stop accepting*: a working set that fits gets a
//! 100 % hit rate, and one that doesn't still gets partial hits instead of
//! zero.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Identity of one filter spectrum.
///
/// `path` alone is NOT an identity. `find_precomputed_filter` keys on the
/// OUTPUT rate only, so every source rate in the same family resolves to the
/// same `.npy` — 44.1 kHz and 88.2 kHz sources at FS×8 both target 352 800 Hz
/// and both load `fir_30M_352800_linear_phase.npy`. They then decompose it
/// with a DIFFERENT stride (L = 8 vs L = 4), producing completely different
/// sub-filters. Worse, `block_size()` clamps at `GPU_MAX_BLOCK_SIZE`, so both
/// land on the same N = 4 194 304 — a key of (path, phase, N) collides across
/// them and would hand one file the other's spectrum. Silently: no error, no
/// failed verification, just wrong audio.
///
/// So `l` is part of the identity, and every constructor makes the caller
/// state which kind of filter this is.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FilterId {
    pub path: String,
    /// Which polyphase sub-filter, or `None` for the undecomposed filter.
    pub phase: Option<usize>,
    /// Polyphase stride the decomposition used. `1` for the whole filter.
    pub l: usize,
}

impl FilterId {
    /// The whole filter, undecomposed — the non-polyphase standard path.
    pub fn whole(path: &str) -> Self {
        Self {
            path: path.to_string(),
            phase: None,
            l: 1,
        }
    }

    /// Sub-filter `phase` of an L-way polyphase decomposition.
    pub fn polyphase(path: &str, phase: usize, l: usize) -> Self {
        Self {
            path: path.to_string(),
            phase: Some(phase),
            l,
        }
    }
}

/// Cache key: identity plus the FFT size the spectrum was partitioned for.
/// `n` is derivable from the coefficient count, so it is belt-and-braces —
/// but it makes a stale hit impossible if `block_size()` is ever re-tuned.
type SpectrumKey = (FilterId, usize);

struct Store {
    twiddles: HashMap<usize, Arc<Vec<f32>>>,
    spectra: HashMap<SpectrumKey, Arc<Vec<f32>>>,
    /// Bytes currently held by `spectra` (twiddles are small and uncapped).
    spectra_bytes: u64,
    /// Set once we first refuse an insert, so the log line is emitted once
    /// per batch instead of once per sub-pass.
    cap_reported: bool,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| {
        Mutex::new(Store {
            twiddles: HashMap::new(),
            spectra: HashMap::new(),
            spectra_bytes: 0,
            cap_reported: false,
        })
    })
}

/// Byte budget for cached spectra. One eighth of physical RAM, hard-capped
/// at 4 GB — enough for two complete ×8 filter sets (linear + minimum phase)
/// at the 128 MB-per-entry common case, while staying well clear of the
/// per-file RAM admission budget in `crate::audio::memory`.
fn spectra_cap_bytes() -> u64 {
    let total_mb = crate::audio::memory::total_ram_mb();
    let eighth = (total_mb / 8).saturating_mul(1024 * 1024);
    eighth.min(4 * 1024 * 1024 * 1024)
}

/// DS twiddle table for an `n`-point FFT, laid out as
/// `(cos_hi, cos_lo, neg_sin_hi, neg_sin_lo)` for `k in 0..n/2`.
///
/// Forward `W_N^k = exp(-2πi·k/N)`; the shader conjugates for the IFFT.
/// This is the single source of truth for that layout — `setup.rs` used to
/// build it inline and must not grow a second copy.
pub fn twiddles(n: usize) -> Arc<Vec<f32>> {
    {
        let guard = store().lock().unwrap();
        if let Some(hit) = guard.twiddles.get(&n) {
            return Arc::clone(hit);
        }
    }

    // Computed outside the lock: at n = 4M this is ~42 ms of f64 sin/cos and
    // there is no reason to serialize two workers behind it. A concurrent
    // duplicate build is harmless — the values are identical, and the insert
    // below simply keeps whichever landed first.
    let mut v = Vec::with_capacity((n / 2) * 4);
    let two_pi = 2.0 * std::f64::consts::PI;
    for k in 0..(n / 2) {
        let angle = two_pi * (k as f64) / (n as f64);
        let cos_v = angle.cos();
        let sin_v = -angle.sin();
        let cos_hi = cos_v as f32;
        let cos_lo = (cos_v - cos_hi as f64) as f32;
        let sin_hi = sin_v as f32;
        let sin_lo = (sin_v - sin_hi as f64) as f32;
        v.push(cos_hi);
        v.push(cos_lo);
        v.push(sin_hi);
        v.push(sin_lo);
    }
    let arc = Arc::new(v);

    let mut guard = store().lock().unwrap();
    Arc::clone(guard.twiddles.entry(n).or_insert(arc))
}

/// Look up a cached partitioned spectrum. `None` on miss, or whenever the
/// caller passed no identity (a filter with no stable provenance — e.g. the
/// unit-test paths — is never cached).
///
/// `expect_words` is the f32 count the caller is about to upload
/// (`num_blocks × n × 4`). A hit whose length disagrees is refused and
/// reported: the key is supposed to determine the shape, so a mismatch means
/// the key is incomplete. Refusing costs one recomputation; serving it would
/// either under-fill `h_freq` (leaving zeroed partitions — silent audio
/// corruption) or overrun the buffer. Defence in depth against exactly the
/// class of bug the `l` field above documents.
pub fn get_spectrum(id: Option<&FilterId>, n: usize, expect_words: usize) -> Option<Arc<Vec<f32>>> {
    let id = id?;
    let guard = store().lock().unwrap();
    let hit = guard.spectra.get(&(id.clone(), n))?;
    if hit.len() != expect_words {
        crate::aelog!(
            "[GPU/CACHE] BUG: cached spectrum for {:?} has {} words, caller expects {} \n             — refusing the hit and recomputing. The cache key is incomplete.",
            id,
            hit.len(),
            expect_words
        );
        return None;
    }
    Some(Arc::clone(hit))
}

/// Offer a freshly computed spectrum to the cache. Returns the `Arc` the
/// caller should use — the stored one on success, a fresh one when the cache
/// declined (over budget, or no identity).
pub fn put_spectrum(id: Option<&FilterId>, n: usize, blob: Vec<f32>) -> Arc<Vec<f32>> {
    let arc = Arc::new(blob);
    let id = match id {
        Some(i) => i,
        None => return arc,
    };
    let bytes = (arc.len() * 4) as u64;
    let cap = spectra_cap_bytes();

    let mut guard = store().lock().unwrap();
    let key = (id.clone(), n);
    if let Some(existing) = guard.spectra.get(&key) {
        return Arc::clone(existing);
    }
    if guard.spectra_bytes + bytes > cap {
        if !guard.cap_reported {
            guard.cap_reported = true;
            crate::aelog!(
                "[GPU/CACHE] spectrum cache full at {} MB ({} entries) — further \
                 filters will be recomputed per file",
                guard.spectra_bytes / 1_048_576,
                guard.spectra.len()
            );
        }
        return arc;
    }
    guard.spectra_bytes += bytes;
    guard.spectra.insert(key, Arc::clone(&arc));
    crate::aelog!(
        "[GPU/CACHE] cached spectrum {}#{} ({} MB) — cache now {} entries / {} MB",
        id.path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&id.path),
        id.phase.map(|p| p.to_string()).unwrap_or_else(|| "full".into()),
        bytes / 1_048_576,
        guard.spectra.len(),
        guard.spectra_bytes / 1_048_576
    );
    arc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store is process-global, so these tests must not run concurrently
    /// with each other — one test's `clear()` would wipe another's fixtures.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The one way this cache can corrupt audio is by serving the spectrum of
    /// a DIFFERENT filter — a key collision. Every component of the key must
    /// therefore discriminate: the source `.npy`, the polyphase sub-filter
    /// index, and the FFT size.
    #[test]
    fn distinct_keys_do_not_collide() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let a0 = FilterId::polyphase("fir_30M_352800_linear_phase.npy", 0, 8);
        let a1 = FilterId::polyphase("fir_30M_352800_linear_phase.npy", 1, 8);
        let b0 = FilterId::polyphase("fir_30M_352800_minimum_phase.npy", 0, 8);
        let whole = FilterId::whole("fir_30M_352800_linear_phase.npy");

        put_spectrum(Some(&a0), 1024, vec![1.0; 4]);
        put_spectrum(Some(&a1), 1024, vec![2.0; 4]);
        put_spectrum(Some(&b0), 1024, vec![3.0; 4]);
        put_spectrum(Some(&whole), 1024, vec![4.0; 4]);

        assert_eq!(get_spectrum(Some(&a0), 1024, 4).unwrap()[0], 1.0);
        assert_eq!(get_spectrum(Some(&a1), 1024, 4).unwrap()[0], 2.0, "phase index must discriminate");
        assert_eq!(get_spectrum(Some(&b0), 1024, 4).unwrap()[0], 3.0, "filter path must discriminate");
        assert_eq!(get_spectrum(Some(&whole), 1024, 4).unwrap()[0], 4.0, "whole filter must not alias phase 0");

        // A different FFT size is a different partitioning — never a hit.
        assert!(get_spectrum(Some(&a0), 2048, 4).is_none(), "FFT size must discriminate");
        // No identity → never cached, never served.
        assert!(get_spectrum(None, 1024, 4).is_none());
        clear();
    }

    /// A hit must hand back exactly the bytes that were stored — the whole
    /// bit-identity argument rests on the value being passed through
    /// untouched.
    #[test]
    fn hit_returns_the_stored_bytes() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let id = FilterId::polyphase("some_filter.npy", 3, 8);
        let blob: Vec<f32> = (0..64).map(|i| (i as f32) * 0.5 - 7.25).collect();
        let n_words = blob.len();
        let stored = put_spectrum(Some(&id), 512, blob.clone());
        assert_eq!(&stored[..], &blob[..]);
        let hit = get_spectrum(Some(&id), 512, n_words).expect("should hit");
        for i in 0..blob.len() {
            assert_eq!(hit[i].to_bits(), blob[i].to_bits(), "word {} differs", i);
        }
        clear();
        assert!(get_spectrum(Some(&id), 512, n_words).is_none(), "clear() must evict");
    }

    /// End-to-end benchmark against a REAL filter from the matrix: builds the
    /// full polyphase bank of GPU processors twice — once cold, once warm —
    /// and reports the per-construction saving. Requires a GPU and the
    /// fir-optimizer matrix, so it is opt-in:
    ///
    ///     cargo test --release -- --ignored filter_cache_bench --nocapture
    ///
    /// Also asserts the property everything else rests on: the spectrum a warm
    /// build uploads is bit-for-bit the one the cold build computed.
    #[test]
    #[ignore]
    fn filter_cache_bench_on_real_filter() {
        use std::time::Instant;
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fir-optimizer/output/fir_30M_352800_linear_phase.npy"
        );
        if !std::path::Path::new(path).exists() {
            eprintln!("[BENCH] {} not present — skipping", path);
            return;
        }

        let coeffs = crate::audio::dsp_core::load_npy_f64(path).expect("load .npy");
        let l = 8usize;
        let phases = crate::audio::converter::dsp::polyphase::polyphase_decompose(&coeffs, l);
        drop(coeffs);

        let build_bank = |label: &str| -> (f64, Vec<Vec<f32>>) {
            let t = Instant::now();
            let mut spectra = Vec::with_capacity(l);
            for (phase, ph) in phases.iter().enumerate() {
                let id = FilterId::polyphase(path, phase, l);
                let p = crate::audio::gpu::GpuDspProcessor::new_with_coefficients_keyed(
                    ph, 64, Some(&id),
                )
                .expect("build processor");
                let (n, num_blocks) = (p.n, p.num_blocks);
                drop(p);
                spectra.push(
                    get_spectrum(Some(&id), n, num_blocks * n * 4)
                        .expect("cached")
                        .to_vec(),
                );
            }
            let secs = t.elapsed().as_secs_f64();
            eprintln!("[BENCH] {}: {} phases in {:.2}s ({:.0} ms each)",
                      label, l, secs, secs * 1000.0 / l as f64);
            (secs, spectra)
        };

        clear();
        let (cold, cold_spectra) = build_bank("cold");
        let (warm, warm_spectra) = build_bank("warm");
        eprintln!(
            "[BENCH] saving {:.2}s per pass ({:.1}%); ×16 sub-passes per hybrid file = {:.1}s",
            cold - warm,
            (cold - warm) / cold * 100.0,
            (cold - warm) * 2.0
        );

        for p in 0..l {
            assert_eq!(cold_spectra[p].len(), warm_spectra[p].len());
            for i in 0..cold_spectra[p].len() {
                assert_eq!(
                    cold_spectra[p][i].to_bits(),
                    warm_spectra[p][i].to_bits(),
                    "phase {} word {}: warm build differs from cold",
                    p, i
                );
            }
        }
        clear();
    }

    /// REGRESSION: 44.1 kHz and 88.2 kHz sources at FS×8 both target 352 800 Hz,
    /// so `find_precomputed_filter` hands both the SAME `.npy`. They decompose it
    /// at L = 8 and L = 4 respectively — completely different sub-filters — and
    /// because `block_size()` clamps at GPU_MAX_BLOCK_SIZE both land on
    /// N = 4 194 304. A key without `l` collides and serves one file the other's
    /// filter: no error, no failed verification, just wrong audio.
    #[test]
    fn polyphase_stride_is_part_of_the_identity() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let path = "fir_30M_352800_linear_phase.npy";
        const N: usize = 4_194_304;

        // 44.1k source: L = 8, sub_taps 3.75M → num_blocks 2.
        let x8 = FilterId::polyphase(path, 3, 8);
        // 88.2k source: L = 4, sub_taps 7.5M → num_blocks 4. Same path, same N.
        let x4 = FilterId::polyphase(path, 3, 4);
        assert_ne!(x8, x4, "L must discriminate — this is the whole point");

        put_spectrum(Some(&x8), N, vec![8.0; 2 * 4]);
        put_spectrum(Some(&x4), N, vec![4.0; 4 * 4]);
        assert_eq!(get_spectrum(Some(&x8), N, 2 * 4).unwrap()[0], 8.0);
        assert_eq!(get_spectrum(Some(&x4), N, 4 * 4).unwrap()[0], 4.0);

        // And even if a future key regression let them collide, the length
        // guard must refuse rather than under-fill h_freq with zeroed
        // partitions.
        assert!(
            get_spectrum(Some(&x8), N, 4 * 4).is_none(),
            "a hit of the wrong length must be refused, not served"
        );
        clear();
    }

    /// The twiddle table is the same pure function of N that setup.rs used to
    /// inline; a second call must return the identical table, not rebuild a
    /// subtly different one.
    #[test]
    fn twiddles_are_deterministic_and_shared() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let a = twiddles(1024);
        let b = twiddles(1024);
        assert_eq!(a.len(), 512 * 4);
        assert!(Arc::ptr_eq(&a, &b), "second call should reuse the cached table");
        // Spot-check the documented layout: k = 0 is (1, 0, -0, -0).
        assert_eq!(a[0], 1.0);
        assert_eq!(a[2], -0.0);
        clear();
    }
}

/// Drop everything. Called at batch start (so a filter regenerated between
/// batches is picked up) and at batch end (so a finished batch does not sit
/// on gigabytes of spectra).
pub fn clear() {
    let mut guard = store().lock().unwrap();
    if guard.spectra.is_empty() && guard.twiddles.is_empty() {
        return;
    }
    crate::aelog!(
        "[GPU/CACHE] cleared — {} spectra ({} MB), {} twiddle tables",
        guard.spectra.len(),
        guard.spectra_bytes / 1_048_576,
        guard.twiddles.len()
    );
    guard.spectra.clear();
    guard.twiddles.clear();
    guard.spectra_bytes = 0;
    guard.cap_reported = false;
}
