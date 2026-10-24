//! The converter's DSF output: the finished chain (after the ceiling, before
//! any PCM dither) → one modulator per channel → a DSF file.
//!
//! The chain keeps the source's family, so the DSD rate is the source family
//! times the chosen multiple. The chain's PCM rate is capped so the encoder
//! gets a power-of-two ratio of at least 8 (`effective_fs`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::audio::dsd::dsf::{DsdRead, DsfReader};
use crate::audio::dsd::dsf_write::{fnv1a, id3v2_tag, ChannelHashes, DsfWriter, FNV_START};
use crate::audio::dsd::encoder::new_encoder;
use crate::audio::dsd::{DsdEncoder, DsdRate, ModulatorKind, CATALOG};

/// What the converter writes when the output is DSF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DsdExport {
    /// 64, 128, 256, 512 or 1024.
    pub multiple: u32,
    pub kind: ModulatorKind,
}

impl DsdExport {
    /// The DSD rate for a chain running at `pcm_rate` (its family decides
    /// the base).
    pub fn rate_for(&self, pcm_rate: u32) -> Option<DsdRate> {
        let base = if pcm_rate % 44_100 == 0 { 44_100 } else { 48_000 };
        DsdRate::new(base, self.multiple)
    }
}

/// The FS multiplier the chain runs at for DSD`multiple`: the chosen one,
/// capped so DSD bit rate / PCM rate stays at least 8 (DSD64: FS8 at most).
pub fn effective_fs(fs: u32, multiple: u32) -> u32 {
    fs.min(multiple / 8).max(1)
}

/// A modulator the converter offers for a rate.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModulatorEntry {
    pub kind: ModulatorKind,
    pub label: &'static str,
    pub rank: u16,
    pub realtime: bool,
}

/// Every modulator built for DSD`multiple`, in the catalog's order (best
/// first). A kind not built yet is refused by `new_encoder` and left out, so
/// the list grows as modulators land.
pub fn modulators(multiple: u32) -> Vec<ModulatorEntry> {
    let Some(rate) = DsdRate::new(44_100, multiple) else { return Vec::new() };
    CATALOG
        .iter()
        .filter(|i| i.kind.supports(rate) && new_encoder(i.kind, rate.bit_rate() / 8, rate).is_ok())
        .map(|i| ModulatorEntry { kind: i.kind, label: i.label, rank: i.rank, realtime: i.realtime })
        .collect()
}

/// The ID3v2 tag a DSF render carries: the source's artist and title and
/// the key/value list the FLAC path writes as VORBIS_COMMENT.
pub fn tag_for(artist: &str, title: &str, flac_tags: &[(String, String)]) -> Vec<u8> {
    id3v2_tag(&[("TPE1", artist), ("TIT2", title)], flac_tags)
}

/// What an encode wrote.
#[derive(Clone, Debug)]
pub struct DsdDone {
    pub rate: DsdRate,
    /// One-bit samples per channel.
    pub samples: u64,
    /// The modulators' stability guard, summed over the channels.
    pub overloads: u64,
    pub hashes: ChannelHashes,
}

/// Input PCM samples per channel encoded at a time.
const CHUNK: usize = 1 << 16;

/// Encodes `n` stereo frames at `pcm_rate` into a DSF file at `path`.
/// `pull(start, len, l, r)` fills `l`/`r` with exactly `len` frames from
/// frame `start` on, in order. Each channel has one encoder that runs over
/// the whole track in order; the two run side by side. The encoder's
/// look-ahead is trimmed from the front and flushed with silence at the end,
/// so the file holds exactly `n · ratio` samples, aligned with the PCM.
pub fn encode_dsf<F>(
    path: &Path,
    pcm_rate: u32,
    export: DsdExport,
    n: usize,
    id3: &[u8],
    cancel: &AtomicBool,
    mut pull: F,
    mut progress: impl FnMut(usize),
) -> Result<DsdDone, String>
where
    F: FnMut(usize, usize, &mut Vec<f64>, &mut Vec<f64>) -> Result<(), String>,
{
    let rate = export
        .rate_for(pcm_rate)
        .ok_or_else(|| format!("DSD{}: not a DSD rate", export.multiple))?;
    let mut encs = Vec::with_capacity(2);
    for _ in 0..2 {
        encs.push(new_encoder(export.kind, pcm_rate, rate).map_err(|e| e.to_string())?);
    }
    encode_with(path, pcm_rate, rate, encs, n, id3, cancel, &mut pull, &mut progress)
}

#[allow(clippy::too_many_arguments)]
fn encode_with(
    path: &Path,
    pcm_rate: u32,
    rate: DsdRate,
    mut encs: Vec<Box<dyn DsdEncoder>>,
    n: usize,
    id3: &[u8],
    cancel: &AtomicBool,
    pull: &mut dyn FnMut(usize, usize, &mut Vec<f64>, &mut Vec<f64>) -> Result<(), String>,
    progress: &mut dyn FnMut(usize),
) -> Result<DsdDone, String> {
    let ratio = encs[0].ratio();
    let latency = encs[0].latency();
    crate::aelog!(
        "[DSD] export: {} Hz PCM → DSD{} ({} Hz), {}, ratio {}, look-ahead {} samples",
        pcm_rate,
        rate.multiple,
        rate.bit_rate(),
        path.display(),
        ratio,
        latency
    );
    let mut writer = DsfWriter::create(path, rate, 2)?;
    let mut skip = latency * ratio / 8;
    let total = n + latency;
    let mut pcm = [Vec::with_capacity(CHUNK), Vec::with_capacity(CHUNK)];
    let mut bytes = [vec![0u8; CHUNK * ratio / 8], vec![0u8; CHUNK * ratio / 8]];
    let mut fed = 0usize;
    while fed < total {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(path);
            return Err("Cancelled".to_string());
        }
        let m = CHUNK.min(total - fed);
        let real = m.min(n.saturating_sub(fed));
        let [l, r] = &mut pcm;
        l.clear();
        r.clear();
        if real > 0 {
            pull(fed, real, l, r)?;
            if l.len() != real || r.len() != real {
                return Err(format!("DSD export: {} of {} frames read", l.len().min(r.len()), real));
            }
        }
        // Past the end: silence, which flushes the look-ahead.
        l.resize(m, 0.0);
        r.resize(m, 0.0);
        let len = m * ratio / 8;
        std::thread::scope(|s| {
            for ((enc, p), out) in encs.iter_mut().zip(&pcm).zip(bytes.iter_mut()) {
                s.spawn(move || enc.process(p, &mut out[..len]));
            }
        });
        let from = skip.min(len);
        skip -= from;
        writer.write(&[&bytes[0][from..len], &bytes[1][from..len]])?;
        fed += m;
        progress(fed.min(n));
    }
    let samples = (n * ratio) as u64;
    debug_assert_eq!(writer.bytes() * 8, samples);
    let hashes = writer.finish(samples, id3)?;
    let overloads: u64 = encs.iter().map(|e| e.overloads()).sum();
    if overloads > 0 {
        crate::aelog!("[DSD] export: the modulator's stability guard fired {} times", overloads);
    }
    Ok(DsdDone { rate, samples, overloads, hashes })
}

/// Reads `path` back and checks it holds what was written.
pub fn verify_dsf(path: &Path, done: &DsdDone) -> Result<(), String> {
    let mut r = DsfReader::open(path)?;
    let info = r.info().clone();
    if info.rate != done.rate || info.channels != 2 || info.samples != done.samples {
        return Err(format!(
            "DSF header reads DSD{} {} ch {} samples, written DSD{} 2 ch {} samples",
            info.rate.multiple, info.channels, info.samples, done.rate.multiple, done.samples
        ));
    }
    let mut bufs = [vec![0u8; 1 << 20], vec![0u8; 1 << 20]];
    let mut h = [FNV_START; 2];
    loop {
        let [a, b] = &mut bufs;
        let got = r.read(&mut [&mut a[..], &mut b[..]])?;
        if got == 0 {
            break;
        }
        h[0] = fnv1a(h[0], &a[..got]);
        h[1] = fnv1a(h[1], &b[..got]);
    }
    if h[..] != done.hashes.0[..] {
        return Err("DSF data reads back different from what was written".to_string());
    }
    Ok(())
}

/// The Done line's note on the modulator ("", or ", 3 modulator overloads").
pub fn overload_note(overloads: u64) -> String {
    match overloads {
        0 => String::new(),
        1 => ", 1 modulator overload".to_string(),
        n => format!(", {} modulator overloads", n),
    }
}

/// Encodes a finished in-RAM render (`gain` applied on the way) and checks
/// it. On a failed check the file is renamed `…_UNVERIFIED.dsf`.
pub fn write_in_ram(
    l: &[f64],
    r: &[f64],
    pcm_rate: u32,
    gain: f64,
    export: DsdExport,
    path: &Path,
    id3: &[u8],
    cancel: &AtomicBool,
    progress: impl FnMut(usize),
) -> Result<DsdDone, String> {
    let n = l.len().min(r.len());
    let done = encode_dsf(
        path,
        pcm_rate,
        export,
        n,
        id3,
        cancel,
        |start, len, lo, ro| {
            lo.extend(l[start..start + len].iter().map(|x| x * gain));
            ro.extend(r[start..start + len].iter().map(|x| x * gain));
            Ok(())
        },
        progress,
    )?;
    check_or_rename(path, &done)?;
    Ok(done)
}

/// `verify_dsf`, renaming the file `…_UNVERIFIED.dsf` when it fails.
pub fn check_or_rename(path: &Path, done: &DsdDone) -> Result<(), String> {
    verify_dsf(path, done).map_err(|e| {
        let failed = unverified_name(path);
        let _ = std::fs::rename(path, &failed);
        format!("Verification failed: {}", e)
    })
}

pub fn unverified_name(path: &Path) -> std::path::PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    path.with_file_name(format!("{}_UNVERIFIED.{}", stem, ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsd::decimate::decode_all;
    use crate::audio::dsd::encoder::new_encoder;

    #[test]
    fn chain_rate_keeps_the_ratio_valid() {
        assert_eq!(effective_fs(16, 64), 8);
        assert_eq!(effective_fs(8, 64), 8);
        assert_eq!(effective_fs(16, 128), 16);
        assert_eq!(effective_fs(2, 1024), 2);
        assert_eq!(effective_fs(32, 512), 32);
        assert_eq!(effective_fs(32, 256), 32);
        for m in [64u32, 128, 256, 512, 1024] {
            for fs in [2u32, 4, 8, 16, 32] {
                for base in [44_100u32, 48_000] {
                    let pcm = base * effective_fs(fs, m);
                    let rate = DsdExport { multiple: m, kind: ModulatorKind::Sdm5 }.rate_for(pcm).unwrap();
                    assert_eq!(rate.base, base);
                    assert!(crate::audio::dsd::encoder_ratio(pcm, rate).is_ok(), "FS{} DSD{} {}", fs, m, base);
                }
            }
        }
    }

    #[test]
    fn modulator_list_is_the_built_kinds_best_first() {
        for m in [64u32, 128, 256, 512, 1024] {
            let list = modulators(m);
            assert!(list.iter().any(|e| e.kind == ModulatorKind::Sdm7), "DSD{}", m);
            assert!(list.windows(2).all(|w| w[0].rank < w[1].rank));
            let rate = DsdRate::new(44_100, m).unwrap();
            for e in &list {
                assert!(e.kind.supports(rate));
                assert!(new_encoder(e.kind, rate.bit_rate() / 8, rate).is_ok());
            }
        }
        assert!(modulators(32).is_empty());
        let json = serde_json::to_string(&modulators(64)[..1]).unwrap();
        assert!(json.contains("\"kind\":") && json.contains("\"realtime\":"), "{}", json);
    }

    /// Shifts its input by `latency` samples and writes each sample's sign
    /// as `ratio` equal bits: what goes in can be told from what comes out.
    struct Delay {
        ratio: usize,
        latency: usize,
        line: std::collections::VecDeque<f64>,
    }

    impl DsdEncoder for Delay {
        fn ratio(&self) -> usize {
            self.ratio
        }
        fn process(&mut self, pcm: &[f64], out: &mut [u8]) {
            let per = self.ratio / 8;
            for (x, o) in pcm.iter().zip(out.chunks_mut(per)) {
                self.line.push_back(*x);
                let y = self.line.pop_front().unwrap();
                o.fill(if y > 0.0 { 0xFF } else { 0x00 });
            }
        }
        fn reset(&mut self) {}
        fn overloads(&self) -> u64 {
            2
        }
        fn latency(&self) -> usize {
            self.latency
        }
    }

    #[test]
    fn look_ahead_is_trimmed_and_flushed_to_the_exact_length() {
        let dir = tempfile::tempdir().unwrap();
        let rate = DsdRate::DSD64;
        for (n, latency) in [(CHUNK * 2 + 77, 300usize), (100, CHUNK + 5), (CHUNK, 0)] {
            let ratio = 16;
            let encs: Vec<Box<dyn DsdEncoder>> = (0..2)
                .map(|_| {
                    Box::new(Delay { ratio, latency, line: std::iter::repeat(0.0).take(latency).collect() })
                        as Box<dyn DsdEncoder>
                })
                .collect();
            // A pattern: positive on frames divisible by 3 (left), by 5 (right).
            let path = dir.path().join(format!("d{}.dsf", n));
            let mut seen = 0;
            let done = encode_with(
                &path,
                rate.bit_rate() / ratio as u32,
                rate,
                encs,
                n,
                &[],
                &AtomicBool::new(false),
                &mut |start, len, l, r| {
                    assert_eq!(start, seen);
                    seen += len;
                    l.extend((start..start + len).map(|i| if i % 3 == 0 { 1.0 } else { -1.0 }));
                    r.extend((start..start + len).map(|i| if i % 5 == 0 { 1.0 } else { -1.0 }));
                    Ok(())
                },
                &mut |_| {},
            )
            .unwrap();
            assert_eq!(seen, n);
            assert_eq!(done.samples, (n * ratio) as u64);
            assert_eq!(done.overloads, 4);
            verify_dsf(&path, &done).unwrap();
            let mut rd = DsfReader::open(&path).unwrap();
            let back = crate::audio::dsd::dsf::test_files::read_all(&mut rd, 5000);
            assert_eq!(back[0].len(), n * 2);
            for i in 0..n {
                assert_eq!(back[0][2 * i], if i % 3 == 0 { 0xFF } else { 0 }, "L frame {} (n {}, lat {})", i, n, latency);
                assert_eq!(back[1][2 * i + 1], if i % 5 == 0 { 0xFF } else { 0 }, "R frame {}", i);
            }
        }
    }

    struct Fit {
        /// (cos, sin) amplitude of each tone.
        coefs: Vec<(f64, f64)>,
        /// The residual against the signal, in dB.
        null_db: f64,
    }

    /// Least squares: `x` (from sample `first` on, at `fs`) as a sum of
    /// cosines and sines at `freqs`.
    fn fit_tones(x: &[f64], first: usize, fs: f64, freqs: &[f64]) -> Fit {
        use std::f64::consts::PI;
        let k = freqs.len() * 2;
        let basis = |i: usize, j: usize| {
            let w = 2.0 * PI * freqs[j / 2] * (first + i) as f64 / fs;
            if j % 2 == 0 { w.sin() } else { w.cos() }
        };
        let mut m = vec![vec![0.0; k + 1]; k];
        for (i, &v) in x.iter().enumerate() {
            let b: Vec<f64> = (0..k).map(|j| basis(i, j)).collect();
            for p in 0..k {
                for q in 0..k {
                    m[p][q] += b[p] * b[q];
                }
                m[p][k] += b[p] * v;
            }
        }
        for p in 0..k {
            let piv = m[p][p];
            for q in p..=k {
                m[p][q] /= piv;
            }
            for r in 0..k {
                if r != p {
                    let f = m[r][p];
                    for q in p..=k {
                        m[r][q] -= f * m[p][q];
                    }
                }
            }
        }
        let w: Vec<f64> = (0..k).map(|p| m[p][k]).collect();
        let (mut res, mut sig) = (0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let y: f64 = (0..k).map(|j| w[j] * basis(i, j)).sum();
            res += (v - y).powi(2);
            sig += y * y;
        }
        // Amplitude a and phase φ of a·sin(ωt + φ) = a cos φ sin + a sin φ cos.
        let coefs = (0..freqs.len()).map(|t| (w[2 * t], w[2 * t + 1])).collect();
        Fit { coefs, null_db: 10.0 * (res / sig).log10() }
    }

    /// PCM → the real modulator → DSF → the DSF reader → the decimator: the
    /// level, the response and the null against the source.
    #[test]
    fn round_trip_through_the_file_matches_the_source() {
        use std::f64::consts::PI;
        let dir = tempfile::tempdir().unwrap();
        for (base, multiple, fs) in [(44_100u32, 64u32, 8u32), (48_000, 128, 4), (44_100, 256, 32)] {
            let pcm_rate = base * effective_fs(fs, multiple);
            let n = pcm_rate as usize / 2; // half a second
            let tones = [(1_000.0, 0.25), (7_000.0, 0.1), (15_000.0, 0.1)];
            let sig = |i: usize, ch: usize| -> f64 {
                let t = i as f64 / pcm_rate as f64;
                tones.iter().map(|(f, a)| a * (2.0 * PI * f * t + ch as f64).sin()).sum()
            };
            let l: Vec<f64> = (0..n).map(|i| sig(i, 0)).collect();
            let r: Vec<f64> = (0..n).map(|i| sig(i, 1)).collect();
            let path = dir.path().join(format!("rt{}.dsf", multiple));
            let export = DsdExport { multiple, kind: ModulatorKind::Sdm5 };
            let tag = tag_for("A", "T", &[("AURA_CHAIN".to_string(), "DC".to_string())]);
            let done = write_in_ram(&l, &r, pcm_rate, 1.0, export, &path, &tag, &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(done.overloads, 0);
            let mut rd = DsfReader::open(&path).unwrap();
            assert_eq!(rd.info().rate, DsdRate::new(base, multiple).unwrap());
            let (dl, dr, out_rate) = decode_all(&mut rd).unwrap();
            assert_eq!(out_rate, 2 * base);
            // Skip the filters' edges.
            let (a, b) = (dl.len() / 10, dl.len() - dl.len() / 10);
            for (ch, d) in [(0usize, &dl), (1, &dr)] {
                let fit = fit_tones(&d[a..b], a, out_rate as f64, &tones.map(|t| t.0));
                let mut delays: Vec<f64> = Vec::new();
                for ((f, amp), (c, s)) in tones.iter().zip(&fit.coefs) {
                    let got = (c * c + s * s).sqrt();
                    let db = 20.0 * (got / amp).log10();
                    assert!(db.abs() < 0.1, "DSD{} {} Hz ch{}: {:.3} dB", multiple, f, ch, db);
                    // The source is a·sin(ωt + ch): its phase against that.
                    let phase = s.atan2(*c) - ch as f64;
                    let phase = (phase + PI).rem_euclid(2.0 * PI) - PI;
                    let mut tau: f64 = -phase / (2.0 * PI * f);
                    // Whole periods: the one nearest the lowest tone's delay.
                    if let Some(&d0) = delays.first() {
                        tau += ((d0 - tau) * *f).round() / *f;
                    }
                    delays.push(tau);
                }
                // One delay for every tone (linear phase), a few microseconds:
                // the interpolator's own group delay.
                let spread = delays.iter().cloned().fold(f64::MIN, f64::max) - delays.iter().cloned().fold(f64::MAX, f64::min);
                assert!(spread < 1e-6, "DSD{} ch{}: delays {:?}", multiple, ch, delays);
                assert!(delays[0].abs() < 50e-6, "DSD{} ch{}: delay {:e} s", multiple, ch, delays[0]);
                eprintln!("DSD{} ch{}: delay {:.2} µs, null {:.1} dB", multiple, ch, delays[0] * 1e6, fit.null_db);
                // What is left once the tones are gone: the modulator's noise.
                assert!(fit.null_db < -90.0, "DSD{} ch{}: null {:.1} dB", multiple, ch, fit.null_db);
            }
            let tags = &rd.info().tags;
            assert!(!tags.is_empty());
        }
    }
}

/// The version of the DSD modulator a render records in its stamp (`AURA_STAGES`).
/// Raise it when a release changes what this stage writes into a render:
/// every render made with the older one then reads as not converted.
pub const STAGE_VERSION: u32 = 1;
