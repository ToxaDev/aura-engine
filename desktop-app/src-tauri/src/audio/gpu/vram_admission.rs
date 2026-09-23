//! VRAM admission control for concurrent GPU convolvers.
//!
//! ## What this replaces
//!
//! The worker count used to be decided once, at batch start, from
//! `recommended_gpu_workers(settings.taps)`. That estimate was wrong in the
//! common case for a structural reason: on the polyphase path a processor is
//! built from **one sub-filter** (`taps / L`), not the whole filter, so the
//! estimate was off by a factor of `L`. A 30M-tap ×8 job was sized as if each
//! worker needed 3136 MB when the allocator actually reported 416 MB — and
//! the batch was pinned to a single worker for the rest of the run.
//!
//! It was also unfixable at that call site: the worker count is chosen before
//! any file is decoded, and `L` depends on each file's source rate. A batch
//! mixing 44.1k and already-at-target files legitimately has different `L`
//! per file.
//!
//! So the decision moves to where the number is actually known: the moment a
//! processor allocates. Threads are no longer rationed by a guess — they are
//! admitted against the demand they are really about to place on the device.
//!
//! ## Budget
//!
//! What is free on the card (`dxgi_memory`: the smaller of what the Windows
//! budget leaves this process and what all processes together leave on the
//! adapter), read once per batch before the first convolver allocates, with a
//! quarter held back for what the accounting below does not see: pipelines,
//! bind groups, driver overhead, another program growing mid-batch.
//!
//! A device that refuses anyway is caught by `note_oom`. Refused next to other
//! convolvers, it lowers this batch's budget to what those others hold — one
//! fewer at a time, not every later convolver of that size sent to the CPU.
//!
//! It used to be `max_storage_buffer_binding_size × 0.7` — a Vulkan limit on
//! ONE buffer, not on the card, reading ~2 GB on an 8 GB and a 24 GB card
//! alike. A 30M ×8 convolver takes 736 MB, so that floor admitted one at a
//! time however many workers there were, on the assumption that convolvers
//! sharing a card would only contend for it. They do contend — on an RTX 4090
//! an OLA block takes 59 ms alone and about 127 ms beside another convolver's
//! — and the batch is faster anyway: 13 files at 30M ×8, three workers,
//! i9-14900K, went from 221.6 s to 179.5 s (two runs each, outputs
//! byte-identical with the dither seeded), after 160 s of worker time queued
//! here per run. The floor serialised more than the blocks: every convolver's
//! construction, upload and flush, and the CPU stages of the file behind it.
//! At 10M, where two convolvers already fit under the floor, the difference
//! was inside the run-to-run spread.
//!
//! The floor remains where there is no reading to be had: off Windows, on an
//! integrated adapter (whose "local" memory is system RAM, already rationed by
//! `memory::reserve_ram`), or with two identical cards DXGI cannot tell apart.
//! `AURA_VRAM_BUDGET_MB` overrides both, so the two behaviours can be timed
//! against each other on one build. Not documented anywhere a user would read.
//!
//! ## Deadlock freedom
//!
//! Two rules, both required:
//!
//!   1. **Nothing outstanding ⇒ admit unconditionally.** A single processor
//!      larger than the whole budget must still be able to run.
//!   2. **A thread already holding a reservation is re-entrant.** The
//!      segmented giant path builds a bank of `L` processors and holds them
//!      all for the file; without this rule it would block against itself
//!      after the first one.
//!
//! Together these guarantee at least one thread always makes progress.
//! Cancellation also admits immediately so a cancel is never queued behind
//! the budget.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

struct Gate {
    /// Bytes reserved by all live GPU processors.
    outstanding: u64,
    /// Per-thread breakdown, for the re-entrancy rule. Deliberately NOT a
    /// thread_local: `GpuDspProcessor` is boxed as `dyn DspProcessor + Send`,
    /// so nothing in the type system stops a future refactor from building a
    /// convolver on one thread and dropping it on another. With a thread_local
    /// that would leave the builder permanently "already holding" — silently
    /// disabling the gate for that thread. Keying on the owner's ThreadId, and
    /// releasing against that same id, is correct wherever the drop happens.
    held: HashMap<ThreadId, u64>,
}

static GATE: OnceLock<(Mutex<Gate>, Condvar)> = OnceLock::new();

fn gate() -> &'static (Mutex<Gate>, Condvar) {
    GATE.get_or_init(|| {
        (
            Mutex::new(Gate {
                outstanding: 0,
                held: HashMap::new(),
            }),
            Condvar::new(),
        )
    })
}

/// RAII ticket. Dropping it releases the budget and wakes a waiting worker.
pub struct VramReservation {
    bytes: u64,
    owner: ThreadId,
}

impl Drop for VramReservation {
    fn drop(&mut self) {
        let (m, cv) = gate();
        {
            let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
            g.outstanding = g.outstanding.saturating_sub(self.bytes);
            if let Some(h) = g.held.get_mut(&self.owner) {
                *h = h.saturating_sub(self.bytes);
                if *h == 0 {
                    g.held.remove(&self.owner);
                }
            }
        }
        cv.notify_all();
    }
}

const MIB: u64 = 1_048_576;

/// This batch's budget, measured on first use and forgotten at batch start.
static BUDGET: Mutex<Option<u64>> = Mutex::new(None);

/// Device-memory budget in bytes for this batch. See the module note.
fn budget_bytes() -> u64 {
    let mut cached = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(bytes) = *cached {
        return bytes;
    }
    let bytes = measure_budget();
    *cached = Some(bytes);
    bytes
}

fn measure_budget() -> u64 {
    let ctx = match crate::audio::gpu::context::try_gpu_context() {
        Ok(ctx) => ctx,
        // Unreachable while every GPU caller holds a context already. If it
        // ever is reached, an unlimited budget is the only safe answer: a
        // budget of zero would park the caller in the wait loop below with
        // nothing to wake it, and a hang is worse than the allocation
        // failure it would be trying to prevent.
        Err(_) => return u64::MAX,
    };
    let floor = (ctx.device.limits().max_storage_buffer_binding_size as f64 * 0.7) as u64;

    if let Some(mb) = std::env::var("AURA_VRAM_BUDGET_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&mb| mb > 0)
    {
        crate::aelog!("[GPU/VRAM] admission budget {} MB — set by AURA_VRAM_BUDGET_MB", mb);
        return mb * MIB;
    }

    if ctx.device_type != wgpu::DeviceType::DiscreteGpu {
        crate::aelog!(
            "[GPU/VRAM] admission budget {} MB — floor: {:?} adapter, its memory is system RAM",
            floor / MIB,
            ctx.device_type
        );
        return floor;
    }

    match crate::audio::gpu::dxgi_memory::query(ctx.vendor_id, ctx.device_id) {
        Ok(m) => {
            let bytes = budget_from_free(m.free());
            crate::aelog!(
                "[GPU/VRAM] admission budget {} MB — {} MB free on the card's {} MB ({} held by all programs, {} MB by this one; Windows budget {} MB), a quarter held back",
                bytes / MIB,
                m.free() / MIB,
                m.dedicated / MIB,
                m.adapter_usage
                    .map(|u| format!("{} MB", u / MIB))
                    .unwrap_or_else(|| "unknown".to_string()),
                m.usage / MIB,
                m.budget / MIB
            );
            bytes
        }
        Err(e) => {
            crate::aelog!("[GPU/VRAM] admission budget {} MB — floor: {}", floor / MIB, e);
            floor
        }
    }
}

/// Three quarters of what is free.
fn budget_from_free(free: u64) -> u64 {
    free / 4 * 3
}

/// Total device memory one `GpuDspProcessor` allocates for `target_taps`.
///
/// Mirrors the allocator in `setup.rs` exactly — h_freq + both delay lines
/// (each `num_blocks × N × 16` in DS layout), the four N-sized scratch
/// buffers, the twiddle table and the readback staging buffer. Keep this in
/// step with `new_uninitialized` if the buffer set ever changes.
pub fn processor_bytes(target_taps: usize) -> u64 {
    let b_size = crate::audio::gpu::GpuDspProcessor::block_size(target_taps);
    let n = b_size * 2;
    let num_blocks = (target_taps + b_size - 1) / b_size;

    let h_or_delay = (num_blocks * n * 16) as u64;
    let scratch = (n * 16 * 4) as u64; // work_l/r + accum_l/r
    let twiddle = ((n / 2) * 16) as u64;
    let staging = (b_size * 16 * 2) as u64;

    h_or_delay * 3 + scratch + twiddle + staging
}

/// The smallest per-processor demand this device has actually refused.
///
/// The budget above is a proxy and says so — wgpu 0.19 has no free-memory
/// query — so the only honest source of a real ceiling is the device saying
/// no. Once it has, every later request at or above that size goes straight
/// to the CPU rather than allocating, failing and falling back once per file
/// for the rest of the batch.
static OOM_FLOOR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Record that the device refused an allocation of `bytes`.
///
/// Must be called on the thread that holds the refused reservation. If other
/// threads held device memory at the time, the refusal says the card cannot
/// take this many at once — not that it cannot take one this size — so the
/// batch's budget drops to what the others held and the size ceiling is left
/// alone.
pub fn note_oom(bytes: u64) {
    let others = {
        let g = gate().0.lock().unwrap_or_else(|e| e.into_inner());
        let mine = g.held.get(&std::thread::current().id()).copied().unwrap_or(0);
        g.outstanding.saturating_sub(mine)
    };
    if others > 0 {
        let lowered = {
            let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
            if budget.map_or(true, |b| others < b) {
                *budget = Some(others);
                true
            } else {
                false
            }
        };
        if lowered {
            crate::aelog!(
                "[GPU/VRAM] device refused {} MB next to {} MB held by other workers — admission budget lowered to {} MB for the rest of this batch",
                bytes / MIB,
                others / MIB,
                others / MIB
            );
        }
        return;
    }
    let prev = OOM_FLOOR.fetch_min(bytes, std::sync::atomic::Ordering::Relaxed);
    if bytes < prev {
        crate::aelog!(
            "[GPU] Device ceiling learned: {} MB refused — convolvers at or above that size will be built on the CPU for the rest of this batch",
            bytes / 1_048_576
        );
    }
}

/// Has this device already refused an allocation this size or smaller?
pub fn known_to_fail(bytes: u64) -> bool {
    bytes >= OOM_FLOOR.load(std::sync::atomic::Ordering::Relaxed)
}

/// Forget the learned ceiling and the measured budget. Called once at batch
/// start: whatever else was holding the card during the last run may be gone,
/// or something new may be holding it now.
pub fn reset_oom_floor() {
    OOM_FLOOR.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    *BUDGET.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Block until `bytes` of device memory can be reserved alongside every
/// other live GPU processor, then reserve it.
pub fn reserve(bytes: u64) -> VramReservation {
    reserve_within(bytes, budget_bytes())
}

/// Core of `reserve`, with the budget injected so the admission rules can be
/// tested without a GPU present.
fn reserve_within(bytes: u64, budget: u64) -> VramReservation {
    let me = std::thread::current().id();
    let (m, cv) = gate();
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());

    // Rule 2: a thread that already holds device memory is re-entrant.
    let reentrant = g.held.get(&me).copied().unwrap_or(0) > 0;
    if !reentrant {
        let mut waited = false;
        // Rule 1: an idle device always admits, however large the demand.
        while g.outstanding != 0 && g.outstanding + bytes > budget {
            if crate::audio::converter::state::CONV_CANCEL.load(
                std::sync::atomic::Ordering::Relaxed,
            ) || crate::audio::cancel_flag::check()
            {
                break;
            }
            if !waited {
                waited = true;
                // Snapshot, then RELEASE the gate before logging: aelog! takes
                // the log mutex and writes+flushes a file, and holding the gate
                // across that would stall a concurrent Drop (which needs the
                // gate to release memory and wake us) for the I/O latency.
                let (need_mb, out_mb, budget_mb) = (
                    bytes / 1_048_576,
                    g.outstanding / 1_048_576,
                    budget / 1_048_576,
                );
                drop(g);
                crate::aelog!(
                    "[GPU/VRAM] convolver needs {} MB, {} MB already committed                      (budget {} MB) — taking turns with the other worker",
                    need_mb,
                    out_mb,
                    budget_mb,
                );
                g = m.lock().unwrap_or_else(|e| e.into_inner());
                // The state may have changed while the gate was open; re-test
                // the loop condition instead of falling into wait_timeout.
                continue;
            }
            // Timeout so a cancel raised while we sleep is still noticed.
            let (guard, _) = cv
                .wait_timeout(g, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
        }
    }

    g.outstanding += bytes;
    *g.held.entry(me).or_insert(0) += bytes;
    VramReservation { bytes, owner: me }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as AtomOrd};
    use std::sync::Arc;

    /// The gate is process-global; these tests assert on `OUTSTANDING`
    /// transitions and must not interleave.
    pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn outstanding() -> u64 {
        gate().0.lock().unwrap_or_else(|e| e.into_inner()).outstanding
    }

    /// Rule 1. A convolver larger than the entire budget must still run —
    /// otherwise a 30M-tap filter on a modest card would hang forever instead
    /// of converting.
    #[test]
    fn oversized_reservation_is_admitted_on_an_idle_device() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        let t = reserve_within(64 << 30, 1 << 30);
        assert_eq!(outstanding(), 64 << 30);
        drop(t);
        assert_eq!(outstanding(), 0);
    }

    /// Rule 2. The segmented giant path builds a bank of L convolvers and
    /// holds them all; without re-entrancy the second one would block against
    /// the first — on the same thread, so nothing could ever release it.
    #[test]
    fn a_thread_holding_a_reservation_never_blocks_against_itself() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        let budget = 1000u64;
        let mut bank = Vec::new();
        // Eight "sub-filters" of 400 each: way past the budget in total.
        for _ in 0..8 {
            bank.push(reserve_within(400, budget));
        }
        assert_eq!(outstanding(), 3200);
        drop(bank);
        assert_eq!(outstanding(), 0);
    }

    /// Two workers must take turns rather than both proceeding (which would
    /// over-commit the device) or both stalling (which would hang the batch).
    #[test]
    fn a_second_worker_waits_then_proceeds_when_the_first_releases() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        let budget = 1000u64;

        let a = reserve_within(600, budget);
        assert_eq!(outstanding(), 600);

        let admitted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&admitted);
        let worker = std::thread::spawn(move || {
            // 600 + 600 > 1000 → must block until `a` is dropped.
            let t = reserve_within(600, budget);
            flag.store(true, AtomOrd::SeqCst);
            t
        });

        // Give the worker a chance to reach the wait; it must NOT be admitted.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !admitted.load(AtomOrd::SeqCst),
            "second worker was admitted while the device was committed"
        );

        drop(a);
        let b = worker.join().expect("worker must not panic — a hang here is the deadlock bug");
        assert!(admitted.load(AtomOrd::SeqCst));
        assert_eq!(outstanding(), 600);
        drop(b);
        assert_eq!(outstanding(), 0);
    }

    /// Two convolvers that genuinely fit share the device without waiting.
    #[test]
    fn two_small_workers_fit_concurrently() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        let budget = 1000u64;
        let a = reserve_within(400, budget);
        let b = std::thread::spawn(move || reserve_within(400, budget))
            .join()
            .expect("must not block");
        assert_eq!(outstanding(), 800);
        drop(a);
        drop(b);
        assert_eq!(outstanding(), 0);
    }

    /// Free is the smaller of what the OS budget leaves and what the card
    /// has left, and never wraps when either is already exceeded.
    #[test]
    fn free_video_memory_is_the_smaller_of_budget_and_card() {
        use crate::audio::gpu::dxgi_memory::LocalMemory;
        let gib = 1u64 << 30;
        let m = |budget, usage, dedicated, adapter_usage| LocalMemory {
            budget,
            usage,
            dedicated,
            adapter_usage,
        };
        // The RTX 4090 reading: a generous budget, 3.7 GB held elsewhere.
        assert_eq!(m(23 * gib, 0, 24 * gib, Some(4 * gib)).free(), 20 * gib);
        // A budget tighter than the card.
        assert_eq!(m(6 * gib, 2 * gib, 8 * gib, Some(3 * gib)).free(), 4 * gib);
        // No counter: the budget alone.
        assert_eq!(m(8 * gib, gib, 8 * gib, None).free(), 7 * gib);
        // Already over: zero, not a wrap.
        assert_eq!(m(gib, 2 * gib, 8 * gib, Some(9 * gib)).free(), 0);
        assert_eq!(budget_from_free(8 * gib), 6 * gib);
    }

    /// Accounting must match what setup.rs actually allocates for the shape
    /// the logs show (3.75M taps → b_size 2 097 152, N 4 194 304, K = 2).
    #[test]
    fn processor_bytes_matches_the_documented_allocation() {
        let bytes = processor_bytes(3_750_000);
        let n = 4_194_304u64;
        let expected = (2 * n * 16) * 3      // h_freq + delay_l + delay_r
            + n * 16 * 4                     // work_l/r + accum_l/r
            + (n / 2) * 16                   // twiddles
            + 2_097_152 * 16 * 2;            // staging
        assert_eq!(bytes, expected);
    }
}

#[cfg(test)]
mod cross_thread_tests {
    use super::*;

    fn outstanding() -> u64 {
        gate().0.lock().unwrap_or_else(|e| e.into_inner()).outstanding
    }
    fn held_threads() -> usize {
        gate().0.lock().unwrap_or_else(|e| e.into_inner()).held.len()
    }

    /// `GpuDspProcessor` is boxed as `dyn DspProcessor + Send`, so a reservation
    /// may legally be released on a thread other than the one that took it.
    /// The accounting must survive that: the global total returns to zero AND
    /// the builder thread must not be left permanently marked as holding, which
    /// would make it re-entrant forever and disable the gate.
    #[test]
    fn a_reservation_released_on_another_thread_is_accounted_correctly() {
        let _g = super::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        assert_eq!(held_threads(), 0);

        let ticket = reserve_within(500, 1000);
        assert_eq!(outstanding(), 500);
        assert_eq!(held_threads(), 1);

        // Hand it to another thread and let that thread drop it.
        std::thread::spawn(move || drop(ticket)).join().unwrap();

        assert_eq!(outstanding(), 0, "global total must return to zero");
        assert_eq!(
            held_threads(),
            0,
            "the builder thread must no longer be marked as holding — otherwise \
             it stays re-entrant forever and the gate stops rationing"
        );

        // Prove the gate still works for this thread afterwards.
        let a = reserve_within(600, 1000);
        let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = std::sync::Arc::clone(&blocked);
        let h = std::thread::spawn(move || {
            let t = reserve_within(600, 1000);
            f.store(true, std::sync::atomic::Ordering::SeqCst);
            t
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !blocked.load(std::sync::atomic::Ordering::SeqCst),
            "gate was disabled by the cross-thread drop"
        );
        drop(a);
        drop(h.join().unwrap());
        assert_eq!(outstanding(), 0);
    }

    /// The learned ceiling is what makes the fallback cost one refusal per
    /// batch instead of one per file. It has to be inclusive at the boundary
    /// (a demand equal to what was refused is not going to fit either) and it
    /// has to survive being told about a LARGER refusal afterwards.
    #[test]
    fn learned_oom_floor_is_inclusive_and_keeps_the_smallest_refusal() {
        // A reservation left live by a test running alongside would turn
        // every refusal below into a lowered budget instead of a ceiling.
        let _g = super::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        reset_oom_floor();
        assert!(!known_to_fail(u64::MAX - 1), "nothing refused yet");

        note_oom(1_000);
        assert!(known_to_fail(1_000), "the refused size itself must not be retried");
        assert!(known_to_fail(1_001), "anything larger must not be retried");
        assert!(!known_to_fail(999), "smaller demands are still worth trying");

        // A later, larger refusal must not raise the ceiling back up.
        note_oom(5_000);
        assert!(!known_to_fail(999), "the floor stays at the smallest refusal");
        assert!(known_to_fail(1_000));

        note_oom(400);
        assert!(known_to_fail(400));
        assert!(!known_to_fail(399));

        reset_oom_floor();
        assert!(!known_to_fail(1_000), "batch start forgets the ceiling");
    }

    /// Refused while another worker's convolver was on the card, a demand says
    /// nothing about whether one that size fits alone. It must lower how much
    /// is admitted at once and leave the size free to try — otherwise one
    /// crowded moment sends every later convolver of that size to the CPU.
    #[test]
    fn a_refusal_next_to_other_workers_lowers_the_budget_not_the_size() {
        let _g = super::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(outstanding(), 0);
        reset_oom_floor();

        let theirs = std::thread::spawn(|| reserve_within(700, u64::MAX))
            .join()
            .unwrap();
        let mine = reserve_within(700, u64::MAX);
        note_oom(700);
        assert!(!known_to_fail(700), "the size must stay allowed");
        assert_eq!(
            *BUDGET.lock().unwrap_or_else(|e| e.into_inner()),
            Some(700),
            "the budget drops to what the other worker held"
        );

        drop(theirs);
        note_oom(700);
        assert!(known_to_fail(700), "refused alone, it is a ceiling");

        drop(mine);
        reset_oom_floor();
        assert_eq!(*BUDGET.lock().unwrap_or_else(|e| e.into_inner()), None);
        assert!(!known_to_fail(700));
    }

}
