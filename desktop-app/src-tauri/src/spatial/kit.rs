//! The drum kit (Anton 28.09): the drums source split into its six pieces —
//! kick, snare, toms, hi-hat, ride, crash — and each piece's level and hits
//! over the whole track.
//!
//! The network is DrumSep (MDX23C, TFC-TDF-Net v3; weights by aufr33 and
//! jarredou, CC BY-NC-SA 4.0) from the spatial pack, with its STFT and its
//! inverse outside the graph (ONNX has no iSTFT), done here as torch does
//! them: a chunk of 130 560 samples (2.96 s at 44.1 kHz) goes in as its
//! spectrogram (n_fft 2048, hop 512, periodic Hann, centred with reflect
//! padding — 256 frames, bins 0…1023), the six pieces' spectrograms come out
//! and go back to sound. Chunks start a quarter chunk apart and are faded
//! into each other (linear fades of a tenth of a chunk at both ends, the sum
//! divided by the fades' sum) — exactly as the research ran it
//! (`drumsep_run.py`; the export and its checks: `export_drumsep.py`, ONNX =
//! torch to 3·10⁻⁶ in fp32, the pack's fp16 weights to 4·10⁻⁴). It runs on
//! the GPU only (Anton's master, 28.09: minutes of full CPU load a track are
//! a risk for the sound); without it the kit stays the drums' bands.
//!
//! The drums source comes in order, as the separation makes it, and a
//! piece's finished sound is never kept — it goes straight into what is
//! measured of it (the research's `kit.py` / `inventory.py`):
//! - its level per frame (hop 512: the mean power of a 2048 window);
//! - its onset function (hop 256: the log-spectral flux of the mono piece in
//!   a 1024 Hann window, and its power), and from it, over the whole track,
//!   its hits: a flux peak over its local mean + 0.15 (of the flux's 99.5th
//!   percentile), the only one within ±30 ms, and the hit's level within
//!   20 dB of the piece's loud hits;
//! - its place (its mean level right against left).
//!
//! A piece is one of the track's instruments when it plays: within 24 dB of
//! the loudest piece and over −35 dB of the mix's loud level, and at least 8
//! hits. On MDB Drums (23 songs, HTDemucs' drums) the research measured F
//! kick/snare/hats 0.94/0.76/0.75.

// The track's job (the track map) takes it into use next.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::stems::percentile;

/// The pieces, in the network's order.
pub const PIECES: [&str; NP] = ["kick", "snare", "toms", "hh", "ride", "crash"];
pub const NP: usize = 6;
pub(super) const SR: f64 = super::core::SR as f64;
const NFFT: usize = 2048;
const HOP: usize = 512;
/// The bins the network sees (the top one, 1024, is dropped).
const DIM_F: usize = 1024;
/// One chunk: 2.96 s, the length the network was trained on; 256 frames.
pub const CHUNK: usize = 130_560;
const FRAMES: usize = CHUNK / HOP + 1;
/// Chunks per chunk length (each sample is in this many): 2 — on MDB (28.09) every
/// piece's F within 0.005 of the research's 4, twice as fast.
pub const OVERLAP: usize = 2;
/// The linear fades at a chunk's ends.
pub(super) const FADE: usize = CHUNK / 10;
/// Free video memory the network wants — TO MEASURE in the GPU window; the
/// separation's for now.
pub(super) const GPU_NEEDS: u64 = 3 << 30;

// What is measured of a piece (kit.py).
/// Frames of the levels: hop 512, window 2048 (86.13 a second, as analysis.rs).
pub(super) const LV_HOP: usize = 512;
pub(super) const LV_N: usize = 2048;
/// The onset function: hop 256, a 1024 (symmetric) Hann window.
pub(super) const ON_HOP: usize = 256;
pub(super) const ON_N: usize = 1024;
pub(super) const ON_BINS: usize = ON_N / 2 + 1;
/// The detector (kit.py `onsets` as inventory.py runs it).
/// Per piece (kick, snare, toms, hh, ride, crash) its δ and floor (dB): the
/// research's best on MDB for each (Anton's master 28.09) — the hits shown.
/// The gates count hits at inventory.py's one setting, which keeps fewer
/// extra pieces (MDB: 75 right, 14 extra, 1 missing; per piece 15 extra).
pub(super) const DETECT: [(f32, f32); NP] = [(0.15, 12.0), (0.10, 30.0), (0.15, 12.0), (0.15, 12.0), (0.15, 30.0), (0.15, 30.0)];
pub(super) const GATE_DETECT: (f32, f32) = (0.15, 20.0);
pub(super) const ON_GAP: f64 = 0.03;
/// The gates (inventory.py `drum_objects`).
pub(super) const MIN_HITS: usize = 8;
pub(super) const UNDER_DB: f64 = 24.0;
pub(super) const FLOOR_DB: f64 = -35.0;

/// The network: DrumSep's core in a session on the pack's runtime.
pub struct Kit {
    session: ort::session::Session,
    /// On the GPU (DirectML), or the CPU (only when asked for: tests).
    pub gpu: bool,
    st: Stft,
}

impl Kit {
    /// A session on `model` with the pack's runtime in `dir`: on the GPU;
    /// the CPU only when `cpu_ok` (the tests — never the player).
    pub fn open(dir: &Path, model: &Path, cpu_ok: bool) -> Result<Kit, String> {
        let (session, gpu) = super::core::session(dir, model, GPU_NEEDS, cpu_ok, "the drum kit")?;
        Ok(Kit { session, gpu, st: Stft::new() })
    }

    /// A session on the GPU only, with `gpu_needs` bytes of video memory free
    /// (a live stream's: it stays open beside the separation's while the
    /// stream plays).
    pub fn open_gpu(dir: &Path, model: &Path, gpu_needs: u64) -> Result<Kit, String> {
        let (session, gpu) = super::core::session(dir, model, gpu_needs, false, "the live drum kit")?;
        Ok(Kit { session, gpu, st: Stft::new() })
    }

    /// One chunk (`CHUNK` samples of left and right) → the six pieces,
    /// `[piece][channel][CHUNK]`.
    pub fn chunk(&mut self, l: &[f32], r: &[f32]) -> Result<Vec<f32>, String> {
        let spec = self.st.spec_in(l, r);
        let t = ort::value::Tensor::from_array(([1usize, 4, DIM_F, FRAMES], spec)).map_err(|e| e.to_string())?;
        let out = self.session.run(ort::inputs!["spec" => t]).map_err(|e| e.to_string())?;
        let (_, y) = out["pieces"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        Ok(self.st.spec_out(y))
    }
}

/// The STFT and its inverse around the network, as torch does them.
pub struct Stft {
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// periodic Hann (torch.hann_window)
    win: Vec<f32>,
    /// torch.istft's window-square envelope over a chunk's frames
    env: Vec<f32>,
}

impl Stft {
    pub fn new() -> Stft {
        let mut p = FftPlanner::<f32>::new();
        let win: Vec<f32> = (0..NFFT)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / NFFT as f64).cos()) as f32)
            .collect();
        let mut env = vec![0f32; (FRAMES - 1) * HOP + NFFT];
        for t in 0..FRAMES {
            for i in 0..NFFT {
                env[t * HOP + i] += win[i] * win[i];
            }
        }
        Stft { fft: p.plan_fft_forward(NFFT), ifft: p.plan_fft_inverse(NFFT), win, env }
    }

    /// torch.stft of both channels of a chunk (centred, reflect padding, not
    /// normalised), the top bin dropped: `[L.re, L.im, R.re, R.im][DIM_F][FRAMES]`.
    pub fn spec_in(&self, l: &[f32], r: &[f32]) -> Vec<f32> {
        assert!(l.len() == CHUNK && r.len() == CHUNK);
        let mut out = vec![0f32; 4 * DIM_F * FRAMES];
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut scratch = vec![Complex32::new(0.0, 0.0); self.fft.get_inplace_scratch_len()];
        for (ch, x) in [l, r].into_iter().enumerate() {
            let x = super::core::reflect(x, NFFT / 2, NFFT / 2);
            for t in 0..FRAMES {
                let s = t * HOP;
                for i in 0..NFFT {
                    buf[i] = Complex32::new(x[s + i] * self.win[i], 0.0);
                }
                self.fft.process_with_scratch(&mut buf, &mut scratch);
                for f in 0..DIM_F {
                    out[((ch * 2) * DIM_F + f) * FRAMES + t] = buf[f].re;
                    out[((ch * 2 + 1) * DIM_F + f) * FRAMES + t] = buf[f].im;
                }
            }
        }
        out
    }

    /// torch.istft of the network's output (the dropped top bin as 0, a real
    /// DC as irfft reads it): `y` `[NP][4][DIM_F][FRAMES]` → `[NP][2][CHUNK]`.
    /// A piece's two channels go through one inverse FFT (left + i·right: both
    /// are real).
    pub fn spec_out(&self, y: &[f32]) -> Vec<f32> {
        use rayon::prelude::*;
        assert_eq!(y.len(), NP * 4 * DIM_F * FRAMES);
        let scale = 1.0 / NFFT as f32;
        let i1 = Complex32::new(0.0, 1.0);
        let mut out = vec![0f32; NP * 2 * CHUNK];
        out.par_chunks_mut(2 * CHUNK).enumerate().for_each(|(p, dst)| {
            let at = |c: usize, f: usize, t: usize| y[((p * 4 + c) * DIM_F + f) * FRAMES + t];
            let mut acc = vec![Complex32::new(0.0, 0.0); self.env.len()];
            let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
            let mut scratch = vec![Complex32::new(0.0, 0.0); self.ifft.get_inplace_scratch_len()];
            for t in 0..FRAMES {
                for f in 0..DIM_F {
                    let mut a = Complex32::new(at(0, f, t), at(1, f, t));
                    let mut b = Complex32::new(at(2, f, t), at(3, f, t));
                    if f == 0 {
                        a.im = 0.0;
                        b.im = 0.0;
                    }
                    buf[f] = a + i1 * b;
                    if f > 0 {
                        buf[NFFT - f] = a.conj() + i1 * b.conj();
                    }
                }
                buf[DIM_F] = Complex32::new(0.0, 0.0);
                self.ifft.process_with_scratch(&mut buf, &mut scratch);
                let o = t * HOP;
                for i in 0..NFFT {
                    acc[o + i] += buf[i] * (self.win[i] * scale);
                }
            }
            // centre=True: the first NFFT/2 samples are the padding
            let (dl, dr) = dst.split_at_mut(CHUNK);
            for j in 0..CHUNK {
                let k = NFFT / 2 + j;
                dl[j] = acc[k].re / self.env[k];
                dr[j] = acc[k].im / self.env[k];
            }
        });
        out
    }
}

/// One chunk through a network: left and right in, `[piece][channel][CHUNK]`
/// out (the session's `Kit::chunk`; the tests give their own).
pub type Net<'a> = dyn FnMut(&[f32], &[f32]) -> Result<Vec<f32>, String> + 'a;

/// A track's kit being made: the drums source pushed in order (as the
/// separation finishes it), each chunk run as soon as its sound is there,
/// each piece's finished sound measured and let go.
pub struct KitRun {
    len: usize,
    step: usize,
    pad: usize,
    chunks: usize,
    next: usize,
    /// the drums source from track sample `in0` on, `known` samples of it in all
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    in0: usize,
    known: usize,
    /// the pieces' faded sums over the padded positions [next·step, next·step + CHUNK)
    acc: Vec<f32>,
    wsum: Vec<f32>,
    fade: Vec<f32>,
    feats: Vec<PieceFeat>,
    /// the tests' look at the finished pieces
    tap: Option<Vec<Vec<f32>>>,
}

impl KitRun {
    /// A track of `len` samples, chunks `overlap` to a chunk length (the research: 4).
    pub fn new(len: usize, overlap: usize) -> KitRun {
        let step = CHUNK / overlap;
        let pad = CHUNK - step;
        let mut fade = vec![1f32; CHUNK];
        // np.linspace(0, 1, FADE) and back
        for i in 0..FADE {
            let v = (i as f64 / (FADE - 1) as f64) as f32;
            fade[i] = v;
            fade[CHUNK - 1 - i] = v;
        }
        let on = OnsetSpec::new();
        let fs = FieldShared::new();
        KitRun {
            len,
            step,
            pad,
            chunks: (2 * pad + len) / step + 1,
            next: 0,
            in_l: Vec::new(),
            in_r: Vec::new(),
            in0: 0,
            known: 0,
            acc: vec![0f32; NP * 2 * CHUNK],
            wsum: vec![0f32; CHUNK],
            fade,
            feats: (0..NP).map(|_| PieceFeat::new(len, on.clone(), &fs)).collect(),
            tap: None,
        }
    }

    /// How far it is, 0…1.
    pub fn progress(&self) -> f32 {
        self.next as f32 / self.chunks as f32
    }

    /// The track sample chunk `i` reads up to (exclusive; the track ends it).
    fn needs(&self, i: usize) -> usize {
        (i * self.step + CHUNK).saturating_sub(self.pad).min(self.len)
    }

    /// The drums source's next samples (in order); the chunks their sound
    /// completes are run.
    pub fn push(&mut self, net: &mut Net, l: &[f32], r: &[f32]) -> Result<(), String> {
        let n = l.len().min(r.len()).min(self.len - self.known);
        self.in_l.extend_from_slice(&l[..n]);
        self.in_r.extend_from_slice(&r[..n]);
        self.known += n;
        while self.next < self.chunks && self.known >= self.needs(self.next) {
            self.run_next(net)?;
        }
        Ok(())
    }

    /// The whole source is in: the last chunks, then each piece's levels,
    /// hits and place, and which pieces play (`mix_ref_db`: the mix's loud
    /// level, `Stems::mix_db`).
    pub fn finish(mut self, net: &mut Net, mix_ref_db: f64) -> Result<KitOut, String> {
        if self.known < self.len {
            let z = vec![0f32; self.len - self.known];
            self.push(net, &z, &z)?;
        }
        while self.next < self.chunks {
            self.run_next(net)?;
        }
        let mut pieces: Vec<Piece> = self.feats.into_iter().enumerate().map(|(i, f)| f.finish(i)).collect();
        gate(&mut pieces, mix_ref_db);
        Ok(KitOut { pieces })
    }

    fn run_next(&mut self, net: &mut Net) -> Result<(), String> {
        let i = self.next;
        let start = (i * self.step) as isize - self.pad as isize;
        let cut = |v: &[f32]| -> Vec<f32> {
            (0..CHUNK as isize)
                .map(|k| {
                    let j = start + k;
                    if j >= self.in0 as isize && (j as usize) < self.known { v[j as usize - self.in0] } else { 0.0 }
                })
                .collect()
        };
        let (cl, cr) = (cut(&self.in_l), cut(&self.in_r));
        let y = net(&cl, &cr)?;
        if y.len() != NP * 2 * CHUNK {
            return Err(format!("the drum network gave {} values, not {}", y.len(), NP * 2 * CHUNK));
        }
        for s in 0..NP * 2 {
            let (a, b) = (&mut self.acc[s * CHUNK..(s + 1) * CHUNK], &y[s * CHUNK..(s + 1) * CHUNK]);
            for k in 0..CHUNK {
                a[k] += b[k] * self.fade[k];
            }
        }
        for k in 0..CHUNK {
            self.wsum[k] += self.fade[k];
        }
        self.next += 1;
        // no later chunk reaches the first `step` positions (the last chunk: none)
        let done = if self.next == self.chunks { CHUNK } else { self.step };
        self.emit(i * self.step, done);
        for s in 0..NP * 2 {
            let a = &mut self.acc[s * CHUNK..(s + 1) * CHUNK];
            a.copy_within(self.step.., 0);
            a[CHUNK - self.step..].fill(0.0);
        }
        self.wsum.copy_within(self.step.., 0);
        self.wsum[CHUNK - self.step..].fill(0.0);
        // the input no chunk reads any more
        let keep = (self.next * self.step).saturating_sub(self.pad).min(self.known);
        if keep > self.in0 {
            self.in_l.drain(..keep - self.in0);
            self.in_r.drain(..keep - self.in0);
            self.in0 = keep;
        }
        Ok(())
    }

    /// The finished padded positions [base, base + n) to the pieces' measures.
    fn emit(&mut self, base: usize, n: usize) {
        use rayon::prelude::*;
        // the track's part of them
        let a = self.pad.saturating_sub(base).min(n);
        let b = (self.pad + self.len).saturating_sub(base).min(n);
        if b <= a {
            return;
        }
        let blocks: Vec<(Vec<f32>, Vec<f32>)> = (0..NP)
            .map(|p| {
                let (l, r) = (&self.acc[(p * 2) * CHUNK..], &self.acc[(p * 2 + 1) * CHUNK..]);
                let w = |k: usize| self.wsum[k].max(1e-6);
                ((a..b).map(|k| l[k] / w(k)).collect(), (a..b).map(|k| r[k] / w(k)).collect())
            })
            .collect();
        if let Some(tap) = self.tap.as_mut() {
            for (p, (l, r)) in blocks.iter().enumerate() {
                tap[p * 2].extend_from_slice(l);
                tap[p * 2 + 1].extend_from_slice(r);
            }
        }
        self.feats.par_iter_mut().zip(blocks.par_iter()).for_each(|(f, (l, r))| f.push(l, r));
    }
}

/// A piece of the kit over the whole track.
pub struct Piece {
    pub name: &'static str,
    /// its level per frame (hop 512 — `LV_FPS` a second, frame j centred on
    /// sample 512·j): the mean power of a 2048 window, dB
    pub db: Vec<f32>,
    /// its hits, s (the piece's own detector setting)
    pub onsets: Vec<f32>,
    /// its hits at the gates' setting (inventory.py's), s
    pub gate_onsets: Vec<f32>,
    /// its place: −1 left … +1 right
    pub pan: f32,
    /// its loud level (the 99.5th percentile of `db`) against the mix's
    pub loud_db: f32,
    /// it is one of the track's instruments (the gates)
    pub plays: bool,
    /// per frame of the map (`len / 512 + 1`): how alike its channels are, 0…1
    pub coh: Vec<f32>,
    /// the ln f centre of its sound over the whole track (ln Hz)
    pub lnf: f32,
}

/// Frames a second of `Piece::db`.
pub const LV_FPS: f64 = SR / LV_HOP as f64;

/// The kit of a track: all six pieces, in the network's order.
pub struct KitOut {
    pub pieces: Vec<Piece>,
}

/// The pieces that play: within `UNDER_DB` of the loudest piece and over
/// `FLOOR_DB` of the mix's loud level, and at least `MIN_HITS` hits
/// (inventory.py `drum_objects`).
fn gate(pieces: &mut [Piece], mix_ref_db: f64) {
    let loud: Vec<f64> =
        pieces.iter().map(|p| percentile(&p.db.iter().map(|d| *d as f64 - mix_ref_db).collect::<Vec<f64>>(), 99.5)).collect();
    let top = loud.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    for (p, l) in pieces.iter_mut().zip(&loud) {
        p.loud_db = *l as f32;
        p.plays = *l >= FLOOR_DB.max(top - UNDER_DB) && p.gate_onsets.len() >= MIN_HITS;
    }
}

/// The spectrogram of the onset function: its FFT and window, shared.
#[derive(Clone)]
struct OnsetSpec {
    fft: Arc<dyn Fft<f32>>,
    /// np.hanning(1024): symmetric
    win: Arc<Vec<f32>>,
}

impl OnsetSpec {
    fn new() -> OnsetSpec {
        let win = (0..ON_N)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (ON_N - 1) as f64).cos()) as f32)
            .collect();
        OnsetSpec { fft: FftPlanner::<f32>::new().plan_fft_forward(ON_N), win: Arc::new(win) }
    }
}

/// A piece's stereo field per frame of the map (hop 512, a 2048 periodic
/// Hann centred on sample 512·j, the sound zero outside): how alike its two
/// channels are — per bin over 60 ms, weighted by energy (as analysis.rs) —
/// and the centre of its spectrum over the whole track.
struct Field {
    len: usize,
    n: usize,
    l: Vec<f32>,
    r: Vec<f32>,
    s0: usize,
    made: usize,
    fft: Arc<dyn Fft<f32>>,
    win: Arc<Vec<f32>>,
    ln_f: Arc<Vec<f32>>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    c: Vec<Complex32>,
    pl: Vec<f32>,
    pr: Vec<f32>,
    coh: Vec<f32>,
    e_sum: f64,
    ef_sum: f64,
}

/// The field's bins: 30 Hz … 16 kHz.
pub(super) const FIELD_LO: f64 = 30.0;
pub(super) const FIELD_HI: f64 = 16_000.0;

impl Field {
    fn new(len: usize, sh: &FieldShared) -> Field {
        Field {
            len,
            n: 0,
            l: Vec::new(),
            r: Vec::new(),
            s0: 0,
            made: 0,
            fft: sh.fft.clone(),
            win: sh.win.clone(),
            ln_f: sh.ln_f.clone(),
            buf: vec![Complex32::new(0.0, 0.0); LV_N],
            scratch: vec![Complex32::new(0.0, 0.0); sh.fft.get_inplace_scratch_len()],
            c: vec![Complex32::new(0.0, 0.0); LV_N / 2 + 1],
            pl: vec![0.0; LV_N / 2 + 1],
            pr: vec![0.0; LV_N / 2 + 1],
            coh: Vec::with_capacity(len / LV_HOP + 1),
            e_sum: 0.0,
            ef_sum: 0.0,
        }
    }

    fn frames(&self) -> usize {
        self.len / LV_HOP + 1
    }

    fn push(&mut self, l: &[f32], r: &[f32]) {
        self.l.extend_from_slice(l);
        self.r.extend_from_slice(r);
        self.n += l.len();
        while self.made < self.frames() && self.n >= (self.made * LV_HOP + LV_N / 2).min(self.len) {
            self.frame();
        }
        let keep = (self.made * LV_HOP).saturating_sub(LV_N / 2);
        if keep >= self.s0 + (1 << 16) {
            self.l.drain(..keep - self.s0);
            self.r.drain(..keep - self.s0);
            self.s0 = keep;
        }
    }

    fn frame(&mut self) {
        let s = self.made as isize * LV_HOP as isize - (LV_N / 2) as isize;
        for i in 0..LV_N {
            let j = s + i as isize;
            self.buf[i] = if j >= 0 && (j as usize) < self.n {
                let k = j as usize - self.s0;
                Complex32::new(self.l[k] * self.win[i], self.r[k] * self.win[i])
            } else {
                Complex32::new(0.0, 0.0)
            };
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let a = 1.0 - (-(1.0 / LV_FPS) / 0.06).exp() as f32;
        let (half, mhalf_i) = (Complex32::new(0.5, 0.0), Complex32::new(0.0, -0.5));
        let (mut e_sum, mut ec_sum) = (0f64, 0f64);
        for k in 1..LV_N / 2 {
            let f = k as f64 * SR / LV_N as f64;
            if !(FIELD_LO..=FIELD_HI).contains(&f) {
                continue;
            }
            let z = self.buf[k];
            let zc = self.buf[LV_N - k].conj();
            let (bl, br) = ((z + zc) * half, (z - zc) * mhalf_i);
            let (el, er) = (bl.norm_sqr(), br.norm_sqr());
            let x = bl * br.conj();
            if self.made == 0 {
                (self.c[k], self.pl[k], self.pr[k]) = (x, el, er);
            } else {
                let c = self.c[k];
                self.c[k] = c + (x - c) * a;
                self.pl[k] += (el - self.pl[k]) * a;
                self.pr[k] += (er - self.pr[k]) * a;
            }
            let coh = if self.c[k].re < 0.0 { 0.0 } else { (self.c[k].norm() / (self.pl[k] * self.pr[k] + 1e-12).sqrt()).min(1.0) };
            let e = (el + er) as f64;
            e_sum += e;
            ec_sum += e * coh as f64;
            self.ef_sum += e * self.ln_f[k] as f64;
        }
        self.e_sum += e_sum;
        self.coh.push(if e_sum > 0.0 { (ec_sum / e_sum) as f32 } else { 0.0 });
        self.made += 1;
    }

    fn finish(mut self) -> (Vec<f32>, f32) {
        while self.made < self.frames() {
            self.frame();
        }
        let lnf = if self.e_sum > 0.0 { (self.ef_sum / self.e_sum) as f32 } else { 0.0 };
        (self.coh, lnf)
    }
}

/// The field's FFT, window and bin frequencies, shared by the pieces.
#[derive(Clone)]
struct FieldShared {
    fft: Arc<dyn Fft<f32>>,
    win: Arc<Vec<f32>>,
    ln_f: Arc<Vec<f32>>,
}

impl FieldShared {
    fn new() -> FieldShared {
        let win = (0..LV_N).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / LV_N as f64).cos()) as f32).collect();
        let ln_f = (0..=LV_N / 2).map(|k| ((k as f64 * SR / LV_N as f64).max(20.0) as f32).ln()).collect();
        FieldShared { fft: FftPlanner::<f32>::new().plan_fft_forward(LV_N), win: Arc::new(win), ln_f: Arc::new(ln_f) }
    }
}

/// What is measured of one piece, as its finished sound comes.
struct PieceFeat {
    len: usize,
    /// samples in
    n: usize,
    /// per `LV_HOP` samples: the sum of the mean power (l² + r²)/2
    hop_pow: Vec<f64>,
    abs_l: f64,
    abs_r: f64,
    /// the mono sound (l + r)/2 from track sample `mono0` on
    mono: Vec<f32>,
    mono0: usize,
    /// onset frames made; frame i reads [i·ON_HOP − ON_N/2, i·ON_HOP + ON_N/2)
    made: usize,
    flux: Vec<f32>,
    power: Vec<f32>,
    /// the last frame's ln(1 + 100·|X|)
    prev: Vec<f32>,
    on: OnsetSpec,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    field: Field,
}

impl PieceFeat {
    fn new(len: usize, on: OnsetSpec, fs: &FieldShared) -> PieceFeat {
        let scratch = vec![Complex32::new(0.0, 0.0); on.fft.get_inplace_scratch_len()];
        PieceFeat {
            len,
            n: 0,
            hop_pow: vec![0.0; (len + LV_HOP - 1) / LV_HOP],
            abs_l: 0.0,
            abs_r: 0.0,
            mono: Vec::new(),
            mono0: 0,
            made: 0,
            flux: Vec::with_capacity(len / ON_HOP + 1),
            power: Vec::with_capacity(len / ON_HOP + 1),
            prev: vec![0.0; ON_BINS],
            on,
            buf: vec![Complex32::new(0.0, 0.0); ON_N],
            scratch,
            field: Field::new(len, fs),
        }
    }

    /// kit.py `_spec`'s frames: len / 256 + 1.
    fn onset_frames(&self) -> usize {
        self.len / ON_HOP + 1
    }

    fn push(&mut self, l: &[f32], r: &[f32]) {
        for i in 0..l.len() {
            let (a, b) = (l[i], r[i]);
            let t = self.n + i;
            self.hop_pow[t / LV_HOP] += (a as f64 * a as f64 + b as f64 * b as f64) / 2.0;
            self.abs_l += a.abs() as f64;
            self.abs_r += b.abs() as f64;
            self.mono.push((a + b) / 2.0);
        }
        self.n += l.len();
        self.frames();
        self.field.push(l, r);
    }

    /// The onset frames whose sound is all in.
    fn frames(&mut self) {
        while self.made < self.onset_frames() {
            let end = (self.made * ON_HOP + ON_N / 2).min(self.len);
            if self.n < end {
                break;
            }
            self.frame();
        }
        let keep = (self.made * ON_HOP).saturating_sub(ON_N / 2);
        if keep >= self.mono0 + (1 << 16) {
            self.mono.drain(..keep - self.mono0);
            self.mono0 = keep;
        }
    }

    fn frame(&mut self) {
        let s = self.made as isize * ON_HOP as isize - (ON_N / 2) as isize;
        for k in 0..ON_N {
            let j = s + k as isize;
            let v = if j >= 0 && (j as usize) < self.n { self.mono[j as usize - self.mono0] } else { 0.0 };
            self.buf[k] = Complex32::new(v * self.on.win[k], 0.0);
        }
        self.on.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let (mut d, mut p) = (0f64, 0f64);
        for k in 0..ON_BINS {
            let m = self.buf[k].norm();
            let lg = (100.0 * m).ln_1p();
            if self.made > 0 {
                d += (lg - self.prev[k]).max(0.0) as f64;
            }
            self.prev[k] = lg;
            p += (m * m) as f64;
        }
        self.flux.push(d as f32);
        self.power.push(10.0 * (p as f32 + 1e-20).log10());
        self.made += 1;
    }

    /// Piece `i`'s measures over the whole track.
    fn finish(self, i: usize) -> Piece {
        let nf = self.hop_pow.len();
        let db: Vec<f32> = (0..nf)
            .map(|j| {
                let s: f64 = (j as isize - 2..=j as isize + 1)
                    .filter(|k| *k >= 0 && (*k as usize) < nf)
                    .map(|k| self.hop_pow[k as usize])
                    .sum();
                (10.0 * (s / LV_N as f64 + 1e-20).log10()) as f32
            })
            .collect();
        let n = self.len.max(1) as f64;
        let (ml, mr) = (self.abs_l / n, self.abs_r / n);
        let pan = ((mr - ml) / ((ml + mr) / 2.0 * 2.0 + 1e-12)).clamp(-1.0, 1.0) as f32;
        let hits = onsets(&self.flux, &self.power, DETECT[i]);
        let gate_onsets = onsets(&self.flux, &self.power, GATE_DETECT);
        let (coh, lnf) = self.field.finish();
        Piece { name: PIECES[i], db, onsets: hits, gate_onsets, pan, loud_db: 0.0, plays: false, coh, lnf }
    }
}

/// kit.py `onsets` on a piece's onset function (per frame its flux and power
/// in dB), with its (δ, floor dB): a flux peak over its local mean (0.2 s) +
/// δ (the flux against its 99.5th percentile), the only one within ±`ON_GAP`,
/// and the hit's level (the most power in the 50 ms from it) within the floor
/// of the 95th percentile of the peaks' levels. Seconds.
fn onsets(flux: &[f32], power: &[f32], (delta, floor): (f32, f32)) -> Vec<f32> {
    let n = flux.len();
    if n == 0 {
        return Vec::new();
    }
    let fr = SR / ON_HOP as f64;
    let top = percentile32(flux, 99.5) + 1e-9;
    let d: Vec<f32> = flux.iter().map(|v| v / top).collect();
    let loc = uniform_filter(&d, (0.2 * fr) as usize | 1);
    let mx = max_filter(&d, (2.0 * ON_GAP * fr) as usize | 1, 0);
    let w = (0.05 * fr) as usize;
    let lv = max_filter(power, w | 1, -((w / 2) as isize));
    let cand: Vec<usize> = (0..n).filter(|&i| d[i] == mx[i] && d[i] > loc[i] + delta).collect();
    if cand.is_empty() {
        return Vec::new();
    }
    let loud = percentile32(&cand.iter().map(|&i| lv[i]).collect::<Vec<f32>>(), 95.0);
    cand.into_iter()
        .filter(|&i| lv[i] >= loud - floor)
        .map(|i| (i * ON_HOP) as f64 / SR)
        .map(|t| t as f32)
        .collect()
}

/// scipy.ndimage's "reflect" extension (half-sample symmetric: d c b a | a b c d).
fn refl(j: isize, n: usize) -> usize {
    let n = n as isize;
    let mut j = j;
    loop {
        if j < 0 {
            j = -j - 1;
        } else if j >= n {
            j = 2 * n - 1 - j;
        } else {
            return j as usize;
        }
    }
}

/// scipy.ndimage.uniform_filter1d(x, size) (mode "reflect"), summed in f64.
pub(super) fn uniform_filter(x: &[f32], size: usize) -> Vec<f32> {
    let n = x.len();
    let h = (size / 2) as isize;
    let mut s: f64 = (-h..size as isize - h).map(|k| x[refl(k, n)] as f64).sum();
    let mut out = Vec::with_capacity(n);
    for i in 0..n as isize {
        out.push((s / size as f64) as f32);
        s += x[refl(i + size as isize - h, n)] as f64 - x[refl(i - h, n)] as f64;
    }
    out
}

/// scipy.ndimage.maximum_filter1d(x, size, origin=origin) (mode "reflect"):
/// output i is the most of x[i − size/2 − origin …] over `size` values.
pub(super) fn max_filter(x: &[f32], size: usize, origin: isize) -> Vec<f32> {
    let n = x.len();
    let h = (size / 2) as isize;
    (0..n as isize)
        .map(|i| (0..size as isize).map(|k| x[refl(i - h - origin + k, n)]).fold(f32::NEG_INFINITY, f32::max))
        .collect()
}

/// numpy.percentile of f32s: numpy answers in f32 for them, and its lerp
/// goes from the nearer end (the kit's detector compares against these).
pub(super) fn percentile32(v: &[f32], q: f64) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pos = q / 100.0 * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    let t = (pos - lo as f64) as f32;
    if t >= 0.5 { s[hi] - (s[hi] - s[lo]) * (1.0 - t) } else { s[lo] + (s[hi] - s[lo]) * t }
}

#[cfg(test)]
mod tests {
    use super::super::stems::frame_db;
    use super::*;

    fn noise(seed: u32) -> impl FnMut() -> f32 {
        let mut s = seed;
        move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }

    /// A network that hands the chunk back as piece 0 (through the STFT and
    /// its inverse) and silence as the others.
    fn echo(st: &Stft) -> impl FnMut(&[f32], &[f32]) -> Result<Vec<f32>, String> + '_ {
        move |l, r| {
            let spec = st.spec_in(l, r);
            let mut y = vec![0f32; NP * 4 * DIM_F * FRAMES];
            y[..spec.len()].copy_from_slice(&spec);
            Ok(st.spec_out(&y))
        }
    }

    #[test]
    fn chunks_and_the_inverse_give_the_sound_back() {
        let st = Stft::new();
        let len = CHUNK * 3 + 12_345;
        // tones well under the top bin (the network never sees it: a sound
        // there would not come back), slowly swelling
        let tones = |f: &[(f64, f64)], n: usize| -> f32 {
            let t = n as f64 / SR;
            f.iter().map(|(hz, ph)| (2.0 * std::f64::consts::PI * hz * t + ph).sin()).sum::<f64>() as f32
                * (0.1 + 0.05 * (t * 1.3).sin() as f32)
        };
        let l: Vec<f32> = (0..len).map(|n| tones(&[(110.0, 0.1), (440.0, 1.0), (1234.5, 2.0), (5000.0, 0.3), (12_000.0, 1.7)], n)).collect();
        let r: Vec<f32> = (0..len).map(|n| tones(&[(220.0, 0.5), (987.0, 1.1), (7777.0, 2.9)], n)).collect();
        for overlap in [4, 2] {
            let mut run = KitRun::new(len, overlap);
            run.tap = Some(vec![Vec::new(); NP * 2]);
            let mut net = echo(&st);
            // pushed in uneven pieces, as segments come
            let mut at = 0;
            for (i, n) in [1000usize, 70_000, 5, 200_000, 3].iter().cycle().enumerate() {
                if at >= len || i > 100 {
                    break;
                }
                let e = (at + n).min(len);
                run.push(&mut net, &l[at..e], &r[at..e]).unwrap();
                at = e;
            }
            let tap = run.tap.take().unwrap();
            // finish without the tap: the rest is checked by the measures
            let mut run2 = KitRun::new(len, overlap);
            run2.tap = Some(vec![Vec::new(); NP * 2]);
            run2.push(&mut net, &l, &r).unwrap();
            while run2.next < run2.chunks {
                run2.run_next(&mut net).unwrap();
            }
            let all = run2.tap.take().unwrap();
            assert_eq!(all[0].len(), len, "overlap {overlap}");
            let err = |a: &[f32], b: &[f32]| a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()));
            // inside; at the track's ends the sound starts and stops at once,
            // and that step's top-bin part is not given back
            let inner = 4096..len - 4096;
            let (el, er) = (err(&all[0][inner.clone()], &l[inner.clone()]), err(&all[1][inner.clone()], &r[inner]));
            assert!(el < 3e-6 && er < 3e-6, "overlap {overlap}: err inside {el} {er}");
            assert!(err(&all[0], &l) < 5e-4 && err(&all[1], &r) < 5e-4, "overlap {overlap}: at the ends {}", err(&all[0], &l));
            assert!(all[2..].iter().all(|v| v.iter().all(|x| *x == 0.0)));
            // streaming gives the same as all at once, as far as it got
            assert!(tap[0].len() <= len && tap[0].len() > len - CHUNK * 2);
            assert_eq!(&tap[0][..], &all[0][..tap[0].len()]);
        }
    }

    #[test]
    fn a_click_train_gives_its_hits_and_the_gates_keep_what_plays() {
        let st = Stft::new();
        let mut rnd = noise(3);
        let len = 44_100 * 12;
        let at: Vec<f64> = (0..24).map(|k| 0.6 + 0.45 * k as f64).collect();
        let click = |gain: f32, rnd: &mut dyn FnMut() -> f32| -> Vec<f32> {
            let mut x = vec![0f32; len];
            for t in &at {
                let s = (t * SR) as usize;
                for i in 0..1500 {
                    x[s + i] += rnd() * gain * (-(i as f32) / 250.0).exp();
                }
            }
            x
        };
        let kick = click(0.8, &mut rnd);
        let quiet = click(0.8e-3, &mut rnd); // −60 dB: under the gate
        let mut net = move |l: &[f32], _r: &[f32]| -> Result<Vec<f32>, String> {
            // the "network": piece 0 = the chunk, piece 1 = the chunk −60 dB, piece 2 = 3 hits only
            let spec = st.spec_in(l, l);
            let mut y = vec![0f32; NP * 4 * DIM_F * FRAMES];
            let n = spec.len();
            y[..n].copy_from_slice(&spec);
            for (i, v) in spec.iter().enumerate() {
                y[n + i] = v * 1e-3;
            }
            let _ = st;
            Ok(st.spec_out(&y))
        };
        let mut run = KitRun::new(len, OVERLAP);
        run.push(&mut net, &kick, &kick).unwrap();
        let out = run.finish(&mut net, frame_db(&kick, &kick).iter().cloned().fold(f64::NEG_INFINITY, f64::max)).unwrap();
        let k = &out.pieces[0];
        assert_eq!(k.onsets.len(), at.len(), "{:?}", k.onsets);
        for (o, t) in k.onsets.iter().zip(&at) {
            assert!((*o as f64 - t).abs() < 0.02, "hit at {o}, click at {t}");
        }
        assert!(k.plays && k.pan.abs() < 1e-3);
        // the same hits 60 dB down: found, but the piece is not one of the instruments
        assert!(!out.pieces[1].plays && out.pieces[1].loud_db < k.loud_db - 50.0);
        // silent pieces: no hits, not playing
        assert!(out.pieces[2..].iter().all(|p| p.onsets.is_empty() && !p.plays));
        let _ = quiet;
    }

    #[test]
    fn the_filters_and_percentiles_are_numpys_and_scipys() {
        let d = [0f32, 3.0, 1.0, 0.0, 5.0, 2.0, 0.0, 0.0, 4.0, 1.0];
        let near = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        // filters_ref.py
        assert!(near(&uniform_filter(&d, 3), &[1.0, 1.3333334, 1.3333334, 2.0, 2.3333333, 2.3333333, 0.6666667, 1.3333334, 1.6666666, 2.0]));
        assert!(near(&uniform_filter(&d, 5), &[1.4, 0.8, 1.8, 2.2, 1.6, 1.4, 2.2, 1.4, 1.2, 2.0]));
        assert!(near(
            &uniform_filter(&d, 35),
            &[1.6285714, 1.7142857, 1.5714285, 1.5142857, 1.6, 1.6285714, 1.5142857, 1.6285714, 1.6571429, 1.5428572]
        ));
        assert_eq!(max_filter(&d, 3, 0), vec![3.0, 3.0, 3.0, 5.0, 5.0, 5.0, 2.0, 4.0, 4.0, 4.0]);
        assert_eq!(max_filter(&d, 9, -4), vec![5.0, 5.0, 5.0, 5.0, 5.0, 4.0, 4.0, 5.0, 5.0, 5.0]);
        assert_eq!(max_filter(&d, 3, -1), vec![3.0, 3.0, 5.0, 5.0, 5.0, 2.0, 4.0, 4.0, 4.0, 4.0]);
        let p: Vec<f32> = [0.0, 50.0, 95.0, 99.5, 100.0].iter().map(|q| percentile32(&d, *q)).collect();
        assert!(near(&p, &[0.0, 1.0, 4.55, 4.955, 5.0]), "{p:?}");
        let d64: Vec<f64> = d.iter().map(|x| *x as f64).collect();
        assert!((percentile(&d64, 99.5) - 4.955).abs() < 1e-12);
    }

    // ---- against the research's Python (`export_drumsep.py`, refs in AURA_KIT_REFS) ----

    fn npy_f32(path: &std::path::Path) -> Vec<f32> {
        let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
        let head = String::from_utf8_lossy(&b[10..10 + hl]).to_string();
        assert!(head.contains("'<f4'") && head.contains("'fortran_order': False"), "{head}");
        b[10 + hl..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn refs() -> Option<std::path::PathBuf> {
        std::env::var_os("AURA_KIT_REFS").map(Into::into)
    }

    fn max_err(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()))
    }

    /// Our STFT = torch.stft, our inverse = torch.istft (a chunk of 80sRock's drums).
    #[test]
    #[ignore]
    fn the_stft_and_its_inverse_are_torchs() {
        let Some(d) = refs() else { return };
        let st = Stft::new();
        let chunk = npy_f32(&d.join("chunk.npy"));
        let want = npy_f32(&d.join("spec.npy"));
        let got = st.spec_in(&chunk[..CHUNK], &chunk[CHUNK..]);
        let peak = want.iter().fold(0f32, |m, v| m.max(v.abs()));
        let e = max_err(&got, &want);
        eprintln!("spec_in: max err {e:e}, peak {peak}");
        assert!(e < 1e-5 * peak.max(1.0));
        let core = npy_f32(&d.join("core.npy"));
        let want = npy_f32(&d.join("wave.npy"));
        let got = st.spec_out(&core);
        let e = max_err(&got, &want);
        eprintln!("spec_out: max err {e:e}");
        assert!(e < 1e-5);
    }

    /// From the torch pieces: each piece's levels and hits, and the gates = the
    /// Python's (8 s: `levels.npy` / `refs.json`; 25 s: `levels25.npy` /
    /// `refs25.json` on `..\torch-80sRock-5-25.npy`).
    #[test]
    #[ignore]
    fn the_measures_are_the_pythons() {
        let Some(d) = refs() else { return };
        for (pieces, levels, json, mix) in [
            (d.join("pieces.npy"), "levels.npy", "refs.json", "mix.npy"),
            (d.join("..").join("torch-80sRock-5-25.npy"), "levels25.npy", "refs25.json", "mix25.npy"),
        ] {
            let pr = npy_f32(&pieces);
            let len = pr.len() / (NP * 2);
            let want_lv = npy_f32(&d.join(levels));
            let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join(json)).unwrap()).unwrap();
            let mix = npy_f32(&d.join(mix));
            assert_eq!(mix.len(), 2 * len);
            let fdb = frame_db(&mix[..len], &mix[len..]);
            let ref_db = percentile(&fdb, 99.0);
            assert!((ref_db - doc["ref_db"].as_f64().unwrap()).abs() < 1e-6, "ref_db {ref_db}");
            let on = OnsetSpec::new();
            let fs = FieldShared::new();
            let mut ps = Vec::new();
            for p in 0..NP {
                let mut f = PieceFeat::new(len, on.clone(), &fs);
                let (l, r) = (&pr[(p * 2) * len..(p * 2 + 1) * len], &pr[(p * 2 + 1) * len..(p * 2 + 2) * len]);
                // in uneven blocks, as the chunks finish them
                let mut at = 0;
                while at < len {
                    let e = (at + 32_640 + (at % 7) * 100).min(len);
                    f.push(&l[at..e], &r[at..e]);
                    at = e;
                }
                ps.push(f.finish(p));
            }
            gate(&mut ps, ref_db);
            for (p, pc) in ps.iter().enumerate() {
                let w = &want_lv[p * pc.db.len()..(p + 1) * pc.db.len()];
                let e = pc.db.iter().zip(w).filter(|(_, b)| **b > -100.0).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
                let want_on: Vec<f64> = doc["onsets"][PIECES[p]].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
                eprintln!(
                    "{json} {:6} levels max diff {e:.2e} dB; hits {} (python {}); loud {:.2} plays {}",
                    PIECES[p], pc.gate_onsets.len(), want_on.len(), pc.loud_db, pc.plays
                );
                assert!(e < 1e-3, "{} levels differ by {e} dB", PIECES[p]);
                assert_eq!(pc.gate_onsets.len(), want_on.len(), "{}: {:?} vs {:?}", PIECES[p], pc.gate_onsets, want_on);
                for (a, b) in pc.gate_onsets.iter().zip(&want_on) {
                    assert!((*a as f64 - b).abs() < 1e-4, "{}: hit {a} vs {b}", PIECES[p]);
                }
            }
            let plays: Vec<&str> = ps.iter().filter(|p| p.plays).map(|p| p.name).collect();
            let want: Vec<&str> = doc["objects"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
            assert_eq!(plays, want);
            for o in doc["objects"].as_array().unwrap() {
                let p = ps.iter().find(|p| p.name == o["name"].as_str().unwrap()).unwrap();
                assert!((p.pan as f64 - o["pan"].as_f64().unwrap()).abs() < 1e-4, "{} pan {}", p.name, p.pan);
                assert!((p.loud_db as f64 - o["db_p99_5"].as_f64().unwrap()).abs() < 1e-3, "{} loud {}", p.name, p.loud_db);
            }
        }
    }

    /// The acceptance on MDB (contract §8) through this very path on the GPU:
    /// AURA_KIT_MDB = the folder `mdb_accept.py prep` wrote (each song's
    /// HTDemucs drums and the mixes' loud levels), AURA_KIT_MODEL,
    /// AURA_SPATIAL_PACK_DIR; writes `out.json` there (each piece's hits
    /// shown, at the gates' setting, whether it plays) for `mdb_accept.py score`.
    #[test]
    #[ignore]
    fn mdb_through_the_app() {
        let (Some(d), Some(model), Some(pack)) =
            (std::env::var_os("AURA_KIT_MDB"), std::env::var_os("AURA_KIT_MODEL"), std::env::var_os("AURA_SPATIAL_PACK_DIR"))
        else {
            return;
        };
        let d = std::path::PathBuf::from(d);
        let refs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("refs.json")).unwrap()).unwrap();
        let mut kit = Kit::open(Path::new(&pack), Path::new(&model), false).expect("the kit on the GPU");
        let mut out = serde_json::Map::new();
        let t = std::time::Instant::now();
        for (name, r) in refs.as_object().unwrap() {
            let x = npy_f32(&d.join(format!("{name}-drums.npy")));
            let len = x.len() / 2;
            let mut run = KitRun::new(len, OVERLAP);
            let mut net = |l: &[f32], r: &[f32]| kit.chunk(l, r);
            run.push(&mut net, &x[..len], &x[len..]).unwrap();
            let k = run.finish(&mut net, r.as_f64().unwrap()).unwrap();
            let ps: serde_json::Map<String, serde_json::Value> = k
                .pieces
                .iter()
                .map(|p| (p.name.to_string(), serde_json::json!({"onsets": p.onsets, "gate_onsets": p.gate_onsets, "plays": p.plays})))
                .collect();
            out.insert(name.clone(), ps.into());
        }
        eprintln!("{} songs in {:.1} s", out.len(), t.elapsed().as_secs_f64());
        std::fs::write(d.join("out.json"), serde_json::Value::Object(out).to_string()).unwrap();
    }

    /// The whole way with the pack's runtime on the CPU (AURA_KIT_MODEL = the
    /// ONNX, AURA_SPATIAL_PACK_DIR = a pack with the runtime): one chunk = the
    /// torch core's, and 8 s of drums give the Python's hits.
    #[test]
    #[ignore]
    fn drumsep_through_the_runtime_is_torchs() {
        let (Some(d), Some(model), Some(pack)) =
            (refs(), std::env::var_os("AURA_KIT_MODEL"), std::env::var_os("AURA_SPATIAL_PACK_DIR"))
        else {
            return;
        };
        if std::env::var_os("AURA_SPATIAL_CPU").is_none() && std::env::var_os("AURA_KIT_GPU_OK").is_none() {
            eprintln!("skipped: set AURA_SPATIAL_CPU=1 (or AURA_KIT_GPU_OK=1 when the GPU may be used)");
            return;
        }
        let mut kit = Kit::open(Path::new(&pack), Path::new(&model), true).unwrap();
        let chunk = npy_f32(&d.join("chunk.npy"));
        let want = npy_f32(&d.join("wave.npy"));
        let t = std::time::Instant::now();
        let got = kit.chunk(&chunk[..CHUNK], &chunk[CHUNK..]).unwrap();
        let e = max_err(&got, &want);
        eprintln!("one chunk on the {}: {:?}, max err {e:e}", if kit.gpu { "GPU" } else { "CPU" }, t.elapsed());
        assert!(e < 2e-3, "err {e}");
        let x = npy_f32(&d.join("x.npy"));
        let len = x.len() / 2;
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("refs.json")).unwrap()).unwrap();
        let mut run = KitRun::new(len, 4);
        let mut net = |l: &[f32], r: &[f32]| kit.chunk(l, r);
        run.push(&mut net, &x[..len], &x[len..]).unwrap();
        let out = run.finish(&mut net, doc["ref_db"].as_f64().unwrap()).unwrap();
        for p in &out.pieces {
            let want: Vec<f64> = doc["onsets"][p.name].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let same = p.gate_onsets.iter().filter(|a| want.iter().any(|b| (**a as f64 - b).abs() < 0.006)).count();
            eprintln!("{:6} hits {} (python {}), {} within one hop", p.name, p.gate_onsets.len(), want.len(), same);
            assert!(same + 1 >= want.len() && p.gate_onsets.len() <= want.len() + 1, "{}: {:?} vs {:?}", p.name, p.gate_onsets, want);
        }
    }
}
