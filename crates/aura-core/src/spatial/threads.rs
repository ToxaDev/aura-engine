//! The threads a Hybrid-Phase lists job works with, kept out of the music's
//! way while it is made ahead (below normal priority, a quarter of the
//! cores) and given everything when the listener waits for it.
//!
//! The job's thread says which it is (`set_light`); the networks' sessions
//! opened on it then take fewer threads, and every thread they or the light
//! pool start follows the job's priority (`set_priority`) for as long as it
//! lives — ONNX Runtime's and rayon's pools are made once per session, so
//! the priority is the one thing that can still change mid-job.

use std::cell::Cell;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::audio::os_thread::{self, NORMAL};

thread_local! {
    /// None: not a lists job's thread.
    static LIGHT: Cell<Option<bool>> = const { Cell::new(None) };
}

/// The calling thread makes lists from now on: ahead (true) or waited for.
pub fn set_light(light: bool) {
    LIGHT.with(|l| l.set(Some(light)));
}

/// The calling thread makes lists: ahead (`Some(true)`) or waited for.
pub fn job() -> Option<bool> {
    LIGHT.with(|l| l.get())
}

/// How many threads a job made ahead takes: a quarter of the cores.
pub fn light_threads() -> usize {
    if let Some(n) = Knobs::get().threads {
        return n;
    }
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (n / 4).max(1)
}

/// Diagnostics for lost window frames while lists are made (HL57); unset,
/// nothing changes. `AURA_HPNET_PRIO`: every job thread at this priority
/// (-15 idle … 0 normal); `AURA_HPNET_THREADS`: this many threads, waited
/// for or not (the light pool); `AURA_HPNET_GAP_MS`: a pause between the
/// networks' segments.
#[derive(Clone, Copy, Debug, Default)]
pub struct Knobs {
    pub prio: Option<i32>,
    pub threads: Option<usize>,
    pub gap_ms: u64,
}

impl Knobs {
    pub fn get() -> Knobs {
        static K: OnceLock<Knobs> = OnceLock::new();
        *K.get_or_init(|| {
            let v = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<i64>().ok());
            let k = Knobs {
                prio: v("AURA_HPNET_PRIO").map(|p| p as i32),
                threads: v("AURA_HPNET_THREADS").filter(|&n| n > 0).map(|n| n as usize),
                gap_ms: v("AURA_HPNET_GAP_MS").filter(|&n| n > 0).unwrap_or(0) as u64,
            };
            if k.prio.is_some() || k.threads.is_some() || k.gap_ms > 0 {
                crate::aelog!("[HP-NET] knobs {k:?}");
            }
            k
        })
    }
}

/// The job's threads' priority now; new ones start at it.
static PRIORITY: AtomicI32 = AtomicI32::new(NORMAL);
/// The job's threads alive (system ids).
static ALIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn this_id() -> u32 {
    #[cfg(windows)]
    // SAFETY: no arguments, the calling thread's id.
    unsafe {
        winapi::um::processthreadsapi::GetCurrentThreadId()
    }
    #[cfg(not(windows))]
    0
}

/// The calling thread joins the job's threads (at their priority).
fn enter() {
    let mut alive = ALIVE.lock().unwrap();
    os_thread::set_priority(PRIORITY.load(Ordering::Acquire));
    alive.push(this_id());
}

fn leave() {
    let id = this_id();
    ALIVE.lock().unwrap().retain(|&t| t != id);
}

/// Every thread of the job's pools to priority `p` (best effort).
pub fn set_priority(p: i32) {
    let alive = ALIVE.lock().unwrap();
    if PRIORITY.swap(p, Ordering::AcqRel) == p {
        return;
    }
    #[cfg(windows)]
    for &id in alive.iter() {
        use winapi::um::{handleapi::CloseHandle, processthreadsapi::{OpenThread, SetThreadPriority}, winnt::THREAD_SET_INFORMATION};
        // SAFETY: a thread of ours, alive while ALIVE is held (it leaves
        // through the same lock); the handle is closed here.
        unsafe {
            let h = OpenThread(THREAD_SET_INFORMATION, 0, id);
            if !h.is_null() {
                SetThreadPriority(h, p);
                CloseHandle(h);
            }
        }
    }
    #[cfg(not(windows))]
    drop(alive);
}

/// ONNX Runtime's pool threads for a session opened by a lists job.
pub struct Pool;

impl ort::environment::ThreadManager for Pool {
    type Thread = std::thread::JoinHandle<()>;

    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        std::thread::Builder::new()
            .name("hp-net-ort".into())
            .spawn(move || {
                enter();
                work();
                leave();
            })
            .map_err(|e| ort::Error::new(e.to_string()))
    }

    fn join(thread: Self::Thread) -> ort::Result<()> {
        let _ = thread.join();
        Ok(())
    }
}

fn light_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(light_threads())
            .thread_name(|i| format!("hp-net-{i}"))
            .start_handler(|_| enter())
            .exit_handler(|_| leave())
            .build()
            .ok()
    })
    .as_ref()
}

/// `f`'s parallel work: on the light pool when the calling thread's work is
/// made ahead, else on the shared one.
pub fn par<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    match (job() == Some(true)).then(light_pool).flatten() {
        Some(p) => p.install(f),
        None => f(),
    }
}
