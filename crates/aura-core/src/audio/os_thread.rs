//! The calling thread's scheduling priority, in one place for every system.
//!
//! The scale is Windows' (`SetThreadPriority`): −1 below normal, 0 normal,
//! 1 above normal, 2 highest, 15 time-critical. On Windows these are the
//! same calls with the same values the call sites made themselves; elsewhere
//! they do nothing yet (the sound backends there come with their own
//! scheduling), and `priority` reads 0.

pub const BELOW_NORMAL: i32 = -1;
pub const NORMAL: i32 = 0;
pub const HIGHEST: i32 = 2;
#[cfg(test)] // the input's bench only
pub const TIME_CRITICAL: i32 = 15;

/// The calling thread's priority (0 where not known).
pub fn priority() -> i32 {
    #[cfg(windows)]
    // SAFETY: this thread's own pseudo-handle.
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, GetThreadPriority};
        GetThreadPriority(GetCurrentThread())
    }
    #[cfg(not(windows))]
    NORMAL
}

/// Set the calling thread's priority (best effort: a refusal is ignored).
pub fn set_priority(p: i32) {
    #[cfg(windows)]
    // SAFETY: this thread's own pseudo-handle.
    unsafe {
        use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
        SetThreadPriority(GetCurrentThread(), p);
    }
    #[cfg(not(windows))]
    let _ = p;
}

/// The calling thread's MMCSS "Pro Audio" registration (Windows; null when
/// refused), handed back to `leave_pro_audio`.
#[cfg(windows)]
pub type ProAudio = winapi::um::winnt::HANDLE;

/// Register the calling thread with MMCSS as "Pro Audio".
#[cfg(windows)]
pub fn enter_pro_audio() -> ProAudio {
    let name_w: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_index: u32 = 0;
    // SAFETY: a NUL-terminated task name and a valid out pointer.
    unsafe { winapi::um::avrt::AvSetMmThreadCharacteristicsW(name_w.as_ptr(), &mut task_index) }
}

/// Undo `enter_pro_audio` (nothing to undo when it was refused).
#[cfg(windows)]
pub fn leave_pro_audio(handle: ProAudio) {
    if !handle.is_null() {
        // SAFETY: a handle `enter_pro_audio` returned on this thread.
        unsafe { winapi::um::avrt::AvRevertMmThreadCharacteristics(handle); }
    }
}

/// Linux: SCHED_FIFO directly or through rtkit, else a lower nice value
/// (`rt_linux`), handed back to `leave_pro_audio`.
#[cfg(target_os = "linux")]
pub type ProAudio = super::rt_linux::Rt;

/// Raise the calling thread for audio (`rt_linux::enter`).
#[cfg(target_os = "linux")]
pub fn enter_pro_audio() -> ProAudio {
    super::rt_linux::enter()
}

/// Undo `enter_pro_audio`.
#[cfg(target_os = "linux")]
pub fn leave_pro_audio(handle: ProAudio) {
    super::rt_linux::leave(handle)
}

/// macOS: whether the calling thread runs under the time-constraint
/// (real-time) policy `enter_pro_audio` asked for.
#[cfg(target_os = "macos")]
pub type ProAudio = bool;

/// macOS: the calling thread under the time-constraint policy, a 10 ms
/// period (the output's IO buffer is ~10 ms) of which it computes 5 ms,
/// within 7.5 ms. A refusal is logged and the thread stays as it was.
#[cfg(target_os = "macos")]
pub fn enter_pro_audio() -> ProAudio {
    // <mach/mach_time.h> (libc's binding is deprecated in favour of a crate
    // the app does not otherwise need).
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    let mut tb = Timebase { numer: 0, denom: 0 };
    // SAFETY: a valid out pointer; this thread's own port.
    let ok = unsafe {
        mach_timebase_info(&mut tb) == 0 && {
            let (period, computation, constraint) = time_constraint_ticks(PRO_AUDIO_PERIOD_NS, tb.numer, tb.denom);
            let mut policy = libc::thread_time_constraint_policy { period, computation, constraint, preemptible: 1 };
            libc::thread_policy_set(
                libc::pthread_mach_thread_np(libc::pthread_self()),
                libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
                &mut policy as *mut _ as libc::thread_policy_t,
                libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            ) == libc::KERN_SUCCESS
        }
    };
    if !ok {
        crate::aelog!("[OUTPUT] real-time thread policy refused; the audio thread runs at normal priority");
    }
    ok
}

/// macOS: the calling thread back under the standard policy.
#[cfg(target_os = "macos")]
pub fn leave_pro_audio(entered: ProAudio) {
    if !entered {
        return;
    }
    let mut policy = libc::thread_standard_policy { no_data: 0 };
    // SAFETY: this thread's own port; the standard policy takes no data.
    unsafe {
        libc::thread_policy_set(
            libc::pthread_mach_thread_np(libc::pthread_self()),
            libc::THREAD_STANDARD_POLICY as libc::thread_policy_flavor_t,
            &mut policy as *mut _ as libc::thread_policy_t,
            libc::THREAD_STANDARD_POLICY_COUNT as libc::mach_msg_type_number_t,
        );
    }
}

/// The time-constraint period the audio thread asks for on macOS.
#[cfg(any(target_os = "macos", test))]
const PRO_AUDIO_PERIOD_NS: f64 = 10e6;

/// A time-constraint policy for a period of `ns` in mach ticks
/// (`numer`/`denom` from mach_timebase_info): (period, computation — half
/// of it, 50 ms at most —, constraint — three quarters).
#[cfg(any(target_os = "macos", test))]
fn time_constraint_ticks(ns: f64, numer: u32, denom: u32) -> (u32, u32, u32) {
    let tick = |ns: f64| (ns * denom as f64 / numer.max(1) as f64).round() as u32;
    let computation = tick((ns * 0.5).min(50e6)).max(1);
    (tick(ns), computation, tick(ns * 0.75).max(computation))
}

/// Elsewhere the audio thread's real-time scheduling, once its system has
/// one here (nothing yet).
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub type ProAudio = ();

/// The audio thread's real-time scheduling (nothing yet on this system).
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn enter_pro_audio() -> ProAudio {}

/// Undo `enter_pro_audio`.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn leave_pro_audio(_handle: ProAudio) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_reads_back_the_priority_it_set() {
        std::thread::spawn(|| {
            set_priority(BELOW_NORMAL);
            let expect = if cfg!(windows) { BELOW_NORMAL } else { NORMAL };
            assert_eq!(priority(), expect);
            set_priority(NORMAL);
            assert_eq!(priority(), NORMAL);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_time_constraint_is_the_period_in_ticks() {
        assert_eq!(time_constraint_ticks(PRO_AUDIO_PERIOD_NS, 1, 1), (10_000_000, 5_000_000, 7_500_000));
        // Apple silicon: 125/3 ns a tick.
        assert_eq!(time_constraint_ticks(PRO_AUDIO_PERIOD_NS, 125, 3), (240_000, 120_000, 180_000));
        // A long period computes 50 ms at most.
        assert_eq!(time_constraint_ticks(1e9, 1, 1).1, 50_000_000);
    }
}
