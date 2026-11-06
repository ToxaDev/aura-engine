//! DSD: one-bit output (PCM → DSD modulation, DoP packing) and DSD input
//! (DSF/DFF reading, DSD → PCM decimation). Background and the staged plan:
//! `docs/18-dsd-research.md`.
//!
//! Bit order everywhere inside the engine is DSDIFF's: one byte holds eight
//! consecutive one-bit samples of ONE channel, and the most significant bit is
//! the earliest. DSF stores the least significant bit first and is flipped on
//! reading; DoP and native ASIO (`ASIOSTDSDInt8MSB1`) take it as is.
//!
//! A one-bit sample of 1 means +1, of 0 means −1.


pub mod decimate;
pub mod dff;
pub mod dop;
pub mod dsf;
pub mod dsf_write;
pub mod encoder;

use std::fmt;

use serde::{Deserialize, Serialize};

/// The DSD idle pattern: four ones and four zeros, an average of zero. Sent
/// as the DoP payload whenever there is no music, so a DAC stays in DSD mode
/// across pauses, seeks and starts.
pub const DSD_SILENCE: u8 = 0x69;

/// PCM full scale maps to this fraction of full one-bit modulation (the SACD
/// convention: "0 dB" is 50 % modulation, peaks may go higher). The encoder
/// applies it; callers hand over PCM as it is.
pub const PCM_FULL_SCALE_MODULATION: f64 = 0.5;

/// A DSD bit rate: a multiple of 44.1 kHz or 48 kHz.
/// DSD64 is 64 × 44.1 kHz = 2.8224 MHz per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DsdRate {
    /// 44 100 or 48 000.
    pub base: u32,
    /// 64, 128, 256, 512 or 1024.
    pub multiple: u32,
}

impl DsdRate {
    #[cfg(test)]
    pub const DSD64: DsdRate = DsdRate { base: 44_100, multiple: 64 };
    #[cfg(test)]
    pub const DSD128: DsdRate = DsdRate { base: 44_100, multiple: 128 };
    #[cfg(test)]
    pub const DSD256: DsdRate = DsdRate { base: 44_100, multiple: 256 };

    pub fn new(base: u32, multiple: u32) -> Option<DsdRate> {
        let base_ok = base == 44_100 || base == 48_000;
        let multiple_ok = matches!(multiple, 64 | 128 | 256 | 512 | 1024);
        (base_ok && multiple_ok).then_some(DsdRate { base, multiple })
    }

    /// One-bit samples per second, per channel.
    pub fn bit_rate(self) -> u32 {
        self.base * self.multiple
    }

    /// The PCM rate DoP carries this rate on: 16 one-bit samples per channel
    /// in every 24-bit frame. DSD64 → 176.4 kHz, DSD256 → 705.6 kHz.
    pub fn dop_carrier(self) -> u32 {
        self.bit_rate() / 16
    }
}

/// Which modulator turns PCM into one-bit DSD. Every kind is a loop order
/// (5, 7 or 9), a noise curve and a search: see [`ModulatorInfo`] and
/// [`CATALOG`]. Settings store the camelCase name ("sdm5", "sdm7La", ...), so
/// a name, once released, never changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModulatorKind {
    /// Fifth order, light on the CPU, DSD64 to DSD1024.
    #[default]
    Sdm5,
    /// Fifth order, the wide noise curve.
    Sdm5Wide,
    /// Fifth order with look-ahead.
    Sdm5La,
    /// Seventh order: deeper noise in the music band, less headroom.
    Sdm7,
    /// Seventh order, the wide noise curve.
    Sdm7Wide,
    /// Seventh order with look-ahead.
    Sdm7La,
    /// Seventh order with a deeper look-ahead (strong CPUs).
    Sdm7LaPlus,
    /// Seventh order, the wide noise curve, with look-ahead.
    Sdm7WideLa,
    /// Ninth order, DSD512 and up only.
    Sdm9,
    /// Ninth order with look-ahead, DSD512 and up only.
    Sdm9La,
    /// Seventh order, full trellis search: the converter only.
    Sdm7Trellis,
    /// Ninth order, full trellis search: the converter only, DSD512 and up.
    Sdm9Trellis,
}

/// The shape of the shaped noise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseCurve {
    /// The deepest noise in the band the rate allows (SDM5's band edges:
    /// 20/30/50/100/160 kHz for DSD64…1024).
    Standard,
    /// A wider quiet band: the noise kept low up to about twice the Standard
    /// edge, so less ultrasonic noise reaches the DAC, paid for with a few dB
    /// in the music band. DSD128 and up: at DSD64 it gains nothing.
    Wide,
}

/// How a modulator chooses each bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// The quantizer decides from the loop filter alone.
    None,
    /// A few best bit paths kept a few bits ahead (real time).
    LookAhead,
    /// More paths, deeper (real time on strong CPUs).
    LookAheadPlus,
    /// A full trellis search, far heavier than real time.
    Trellis,
}

/// What the catalog knows of a modulator. One table, so the player's list,
/// the converter's list and the status line agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModulatorInfo {
    pub kind: ModulatorKind,
    /// The name the user sees, e.g. "SDM7 LA".
    pub label: &'static str,
    pub order: u8,
    pub curve: NoiseCurve,
    pub search: Search,
    /// The lowest and highest DSD multiple (64…1024) it is built for.
    pub min_multiple: u32,
    pub max_multiple: u32,
    /// False: too heavy for playback, offered by the converter only.
    pub realtime: bool,
    /// The quality order, 1 = best. Lists show the best first. Ranked by the
    /// measurement reports (in-band noise and N+D, DSD64–256 and, for the
    /// order-9 kinds, DSD512) where two kinds share a rate.
    pub rank: u16,
}

const fn info(
    kind: ModulatorKind,
    label: &'static str,
    order: u8,
    curve: NoiseCurve,
    search: Search,
    (min_multiple, max_multiple): (u32, u32),
    realtime: bool,
    rank: u16,
) -> ModulatorInfo {
    ModulatorInfo { kind, label, order, curve, search, min_multiple, max_multiple, realtime, rank }
}

/// Every modulator, best first.
pub const CATALOG: [ModulatorInfo; 12] = {
    use ModulatorKind as K;
    use NoiseCurve::{Standard as Std, Wide};
    use Search::{LookAhead as La, LookAheadPlus as LaPlus, None as Plain, Trellis};
    // Plain SDM5 stops at DSD256: from DSD512 its loop is stable only to 0.54
    // of full modulation, under the 0.57 input limit.
    [
        info(K::Sdm9Trellis, "SDM9 Trellis", 9, Std, Trellis, (512, 1024), false, 1),
        info(K::Sdm9La, "SDM9 LA", 9, Std, La, (512, 1024), true, 2),
        info(K::Sdm7Trellis, "SDM7 Trellis", 7, Std, Trellis, (64, 1024), false, 3),
        info(K::Sdm9, "SDM9", 9, Std, Plain, (512, 1024), true, 4),
        info(K::Sdm7LaPlus, "SDM7 LA+", 7, Std, LaPlus, (64, 1024), true, 5),
        info(K::Sdm7La, "SDM7 LA", 7, Std, La, (64, 1024), true, 6),
        info(K::Sdm7WideLa, "SDM7 Wide LA", 7, Wide, La, (128, 1024), true, 7),
        info(K::Sdm7, "SDM7", 7, Std, Plain, (64, 1024), true, 8),
        info(K::Sdm7Wide, "SDM7 Wide", 7, Wide, Plain, (128, 1024), true, 9),
        info(K::Sdm5La, "SDM5 LA", 5, Std, La, (64, 1024), true, 10),
        info(K::Sdm5, "SDM5", 5, Std, Plain, (64, 256), true, 11),
        info(K::Sdm5Wide, "SDM5 Wide", 5, Wide, Plain, (128, 256), true, 12),
    ]
};

impl ModulatorKind {
    pub fn info(self) -> &'static ModulatorInfo {
        CATALOG.iter().find(|i| i.kind == self).expect("every kind is in the catalog")
    }

    pub fn label(self) -> &'static str {
        self.info().label
    }

    /// Built for this rate (the 48 kHz family counts by its multiple too).
    pub fn supports(self, rate: DsdRate) -> bool {
        let i = self.info();
        (i.min_multiple..=i.max_multiple).contains(&rate.multiple)
    }
}

/// PCM → DSD for ONE channel: interpolation from the input PCM rate up to the
/// DSD bit rate, then the modulator. `process` must not allocate, lock or
/// block — it runs on the audio thread.
pub trait DsdEncoder: Send {
    /// One-bit samples per input PCM sample (bit rate / input rate): a power
    /// of two, at least 8, so every input sample yields whole bytes.
    fn ratio(&self) -> usize;

    /// Encodes `pcm` (one channel, PCM full scale = ±1.0) into `out`, which
    /// must be exactly `pcm.len() * ratio() / 8` bytes. MSB first.
    fn process(&mut self, pcm: &[f64], out: &mut [u8]);

    /// Forgets all state (seek, stream restart); the next output starts as
    /// if from silence.
    fn reset(&mut self);

    /// How many times the modulator's stability guard fired (input limited or
    /// integrators reset) since it was created. Shown to the user, so a
    /// modulator that is near its limit is not a silent failure.
    fn overloads(&self) -> u64;

    /// How many input PCM samples the output lags behind the plain loop: a
    /// look-ahead decides each bit this many samples late (rounded up). After
    /// `new`/`reset` the first `latency() * ratio() / 8` bytes are filler
    /// ([`DSD_SILENCE`]), and the bits of input sample k start at output bit
    /// `(k + latency()) * ratio()`. The converter drops the filler and
    /// flushes with `latency()` zero samples at the end; the player counts it
    /// in the position and the analyzer's alignment. Zero for modulators
    /// without look-ahead.
    fn latency(&self) -> usize {
        0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DsdError {
    /// The bit rate is not a power-of-two multiple (≥ 8) of the input rate.
    UnsupportedRatio { input_rate: u32, bit_rate: u32 },
    /// This modulator is not built for this rate (or not built yet).
    UnsupportedModulator { kind: ModulatorKind, multiple: u32 },
}

impl fmt::Display for DsdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsdError::UnsupportedRatio { input_rate, bit_rate } => write!(
                f,
                "cannot encode {input_rate} Hz PCM to a {bit_rate} Hz DSD stream: \
                 the ratio must be a power of two of at least 8"
            ),
            DsdError::UnsupportedModulator { kind, multiple } => {
                write!(f, "the {} modulator is not available for DSD{multiple}", kind.label())
            }
        }
    }
}

impl std::error::Error for DsdError {}

/// `bit_rate / input_rate` when it is a power of two of at least 8.
pub fn encoder_ratio(input_rate: u32, rate: DsdRate) -> Result<usize, DsdError> {
    let bit_rate = rate.bit_rate();
    let err = DsdError::UnsupportedRatio { input_rate, bit_rate };
    if input_rate == 0 || bit_rate % input_rate != 0 {
        return Err(err);
    }
    let ratio = (bit_rate / input_rate) as usize;
    if ratio < 8 || !ratio.is_power_of_two() {
        return Err(err);
    }
    Ok(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_carriers() {
        assert_eq!(DsdRate::DSD64.dop_carrier(), 176_400);
        assert_eq!(DsdRate::DSD128.dop_carrier(), 352_800);
        assert_eq!(DsdRate::DSD256.dop_carrier(), 705_600);
        assert_eq!(DsdRate::new(48_000, 64).unwrap().dop_carrier(), 192_000);
        assert!(DsdRate::new(44_100, 32).is_none());
        assert!(DsdRate::new(96_000, 64).is_none());
    }

    #[test]
    fn ratios() {
        // DoP: the chain runs at the carrier rate, 16 bits per PCM sample.
        assert_eq!(encoder_ratio(705_600, DsdRate::DSD256), Ok(16));
        assert_eq!(encoder_ratio(352_800, DsdRate::DSD64), Ok(8));
        assert!(encoder_ratio(705_600, DsdRate::DSD64).is_err()); // ratio 4
        assert!(encoder_ratio(48_000 * 8, DsdRate::DSD256).is_err()); // wrong family
    }

    #[test]
    fn catalog_lists_every_kind_once_best_first() {
        // Adding a kind breaks this match until the catalog has it too.
        let named = |k: ModulatorKind| match k {
            ModulatorKind::Sdm5
            | ModulatorKind::Sdm5Wide
            | ModulatorKind::Sdm5La
            | ModulatorKind::Sdm7
            | ModulatorKind::Sdm7Wide
            | ModulatorKind::Sdm7La
            | ModulatorKind::Sdm7LaPlus
            | ModulatorKind::Sdm7WideLa
            | ModulatorKind::Sdm9
            | ModulatorKind::Sdm9La
            | ModulatorKind::Sdm7Trellis
            | ModulatorKind::Sdm9Trellis => 12,
        };
        assert_eq!(CATALOG.len(), named(ModulatorKind::Sdm5));
        for (n, i) in CATALOG.iter().enumerate() {
            assert_eq!(i.rank as usize, n + 1, "{} out of order", i.label);
            assert_eq!(i.kind.info(), i);
            assert_eq!(CATALOG.iter().filter(|j| j.kind == i.kind || j.label == i.label).count(), 1);
            assert!(i.min_multiple <= i.max_multiple);
            assert_eq!(i.realtime, i.search != Search::Trellis);
            let name = serde_json::to_string(&i.kind).unwrap();
            assert_eq!(serde_json::from_str::<ModulatorKind>(&name).unwrap(), i.kind);
        }
        assert_eq!(serde_json::to_string(&ModulatorKind::Sdm5).unwrap(), "\"sdm5\"");
        assert_eq!(serde_json::to_string(&ModulatorKind::Sdm7LaPlus).unwrap(), "\"sdm7LaPlus\"");
        assert!(ModulatorKind::Sdm5.supports(DsdRate::DSD64));
        assert!(!ModulatorKind::Sdm9.supports(DsdRate::DSD256));
        assert!(ModulatorKind::Sdm9.supports(DsdRate::new(48_000, 512).unwrap()));
        assert!(!ModulatorKind::Sdm5.supports(DsdRate::new(44_100, 512).unwrap()));
    }
}
