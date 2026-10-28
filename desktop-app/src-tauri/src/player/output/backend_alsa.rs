//! Linux: ALSA, straight to the card's `hw:` device — no dmix, no plug, no
//! sound server in the path — so the stream reaches the DAC bit for bit at
//! exactly its rate, or does not open (`alsa_logic::choose`). Periods of
//! ~10 ms in a three-period buffer, as WASAPI exclusive runs.

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};

use super::alsa_logic::*;
use super::*;

pub(super) struct AlsaOut;

/// The PCMs closing now (`Closing`): an open of a card whose close hangs in
/// the kernel would hang as well.
static CLOSING: Closing = Closing::new();

/// `d`, unless its last stream is still closing (`Closing`).
fn not_stuck(d: &HwDevice) -> Result<(), String> {
    CLOSING.wait_clear(&d.id, CLOSE_WAIT).map_err(|since| stuck_message(&d.name, since))
}

/// A playback device of a card.
struct HwDevice {
    id: String,
    name: String,
    card: i32,
    dev: u32,
}

fn hw_devices() -> Vec<HwDevice> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new().flatten() {
        let Ok(ctl) = alsa::Ctl::from_card(&card, false) else { continue };
        let Ok(info) = ctl.card_info() else { continue };
        let (Ok(card_id), Ok(card_name)) = (info.get_id(), info.get_name()) else { continue };
        let pcms: Vec<(u32, String)> = alsa::ctl::DeviceIter::new(&ctl)
            .filter_map(|dev| {
                let dev = dev as u32;
                let pi = ctl.pcm_info(dev, 0, Direction::Playback).ok()?;
                Some((dev, pi.get_name().unwrap_or("").to_string()))
            })
            .collect();
        for (dev, pcm_name) in &pcms {
            out.push(HwDevice {
                id: hw_id(card_id, *dev),
                name: device_name(card_name, pcm_name, pcms.len()),
                card: card.get_index(),
                dev: *dev,
            });
        }
    }
    out
}

/// The device `id` names (None: the first card's first), or why not.
fn find(id: Option<&str>) -> Result<HwDevice, String> {
    let mut all = hw_devices();
    match id {
        None if all.is_empty() => Err("No sound card found".to_string()),
        None => Ok(all.remove(0)),
        Some(id) => all.into_iter().find(|d| d.id == id).ok_or_else(|| format!("{id}: the device is not connected")),
    }
}

/// The program holding a busy device, from `/proc/asound`.
fn holder(d: &HwDevice) -> Option<(u32, String)> {
    let dir = std::fs::read_dir(format!("/proc/asound/card{}/pcm{}p", d.card, d.dev)).ok()?;
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("sub"))
        .find_map(|e| owner_pid(&std::fs::read_to_string(e.path().join("status")).ok()?))
        .map(|pid| {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            (pid, comm.trim().to_string())
        })
}

/// Open the device's PCM for playback; refused, in the listener's words.
/// Non-blocking: a blocking open of a busy `hw:` device waits until its
/// holder lets it go; the loop waits on the device itself (`AlsaDev`).
fn open_pcm(d: &HwDevice) -> Result<PCM, String> {
    not_stuck(d)?;
    PCM::new(&d.id, Direction::Playback, true).map_err(|e| match e.errno() {
        errno::EBUSY => busy_message(&d.name, holder(d).as_ref().map(|(p, c)| (*p, c.as_str()))),
        n if fault(n) == Fault::Lost || n == libc::ENOENT => format!("{}: the device is not connected", d.name),
        n => format!("{}: cannot open {} ({})", d.name, d.id, errno_text(n)),
    })
}

fn format_of(f: AlsaFmt) -> Format {
    match f {
        AlsaFmt::S16 => Format::S16LE,
        AlsaFmt::S24_3 => Format::S243LE,
        AlsaFmt::S32 => Format::S32LE,
        AlsaFmt::Float => Format::FloatLE,
    }
}

/// Hardware parameters for stereo `fmt` at exactly `rate`, nothing
/// converted; not yet applied.
fn hw_params<'a>(pcm: &'a PCM, fmt: AlsaFmt, rate: u32) -> alsa::Result<HwParams<'a>> {
    let hwp = HwParams::any(pcm)?;
    hwp.set_rate_resample(false)?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_channels(2)?;
    hwp.set_format(format_of(fmt))?;
    hwp.set_rate(rate, ValueOr::Nearest)?;
    Ok(hwp)
}

struct Caps<'a>(&'a PCM);

impl HwCaps for Caps<'_> {
    fn accepts(&self, fmt: AlsaFmt, rate: u32) -> bool {
        hw_params(self.0, fmt, rate).is_ok_and(|h| h.get_rate().ok() == Some(rate))
    }
}

impl Backend for AlsaOut {
    type Stream = AlsaStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        Ok(hw_devices()
            .into_iter()
            .enumerate()
            .map(|(i, d)| DeviceInfo { id: d.id, name: d.name, is_default: i == 0 })
            .collect())
    }

    fn probe_formats(device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        let d = find(device_id)?;
        let pcm = open_pcm(&d)?;
        let caps = Caps(&pcm);
        Ok(rates
            .iter()
            .map(|&r| {
                let f = dsp_formats().iter().find(|&&f| caps.accepts(alsa_fmt(f), r));
                (r, f.map_or_else(|| "unsupported".to_string(), |f| f.label()))
            })
            .collect())
    }

    fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        use std::fmt::Write;
        let mut s = String::new();
        let d = find(device_id)?;
        let _ = writeln!(s, "probing {:?} ({}), stereo, no resampling:", d.name, d.id);
        let pcm = match open_pcm(&d) {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(s, "  {e}");
                return Ok(s);
            }
        };
        let caps = Caps(&pcm);
        let rates = if rates.is_empty() { &COMMON_RATES[..] } else { rates };
        for &r in rates {
            let fmts: Vec<&str> = [AlsaFmt::S32, AlsaFmt::S24_3, AlsaFmt::S16, AlsaFmt::Float]
                .into_iter()
                .filter(|&f| caps.accepts(f, r))
                .map(|f| f.name())
                .collect();
            let took = if fmts.is_empty() { "refused".to_string() } else { fmts.join(", ") };
            let _ = writeln!(s, "  {r:>8} Hz: {took}");
        }
        Ok(s)
    }

    fn open(cfg: &OutputConfig) -> Result<Opened<AlsaStream>, String> {
        let d = find(cfg.device_id.as_deref())?;
        let pcm = open_pcm(&d)?;
        let spec = choose(&cfg.mode, cfg.rate, &Caps(&pcm), &d.name)?;
        let fmt = alsa_fmt(spec);
        let rate = cfg.rate;
        let failed = |e: alsa::Error| format!("{}: {} at {rate} Hz refused ({})", d.name, fmt.name(), errno_text(e.errno()));
        let (period, buffer) = {
            let hwp = hw_params(&pcm, fmt, rate).map_err(failed)?;
            hwp.set_period_size_near(period_frames(rate) as alsa::pcm::Frames, ValueOr::Nearest).map_err(failed)?;
            hwp.set_buffer_size_near((period_frames(rate) * PERIODS) as alsa::pcm::Frames).map_err(failed)?;
            pcm.hw_params(&hwp).map_err(failed)?;
            let cur = pcm.hw_params_current().map_err(failed)?;
            if cur.get_rate().map_err(failed)? != rate {
                return Err(format!("{}: the device moved {rate} Hz to {} Hz - refused", d.name, cur.get_rate().unwrap_or(0)));
            }
            (cur.get_period_size().map_err(failed)? as usize, cur.get_buffer_size().map_err(failed)? as usize)
        };
        {
            let (avail_min, start) = thresholds(period, buffer);
            let swp = pcm.sw_params_current().map_err(failed)?;
            swp.set_avail_min(avail_min as alsa::pcm::Frames).map_err(failed)?;
            swp.set_start_threshold(start as alsa::pcm::Frames).map_err(failed)?;
            pcm.sw_params(&swp).map_err(failed)?;
        }
        let packer = spec.packer();
        let ms = |f: usize| f as f64 * 1000.0 / rate as f64;
        crate::aelog!(
            "[PLAYER] stream open: device={:?} alsa={} rate={rate} ch=2 format={} bits={} valid={} \
             blockalign={} packer={packer:?} mode={} period={period} frames ({:.2} ms) buffer={buffer} frames ({:.2} ms)",
            d.name,
            d.id,
            fmt.name(),
            spec.store,
            spec.valid,
            fmt.frame_bytes(),
            mode_label(&cfg.mode),
            ms(period),
            ms(buffer)
        );
        let info = StreamInfo {
            device_name: d.name.clone(),
            exclusive: true,
            format: spec.label(),
            rate,
            period_frames: period as u32,
            buffer_frames: period as u32,
            latency_frames: buffer as u32,
        };
        let blockalign = fmt.frame_bytes();
        Ok(Opened { stream: AlsaStream { pcm: Some(pcm), id: d.id, chunk: period, blockalign }, fmt: packer, blockalign, info })
    }
}

/// An opened `hw:` PCM (non-blocking), prepared, not started.
pub(super) struct AlsaStream {
    /// Taken only by the close (`Drop`).
    pcm: Option<PCM>,
    id: String,
    chunk: usize,
    blockalign: usize,
}

impl AlsaStream {
    fn pcm(&self) -> &PCM {
        self.pcm.as_ref().expect("open until dropped")
    }
}

impl Drop for AlsaStream {
    /// The close, noted while it runs (`CLOSING`): it can hang in the kernel
    /// for good, on the audio thread, which the stop then leaves behind
    /// (`OutputStream::stop`).
    fn drop(&mut self) {
        CLOSING.begin(&self.id);
        drop(self.pcm.take());
        CLOSING.end(&self.id);
    }
}

/// The PCM under the render loop (`AlsaDev`).
pub(super) struct Pcm<'a>(&'a PCM);

impl PcmIo for Pcm<'_> {
    fn wait(&mut self, ms: u32) -> Result<bool, i32> {
        self.0.wait(Some(ms)).map_err(|e| e.errno())
    }

    fn avail(&mut self) -> Result<usize, i32> {
        self.0.avail_update().map(|n| n.max(0) as usize).map_err(|e| e.errno())
    }

    fn writei(&mut self, bytes: &[u8]) -> Result<usize, i32> {
        self.0.io_bytes().writei(bytes).map_err(|e| e.errno())
    }

    fn prepare(&mut self) -> Result<(), i32> {
        self.0.prepare().map_err(|e| e.errno())
    }

    fn resume(&mut self) -> Result<(), i32> {
        self.0.resume().map_err(|e| e.errno())
    }

    fn start(&mut self) -> Result<(), i32> {
        self.0.start().map_err(|e| e.errno())
    }
}

impl Stream for AlsaStream {
    type Dev<'a> = AlsaDev<Pcm<'a>>;

    fn prefill(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        let mut dev = AlsaDev::new(Pcm(self.pcm()), self.chunk, self.blockalign);
        dev.write(frames, bytes)
    }

    fn start<'a>(&'a mut self, _shared: &'a OutputShared) -> Result<AlsaDev<Pcm<'a>>, String> {
        let mut pcm = Pcm(self.pcm());
        if self.pcm().state() == State::Prepared {
            pcm.start().map_err(|e| format!("start: {}", errno_text(e)))?;
        }
        Ok(AlsaDev::new(pcm, self.chunk, self.blockalign))
    }

    fn stop(&mut self) {
        // The close (`period::close`) has handed it silence already.
        let _ = self.pcm().drop();
    }
}
