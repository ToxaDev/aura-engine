//! The lamp on HP and αHP for a converted file: the blend is in the file's
//! samples, so nothing notes it as it plays. Its meter (`PhaseMeter::for_file`)
//! is given the whole file's timeline instead, worked out here in the
//! background from the file's source — its onset envelope, the plan the
//! converter made of it (`phase_meter::file_timeline`) — while the file
//! plays as it is. The sound is not touched: the disk chain reads the file,
//! the meter only rides along.
//!
//! One job at a time, on its own thread. A file asked for while another is
//! worked out stops that one: the track that comes on is the one to light.
//! Finished timelines are kept for a few files, so a seek or a replay finds
//! its meter again instead of working it out anew.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::blend::Envelope;
use super::phase_meter::{file_timeline, BlendKind, PhaseMeter, PlanState};

/// Meters kept for a seek or a replay (a timeline is a few hundred KB).
const KEEP: usize = 4;

/// A file's timeline: the source it was converted from, the file, its rate,
/// the Hybrid-Phase sensitivity step it was converted with (`file_blend`) —
/// files at different steps light differently, so they do not share a meter.
pub type Key = (String, String, u32, i8);

/// The source's onset envelope, made on the job's thread; stops early when
/// the flag goes up. Err: why there is none (`NoSource`, `LowMemory`,
/// `Converting`, `Failed`).
pub type EnvelopeFn = Box<dyn FnOnce(&AtomicBool) -> Result<Arc<Envelope>, PlanState> + Send>;

struct Job {
    meter: Arc<PhaseMeter>,
    key: Key,
    rate: u32,
    total: usize,
    envelope: EnvelopeFn,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    /// The meters made, the latest asked for last.
    kept: VecDeque<(Key, Arc<PhaseMeter>)>,
    /// The job being worked out and its stop flag.
    running: Option<(Key, Arc<AtomicBool>)>,
    /// The one after it.
    queued: Option<Job>,
}

static INNER: Mutex<Inner> = Mutex::new(Inner { kept: VecDeque::new(), running: None, queued: None });

fn inner() -> std::sync::MutexGuard<'static, Inner> {
    INNER.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lamp's meter for a converted file of blend `kind`, `total` frames at
/// `rate`: the one kept under `key`, else a new one whose timeline is worked
/// out in the background from `envelope` (pending until then), at the
/// sensitivity step the key carries.
pub fn meter_for_file(kind: BlendKind, key: Key, rate: u32, total: usize, envelope: EnvelopeFn) -> Arc<PhaseMeter> {
    let mut g = inner();
    if let Some(i) = g.kept.iter().position(|(k, _)| *k == key) {
        let e = g.kept.remove(i).expect("found");
        let m = e.1.clone();
        g.kept.push_back(e);
        return m;
    }
    let meter = PhaseMeter::for_file(kind, rate as f64);
    meter.set_sens(key.3);
    g.kept.push_back((key.clone(), meter.clone()));
    while g.kept.len() > KEEP {
        g.kept.pop_front();
    }
    let job = Job { meter: meter.clone(), key, rate, total, envelope, cancel: Arc::new(AtomicBool::new(false)) };
    if let Some(q) = g.queued.take() {
        forget(&mut g, &q.meter);
    }
    if let Some((_, stop)) = g.running.as_ref() {
        stop.store(true, Ordering::Release);
        g.queued = Some(job);
        return meter;
    }
    g.running = Some((job.key.clone(), job.cancel.clone()));
    drop(g);
    let failed = meter.clone();
    if std::thread::Builder::new().name("aura-lamp-plan".into()).spawn(move || work(job)).is_err() {
        failed.set_state(PlanState::Failed);
        inner().running = None;
    }
    meter
}

/// Stop what is worked out: the track on the air is not a converted file
/// any more. Its meter is not kept (it is worked out again if it comes back).
pub fn stop() {
    let mut g = inner();
    if let Some((_, stop)) = g.running.as_ref() {
        stop.store(true, Ordering::Release);
    }
    if let Some(q) = g.queued.take() {
        forget(&mut g, &q.meter);
    }
}

/// Drop `meter` from the kept ones (by identity: a new meter may be kept
/// under the same key by now).
fn forget(g: &mut Inner, meter: &Arc<PhaseMeter>) {
    g.kept.retain(|(_, m)| !Arc::ptr_eq(m, meter));
}

/// The job thread: `job`, then whatever was queued meanwhile.
fn work(mut job: Job) {
    loop {
        run(job);
        let mut g = inner();
        match g.queued.take() {
            Some(next) => {
                g.running = Some((next.key.clone(), next.cancel.clone()));
                drop(g);
                job = next;
            }
            None => {
                g.running = None;
                return;
            }
        }
    }
}

/// A conversion is running: the source is not decoded beside it.
fn converting() -> bool {
    #[cfg(test)]
    {
        false
    }
    #[cfg(not(test))]
    {
        crate::audio::converter::manager::is_running()
    }
}

fn run(job: Job) {
    let Job { meter, key, rate, total, envelope, cancel } = job;
    let t0 = std::time::Instant::now();
    let stopped = || cancel.load(Ordering::Acquire);
    let outcome = if converting() { Err(PlanState::Converting) } else { envelope(&cancel) };
    if stopped() {
        // Pending, and not kept: asked for again, it is worked out again.
        forget(&mut inner(), &meter);
        return;
    }
    match outcome {
        Ok(env) => {
            let timeline = file_timeline(meter.kind, &env, rate as f64, total, key.3);
            if stopped() {
                forget(&mut inner(), &meter);
                return;
            }
            let lit = timeline.iter().filter(|&&w| w > 0).count();
            meter.set_plan(timeline.into());
            // The rack's background draws it under the timeline (~100 points a second).
            meter.set_envelope(env);
            crate::aelog!(
                "[PLAYER] lamp of {}: {:?} worked out from its source in {:.2} s ({} of the file's 2 ms slots lit)",
                key.1,
                meter.kind,
                t0.elapsed().as_secs_f64(),
                lit
            );
        }
        Err(why) => {
            // What may pass (a conversion, low memory, a source back in its
            // place) is tried again when the file is asked for again.
            if why != PlanState::Failed {
                forget(&mut inner(), &meter);
            }
            meter.set_state(why);
            crate::aelog!("[PLAYER] lamp of {}: no timeline ({:?})", key.1, why);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn key(name: &str) -> Key {
        (format!("{name}.wav"), format!("{name} [AE].flac"), 48_000, 0)
    }

    fn wait_for(m: &PhaseMeter, want: PlanState) {
        let t0 = Instant::now();
        while m.state() != want {
            assert!(t0.elapsed() < Duration::from_secs(5), "still {:?}, waiting for {:?}", m.state(), want);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The jobs share one thread and one store: one test walks through them.
    #[test]
    fn a_files_timeline_is_worked_out_kept_and_stopped_for_the_next() {
        // An attack at 0.5 s: HP is on across it.
        let env = || {
            let mut analysis = vec![0.0; 100];
            analysis[50] = 1.0;
            Ok(Arc::new(Envelope { analysis, analysis_sr: 100.0 }))
        };
        let a = meter_for_file(BlendKind::Hybrid, key("a"), 48_000, 48_000, Box::new(move |_| env()));
        wait_for(&a, PlanState::Ready);
        assert!(a.level(24_000 + 960, 960) > 0.99, "lit at the attack");
        assert_eq!(a.level(4_800, 960), 0.0, "dark before it");
        let again = meter_for_file(BlendKind::Hybrid, key("a"), 48_000, 48_000, Box::new(|_| Err(PlanState::Failed)));
        assert!(Arc::ptr_eq(&a, &again), "a seek finds the meter worked out");
        // The same file at another sensitivity step is another meter, on
        // its own timeline at that step.
        let a2 = meter_for_file(BlendKind::Hybrid, (key("a").0, key("a").1, 48_000, 2), 48_000, 48_000, Box::new(move |_| env()));
        assert!(!Arc::ptr_eq(&a, &a2), "a step does not share a meter");
        wait_for(&a2, PlanState::Ready);
        assert_eq!((a.sens(), a2.sens()), (0, 2));
        assert!(a2.level(24_000 + 960, 960) > 0.99, "lit at the attack");

        // No source: said so, and tried again when asked again.
        let b = meter_for_file(BlendKind::Alpha, key("b"), 48_000, 48_000, Box::new(|_| Err(PlanState::NoSource)));
        wait_for(&b, PlanState::NoSource);
        let b2 = meter_for_file(BlendKind::Alpha, key("b"), 48_000, 48_000, Box::new(|_| Err(PlanState::NoSource)));
        assert!(!Arc::ptr_eq(&b, &b2), "not kept");
        wait_for(&b2, PlanState::NoSource);

        // A file asked for while another is worked out stops that one.
        let c = meter_for_file(
            BlendKind::Hybrid,
            key("c"),
            48_000,
            48_000,
            Box::new(|stop: &AtomicBool| {
                let t0 = Instant::now();
                while !stop.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(PlanState::Failed)
            }),
        );
        let d = meter_for_file(BlendKind::Hybrid, key("d"), 48_000, 48_000, Box::new(move |_| env()));
        wait_for(&d, PlanState::Ready);
        assert_eq!(c.state(), PlanState::Pending, "stopped, not failed");
        assert!(inner().kept.iter().all(|(_, m)| !Arc::ptr_eq(m, &c)), "a stopped one is not kept");
        assert!(inner().kept.len() <= KEEP);
    }
}
