//! The separation network: one 7.8 s segment of a 44.1 kHz stereo mix in, six
//! sources out (drums, bass, other, vocals, guitar, piano).
//!
//! The pack's model is HTDemucs with its STFT and iSTFT taken out of the graph
//! (ONNX has no iSTFT); both are done here exactly as torch does them —
//! `spec_in` / `spec_out` mirror HTDemucs `_spec` + `_magnitude` and
//! `_mask` + `_ispec` (a test checks them against torch's own numbers).
//!
//! It runs on ONNX Runtime from the pack (loaded at run time): DirectML on the
//! GPU when there is one (about 60 ms a segment on a 4090, ~125× real time),
//! else the CPU (about 3 s a segment on a 14900K, ~2.4× real time).
//! DirectML's graph fusion gives wrong numbers on this network (found
//! 27.09: every single op is right, the fused graph is not), so it is off.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

pub const SR: u32 = 44_100;
/// One segment: 7.8 s, the length the network was trained on.
pub const SEG: usize = 343_980;
pub const SOURCES: usize = 6;
pub const NAMES: [&str; SOURCES] = ["drums", "bass", "other", "vocals", "guitar", "piano"];
const NFFT: usize = 4096;
const HOP: usize = 1024;
const BINS: usize = 2048;
/// ceil(SEG / HOP)
const FRAMES: usize = 336;
/// The re-padding `_spec` does so that frames line up with the hop.
const PAD: usize = HOP / 2 * 3;
/// Free video memory the GPU path wants: the network's ~2.4 GB and a margin.
pub const GPU_NEEDS: u64 = 3 << 30;
/// What the network's session holds of the card while it is open (measured 27.09).
pub const SESSION_VRAM: u64 = 2_400 << 20;

/// Load the pack's runtime once: its `DirectML.dll` first, by full path
/// (Windows has an older one in System32, and the runtime would pick that),
/// then `onnxruntime.dll`.
pub(super) fn runtime(dir: &Path) -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::ffi::OsStrExt;
            let p = dir.join(super::pack::DIRECTML);
            let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            if winapi::um::libloaderapi::LoadLibraryW(w.as_ptr()).is_null() {
                return Err(format!("cannot load {}", p.display()));
            }
        }
        let b = ort::init_from(dir.join(super::pack::RUNTIME)).map_err(|e| e.to_string())?;
        b.with_name("aura-spatial").commit();
        Ok(())
    })
    .clone()
}

/// What takes one segment (`SEG` frames of left and right) apart into the
/// six sources, `[source][channel][SEG]` flattened: the network, or a
/// stand-in of the tests.
pub trait Separate {
    fn separate(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String>;
}

impl Separate for Core {
    fn separate(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
        Core::separate(self, l, r)
    }
}

pub struct Core {
    session: ort::session::Session,
    /// On the GPU (DirectML), or the CPU.
    pub gpu: bool,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
}

/// A session on the network `model` with the pack's runtime from `dir`:
/// DirectML when it comes up and the card has `gpu_needs` bytes of video
/// memory free (with less, the card is left to the engine's own convolution),
/// else the CPU — when `cpu_ok`. `AURA_SPATIAL_CPU`: the CPU even with a GPU
/// there (to test that path). `what` names the network in the log. Answers
/// the session and whether it is on the GPU.
pub(super) fn session(dir: &Path, model: &Path, gpu_needs: u64, cpu_ok: bool, what: &str) -> Result<(ort::session::Session, bool), String> {
    runtime(dir)?;
    let build = |gpu: bool| -> Result<ort::session::Session, String> {
        let e = |e: ort::Error<ort::session::builder::SessionBuilder>| e.to_string();
        let mut b = ort::session::Session::builder()
            .map_err(|e| e.to_string())?
            .with_memory_pattern(false)
            .map_err(e)?
            .with_parallel_execution(false)
            .map_err(e)?
            .with_config_entry("ep.dml.disable_graph_fusion", "1")
            .map_err(e)?;
        if gpu {
            b = b
                .with_execution_providers([ort::ep::DirectML::default()
                    .with_performance_preference(ort::ep::directml::PerformancePreference::HighPerformance)
                    .build()
                    .error_on_failure()])
                .map_err(e)?;
        } else {
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            b = b.with_intra_threads((n / 2).max(1)).map_err(e)?;
        }
        b.commit_from_file(model).map_err(|e| e.to_string())
    };
    let gpu_first = if std::env::var_os("AURA_SPATIAL_CPU").is_some() {
        Err("the CPU was asked for".to_string())
    } else {
        let free = crate::audio::gpu::dxgi_memory::largest_adapter()
            .and_then(|(v, d)| crate::audio::gpu::dxgi_memory::query(v, d).ok())
            .map(|m| m.free());
        if free.is_some_and(|f| f < gpu_needs) {
            Err(format!("{} MB of video memory free, it needs {}", free.unwrap_or(0) >> 20, gpu_needs >> 20))
        } else {
            build(true)
        }
    };
    match gpu_first {
        Ok(s) => Ok((s, true)),
        Err(g) if cpu_ok => {
            crate::aelog!("[SPATIAL] DirectML did not come up for {what} ({g}); it runs on the CPU");
            Ok((build(false)?, false))
        }
        Err(g) => Err(format!("{what} runs on the GPU only, and DirectML did not come up ({g})")),
    }
}

impl Core {
    /// A session on the pack in `dir`: DirectML if it comes up, else the CPU.
    pub fn open(dir: &Path) -> Result<Core, String> {
        // The network takes about 2.4 GB of video memory while it runs
        // (measured 27.09).
        Core::with(dir, GPU_NEEDS, true, "the separation")
    }

    /// A session on the GPU only, with `gpu_needs` bytes of video memory
    /// free (a live stream's: on the CPU it would sit beside the engine's
    /// own convolution for as long as the stream plays).
    pub fn open_gpu(dir: &Path, gpu_needs: u64) -> Result<Core, String> {
        Core::with(dir, gpu_needs, false, "the live separation")
    }

    fn with(dir: &Path, gpu_needs: u64, cpu_ok: bool, what: &str) -> Result<Core, String> {
        let (session, gpu) = session(dir, &dir.join(super::pack::MODEL), gpu_needs, cpu_ok, what)?;
        let mut planner = FftPlanner::<f32>::new();
        let win = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos()) as f32)
            .collect();
        Ok(Core { session, gpu, fft: planner.plan_fft_forward(NFFT), ifft: planner.plan_fft_inverse(NFFT), win })
    }

    /// One segment (`SEG` frames of left and right) → the six sources,
    /// `[source][channel][SEG]` flattened.
    pub fn separate(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
        assert!(l.len() == SEG && r.len() == SEG);
        let mag = spec_in(&*self.fft, &self.win, l, r);
        let mut mix = Vec::with_capacity(2 * SEG);
        mix.extend_from_slice(l);
        mix.extend_from_slice(r);
        let t_mag = ort::value::Tensor::from_array(([1usize, 4, BINS, FRAMES], mag)).map_err(|e| e.to_string())?;
        let t_mix = ort::value::Tensor::from_array(([1usize, 2, SEG], mix)).map_err(|e| e.to_string())?;
        let out = self
            .session
            .run(ort::inputs!["mag" => t_mag, "mix" => t_mix])
            .map_err(|e| e.to_string())?;
        let (_, spec) = out["spec"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        let (_, wave) = out["wave"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        Ok(spec_out(&*self.ifft, &self.win, spec, wave))
    }
}

/// numpy/torch "reflect" padding of `x` by `left` / `right` (no edge repeat).
pub(super) fn reflect(x: &[f32], left: usize, right: usize) -> Vec<f32> {
    let n = x.len() as isize;
    let at = |i: isize| -> f32 {
        let mut i = i;
        // fold into [0, n) by mirroring about the ends
        loop {
            if i < 0 {
                i = -i;
            } else if i >= n {
                i = 2 * (n - 1) - i;
            } else {
                return x[i as usize];
            }
        }
    };
    (-(left as isize)..n + right as isize).map(at).collect()
}

/// HTDemucs `_spec` (+ `_magnitude` with complex-as-channels): both channels'
/// normalized STFT, frames [2, 2 + FRAMES), the last bin dropped, laid out
/// `[L.re, L.im, R.re, R.im][BINS][FRAMES]`.
pub fn spec_in(fft: &dyn Fft<f32>, win: &[f32], l: &[f32], r: &[f32]) -> Vec<f32> {
    let le = (SEG + HOP - 1) / HOP;
    debug_assert_eq!(le, FRAMES);
    let norm = 1.0 / (NFFT as f32).sqrt();
    let mut out = vec![0f32; 4 * BINS * FRAMES];
    let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
    for (ch, x) in [l, r].into_iter().enumerate() {
        // `_spec`'s own padding, then torch.stft's centre padding
        let x = reflect(x, PAD, PAD + le * HOP - SEG);
        let x = reflect(&x, NFFT / 2, NFFT / 2);
        for t in 0..FRAMES {
            let start = (t + 2) * HOP;
            for i in 0..NFFT {
                buf[i] = Complex32::new(x[start + i] * win[i], 0.0);
            }
            fft.process(&mut buf);
            for f in 0..BINS {
                out[((ch * 2) * BINS + f) * FRAMES + t] = buf[f].re * norm;
                out[((ch * 2 + 1) * BINS + f) * FRAMES + t] = buf[f].im * norm;
            }
        }
    }
    out
}

/// HTDemucs `_mask` (complex-as-channels) + `_ispec` + the time branch:
/// `spec` `[SOURCES][4][BINS][FRAMES]`, `wave` `[SOURCES][2][SEG]` →
/// `[SOURCES][2][SEG]`.
pub fn spec_out(ifft: &dyn Fft<f32>, win: &[f32], spec: &[f32], wave: &[f32]) -> Vec<f32> {
    use rayon::prelude::*;
    let le = HOP * ((SEG + HOP - 1) / HOP) + 2 * PAD; // what _ispec asks torch.istft for
    let frames = FRAMES + 4; // two empty frames each side
    let total = (frames - 1) * HOP + NFFT;
    // torch.istft: the window-square envelope it divides by
    let mut env = vec![0f32; total];
    for j in 0..frames {
        for i in 0..NFFT {
            env[j * HOP + i] += win[i] * win[i];
        }
    }
    let scale = (NFFT as f32).sqrt() / NFFT as f32; // un-normalize, and the inverse FFT's 1/N
    let mut out = vec![0f32; SOURCES * 2 * SEG];
    out.par_chunks_mut(SEG).enumerate().for_each(|(sc, dst)| {
        let (s, ch) = (sc / 2, sc % 2);
        let mut y = vec![0f32; total];
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let re = |f: usize, t: usize| spec[(((s * 4) + ch * 2) * BINS + f) * FRAMES + t];
        let im = |f: usize, t: usize| spec[(((s * 4) + ch * 2 + 1) * BINS + f) * FRAMES + t];
        for j in 2..FRAMES + 2 {
            let t = j - 2;
            buf.iter_mut().for_each(|b| *b = Complex32::new(0.0, 0.0));
            for f in 0..BINS {
                let z = Complex32::new(re(f, t), im(f, t));
                buf[f] = z;
                if f > 0 {
                    buf[NFFT - f] = z.conj();
                }
            }
            // bin NFFT/2 (the Nyquist one) was dropped by _spec: zero
            ifft.process(&mut buf);
            let at = j * HOP;
            for i in 0..NFFT {
                y[at + i] += buf[i].re * scale * win[i];
            }
        }
        // centre=True: drop NFFT/2; then _ispec keeps [PAD, PAD + SEG)
        let base = NFFT / 2 + PAD;
        let w = &wave[(s * 2 + ch) * SEG..(s * 2 + ch + 1) * SEG];
        for i in 0..SEG {
            let k = base + i;
            let e = env[k];
            let v = if e > 1e-11 { y[k] / e } else { 0.0 };
            dst[i] = v + w[i];
        }
        let _ = le;
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian float32 .npy (C order), as numpy writes it.
    fn npy_f32(path: &std::path::Path) -> Vec<f32> {
        let b = std::fs::read(path).unwrap();
        let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
        let data = &b[10 + hl..];
        data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn refs() -> Option<std::path::PathBuf> {
        std::env::var_os("AURA_SPATIAL_REFS").map(Into::into)
    }

    /// Our STFT = torch's, on the export script's reference segment
    /// (`AURA_SPATIAL_REFS` = the folder with ref_mix.npy / ref_mag.npy).
    #[test]
    #[ignore]
    fn spec_in_is_torchs() {
        let Some(d) = refs() else { return };
        let mix = npy_f32(&d.join("ref_mix.npy"));
        let want = npy_f32(&d.join("ref_mag.npy"));
        let mut p = FftPlanner::<f32>::new();
        let win: Vec<f32> = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos()) as f32)
            .collect();
        let got = spec_in(&*p.plan_fft_forward(NFFT), &win, &mix[..SEG], &mix[SEG..]);
        let peak = want.iter().fold(0f32, |m, v| m.max(v.abs()));
        let err = got.iter().zip(&want).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-4 * peak.max(1.0), "err {err} peak {peak}");
    }

    /// The network's outputs back to sources = torch's (ref_spec / ref_wave →
    /// ref_out).
    #[test]
    #[ignore]
    fn spec_out_is_torchs() {
        let Some(d) = refs() else { return };
        let spec = npy_f32(&d.join("ref_spec.npy"));
        let wave = npy_f32(&d.join("ref_wave.npy"));
        let want = npy_f32(&d.join("ref_out.npy"));
        let mut p = FftPlanner::<f32>::new();
        let win: Vec<f32> = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos()) as f32)
            .collect();
        let got = spec_out(&*p.plan_fft_inverse(NFFT), &win, &spec, &wave);
        let err = got.iter().zip(&want).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-4, "err {err}");
    }

    /// The whole segment through the pack's runtime (`AURA_SPATIAL_PACK_DIR`
    /// = an installed pack) = torch's model.forward.
    #[test]
    #[ignore]
    fn separate_is_torchs() {
        let (Some(d), Some(pack)) = (refs(), std::env::var_os("AURA_SPATIAL_PACK_DIR")) else { return };
        let mix = npy_f32(&d.join("ref_mix.npy"));
        let want = npy_f32(&d.join("ref_out.npy"));
        let mut core = Core::open(std::path::Path::new(&pack)).unwrap();
        let t = std::time::Instant::now();
        let got = core.separate(&mix[..SEG], &mix[SEG..]).unwrap();
        let t1 = t.elapsed();
        let t = std::time::Instant::now();
        let _ = core.separate(&mix[..SEG], &mix[SEG..]).unwrap();
        eprintln!("gpu {} first {:?} again {:?}", core.gpu, t1, t.elapsed());
        let err = got.iter().zip(&want).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-3, "err {err}");
    }
}
