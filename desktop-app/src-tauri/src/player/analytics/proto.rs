//! Wire frame encoders for all five PROTOCOL.md frames (AAN1, AAN2, AAN3, AAWT, AAST).
//!
//! All encoders return owned `Vec<u8>`.  Decoders live only in the `golden`
//! test submodule and are used exclusively to round-trip the checked-in .bin files.
//!
//! analytics: owner A

use std::sync::Arc;

pub const N_METRICS: usize = 26;
/// Packed provenance-nibble bytes for N_METRICS = 26 metrics in AAN1.
pub const N_PROV_BYTES: usize = (N_METRICS + 1) / 2; // 13
/// Packed provenance-nibble bytes per tap in AAN2 (32-slot array → 16 bytes).
pub const N_PROV_BYTES_AAN2: usize = 16;
/// Total metric-array length (IDs 0–25 valid; 26–31 are reserved NaN slots).
pub const METRIC_ARRAY_LEN: usize = 32;

// ─── Metric IDs (PROTOCOL.md §3) ────────────────────────────────────────────

pub mod mid {
    pub const LUFS_I:         usize = 0;
    pub const LUFS_S_LIVE:    usize = 1;
    pub const LUFS_M_LIVE:    usize = 2;
    pub const LRA:            usize = 3;
    pub const TP_EBUR:        usize = 4;
    pub const TP_ENGINE:      usize = 5;
    pub const SP:             usize = 6;
    pub const PEAK_AT:        usize = 7;
    pub const DR:             usize = 8;
    pub const RMS:            usize = 9;
    pub const RMS_TOP20:      usize = 10;
    pub const GAIN_18LUFS:    usize = 11;
    pub const CLIPS_GE2:      usize = 12;
    pub const CLIPS_GE17:     usize = 13;
    pub const CLIPS_OUT_X1:   usize = 14;
    pub const TP_OVER0_EVENTS:usize = 15;
    pub const DC_OFFSET:      usize = 16;
    pub const ULTRA_PEAK:     usize = 17;
    pub const ULTRA_RMS:      usize = 18;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const ULTRA_EVENTS:   usize = 19;
    pub const INFRA_RMS:      usize = 20;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const SUB_REMOVED:    usize = 21;
    pub const PLR:            usize = 22;
    pub const STEREO_CORR:    usize = 23;
    pub const EFF_BW:         usize = 24;
    pub const COVERAGE_O:     usize = 25;
}

// ─── Tile enums and cache key ────────────────────────────────────────────────

/// Which signal tap a tile belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileSrc { S, B, O }

impl TileSrc {
    /// Wire byte value (0/1/2).
    #[inline]
    pub fn flag(self) -> u8 {
        match self { TileSrc::S => 0, TileSrc::B => 1, TileSrc::O => 2 }
    }
    /// Decode a wire byte.
    pub fn from_flag(f: u8) -> Option<Self> {
        match f { 0 => Some(Self::S), 1 => Some(Self::B), 2 => Some(Self::O), _ => None }
    }
}

/// Which kind of tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
pub enum TileKind { Wave, Spec, Raw }

/// Cache/lookup key for one tile.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub src: TileSrc,
    pub kind: TileKind,
    pub lod: u8,
    pub idx: u32,
}

/// Pre-decimation chain-response data stored per subject.
///
/// The route handler decimates on the fly (O(n_pts)) from these full-resolution
/// arrays; no FFT or heavy allocation needed at request time.
pub struct ChainRespData {
    pub chain_rev: u32,
    /// Full-resolution filter response in dBr; length = N_FFT/2 + 1.
    pub h_dbr: Vec<f64>,
    /// Full-resolution output PSD in dBFS; same length.
    pub o_dbfs: Vec<f64>,
    /// Nyquist frequency in Hz (= source_rate / 2).
    pub f_max_hz: f32,
}

// ─── Provenance codes (PROTOCOL.md §4) ──────────────────────────────────────

pub mod prov {
    pub const UNAVAIL:    u8 = 0;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const ANALYTIC:   u8 = 1;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const FORECAST:   u8 = 2;
    pub const MEASURED:   u8 = 3;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const UNCHANGED:  u8 = 4;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const HP_PENDING: u8 = 5;
    #[allow(dead_code)] // the full protocol table (PROTOCOL.md); the JS side decodes every value
    pub const STALE:      u8 = 6;
    pub const COMPUTING:  u8 = 7;
}

// ─── Internal byte writer ────────────────────────────────────────────────────

struct W(Vec<u8>);

impl W {
    fn new(cap: usize) -> Self { W(Vec::with_capacity(cap)) }
    fn u8(&mut self, v: u8)  { self.0.push(v); }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn f32(&mut self, v: f32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn f64(&mut self, v: f64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn raw(&mut self, v: &[u8]) { self.0.extend_from_slice(v); }
    fn finish(self) -> Vec<u8> { self.0 }
    #[cfg(debug_assertions)]
    fn len(&self) -> usize { self.0.len() }
}

// ─── Provenance nibble packing ───────────────────────────────────────────────

/// Pack the first `n` nibble codes from `codes` into `out_bytes` bytes.
///
/// Metric i occupies nibble i of the packed output:
///   byte  = i >> 1
///   bits  = (i & 1) * 4 … +3   (little-nibble-first, per PROTOCOL.md §4 rule 5)
///
/// Bytes beyond ceil(n/2) are zero (padding for AAN2's 16-byte slots).
fn pack_prov(codes: &[u8; 32], n: usize, out_bytes: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_bytes];
    for i in 0..n {
        out[i >> 1] |= (codes[i] & 0x0F) << ((i & 1) * 4);
    }
    out
}

// ─── Input structs ───────────────────────────────────────────────────────────

/// Input to `encode_aan1`.  All metric arrays are indexed 0–31; only 0–25 are
/// written on the wire.  Indices 26–31 are ignored.
pub struct LiveFrame {
    /// Bit 0 = hp_active, 1 = b_ready, 2 = conv_exists, 3 = xtc_active,
    /// 4 = stale, 5 = isp_out_active.
    pub flags: u8,
    pub seq: u32,
    pub track_id: u64,
    pub chain_rev: u32,
    pub coverage_pct: u8,
    /// log₂ of the bytes per spectrogram column (the column's bands).
    pub n_fft_bins_log2: u8,
    pub spec_floor_neg: u8,
    /// log₂ of the column hop in output samples (0 = not sent: older frames).
    pub spec_hop_log2: u8,
    /// Live metric values (f64; NaN = UNAVAIL).  Encoded as f32 on the wire.
    pub live_metrics: [f64; 32],
    /// Provenance nibble codes (0–7).  Only indices 0–25 packed on the wire.
    pub prov_codes: [u8; 32],
    /// Loudness history points since `since`.
    pub loud_pts: Vec<LoudPt>,
    /// Spectrogram columns since `since`.  Each inner Vec must have exactly
    /// `1 << n_fft_bins_log2` bytes.
    pub spec_cols: Vec<Vec<u8>>,

    // analytics: backward-compat SB-EXT appended after spec_cols (PROTOCOL.md §SB-EXT)
    // JS checks byteLength before reading; NaN = UNAVAIL.
    pub s_lufs_m: f32,
    pub s_lufs_s: f32,
    pub s_tp:     f32,
    pub b_lufs_m: f32,
    pub b_lufs_s: f32,
    pub b_tp:     f32,
    /// Packed provenance: bits 0-3 = S prov code, bits 4-7 = B prov code.
    pub sb_prov:  u8,
    /// On a stream: the STRM tail after SB-EXT (stream.rs); empty else.
    pub strm: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LoudPt {
    pub time_s: f32,
    pub lufs_s: f32,
    pub lufs_m: f32,
}

/// Input to `encode_aan2`.
#[derive(Clone)]
pub struct TrackSummary {
    pub rev: u32,
    pub chain_rev: u32,
    /// Total track duration in seconds (f32).  Written at offset 16.
    pub duration_s: f32,
    /// Whole-track S/B/O metrics, indices 0–31; 26–31 must be NaN.
    pub s_metrics: [f64; 32],
    pub b_metrics: [f64; 32],
    pub o_metrics: [f64; 32],
    /// Provenance nibble codes per tap, indices 0–31.
    pub s_prov: [u8; 32],
    pub b_prov: [u8; 32],
    pub o_prov: [u8; 32],
    pub dr_s: Vec<DrChannel>,
    pub dr_o: Vec<DrChannel>,
    /// Short-term loudness histogram, must be 256 bins.
    pub hist_s: Vec<f32>,
    pub hist_b: Vec<f32>,
    pub dr_blocks_s: Vec<DrBlock>,
    pub clips_s: Vec<ClipEvent>,
    pub over_o: Vec<OverEvent>,
    pub chain_str: String,
    pub chain_marks: Vec<ChainMark>,
    pub lufs_s_series_s: Vec<f32>,
    pub lufs_m_series_s: Vec<f32>,
    pub lufs_s_series_b: Vec<f32>,
    pub lufs_m_series_b: Vec<f32>,
    pub n_lods: u8,
    pub n_wave_channels: u8,
    pub tile_samples: u16,
    pub wave_tile_counts_s: Vec<u32>,
    pub wave_tile_counts_o: Vec<u32>,
    pub spec_tile_counts_s: Vec<u32>,
    /// The analysed signal's sample rate (the spectrogram tiles span 0 to
    /// its Nyquist). 0 = unknown: then the frame ends before it.
    pub src_rate: u32,
    /// The spectrogram tiles' hop in samples of that rate: column c stands
    /// for samples [c·hop, (c+1)·hop). 0 = unknown (the frame ends before it).
    pub spec_hop: u32,
    /// True peak (dBTP, both channels) of each 100 ms block of S and of B,
    /// on the loudness series' grid. Empty = not sent.
    pub tp100_s: Vec<f32>,
    pub tp100_b: Vec<f32>,
    /// The spectrogram tiles of S, B and O (in that order; empty = not
    /// sent): all on the S tiles' time grid.
    pub spec_info: Vec<SpecInfo>,
    /// O's loudness series from the O pass, as S's (one count, index i =
    /// the window ending at (i + 4) × 100 ms); empty while it runs.
    pub lufs_s_series_o: Vec<f32>,
    pub lufs_m_series_o: Vec<f32>,
}

/// One signal's spectrogram tiles: the pass that made them (its tiles carry
/// the low byte), how many are done of how many, and their bands (512, the
/// output's more: its bands go on above the source's Nyquist at the same
/// step).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecInfo {
    pub gen: u32,
    pub ready: u32,
    pub total: u32,
    pub n_bins: u16,
}

impl Default for TrackSummary {
    fn default() -> Self {
        Self {
            rev: 0,
            chain_rev: 0,
            duration_s: 0.0,
            s_metrics: [f64::NAN; 32],
            b_metrics: [f64::NAN; 32],
            o_metrics: [f64::NAN; 32],
            s_prov: [prov::UNAVAIL; 32],
            b_prov: [prov::UNAVAIL; 32],
            o_prov: [prov::UNAVAIL; 32],
            dr_s: Vec::new(),
            dr_o: Vec::new(),
            hist_s: Vec::new(),
            hist_b: Vec::new(),
            dr_blocks_s: Vec::new(),
            clips_s: Vec::new(),
            over_o: Vec::new(),
            chain_str: String::new(),
            chain_marks: Vec::new(),
            lufs_s_series_s: Vec::new(),
            lufs_m_series_s: Vec::new(),
            lufs_s_series_b: Vec::new(),
            lufs_m_series_b: Vec::new(),
            n_lods: 0,
            n_wave_channels: 0,
            tile_samples: 256,
            wave_tile_counts_s: Vec::new(),
            wave_tile_counts_o: Vec::new(),
            spec_tile_counts_s: Vec::new(),
            src_rate: 0,
            spec_hop: 0,
            tp100_s: Vec::new(),
            tp100_b: Vec::new(),
            spec_info: Vec::new(),
            lufs_s_series_o: Vec::new(),
            lufs_m_series_o: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DrChannel {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
    pub dr: f32,
}

#[derive(Clone, Debug)]
pub struct DrBlock {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
}

#[derive(Clone, Debug)]
pub struct ClipEvent {
    pub start_sample: u32,
    pub run_len: u16,
}

#[derive(Clone, Debug)]
pub struct OverEvent {
    pub time_s: f32,
    pub level_dbtp: f32,
}

#[derive(Clone, Debug)]
pub struct ChainMark {
    pub time_s: f32,
    pub from_utf8: [u8; 10],
    pub to_utf8: [u8; 10],
    pub stage_id: u8,
}

/// Input to `encode_aan3`.  All four pair arrays are already decimated to
/// the requested resolution by the caller (routes.rs).
pub struct ChainResp {
    pub chain_rev: u32,
    pub log_scale: bool,
    pub f0_actual: f32,
    pub f1_actual: f32,
    /// (min_dbr, max_dbr) per output point — |H(f)|.
    pub h_pairs: Vec<(f32, f32)>,
    /// (min_dbfs, max_dbfs) per output point — analytic O PSD.
    pub o_pairs: Vec<(f32, f32)>,
    /// (min_dbfs, max_dbfs) per output point — S Welch PSD.
    pub s_pairs: Vec<(f32, f32)>,
    /// (min_dbfs, max_dbfs) per output point — B Welch PSD.
    pub b_pairs: Vec<(f32, f32)>,
}

/// Input to `encode_aawt`.
pub struct WaveTile {
    pub src_flag: u8,   // 0=S 1=B 2=O
    pub lod: u8,
    pub n_channels: u8,
    pub tile_idx: u32,
    pub n_samples: u32,
    /// n_samples × n_channels entries, interleaved [sample][channel].
    pub samples: Vec<WaveSample>,
}

#[derive(Clone, Debug)]
pub struct WaveSample {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
}

/// Input to `encode_aast`.
pub struct SpectTile {
    pub src_flag: u8,
    pub lod: u8,
    /// The low byte of the pass that made it (`SpecInfo::gen`): a tile
    /// that arrives after its pass was replaced is not placed.
    pub gen8: u8,
    pub tile_idx: u32,
    pub db_floor: f32,
    pub db_range: f32,
    pub n_cols: u16,
    pub n_bins: u16,
    /// n_cols × n_bins bytes, row-major [col][bin].
    pub data: Vec<u8>,
}

// ─── Encoders ────────────────────────────────────────────────────────────────

/// Encode a live frame (AAN1 v0x02).  Returns an owned `Vec<u8>`.
pub fn encode_aan1(frame: &LiveFrame) -> Vec<u8> {
    let n_fft_bins = 1usize << frame.n_fft_bins_log2;
    let n_loud = frame.loud_pts.len();
    let n_spec = frame.spec_cols.len();

    #[cfg(debug_assertions)]
    for col in &frame.spec_cols {
        debug_assert_eq!(col.len(), n_fft_bins,
            "spec_col length must equal 1 << n_fft_bins_log2");
    }

    // SB-EXT: 6 × f32 (24 bytes) + 1 byte sb_prov = 25 bytes
    const SB_EXT_LEN: usize = 25;
    let cap = 144 + 2 + n_loud * 12 + 2 + n_spec * n_fft_bins + SB_EXT_LEN;
    let mut w = W::new(cap);

    // Fixed header — 26 bytes
    w.raw(b"AAN1");     // 0..4
    w.u8(0x02);         // 4  version
    w.u8(frame.flags);  // 5
    w.u32(frame.seq);   // 6..10
    w.u64(frame.track_id);   // 10..18
    w.u32(frame.chain_rev);  // 18..22
    w.u8(frame.coverage_pct);    // 22
    w.u8(frame.n_fft_bins_log2); // 23
    w.u8(frame.spec_floor_neg);  // 24
    w.u8(frame.spec_hop_log2);   // 25 (was reserved, 0)

    // Metrics f32[26] — offsets 26..130
    for i in 0..N_METRICS {
        // analytics: NaN round-trips correctly: f64::NAN as f32 == f32::NAN (0x7FC00000)
        w.f32(frame.live_metrics[i] as f32);
    }

    // Provenance nibbles [13] + 1 reserved byte — offsets 130..144
    let prov = pack_prov(&frame.prov_codes, N_METRICS, N_PROV_BYTES);
    w.raw(&prov);
    w.u8(0); // reserved

    #[cfg(debug_assertions)]
    debug_assert_eq!(w.len(), 144, "AAN1 fixed section must be exactly 144 bytes");

    // Variable: loudness history
    w.u16(n_loud as u16);
    for pt in &frame.loud_pts {
        w.f32(pt.time_s);
        w.f32(pt.lufs_s);
        w.f32(pt.lufs_m);
    }

    // Variable: spectrogram columns
    w.u16(n_spec as u16);
    for col in &frame.spec_cols {
        w.raw(col);
    }

    // analytics: SB-EXT — backward-compat S/B live metrics (PROTOCOL.md §SB-EXT)
    // JS reads this section only when byteLength >= base + SB_EXT_LEN.
    w.f32(frame.s_lufs_m);
    w.f32(frame.s_lufs_s);
    w.f32(frame.s_tp);
    w.f32(frame.b_lufs_m);
    w.f32(frame.b_lufs_s);
    w.f32(frame.b_tp);
    w.u8(frame.sb_prov);
    // A stream's S beside O (stream.rs): read only when present.
    w.raw(&frame.strm);

    w.finish()
}

/// Encode a track-summary frame (AAN2 v0x01).  Returns an owned `Vec<u8>`.
pub fn encode_aan2(summary: &TrackSummary) -> Vec<u8> {
    let chain_utf8 = summary.chain_str.as_bytes();
    let n_loud_s = summary.lufs_s_series_s.len();
    let n_loud_b = summary.lufs_s_series_b.len();

    let mut w = W::new(4096);

    // Header — 20 bytes (PROTOCOL.md §6: duration_s at offset 16)
    w.raw(b"AAN2");
    w.u8(0x01);               // version
    w.u8(0); w.u8(0); w.u8(0); // reserved × 3
    w.u32(summary.rev);
    w.u32(summary.chain_rev);
    w.f32(summary.duration_s); // offset 16

    // Whole-track metrics: S, B, O — 32 × f64 each (256 bytes each)
    for v in &summary.s_metrics { w.f64(*v); }
    for v in &summary.b_metrics { w.f64(*v); }
    for v in &summary.o_metrics { w.f64(*v); }

    // Provenance nibbles — 16 bytes each tap
    // pack_prov packs N_METRICS nibbles and zero-pads to N_PROV_BYTES_AAN2
    w.raw(&pack_prov(&summary.s_prov, N_METRICS, N_PROV_BYTES_AAN2));
    w.raw(&pack_prov(&summary.b_prov, N_METRICS, N_PROV_BYTES_AAN2));
    w.raw(&pack_prov(&summary.o_prov, N_METRICS, N_PROV_BYTES_AAN2));

    // Per-channel DR: S tap
    w.u8(summary.dr_s.len() as u8);
    for ch in &summary.dr_s {
        w.f32(ch.peak_dbfs);
        w.f32(ch.rms_dbfs);
        w.f32(ch.dr);
        w.f32(0.0); // _pad
    }

    // Per-channel DR: O tap
    w.u8(summary.dr_o.len() as u8);
    for ch in &summary.dr_o {
        w.f32(ch.peak_dbfs);
        w.f32(ch.rms_dbfs);
        w.f32(ch.dr);
        w.f32(0.0); // _pad
    }

    // Short-term loudness histogram S
    w.u16(summary.hist_s.len() as u16);
    for v in &summary.hist_s { w.f32(*v); }

    // Short-term loudness histogram B
    w.u16(summary.hist_b.len() as u16);
    for v in &summary.hist_b { w.f32(*v); }

    // DR block list S
    w.u16(summary.dr_blocks_s.len() as u16);
    for b in &summary.dr_blocks_s {
        w.f32(b.rms_dbfs);
        w.f32(b.peak_dbfs);
    }

    // Clip event list S
    w.u16(summary.clips_s.len() as u16);
    for cl in &summary.clips_s {
        w.u32(cl.start_sample);
        w.u16(cl.run_len);
        w.u16(0); // _pad
    }

    // Over / TP-over events O
    w.u16(summary.over_o.len() as u16);
    for ov in &summary.over_o {
        w.f32(ov.time_s);
        w.f32(ov.level_dbtp);
    }

    // Chain token string
    w.u16(chain_utf8.len() as u16);
    w.raw(chain_utf8);

    // Chain-change markers
    w.u16(summary.chain_marks.len() as u16);
    for m in &summary.chain_marks {
        w.f32(m.time_s);
        w.raw(&m.from_utf8);
        w.raw(&m.to_utf8);
        w.u8(m.stage_id);
        w.u8(0); // _pad
    }

    // S loudness series: LUFS-S then LUFS-M at 10 Hz
    // One count for both series: a shorter M series is padded with NaN, a
    // longer one cut, so the frame never goes out of step.
    let m_at = |m: &[f32], i: usize| m.get(i).copied().unwrap_or(f32::NAN);
    w.u32(n_loud_s as u32);
    for v in &summary.lufs_s_series_s { w.f32(*v); }
    for i in 0..n_loud_s { w.f32(m_at(&summary.lufs_m_series_s, i)); }

    // B loudness series
    w.u32(n_loud_b as u32);
    for v in &summary.lufs_s_series_b { w.f32(*v); }
    for i in 0..n_loud_b { w.f32(m_at(&summary.lufs_m_series_b, i)); }

    // Mipmap metadata
    w.u8(summary.n_lods);
    w.u8(summary.n_wave_channels);
    w.u16(summary.tile_samples);
    // Each count array goes as exactly n_lods values (the decoder reads that
    // many): a shorter one is padded with 0 (no tiles), a longer one cut.
    for arr in [&summary.wave_tile_counts_s, &summary.wave_tile_counts_o, &summary.spec_tile_counts_s] {
        for i in 0..summary.n_lods as usize {
            w.u32(arr.get(i).copied().unwrap_or(0));
        }
    }
    // Optional tail (older frames end above): the signal's sample rate,
    // then the spectrogram hop.
    if summary.src_rate != 0 {
        w.u32(summary.src_rate);
        if summary.spec_hop != 0 {
            w.u32(summary.spec_hop);
            let spec = !summary.spec_info.is_empty();
            if spec || !summary.tp100_s.is_empty() || !summary.tp100_b.is_empty() {
                for arr in [&summary.tp100_s, &summary.tp100_b] {
                    w.u32(arr.len() as u32);
                    for &v in arr.iter() { w.f32(v); }
                }
            }
            // Then the tiles of S, B and O: 16 bytes each; then O's loudness
            // series (one count, S then M).
            if spec {
                for i in 0..3 {
                    let s = summary.spec_info.get(i).copied().unwrap_or_default();
                    w.u32(s.gen);
                    w.u32(s.ready);
                    w.u32(s.total);
                    w.u16(s.n_bins);
                    w.u16(0);
                }
                let n = summary.lufs_s_series_o.len();
                w.u32(n as u32);
                for v in &summary.lufs_s_series_o { w.f32(*v); }
                for i in 0..n { w.f32(m_at(&summary.lufs_m_series_o, i)); }
            }
        }
    }

    w.finish()
}

/// A selection's statistics (AASL v1): `status` 0 = being computed, 1 =
/// done, 2 = not available for this signal (then the frame ends after s1).
/// The metrics are the whole-track slots measured over samples `[s0, s1)`
/// of the source grid; the spectrum is the range's Welch (`welch_n` points),
/// decimated to `pairs` (min, max) on log points from `f0` to `f1`.
pub struct SelStats {
    pub status: u8,
    pub src: TileSrc,
    /// The signal is kept in f32 though it has more (O): levels below
    /// about −180 dBFS are its rounding, not the signal.
    pub f32_kept: bool,
    pub gen: u32,
    pub s0: u64,
    pub s1: u64,
    pub metrics: [f64; METRIC_ARRAY_LEN],
    pub prov: [u8; METRIC_ARRAY_LEN],
    pub welch_n: u32,
    pub f0: f32,
    pub f1: f32,
    pub pairs: Vec<(f32, f32)>,
}

pub fn encode_aasl(s: &SelStats) -> Vec<u8> {
    let mut w = W::new(64 + 256 + 16 + s.pairs.len() * 8);
    w.raw(b"AASL");
    w.u8(0x01);
    w.u8(s.status);
    w.u8(s.src.flag());
    w.u8(s.f32_kept as u8);
    w.u32(s.gen);
    w.u64(s.s0);
    w.u64(s.s1);
    if s.status != 1 {
        return w.finish();
    }
    for v in &s.metrics { w.f64(*v); }
    w.raw(&pack_prov(&s.prov, N_METRICS, N_PROV_BYTES_AAN2));
    w.u32(s.welch_n);
    w.u16(s.pairs.len() as u16);
    w.u16(0);
    w.f32(s.f0);
    w.f32(s.f1);
    for &(a, b) in &s.pairs {
        w.f32(a);
        w.f32(b);
    }
    w.finish()
}

/// Encode a zoom reply (AAZR v1): what the page asked for that is ready.
/// `status` 0 = more is being computed, 1 = everything asked for is sent
/// or was held already, 2 = no zoom for this track. `gen` = the S pass the
/// tiles belong to (low 32 bits); each tile goes as u32 length + AAST.
pub fn encode_aazr(status: u8, gen: u32, tiles: &[Arc<[u8]>]) -> Vec<u8> {
    let mut w = W::new(12 + tiles.iter().map(|t| 4 + t.len()).sum::<usize>());
    w.raw(b"AAZR");
    w.u8(0x01);
    w.u8(status);
    w.u16(tiles.len() as u16);
    w.u32(gen);
    for t in tiles {
        w.u32(t.len() as u32);
        w.raw(t);
    }
    w.finish()
}

/// Encode the 8-byte not-changed stub for AAN2 (magic + rev only).
pub fn encode_aan2_unchanged(rev: u32) -> Vec<u8> {
    let mut w = W::new(8);
    w.raw(b"AAN2");
    w.u32(rev);
    w.finish()
}

/// Encode a response/chain frame (AAN3 v0x02).
///
/// All four pair arrays must have the same length (= effective n_pts).
/// The `_n`, `_f0`, `_f1`, `_log` parameters mirror the route query params;
/// the struct fields are authoritative for the encoded values.
pub fn encode_aan3(
    resp: &ChainResp,
    _n: u16,
    _f0: f32,
    _f1: f32,
    _log: bool,
) -> Vec<u8> {
    let n_pts = resp.h_pairs.len();
    debug_assert_eq!(resp.o_pairs.len(), n_pts, "pair arrays must have equal length");
    debug_assert_eq!(resp.s_pairs.len(), n_pts, "pair arrays must have equal length");
    debug_assert_eq!(resp.b_pairs.len(), n_pts, "pair arrays must have equal length");

    let mut w = W::new(22 + n_pts * 32);

    w.raw(b"AAN3");
    w.u8(0x02);                                  // version 2
    w.u8(if resp.log_scale { 0 } else { 1 });    // scale_flag: 0=log, 1=lin
    w.u32(resp.chain_rev);
    w.u32(n_pts as u32);
    w.f32(resp.f0_actual);
    w.f32(resp.f1_actual);

    // |H(f)| pairs at offset 22: n_pts × (min f32, max f32)
    for &(mn, mx) in &resp.h_pairs {
        w.f32(mn);
        w.f32(mx);
    }

    // Analytic O PSD pairs at offset 22 + n_pts*8
    for &(mn, mx) in &resp.o_pairs {
        w.f32(mn);
        w.f32(mx);
    }

    // S Welch PSD pairs at offset 22 + n_pts*16
    for &(mn, mx) in &resp.s_pairs {
        w.f32(mn);
        w.f32(mx);
    }

    // B Welch PSD pairs at offset 22 + n_pts*24
    for &(mn, mx) in &resp.b_pairs {
        w.f32(mn);
        w.f32(mx);
    }

    w.finish()
}

/// The whole-track source spectrum (S Welch PSD) of a subject, on its own
/// grid: bins 0..len-1 span 0..f_max_hz (the source Nyquist).
#[derive(Debug, Clone)]
pub struct SourcePsd {
    pub dbfs: Vec<f64>,
    pub f_max_hz: f32,
}

/// Min and max of `arr` over each of `n_pts` display points spanning
/// `[f0, f1]` Hz; `arr` bins 0..len-1 span 0..`f_max` Hz. A point with no
/// finite bin (outside the array's band, or not measured yet) is NaN.
pub(crate) fn decimate_pairs(arr: &[f64], f_max: f64, f0: f64, f1: f64, n_pts: usize, log: bool) -> Vec<(f32, f32)> {
    let nan = (f32::NAN, f32::NAN);
    if arr.len() < 2 || !(f_max > 0.0) {
        return vec![nan; n_pts];
    }
    let hz_per_bin = f_max / (arr.len() - 1) as f64;
    let edge = |t: f64| {
        if log {
            let (a, b) = ((f0 + 1.0).ln(), (f1 + 1.0).ln());
            (a + t * (b - a)).exp() - 1.0
        } else {
            f0 + t * (f1 - f0)
        }
    };
    (0..n_pts).map(|i| {
        let hz0 = edge(i as f64 / n_pts as f64);
        let hz1 = edge((i + 1) as f64 / n_pts as f64);
        if hz0 >= f_max {
            return nan;
        }
        let b0 = ((hz0 / hz_per_bin).floor().max(0.0) as usize).min(arr.len() - 1);
        let b1 = (((hz1 / hz_per_bin).ceil() as usize) + 1).min(arr.len()).max(b0 + 1);
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for &v in &arr[b0..b1] {
            if v < lo { lo = v; }
            if v > hi { hi = v; }
        }
        // analytics: PROTOCOL rule 4 — NaN = value not yet available.
        if hi.is_infinite() { nan } else { (lo as f32, hi as f32) }
    }).collect()
}

/// Encode an AAN3 frame by decimating the full-resolution arrays on the fly:
/// the chain's |H| and O (`chain`, output grid), the source spectrum S
/// (`src`) and the heard variant's spectrum B (`b_src`), each on its own grid.
///
/// `n` output points span the `[f0_hz, f1_hz]` window, each carrying
/// `(min, max)` of the bins that map to it; `log` spaces them
/// logarithmically. When `n` is 0 the window's |H| bin count is used (1:1).
pub fn encode_aan3_from_data(
    chain: Option<&ChainRespData>,
    src: Option<&SourcePsd>,
    b_src: Option<&SourcePsd>,
    n: u16,
    f0_hz: f32,
    f1_hz: f32,
    log: bool,
) -> Vec<u8> {
    let f_top = chain.map(|c| c.f_max_hz as f64)
        .into_iter()
        .chain(src.map(|s| s.f_max_hz as f64))
        .chain(b_src.map(|s| s.f_max_hz as f64))
        .fold(0.0, f64::max);
    let f0 = (f0_hz as f64).clamp(0.0, f_top);
    let f1 = (f1_hz as f64).clamp(f0, f_top);
    let n_pts = if n != 0 {
        n as usize
    } else {
        chain.filter(|c| c.h_dbr.len() > 1)
            .map(|c| ((f1 - f0) / (c.f_max_hz as f64 / (c.h_dbr.len() - 1) as f64)).ceil() as usize)
            .unwrap_or(1024)
            .max(1)
    };
    let nan_pairs = || vec![(f32::NAN, f32::NAN); n_pts];
    let (h_pairs, o_pairs) = match chain {
        Some(c) => (
            decimate_pairs(&c.h_dbr, c.f_max_hz as f64, f0, f1, n_pts, log),
            decimate_pairs(&c.o_dbfs, c.f_max_hz as f64, f0, f1, n_pts, log),
        ),
        None => (nan_pairs(), nan_pairs()),
    };
    let psd_pairs = |p: Option<&SourcePsd>| match p {
        Some(s) => decimate_pairs(&s.dbfs, s.f_max_hz as f64, f0, f1, n_pts, log),
        None => nan_pairs(),
    };
    let (s_pairs, b_pairs) = (psd_pairs(src), psd_pairs(b_src));

    encode_aan3(&ChainResp {
        chain_rev: chain.map_or(0, |c| c.chain_rev),
        log_scale: log,
        f0_actual: f0_hz,
        f1_actual: f1_hz,
        h_pairs,
        o_pairs,
        s_pairs,
        b_pairs,
    }, n, f0_hz, f1_hz, log)
}

/// Encode a waveform tile (AAWT v0x01).
pub fn encode_aawt(tile: &WaveTile) -> Vec<u8> {
    let n_entries = tile.samples.len();
    let mut w = W::new(16 + n_entries * 12);

    w.raw(b"AAWT");
    w.u8(0x01);            // version
    w.u8(tile.src_flag);
    w.u8(tile.lod);
    w.u8(tile.n_channels);
    w.u32(tile.tile_idx);
    w.u32(tile.n_samples);

    // Interleaved [sample][channel]: {min f32, max f32, rms f32}
    for s in &tile.samples {
        w.f32(s.min);
        w.f32(s.max);
        w.f32(s.rms);
    }

    w.finish()
}

/// Encode a spectrogram tile (AAST v0x01).
pub fn encode_aast(tile: &SpectTile) -> Vec<u8> {
    let mut w = W::new(24 + tile.data.len());

    w.raw(b"AAST");
    w.u8(0x01);            // version
    w.u8(tile.src_flag);
    w.u8(tile.lod);
    w.u8(tile.gen8);       // was reserved (0)
    w.u32(tile.tile_idx);
    w.f32(tile.db_floor);
    w.f32(tile.db_range);
    w.u16(tile.n_cols);
    w.u16(tile.n_bins);
    w.raw(&tile.data);

    w.finish()
}

// ─── Golden round-trip tests ─────────────────────────────────────────────────

#[cfg(test)]
mod golden {
    use super::*;
    use std::path::PathBuf;

    /// The golden vectors next to this module, shared with the page's
    /// `protocol.test.mjs`: both decoders must read each .bin as its .json.
    fn golden_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/player/analytics/golden")
            .join(name)
    }

    fn read_golden(name: &str) -> Vec<u8> {
        std::fs::read(golden_path(name))
            .unwrap_or_else(|e| panic!("Cannot read golden file {name}: {e}"))
    }

    // ── Minimal binary reader ────────────────────────────────────────────────

    struct R<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> R<'a> {
        fn new(data: &'a [u8]) -> Self { R { data, pos: 0 } }

        fn u8(&mut self) -> u8 {
            let v = self.data[self.pos]; self.pos += 1; v
        }
        fn u16(&mut self) -> u16 {
            let v = u16::from_le_bytes(self.data[self.pos..self.pos+2].try_into().unwrap());
            self.pos += 2; v
        }
        fn u32(&mut self) -> u32 {
            let v = u32::from_le_bytes(self.data[self.pos..self.pos+4].try_into().unwrap());
            self.pos += 4; v
        }
        fn u64(&mut self) -> u64 {
            let v = u64::from_le_bytes(self.data[self.pos..self.pos+8].try_into().unwrap());
            self.pos += 8; v
        }
        fn f32(&mut self) -> f32 {
            let v = f32::from_le_bytes(self.data[self.pos..self.pos+4].try_into().unwrap());
            self.pos += 4; v
        }
        fn f64(&mut self) -> f64 {
            let v = f64::from_le_bytes(self.data[self.pos..self.pos+8].try_into().unwrap());
            self.pos += 8; v
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            let v = self.data[self.pos..self.pos+n].to_vec();
            self.pos += n; v
        }
        fn remaining(&self) -> usize { self.data.len() - self.pos }
        fn done(&self) -> bool { self.remaining() == 0 }
    }

    /// Unpack nibbles back to a [u8; 32] codes array.
    fn unpack_prov(bytes: &[u8], n: usize) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..n {
            out[i] = (bytes[i >> 1] >> ((i & 1) * 4)) & 0x0F;
        }
        out
    }

    // ── AAN1 round-trip ──────────────────────────────────────────────────────

    #[test]
    fn aan1_golden_roundtrip() {
        let bin = read_golden("aan1.bin");
        let mut r = R::new(&bin);

        assert_eq!(r.bytes(4), b"AAN1", "magic");
        assert_eq!(r.u8(), 0x02, "version");
        let flags = r.u8();
        let seq = r.u32();
        let track_id = r.u64();
        let chain_rev = r.u32();
        let coverage_pct = r.u8();
        let n_fft_bins_log2 = r.u8();
        let spec_floor_neg = r.u8();
        let spec_hop_log2 = r.u8();

        // Metrics: 26 × f32 — round-trip via f64 preserves bit pattern
        let mut live_metrics = [f64::NAN; 32];
        for i in 0..N_METRICS {
            // analytics: read as f32 bits, widen to f64 for storage;
            // encode_aan1 narrows back with `as f32` which is bit-exact for NaN
            // and for values that were originally f32 (the golden was built from
            // Float32Array, so every stored value IS an exact f32).
            live_metrics[i] = r.f32() as f64;
        }

        // Prov nibbles + reserved
        let prov_raw = r.bytes(N_PROV_BYTES);
        let _reserved2 = r.u8();
        let prov_codes = unpack_prov(&prov_raw, N_METRICS);

        assert_eq!(r.pos, 144, "AAN1 fixed section must be exactly 144 bytes");

        let n_loud = r.u16() as usize;
        let mut loud_pts = Vec::with_capacity(n_loud);
        for _ in 0..n_loud {
            loud_pts.push(LoudPt { time_s: r.f32(), lufs_s: r.f32(), lufs_m: r.f32() });
        }

        let n_spec = r.u16() as usize;
        let n_fft_bins = 1usize << n_fft_bins_log2;
        let mut spec_cols = Vec::with_capacity(n_spec);
        for _ in 0..n_spec {
            spec_cols.push(r.bytes(n_fft_bins));
        }

        // analytics: SB-EXT — backward-compat; present in new goldens (≥25 trailing bytes)
        let (s_lufs_m, s_lufs_s, s_tp, b_lufs_m, b_lufs_s, b_tp, sb_prov) =
            if r.remaining() >= 25 {
                (r.f32(), r.f32(), r.f32(), r.f32(), r.f32(), r.f32(), r.u8())
            } else {
                (f32::NAN, f32::NAN, f32::NAN, f32::NAN, f32::NAN, f32::NAN, 0u8)
            };

        assert!(r.done(), "AAN1 decoder left {} trailing bytes", r.remaining());

        let re = encode_aan1(&LiveFrame {
            flags, seq, track_id, chain_rev,
            coverage_pct, n_fft_bins_log2, spec_floor_neg, spec_hop_log2,
            live_metrics, prov_codes, loud_pts, spec_cols,
            s_lufs_m, s_lufs_s, s_tp, b_lufs_m, b_lufs_s, b_tp, sb_prov,
            strm: Vec::new(),
        });
        assert_eq!(re, bin, "AAN1 round-trip: re-encoded bytes differ from golden");
    }

    // ── AAN2 round-trip ──────────────────────────────────────────────────────

    #[test]
    fn aan2_golden_roundtrip() {
        let bin = read_golden("aan2.bin");
        let mut r = R::new(&bin);

        assert_eq!(r.bytes(4), b"AAN2", "magic");
        assert_eq!(r.u8(), 0x01, "version");
        r.u8(); r.u8(); r.u8(); // reserved × 3
        let rev = r.u32();
        let chain_rev = r.u32();
        let duration_s = r.f32(); // offset 16

        let mut s_metrics = [0f64; 32];
        let mut b_metrics = [0f64; 32];
        let mut o_metrics = [0f64; 32];
        for v in &mut s_metrics { *v = r.f64(); }
        for v in &mut b_metrics { *v = r.f64(); }
        for v in &mut o_metrics { *v = r.f64(); }

        let sp_raw = r.bytes(N_PROV_BYTES_AAN2);
        let bp_raw = r.bytes(N_PROV_BYTES_AAN2);
        let op_raw = r.bytes(N_PROV_BYTES_AAN2);
        let s_prov = unpack_prov(&sp_raw, N_METRICS);
        let b_prov = unpack_prov(&bp_raw, N_METRICS);
        let o_prov = unpack_prov(&op_raw, N_METRICS);

        let n_ch_s = r.u8() as usize;
        let mut dr_s = Vec::with_capacity(n_ch_s);
        for _ in 0..n_ch_s {
            let peak = r.f32(); let rms = r.f32(); let dr = r.f32(); let _pad = r.f32();
            dr_s.push(DrChannel { peak_dbfs: peak, rms_dbfs: rms, dr });
        }

        let n_ch_o = r.u8() as usize;
        let mut dr_o = Vec::with_capacity(n_ch_o);
        for _ in 0..n_ch_o {
            let peak = r.f32(); let rms = r.f32(); let dr = r.f32(); let _pad = r.f32();
            dr_o.push(DrChannel { peak_dbfs: peak, rms_dbfs: rms, dr });
        }

        let hn_s = r.u16() as usize;
        let mut hist_s = Vec::with_capacity(hn_s);
        for _ in 0..hn_s { hist_s.push(r.f32()); }

        let hn_b = r.u16() as usize;
        let mut hist_b = Vec::with_capacity(hn_b);
        for _ in 0..hn_b { hist_b.push(r.f32()); }

        let n_dr = r.u16() as usize;
        let mut dr_blocks_s = Vec::with_capacity(n_dr);
        for _ in 0..n_dr {
            dr_blocks_s.push(DrBlock { rms_dbfs: r.f32(), peak_dbfs: r.f32() });
        }

        let n_clips = r.u16() as usize;
        let mut clips_s = Vec::with_capacity(n_clips);
        for _ in 0..n_clips {
            let start = r.u32(); let len = r.u16(); let _pad = r.u16();
            clips_s.push(ClipEvent { start_sample: start, run_len: len });
        }

        let n_over = r.u16() as usize;
        let mut over_o = Vec::with_capacity(n_over);
        for _ in 0..n_over {
            over_o.push(OverEvent { time_s: r.f32(), level_dbtp: r.f32() });
        }

        let cs_len = r.u16() as usize;
        let chain_str = String::from_utf8(r.bytes(cs_len)).expect("chain_str UTF-8");

        let n_marks = r.u16() as usize;
        let mut chain_marks = Vec::with_capacity(n_marks);
        for _ in 0..n_marks {
            let time_s = r.f32();
            let from_raw = r.bytes(10);
            let to_raw   = r.bytes(10);
            let stage_id = r.u8();
            let _pad = r.u8();
            let mut from_utf8 = [0u8; 10];
            let mut to_utf8   = [0u8; 10];
            from_utf8.copy_from_slice(&from_raw);
            to_utf8.copy_from_slice(&to_raw);
            chain_marks.push(ChainMark { time_s, from_utf8, to_utf8, stage_id });
        }

        let n_ls = r.u32() as usize;
        let mut lufs_s_series_s = Vec::with_capacity(n_ls);
        let mut lufs_m_series_s = Vec::with_capacity(n_ls);
        for _ in 0..n_ls { lufs_s_series_s.push(r.f32()); }
        for _ in 0..n_ls { lufs_m_series_s.push(r.f32()); }

        let n_lb = r.u32() as usize;
        let mut lufs_s_series_b = Vec::with_capacity(n_lb);
        let mut lufs_m_series_b = Vec::with_capacity(n_lb);
        for _ in 0..n_lb { lufs_s_series_b.push(r.f32()); }
        for _ in 0..n_lb { lufs_m_series_b.push(r.f32()); }

        let n_lods = r.u8();
        let n_wave_channels = r.u8();
        let tile_samples = r.u16();
        let mut wave_tile_counts_s = Vec::new();
        let mut wave_tile_counts_o = Vec::new();
        let mut spec_tile_counts_s = Vec::new();
        for _ in 0..n_lods as usize { wave_tile_counts_s.push(r.u32()); }
        for _ in 0..n_lods as usize { wave_tile_counts_o.push(r.u32()); }
        for _ in 0..n_lods as usize { spec_tile_counts_s.push(r.u32()); }
        let src_rate = if r.done() { 0 } else { r.u32() };
        let spec_hop = if r.done() { 0 } else { r.u32() };
        let mut tp100 = [Vec::new(), Vec::new()];
        if !r.done() {
            for arr in tp100.iter_mut() {
                let n = r.u32() as usize;
                for _ in 0..n { arr.push(r.f32()); }
            }
        }
        let [tp100_s, tp100_b] = tp100;
        let mut spec_info = Vec::new();
        let (mut lufs_s_series_o, mut lufs_m_series_o) = (Vec::new(), Vec::new());
        if !r.done() {
            for _ in 0..3 {
                let (gen, ready, total, n_bins) = (r.u32(), r.u32(), r.u32(), r.u16());
                r.u16();
                spec_info.push(SpecInfo { gen, ready, total, n_bins });
            }
            if !r.done() {
                let n = r.u32() as usize;
                for _ in 0..n { lufs_s_series_o.push(r.f32()); }
                for _ in 0..n { lufs_m_series_o.push(r.f32()); }
            }
        }

        assert!(r.done(), "AAN2 decoder left {} trailing bytes", r.remaining());

        let re = encode_aan2(&TrackSummary {
            rev, chain_rev, duration_s,
            s_metrics, b_metrics, o_metrics,
            s_prov, b_prov, o_prov,
            dr_s, dr_o, hist_s, hist_b,
            dr_blocks_s, clips_s, over_o,
            chain_str, chain_marks,
            lufs_s_series_s, lufs_m_series_s,
            lufs_s_series_b, lufs_m_series_b,
            n_lods, n_wave_channels, tile_samples,
            wave_tile_counts_s, wave_tile_counts_o, spec_tile_counts_s,
            src_rate, spec_hop, tp100_s, tp100_b, spec_info, lufs_s_series_o, lufs_m_series_o,
        });
        assert_eq!(re, bin, "AAN2 round-trip: re-encoded bytes differ from golden");
    }

    // ── AAN3 round-trip ──────────────────────────────────────────────────────

    #[test]
    fn aan3_golden_roundtrip() {
        let bin = read_golden("aan3.bin");
        let mut r = R::new(&bin);

        assert_eq!(r.bytes(4), b"AAN3", "magic");
        assert_eq!(r.u8(), 0x02, "version");
        let scale_flag = r.u8();
        let chain_rev = r.u32();
        let n_pts = r.u32() as usize;
        let f0_actual = r.f32();
        let f1_actual = r.f32();

        let mut h_pairs = Vec::with_capacity(n_pts);
        for _ in 0..n_pts { h_pairs.push((r.f32(), r.f32())); }

        let mut o_pairs = Vec::with_capacity(n_pts);
        for _ in 0..n_pts { o_pairs.push((r.f32(), r.f32())); }

        let mut s_pairs = Vec::with_capacity(n_pts);
        for _ in 0..n_pts { s_pairs.push((r.f32(), r.f32())); }

        let mut b_pairs = Vec::with_capacity(n_pts);
        for _ in 0..n_pts { b_pairs.push((r.f32(), r.f32())); }

        assert!(r.done(), "AAN3 decoder left {} trailing bytes", r.remaining());

        let resp = ChainResp {
            chain_rev,
            log_scale: scale_flag == 0,
            f0_actual,
            f1_actual,
            h_pairs,
            o_pairs,
            s_pairs,
            b_pairs,
        };
        let re = encode_aan3(&resp, n_pts as u16, f0_actual, f1_actual, scale_flag == 0);
        assert_eq!(re, bin, "AAN3 round-trip: re-encoded bytes differ from golden");
    }

    // ── AAWT round-trip ──────────────────────────────────────────────────────

    #[test]
    fn aawt_golden_roundtrip() {
        let bin = read_golden("wave_tile.bin");
        let mut r = R::new(&bin);

        assert_eq!(r.bytes(4), b"AAWT", "magic");
        assert_eq!(r.u8(), 0x01, "version");
        let src_flag   = r.u8();
        let lod        = r.u8();
        let n_channels = r.u8();
        let tile_idx   = r.u32();
        let n_samples  = r.u32();

        let n_entries = n_samples as usize * n_channels as usize;
        let mut samples = Vec::with_capacity(n_entries);
        for _ in 0..n_entries {
            samples.push(WaveSample { min: r.f32(), max: r.f32(), rms: r.f32() });
        }

        assert!(r.done(), "AAWT decoder left {} trailing bytes", r.remaining());

        let re = encode_aawt(&WaveTile { src_flag, lod, n_channels, tile_idx, n_samples, samples });
        assert_eq!(re, bin, "AAWT round-trip: re-encoded bytes differ from golden");
    }

    // ── AAST round-trip ──────────────────────────────────────────────────────

    #[test]
    fn aast_golden_roundtrip() {
        let bin = read_golden("spec_tile.bin");
        let mut r = R::new(&bin);

        assert_eq!(r.bytes(4), b"AAST", "magic");
        assert_eq!(r.u8(), 0x01, "version");
        let src_flag  = r.u8();
        let lod       = r.u8();
        let gen8      = r.u8();
        let tile_idx  = r.u32();
        let db_floor  = r.f32();
        let db_range  = r.f32();
        let n_cols    = r.u16();
        let n_bins    = r.u16();
        let data      = r.bytes(n_cols as usize * n_bins as usize);

        assert!(r.done(), "AAST decoder left {} trailing bytes", r.remaining());

        let re = encode_aast(&SpectTile { src_flag, lod, gen8, tile_idx, db_floor, db_range, n_cols, n_bins, data });
        assert_eq!(re, bin, "AAST round-trip: re-encoded bytes differ from golden");
    }

    // ── Constants ────────────────────────────────────────────────────────────

    #[test]
    fn n_metrics_is_26() {
        assert_eq!(N_METRICS, 26);
        assert_eq!(N_PROV_BYTES, 13);
        assert_eq!(N_PROV_BYTES_AAN2, 16);
    }

    // ── AAN2 not-changed stub ────────────────────────────────────────────────

    #[test]
    fn aan2_unchanged_is_8_bytes() {
        let stub = encode_aan2_unchanged(42);
        assert_eq!(stub.len(), 8, "stub must be exactly 8 bytes");
        assert_eq!(&stub[0..4], b"AAN2", "magic");
        assert_eq!(
            u32::from_le_bytes(stub[4..8].try_into().unwrap()),
            42,
            "rev field"
        );
    }

    // ── Provenance nibble pack / unpack ──────────────────────────────────────

    #[test]
    fn prov_nibble_pack_unpack_all_26() {
        let mut codes = [0u8; 32];
        // Assign each metric a distinct code cycling 0–7
        for i in 0..N_METRICS {
            codes[i] = (i % 8) as u8;
        }
        // Test AAN1 packing (13 bytes)
        let packed = pack_prov(&codes, N_METRICS, N_PROV_BYTES);
        assert_eq!(packed.len(), N_PROV_BYTES);
        let unpacked = unpack_prov(&packed, N_METRICS);
        for i in 0..N_METRICS {
            assert_eq!(codes[i], unpacked[i], "nibble mismatch at metric {i}");
        }

        // Test AAN2 packing (16 bytes); padding bytes must be zero
        let packed16 = pack_prov(&codes, N_METRICS, N_PROV_BYTES_AAN2);
        assert_eq!(packed16.len(), N_PROV_BYTES_AAN2);
        assert_eq!(&packed16[N_PROV_BYTES..], &[0u8; 3], "padding bytes must be zero");
        let unpacked16 = unpack_prov(&packed16, N_METRICS);
        for i in 0..N_METRICS {
            assert_eq!(codes[i], unpacked16[i], "nibble16 mismatch at metric {i}");
        }
    }

    // ── Metric 0 in low nibble, metric 1 in high nibble ─────────────────────

    #[test]
    fn prov_nibble_byte0_layout() {
        let mut codes = [0u8; 32];
        codes[0] = 0x3; // MEASURED  → low nibble of byte 0
        codes[1] = 0x1; // ANALYTIC  → high nibble of byte 0
        let packed = pack_prov(&codes, N_METRICS, N_PROV_BYTES);
        // byte 0 should be 0x13 (high = 1, low = 3)
        assert_eq!(packed[0], 0x13,
            "byte 0 should be 0x13 (metric1 in high nibble, metric0 in low nibble)");
    }

    /// The S and M loudness series go under one count: a momentary series
    /// longer than the short-term one (the real meters: 0.4 s vs 3 s window)
    /// must not shift the fields after it.
    #[test]
    fn aan2_series_share_one_count() {
        let mk = |n_m: usize| {
            let mut a = TrackSummary::default();
            a.lufs_s_series_s = vec![-20.0; 3];
            a.lufs_m_series_s = vec![-20.0; n_m];
            a.n_lods = 1;
            a.wave_tile_counts_s = vec![7];
            a.wave_tile_counts_o = vec![0];
            a.spec_tile_counts_s = vec![0];
            a
        };
        let (ea, eb) = (encode_aan2(&mk(3)), encode_aan2(&mk(5)));
        assert_eq!(ea.len(), eb.len());
        // n_lods = 1: three u32 counts close the frame, whatever the arrays hold.
        let mut c = mk(3);
        c.spec_tile_counts_s = vec![];
        c.wave_tile_counts_o = vec![0, 0, 0];
        assert_eq!(encode_aan2(&c).len(), ea.len());
        assert_eq!(&ea[ea.len() - 16..], &eb[eb.len() - 16..], "the tail stays where the decoder reads it");
    }

    /// |H| on the output grid and S on the source grid share one display
    /// window: S is NaN above its Nyquist, and each curve reads its own bins.
    #[test]
    fn aan3_decimates_each_curve_on_its_own_grid() {
        let chain = ChainRespData {
            chain_rev: 7,
            h_dbr: vec![-1.0; 8193],               // 0..176.4 kHz
            o_dbfs: vec![f64::NAN; 8193],
            f_max_hz: 176_400.0,
        };
        let src = SourcePsd {
            dbfs: (0..1025).map(|k| -(k as f64)).collect(), // 0..22.05 kHz, falling
            f_max_hz: 22_050.0,
        };
        let h = decimate_pairs(&chain.h_dbr, 176_400.0, 0.0, 44_100.0, 4, false);
        let s = decimate_pairs(&src.dbfs, 22_050.0, 0.0, 44_100.0, 4, false);
        assert!(h.iter().all(|&(lo, hi)| lo == -1.0 && hi == -1.0));
        assert_eq!(s[0].1, 0.0, "the first point starts at DC");
        assert!(s[1].0 <= -500.0 && s[1].1 > -1024.0, "the second point ends near Nyquist: {:?}", s[1]);
        assert!(s[2].0.is_nan() && s[3].0.is_nan(), "above the source Nyquist: nothing");
        let bytes = encode_aan3_from_data(Some(&chain), Some(&src), None, 4, 0.0, 44_100.0, false);
        assert_eq!(&bytes[..4], b"AAN3");
        assert_eq!(u32::from_le_bytes(bytes[6..10].try_into().unwrap()), 7, "chain_rev");
    }
}
