use lazy_static::lazy_static;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use sysinfo::{System, SystemExt};

use crate::audio::converter::decode::set_status;

lazy_static! {
    static ref GLOBAL_RAM_LOCK: Mutex<System> = Mutex::new(System::new());
}

/// Sum of all outstanding `RamReservation`s in MB.
static RESERVED_MB: AtomicU64 = AtomicU64::new(0);

/// The workers waiting in `reserve_ram`, in the order they came.
static RAM_GATE: Gate = Gate::new();

/// A first-come queue in front of an admission rule: only the first waiter
/// may be admitted, and a released ticket wakes the queue at once. Until
/// 1.9.2 the waiters polled every 1.5 s; a worker that finished a file and
/// asked again for its next one found nothing reserved and went straight
/// in, so files that could only start alone waited for the whole batch
/// (three of eight on a 32 GB machine at 1G taps, until the app was quit).
struct Gate {
    waiters: Mutex<VecDeque<u64>>,
    next: AtomicU64,
    freed: Condvar,
}

impl Gate {
    const fn new() -> Gate {
        Gate { waiters: Mutex::new(VecDeque::new()), next: AtomicU64::new(0), freed: Condvar::new() }
    }

    /// Waits for this caller's turn and `admit`. Returns true when admitted,
    /// false when `give_up` said so first (the caller leaves the queue).
    /// `waiting` runs each time a turn passes without admission.
    fn enter(
        &self,
        mut admit: impl FnMut() -> bool,
        give_up: impl Fn() -> bool,
        mut waiting: impl FnMut(),
        poll: Duration,
    ) -> bool {
        let me = self.next.fetch_add(1, Ordering::Relaxed);
        let mut q = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(me);
        loop {
            if q.front() == Some(&me) && admit() {
                q.pop_front();
                // The next in line may fit beside this one.
                self.freed.notify_all();
                return true;
            }
            if give_up() {
                q.retain(|&w| w != me);
                self.freed.notify_all();
                return false;
            }
            waiting();
            q = self.freed.wait_timeout(q, poll).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Wakes the queue: a ticket was let go.
    fn released(&self) {
        // Taken so a waiter between its check and its wait cannot miss this.
        drop(self.waiters.lock().unwrap_or_else(|e| e.into_inner()));
        self.freed.notify_all();
    }
}

/// An MB figure from the environment, read once: lets a test or a support
/// session play a smaller machine (`AURA_EMULATE_TOTAL_RAM_MB`,
/// `AURA_EMULATE_AVAIL_RAM_MB`) without one at hand.
fn emulated_mb(var: &'static str, cell: &'static std::sync::OnceLock<Option<u64>>) -> Option<u64> {
    *cell.get_or_init(|| {
        let mb = std::env::var(var).ok()?.trim().parse::<u64>().ok()?;
        crate::aelog!("[MEM] {} = {} MB — RAM figures are emulated", var, mb);
        Some(mb)
    })
}

/// Total and available RAM in MB from a freshly refreshed `sys`, after the
/// emulation overrides: an emulated total replaces the real one, an emulated
/// available figure caps the real one (it stays what the machine can give).
fn ram_of(sys: &mut System) -> (u64, u64) {
    static TOTAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    static AVAIL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    sys.refresh_memory();
    let total = sys.total_memory() / 1024 / 1024;
    let avail = sys.available_memory() / 1024 / 1024;
    let total = emulated_mb("AURA_EMULATE_TOTAL_RAM_MB", &TOTAL).unwrap_or(total);
    let avail = emulated_mb("AURA_EMULATE_AVAIL_RAM_MB", &AVAIL).map_or(avail, |e| e.min(avail));
    (total, avail.min(total))
}

/// Total physical RAM in MB (refreshes once per call; cheap).
pub fn total_ram_mb() -> u64 {
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    ram_of(&mut sys).0
}

/// Total and available RAM in MB, read together.
pub fn ram_now_mb() -> (u64, u64) {
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ram_of(&mut sys)
}

/// The most one file may be planned to hold: the installed RAM less the
/// margin the gates leave to the OS and other apps. A file estimated above
/// it is refused rather than started into swap.
pub fn ram_cap_mb() -> u64 {
    let total = total_ram_mb();
    total.saturating_sub(safety_margin_mb(total))
}

/// Memory the system has available now, in MB — read again at most once a
/// second (the player's tick asks every 200 ms).
pub fn available_ram_mb() -> u64 {
    static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, mb)) = *last {
        if at.elapsed() < Duration::from_secs(1) {
            return mb;
        }
    }
    let mb = ram_now_mb().1;
    *last = Some((std::time::Instant::now(), mb));
    mb
}

/// Log this process's own memory footprint (working set + committed
/// private bytes) plus system-wide availability into the session log.
/// Called at batch start, after every finished file and around the heavy
/// stages, so a multi-hour batch leaves a memory CURVE in the log — the
/// only way to tell "high but stable peak" apart from "monotonic leak".
pub fn log_process_memory(tag: &str) {
    use sysinfo::{PidExt, ProcessExt};
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let (ws_mb, commit_mb, avail_mb) = {
        let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
        sys.refresh_process(pid);
        let (ws, commit) = sys
            .process(pid)
            .map(|p| (p.memory() / 1024 / 1024, p.virtual_memory() / 1024 / 1024))
            .unwrap_or((0, 0));
        (ws, commit, ram_of(&mut sys).1)
    };
    crate::aelog!(
        "[MEM] {} — working set {} MB (peak {} MB) | commit {} MB | batch reserved {} MB | system avail {} MB",
        tag,
        ws_mb,
        peak_working_set_mb(),
        commit_mb,
        RESERVED_MB.load(Ordering::Acquire),
        avail_mb
    );
}

/// The most this process's working set has ever been, in MB: the samples
/// above miss a peak between two of them, this does not (0 where unknown).
pub fn peak_working_set_mb() -> u64 {
    process_memory().peak_working_set as u64 / 1024 / 1024
}

/// This process's memory in bytes; a field the system does not tell is 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessMemory {
    /// Private bytes on Windows; resident anonymous memory on Linux.
    pub private: usize,
    /// Working set on Windows; resident set elsewhere.
    pub working_set: usize,
    /// The most `working_set` has ever been.
    pub peak_working_set: usize,
}

/// `K32GetProcessMemoryInfo` on Windows; `/proc/self/status` on Linux;
/// only the peak (`getrusage`) on macOS.
pub fn process_memory() -> ProcessMemory {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Pmc {
            cb: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged: usize,
            quota_paged: usize,
            quota_peak_nonpaged: usize,
            quota_nonpaged: usize,
            pagefile: usize,
            peak_pagefile: usize,
            private: usize,
        }
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn K32GetProcessMemoryInfo(h: *mut std::ffi::c_void, p: *mut Pmc, cb: u32) -> i32;
        }
        let mut m: Pmc = unsafe { std::mem::zeroed() };
        m.cb = std::mem::size_of::<Pmc>() as u32;
        // SAFETY: a pseudo-handle of this process and a correctly sized struct.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) };
        if ok == 0 {
            ProcessMemory::default()
        } else {
            ProcessMemory { private: m.private, working_set: m.working_set, peak_working_set: m.peak_working_set }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let mut m = proc_status_memory(&status);
        if m.peak_working_set == 0 {
            m.peak_working_set = rusage_peak_bytes();
        }
        m
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        ProcessMemory { peak_working_set: rusage_peak_bytes(), ..ProcessMemory::default() }
    }
    #[cfg(not(any(windows, unix)))]
    ProcessMemory::default()
}

/// `getrusage`'s peak resident set in bytes: Linux counts `ru_maxrss` in KB,
/// macOS in bytes.
#[cfg(unix)]
fn rusage_peak_bytes() -> usize {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer for this process's own usage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let max = ru.ru_maxrss.max(0) as usize;
    if cfg!(any(target_os = "macos", target_os = "ios")) { max } else { max * 1024 }
}

/// `RssAnon`, `VmRSS` and `VmHWM` (kB) out of `/proc/<pid>/status`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_status_memory(status: &str) -> ProcessMemory {
    let kb = |key: &str| -> usize {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<usize>().ok())
            .map_or(0, |k| k * 1024)
    };
    ProcessMemory { private: kb("RssAnon"), working_set: kb("VmRSS"), peak_working_set: kb("VmHWM") }
}

/// RAII admission ticket for one file's peak RAM demand. Dropping it
/// releases the budget for the next waiting worker.
pub struct RamReservation {
    mb: u64,
}

impl RamReservation {
    /// The MB this ticket holds.
    pub fn mb(&self) -> u64 {
        self.mb
    }
}

impl Drop for RamReservation {
    fn drop(&mut self) {
        RESERVED_MB.fetch_sub(self.mb, Ordering::AcqRel);
        RAM_GATE.released();
    }
}

/// Admission control for the heavy per-file stage: blocks until the file's
/// ESTIMATED peak RAM demand fits alongside every other in-flight file.
///
/// This is what `await_free_ram*` alone cannot do: those gates check free
/// RAM at one allocation site, but a worker mid-file cannot back out — two
/// workers can both pass an early gate and then balloon together until the
/// whole system swaps. Reserving the full estimated peak UP FRONT means the
/// second worker simply doesn't start its file until the first one's
/// reservation is released.
///
/// Deadlock safety: when no reservations are outstanding the caller is
/// admitted unconditionally (a single file larger than the budget must
/// still convert — the per-allocation `await_free_ram*` gates remain the
/// backstop). A global cancel also admits immediately so cancellation is
/// never blocked by the queue.
///
/// Waiters are admitted in the order they came (`Gate`): a worker asking
/// for its next file stands behind the ones already waiting.
pub fn reserve_ram(required_mb: u64, label: &str) -> RamReservation {
    let admitted = RAM_GATE.enter(
        || {
            let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
            let outstanding = RESERVED_MB.load(Ordering::Acquire);
            let (total_mb, avail_mb) = ram_of(&mut sys);
            if ticket_admits(outstanding, required_mb, total_mb, avail_mb) {
                RESERVED_MB.fetch_add(required_mb, Ordering::AcqRel);
                true
            } else {
                false
            }
        },
        // Don't hold up cancellation — the caller re-checks the flag.
        crate::audio::converter::state::cancel_requested,
        || {
            set_status(&format!(
                "{}: waiting for RAM budget ({:.1} GB estimated peak)...",
                label,
                required_mb as f64 / 1024.0
            ))
        },
        Duration::from_millis(1500),
    );
    RamReservation { mb: if admitted { required_mb } else { 0 } }
}

/// `reserve_ram` for work that must start now, from a thread that also
/// holds tickets for work queued AFTER it (an album's tracks kept from its
/// pre-scan). When the budget does not admit it, `let_go` drops those
/// tickets first: they are released only when their own turn comes, after
/// this one, so waiting on them never ends. 1.8.3 waited, and an album on
/// an 8 GB laptop stopped for good at its first track.
pub fn reserve_ram_before_held(required_mb: u64, label: &str, let_go: impl FnOnce()) -> RamReservation {
    if let Some(ticket) = try_reserve_ram(required_mb) {
        return ticket;
    }
    let_go();
    reserve_ram(required_mb, label)
}

/// `reserve_ram` without the wait: the ticket if the budget admits it now,
/// None if it does not. For memory that is only worth holding when it is
/// free to hold — an album's tracks prepared ahead of their turn — and must
/// never be what a waiting worker waits on. Unlike `reserve_ram` it does not
/// admit unconditionally when nothing else is outstanding.
pub fn try_reserve_ram(required_mb: u64) -> Option<RamReservation> {
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    let outstanding = RESERVED_MB.load(Ordering::Acquire);
    let (total_mb, avail_mb) = ram_of(&mut sys);
    if budget_admits(outstanding, required_mb, total_mb, avail_mb) {
        RESERVED_MB.fetch_add(required_mb, Ordering::AcqRel);
        Some(RamReservation { mb: required_mb })
    } else {
        None
    }
}

/// RAM the free-memory gates leave to the OS and other apps: 2 GB on a
/// 16 GB+ machine, an eighth of the RAM below that (1 GB on 8 GB), never
/// under 512 MB. A flat 2 GB asked an 8 GB laptop with ~2 GB free for
/// 4.4 GB before the first track could even start.
pub fn safety_margin_mb(total_mb: u64) -> u64 {
    (total_mb / 8).clamp(512, 2048)
}

/// Whether one more file of `required_mb` may start next to the
/// `outstanding` reservations: all of them within the installed RAM less the
/// margin, and the new one within what is available less the margin. Until
/// 1.8.3 these were a flat 4 GB and 2 GB — on an 8 GB laptop a second file
/// could never start.
fn budget_admits(outstanding: u64, required_mb: u64, total_mb: u64, avail_mb: u64) -> bool {
    outstanding + required_mb <= total_mb.saturating_sub(safety_margin_mb(total_mb))
        && ram_fits(avail_mb, total_mb, required_mb)
}

/// Whether `reserve_ram` hands out its ticket now: unconditionally when
/// nothing is reserved (a file larger than the budget must still convert),
/// otherwise when the budget admits it.
fn ticket_admits(outstanding: u64, required_mb: u64, total_mb: u64, avail_mb: u64) -> bool {
    outstanding == 0 || budget_admits(outstanding, required_mb, total_mb, avail_mb)
}

/// How long a free-memory gate waits for other apps or workers to give
/// RAM back. Past it the caller goes ahead: on a small machine the RAM may
/// never come free, and the page file makes a slow conversion better than
/// one that waits forever. The per-file `reserve_ram` tickets still keep
/// parallel workers from ballooning together.
const LOW_RAM_MAX_WAIT: Duration = Duration::from_secs(15);

/// Whether a request fits what is available now, after the safety margin.
fn ram_fits(avail_mb: u64, total_mb: u64, required_mb: u64) -> bool {
    avail_mb.saturating_sub(safety_margin_mb(total_mb)) > required_mb
}

/// Checks the gate once under the global lock; on a fit (or once the wait
/// is over) runs `on_pass` while still holding the lock.
fn gate_once<F, R>(required_mb: u64, give_up: bool, requester: &str, on_pass: F) -> Result<R, F>
where
    F: FnOnce() -> R,
{
    let mut sys = GLOBAL_RAM_LOCK.lock().unwrap();
    // Refresh ONLY memory info to save CPU overhead
    let (total_mb, avail_mb) = ram_of(&mut sys);
    if ram_fits(avail_mb, total_mb, required_mb) {
        return Ok(on_pass());
    }
    if give_up {
        crate::aelog!(
            "[MEM] {} goes ahead with low RAM — needs ~{} MB, system avail {} MB of {} MB (the page file takes the rest)",
            requester,
            required_mb,
            avail_mb,
            total_mb
        );
        return Ok(on_pass());
    }
    set_status(&format!(
        "SYSTEM RAM LOW. {} waiting for {:.1} GB free...",
        requester,
        (required_mb + safety_margin_mb(total_mb)) as f64 / 1024.0
    ));
    Err(on_pass)
}

fn await_gate<F, R>(required_mb: u64, requester: &str, mut on_pass: F) -> R
where
    F: FnOnce() -> R,
{
    let started = std::time::Instant::now();
    loop {
        // We lock inside the check so we don't hold the lock while sleeping.
        let give_up = started.elapsed() >= LOW_RAM_MAX_WAIT
            || crate::audio::converter::state::cancel_requested();
        match gate_once(required_mb, give_up, requester, on_pass) {
            Ok(r) => return r,
            Err(f) => on_pass = f,
        }
        // Sleep outside lock to let other threads check
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// Waits until the system has enough available RAM (keeping a safety margin
/// free, see `safety_margin_mb`), at most `LOW_RAM_MAX_WAIT`.
/// Once it does, the provided `alloc_fn` is executed BEFORE the global
/// memory lock is released. This guarantees that multiple threads will not bypass the gate
/// simultaneously and allocate massive buffers before the OS has time to reflect the usage in `sysinfo`.
pub fn await_free_ram_and_allocate<F, R>(required_mb: u64, alloc_fn: F) -> R
where
    F: FnOnce() -> R,
{
    await_gate(required_mb, "GPU thread", alloc_fn)
}

/// Waits until the system has enough available RAM before returning, at
/// most `LOW_RAM_MAX_WAIT`.
/// Used for synchronous protection where we don't need to hold the lock during allocation.
pub fn await_free_ram(required_mb: u64, requester_name: &str) {
    await_gate(required_mb, requester_name, || ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker that lets its ticket go and asks again at once stands behind
    /// the one already waiting (1.9.1 let it straight back in: the waiter
    /// waited for the whole batch).
    #[test]
    fn a_worker_asking_again_does_not_pass_a_waiting_one() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let gate = Arc::new(Gate::new());
        // One ticket at a time, as two files that only fit alone.
        let held = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        fn take(held: &AtomicBool) -> impl FnMut() -> bool + '_ {
            move || !held.swap(true, Ordering::AcqRel)
        }
        let never = || false;
        let poll = Duration::from_secs(5);

        assert!(gate.enter(take(&held), never, || {}, poll), "the first worker goes in");
        let waiter = {
            let (gate, held, order) = (Arc::clone(&gate), Arc::clone(&held), Arc::clone(&order));
            std::thread::spawn(move || {
                assert!(gate.enter(take(&held), never, || {}, poll));
                order.lock().unwrap().push("waiter");
                held.store(false, Ordering::Release);
                gate.released();
            })
        };
        while gate.waiters.lock().unwrap().len() < 1 {
            std::thread::yield_now();
        }
        // The first worker's file is done; it asks for its next one at once.
        held.store(false, Ordering::Release);
        gate.released();
        assert!(gate.enter(take(&held), never, || {}, poll));
        order.lock().unwrap().push("again");
        waiter.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["waiter", "again"]);
    }

    /// A waiter told to give up leaves the queue, so the one behind it is not
    /// held up.
    #[test]
    fn a_waiter_that_gives_up_leaves_the_queue() {
        let gate = Gate::new();
        let mut asked = 0;
        let admitted = gate.enter(|| false, || true, || asked += 1, Duration::from_millis(1));
        assert!(!admitted);
        assert_eq!(asked, 0);
        assert!(gate.waiters.lock().unwrap().is_empty());
        assert!(gate.enter(|| true, || false, || {}, Duration::from_millis(1)));
    }

    #[test]
    fn linux_status_gives_private_resident_and_peak() {
        let status = "Name:\taura-engine\nVmHWM:\t  204800 kB\nVmRSS:\t  102400 kB\nRssAnon:\t   51200 kB\n";
        let m = proc_status_memory(status);
        assert_eq!(m, ProcessMemory { private: 50 << 20, working_set: 100 << 20, peak_working_set: 200 << 20 });
        assert_eq!(proc_status_memory(""), ProcessMemory::default());
    }

    #[test]
    fn this_process_has_a_peak() {
        assert!(process_memory().peak_working_set > 0);
    }

    #[test]
    fn margin_scales_with_installed_ram() {
        assert_eq!(safety_margin_mb(4 * 1024), 512);
        assert_eq!(safety_margin_mb(8 * 1024), 1024);
        assert_eq!(safety_margin_mb(16 * 1024), 2048);
        assert_eq!(safety_margin_mb(64 * 1024), 2048);
    }

    #[test]
    fn eight_gb_laptop_fits_a_normal_track() {
        // The reported case: 8 GB, ~2 GB available. Decoding a 4-minute
        // 44.1 kHz track is estimated at ~600 MB — fits; the old flat
        // 2.5 GB request did not.
        assert!(ram_fits(1990, 8 * 1024, 600));
        assert!(!ram_fits(1990, 8 * 1024, 2500));
        // The same 2 GB free on a 32 GB machine keeps the full 2 GB margin.
        assert!(!ram_fits(1990, 32 * 1024, 600));
    }

    #[test]
    fn second_file_may_start_on_an_eight_gb_laptop() {
        // 7.5 GB seen, 2.3 GB free, one 300 MB file in flight: a second
        // 300 MB file fits (the old flat 4 GB / 2 GB budget refused it).
        assert!(budget_admits(300, 300, 7680, 2300));
        assert!(!budget_admits(300, 1400, 7680, 2300));
        // The planned total stays within the installed RAM less the margin.
        assert!(!budget_admits(6500, 300, 7680, 7000));
    }

    #[test]
    fn album_track_on_an_eight_gb_laptop_gets_in_once_the_kept_tracks_go() {
        // Stefano's 1.8.3 log: 7.5 GB seen, ~2.3 GB free, the album's kept
        // tracks holding 1862 MB, the first track's preparation asking
        // 2.9 GB. The kept tracks wait for their turn after this one, so
        // nothing else ever gives the RAM back: the ticket never came.
        assert!(!ticket_admits(1862, 2900, 7680, 2300));
        // With them let go nothing is reserved, and the ticket is given.
        assert!(ticket_admits(0, 2900, 7680, 2300));
        // 1.8.4 got through the same album only because 4.4 GB was free.
        assert!(ticket_admits(1862, 2900, 7680, 4410));
    }
}
