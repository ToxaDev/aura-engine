//! Linux: the audio thread's real-time scheduling, with no root needed.
//! SCHED_FIFO set directly where the user may (an `rtprio` limit, as the
//! audio group often has), else asked of rtkit over D-Bus (what desktop
//! sound servers use), else a lower nice value — and else the thread runs
//! as it is, which plays, with less margin.

use std::time::Duration;

/// The SCHED_FIFO priority asked for: above the sound server's (PipeWire
/// asks for 20 or 88), below the kernel's own threads (≥ 90).
const FIFO_PRIORITY: i32 = 70;
/// The nice value of the last resort.
const NICE: i32 = -11;

/// How the audio thread got its priority (`enter`), for `leave`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rt {
    Fifo(i32),
    Nice(i32),
    Normal,
}

fn tid() -> libc::pid_t {
    // SAFETY: gettid takes nothing and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t }
}

fn set_fifo(priority: i32) -> bool {
    let param = libc::sched_param { sched_priority: priority };
    // SAFETY: this thread, a valid sched_param.
    unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) == 0 }
}

fn set_nice(nice: i32) -> bool {
    // SAFETY: this thread's id; setpriority on a thread id sets that thread.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, tid() as libc::id_t, nice) == 0 }
}

/// The `rtprio` limit (0: SCHED_FIFO not allowed unprivileged).
fn rtprio_limit() -> i32 {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: a valid out pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut lim) } != 0 {
        return 0;
    }
    lim.rlim_cur.min(99) as i32
}

/// rtkit: SCHED_FIFO (or, refused, the nice value) for this thread.
fn rtkit() -> Result<Rt, String> {
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    const IFACE: &str = "org.freedesktop.RealtimeKit1";
    let conn = dbus::blocking::Connection::new_system().map_err(|e| format!("system bus: {e}"))?;
    let kit = conn.with_proxy(IFACE, "/org/freedesktop/RealtimeKit1", Duration::from_millis(500));
    let max: i32 = kit.get(IFACE, "MaxRealtimePriority").map_err(|e| format!("rtkit: {e}"))?;
    // rtkit grants SCHED_FIFO only to a process that limits a real-time
    // thread's CPU time without a block (it blocks every period).
    let usec: i64 = kit.get(IFACE, "RTTimeUSecMax").unwrap_or(200_000);
    let lim = libc::rlimit { rlim_cur: usec as libc::rlim_t, rlim_max: usec as libc::rlim_t };
    // SAFETY: a valid rlimit.
    unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &lim) };
    let priority = max.clamp(1, FIFO_PRIORITY);
    let fifo: Result<(), dbus::Error> = kit.method_call(IFACE, "MakeThreadRealtime", (tid() as u64, priority as u32));
    match fifo {
        Ok(()) => Ok(Rt::Fifo(priority)),
        Err(e) => {
            let min_nice: i32 = kit.get(IFACE, "MinNiceLevel").unwrap_or(NICE);
            let nice = NICE.max(min_nice);
            let high: Result<(), dbus::Error> = kit.method_call(IFACE, "MakeThreadHighPriority", (tid() as u64, nice));
            high.map(|()| Rt::Nice(nice)).map_err(|e2| format!("rtkit refused: {e}; {e2}"))
        }
    }
}

/// Raise the calling thread for audio, the best way the system allows.
pub fn enter() -> Rt {
    let lim = rtprio_limit();
    let rt = if set_fifo(FIFO_PRIORITY) {
        Ok(("the scheduler", Rt::Fifo(FIFO_PRIORITY)))
    } else if lim > 0 && set_fifo(lim.min(FIFO_PRIORITY)) {
        Ok(("the scheduler (rtprio limit)", Rt::Fifo(lim.min(FIFO_PRIORITY))))
    } else {
        match rtkit() {
            Ok(rt) => Ok(("rtkit", rt)),
            Err(e) if set_nice(NICE) => {
                crate::aelog!("[PLAYER] audio thread: no real-time scheduling ({e})");
                Ok(("setpriority", Rt::Nice(NICE)))
            }
            Err(e) => Err(e),
        }
    };
    match rt {
        Ok((how, rt)) => {
            crate::aelog!("[PLAYER] audio thread: {rt:?} via {how}");
            rt
        }
        Err(e) => {
            crate::aelog!("[PLAYER] audio thread: normal priority (no real-time scheduling: {e})");
            Rt::Normal
        }
    }
}

/// Undo `enter` (a lower priority is always allowed).
pub fn leave(rt: Rt) {
    match rt {
        Rt::Fifo(_) => {
            let param = libc::sched_param { sched_priority: 0 };
            // SAFETY: this thread, a valid sched_param.
            unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_OTHER, &param) };
        }
        Rt::Nice(_) => {
            set_nice(0);
        }
        Rt::Normal => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the machine allows, the thread comes back to normal.
    #[test]
    fn a_thread_raised_for_audio_comes_back() {
        std::thread::spawn(|| {
            let rt = enter();
            leave(rt);
            let mut policy = 0;
            let mut param = libc::sched_param { sched_priority: 0 };
            // SAFETY: this thread, valid out pointers.
            unsafe { libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param) };
            assert_eq!(policy, libc::SCHED_OTHER);
        })
        .join()
        .unwrap();
    }
}
