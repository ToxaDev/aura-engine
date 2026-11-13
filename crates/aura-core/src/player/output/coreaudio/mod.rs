//! Output on macOS: CoreAudio's HAL, the device in hog mode at the stream's
//! exact rate with an integer physical format (`plan`), the periods the
//! render loop writes (`period::run`) handed to the IOProc through a ring
//! (`ring`), the device given back as it was when the stream stops.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

pub(super) mod plan;
pub(super) mod ring;
#[cfg(target_os = "macos")]
pub(super) mod hal;

use super::period::SampleFmt;
use plan::Kind;

/// The packer's layout for what the IOProc is handed.
pub(super) fn sample_fmt(k: Kind) -> SampleFmt {
    match k {
        Kind::Int16 => SampleFmt::Int16,
        Kind::Int24Packed => SampleFmt::Int24Packed,
        Kind::Int24In32 | Kind::Int32 => SampleFmt::Int32,
        Kind::Float32 => SampleFmt::Float32,
    }
}

#[cfg(target_os = "macos")]
pub(super) use mac::*;

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::hal::CoreAudio;
    use super::plan::{self, Hal, Leftovers, Want};
    use super::super::DeviceInfo;

    /// Nominal rates to set back once no stream follows within the grace.
    static LEFTOVERS: Mutex<Leftovers> = Mutex::new(Leftovers::new(Duration::from_secs(3)));
    static RESTORER: AtomicBool = AtomicBool::new(false);

    /// The rate waiting to be set back on `dev`, taken by a stream opening
    /// on it (the rate to set back after it is the one before them both).
    pub fn take_leftover(dev: plan::Id) -> Option<f64> {
        LEFTOVERS.lock().unwrap().take(dev)
    }

    /// Set `dev` back to `rate` once no stream follows within the grace.
    pub fn set_back_later(dev: plan::Id, rate: f64) {
        LEFTOVERS.lock().unwrap().put(dev, rate, Instant::now());
        if RESTORER.swap(true, Ordering::AcqRel) {
            return;
        }
        std::thread::Builder::new()
            .name("aura-ca-restore".into())
            .spawn(|| loop {
                let next = LEFTOVERS.lock().unwrap().next_due();
                let Some(at) = next else {
                    RESTORER.store(false, Ordering::Release);
                    // One put between the look and the flag: go on.
                    if LEFTOVERS.lock().unwrap().next_due().is_none() || RESTORER.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    continue;
                };
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                let due = LEFTOVERS.lock().unwrap().due(Instant::now());
                set_back(due);
            })
            .ok();
    }

    fn set_back(rates: Vec<(plan::Id, f64)>) {
        let mut h = CoreAudio;
        for (dev, rate) in rates {
            match h.set_nominal_rate(dev, rate) {
                Ok(()) => crate::aelog!("[OUTPUT] CoreAudio: device {dev} set back to {rate} Hz"),
                Err(c) => crate::aelog!("[OUTPUT] CoreAudio: device {dev} not set back to {rate} Hz: {}", plan::status_text(c)),
            }
        }
    }

    /// Every rate still waiting, set back now (the app closing).
    pub fn restore_now() {
        let all = LEFTOVERS.lock().unwrap().all();
        set_back(all);
    }

    pub fn list_devices_impl() -> Result<Vec<DeviceInfo>, String> {
        Ok(plan::list(&CoreAudio)?
            .into_iter()
            .map(|d| DeviceInfo { id: d.uid, name: d.name, is_default: d.is_default })
            .collect())
    }

    /// Per rate, the physical format a DSP stream would get in hog mode (or
    /// "unsupported"); nothing is changed on the device.
    pub fn probe_formats_impl(device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        let h = CoreAudio;
        let dev = plan::find(&h, device_id)?;
        let ranges = h.nominal_rates(dev).map_err(plan::status_text)?;
        let avail = h.output_stream(dev).and_then(|s| h.physical_formats(s)).map_err(plan::status_text)?;
        Ok(rates
            .iter()
            .map(|&r| {
                let label = match plan::pick_physical(&avail, r, Want::Dsp, true) {
                    Ok(f) if plan::rate_ok(&ranges, r) => plan::kind_of(&f).map_or("unsupported", |k| k.label()),
                    _ => "unsupported",
                };
                (r, label.to_string())
            })
            .collect())
    }

    /// What the device takes, for `--probe-device` and the testers' logs.
    pub fn diagnose_impl(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        let h = CoreAudio;
        let dev = plan::find(&h, device_id)?;
        let mut out = format!("Device: {} ({})\n", h.name(dev).unwrap_or_default(), h.uid(dev).unwrap_or_default());
        let ranges = h.nominal_rates(dev).unwrap_or_default();
        out += &format!("Nominal rates: {}\n", plan::rates_text(&ranges));
        out += &format!("Now: {} Hz, hog owner {}\n", h.nominal_rate(dev).unwrap_or(0.0), h.hog_owner(dev).unwrap_or(-1));
        if let Ok(stream) = h.output_stream(dev) {
            for f in h.physical_formats(stream).unwrap_or_default() {
                out += &format!(
                    "  physical {:?} {}ch {}..{} Hz{}\n",
                    plan::kind_of(&f.fmt),
                    f.fmt.channels,
                    f.min_rate,
                    f.max_rate,
                    if f.fmt.non_mixable() { " non-mixable" } else { "" }
                );
            }
        }
        for &r in rates {
            out += &format!("  {r} Hz: {}\n", if plan::rate_ok(&ranges, r) { "yes" } else { "no" });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_packer_and_frame() {
        for (k, f) in [
            (Kind::Int16, SampleFmt::Int16),
            (Kind::Int24Packed, SampleFmt::Int24Packed),
            (Kind::Int24In32, SampleFmt::Int32),
            (Kind::Int32, SampleFmt::Int32),
            (Kind::Float32, SampleFmt::Float32),
        ] {
            assert_eq!(sample_fmt(k), f);
        }
    }
}
