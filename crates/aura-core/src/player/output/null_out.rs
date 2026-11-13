//! `AURA_NULL_OUTPUT=1` (diagnostics): the stream never opens the device; a
//! clock takes its periods at the stream's rate, so playback runs its whole
//! load at any rate the device itself would refuse, in silence.

use super::*;
use std::time::{Duration, Instant};

/// True when the output streams are clocked in-process instead of opened.
pub(super) fn enabled() -> bool {
    std::env::var("AURA_NULL_OUTPUT").map(|v| v == "1").unwrap_or(false)
}

pub(super) struct NullOut;

impl Backend for NullOut {
    type Stream = NullStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        Active::list_devices()
    }

    fn probe_formats(_device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        Ok(rates.iter().map(|&r| (r, "float32".to_string())).collect())
    }

    fn diagnose(_device_id: Option<&str>, _rates: &[u32]) -> Result<String, String> {
        Ok("null output: every rate".to_string())
    }

    fn open(cfg: &OutputConfig) -> Result<Opened<NullStream>, String> {
        // A 20 ms buffer taken in 5 ms periods, like an exclusive stream.
        let buffer = (cfg.rate / 50).max(64);
        crate::aelog!("[PLAYER] stream open (null): rate={} buffer={buffer} - AURA_NULL_OUTPUT=1, no device", cfg.rate);
        // Named after the device it stands in for, so whatever checks the device still sees it.
        let name = Active::list_devices().ok().and_then(|l| {
            l.into_iter().find(|d| Some(&d.id) == cfg.device_id.as_ref() || (cfg.device_id.is_none() && d.is_default))
        });
        let info = StreamInfo {
            device_name: format!("{} [null output]", name.map(|d| d.name).unwrap_or_default()),
            exclusive: true,
            format: "float32 (null)".to_string(),
            rate: cfg.rate,
            period_frames: buffer / 4,
            buffer_frames: buffer,
            latency_frames: buffer,
        };
        let stream = NullStream { rate: cfg.rate as f64, buffer: buffer as f64, filled: 0.0, last: Instant::now() };
        Ok(Opened { stream, fmt: SampleFmt::Float32, blockalign: 8, info })
    }
}

pub(super) struct NullStream {
    rate: f64,
    buffer: f64,
    /// Frames handed over and not yet "played" by the clock.
    filled: f64,
    last: Instant,
}

impl NullStream {
    fn drain(&mut self) {
        let now = Instant::now();
        self.filled = (self.filled - now.duration_since(self.last).as_secs_f64() * self.rate).max(0.0);
        self.last = now;
    }
}

impl Stream for NullStream {
    type Dev<'a> = &'a mut NullStream;

    fn prefill(&mut self, frames: usize, _bytes: &[u8]) -> Result<(), String> {
        self.filled = frames as f64;
        self.last = Instant::now();
        Ok(())
    }

    fn start<'a>(&'a mut self, _shared: &'a OutputShared) -> Result<&'a mut NullStream, String> {
        self.last = Instant::now();
        Ok(self)
    }

    fn stop(&mut self) {}
}

impl Device for &mut NullStream {
    fn wait(&mut self) -> bool {
        std::thread::sleep(Duration::from_millis(5));
        true
    }

    fn space(&mut self) -> Result<usize, String> {
        self.drain();
        Ok((self.buffer - self.filled).max(0.0) as usize)
    }

    fn write(&mut self, frames: usize, _bytes: &[u8]) -> Result<(), String> {
        self.filled += frames as f64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_takes_frames_at_the_rate() {
        let mut s = NullStream { rate: 100_000.0, buffer: 2000.0, filled: 0.0, last: Instant::now() };
        s.prefill(2000, &[]).unwrap();
        let mut d = &mut s;
        assert!(d.space().unwrap() <= 100, "a full buffer has no room yet");
        d.write(0, &[]).unwrap();
        d.last -= Duration::from_millis(10); // 10 ms pass on the clock
        let room = d.space().unwrap();
        assert!((900..=2000).contains(&room), "10 ms at 100 kHz frees ~1000 frames, got {room}");
        d.write(room, &[]).unwrap();
        assert!(d.space().unwrap() < 200);
    }
}
