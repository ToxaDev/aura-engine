/**
 * protocol.js — AuraEngine Analyzer binary decoder.
 *
 * Implements PROTOCOL.md v1 exactly.  This file is the single source of truth
 * for all frame constants on the JS side (METRIC_ID, PROV, N_METRICS).
 *
 * Exports:
 *   METRIC_ID, PROV, N_METRICS, N_PROV_BYTES,
 *   decodeAAN1, decodeAAN2, decodeAAN3, decodeWaveTile, decodeSpecTile, decodeAASL,
 *   formatMetric
 *
 * Every decode* function throws TypeError on wrong magic or unsupported version.
 * Unknown trailing bytes are silently ignored (versioning rule).
 */

// ── Constants ──────────────────────────────────────────────────────────────────

/** @type {Readonly<Record<string,number>>} Metric ID table (PROTOCOL.md §3) */
export const METRIC_ID = Object.freeze({
  LUFS_I:        0,
  LUFS_S_LIVE:   1,
  LUFS_M_LIVE:   2,
  LRA:           3,
  TP_EBUR:       4,
  TP_ENGINE:     5,
  SP:            6,
  PEAK_AT:       7,
  DR:            8,
  RMS:           9,
  RMS_TOP20:    10,
  GAIN_18LUFS:  11,
  CLIPS_GE2:    12,
  CLIPS_GE17:   13,
  CLIPS_OUT_X1: 14,
  TP_OVER0_EVENTS: 15,
  DC_OFFSET:    16,
  ULTRA_PEAK:   17,
  ULTRA_RMS:    18,
  ULTRA_EVENTS: 19,
  INFRA_RMS:    20,
  SUB_REMOVED:  21,
  PLR:          22,
  STEREO_CORR:  23,
  EFF_BW:       24,
  COVERAGE_O:   25,
});

/** Number of defined metrics (IDs 0–25). Slots 26–31 are reserved (NaN). */
export const N_METRICS = 26;

/** Number of provenance bytes for N_METRICS metrics (ceil(26/2) = 13). */
export const N_PROV_BYTES = Math.ceil(N_METRICS / 2);  // 13

/** Provenance code table (PROTOCOL.md §4) */
/** Why the whole-track measures are missing while a live stream plays. */
export const STREAM_NOTE = 'A live stream has no whole track: these are measured on files.';

/** The analyzer's words for a stream (the radio). */
export const STREAM_TEXTS = Object.freeze({
  song: 'This song',
  songTip: "Since this song began (S: the stream as it came in, O: the output). Songs are cut where the stream's title changes: a title can come a few seconds early or late and songs can run into each other, so the edges are approximate.",
  session: 'Session',
  sessionTip: 'Since you tuned in or pressed ↺: all you have heard of this station.',
  reset: '↺',
  resetTip: "Start the session's totals afresh",
  rms: 'RMS (3 s)',
  rmsTip: 'The mean level (RMS) of the last 3 seconds, both channels.',
  liveTail: 'S: the stream as it came in, O: the output.',
  thS: "S — the stream as decoded, before the rack's source stages",
  bp: '= S',
  bpTip: 'BIT-PERFECT: the output is the source, sample for sample.',
  bwTip: "Where the stream's spectrum really ends, measured on what has played (quiet passages left out): a lossy codec cuts the top — MP3 at 128 kbps near 16 kHz, AAC near 19–20 kHz; a lossless stream reaches half its sample rate.",
  tpMax: 'TP max',
  songs: 'Songs',
  colSong: 'Song',
  colPlayed: 'Played',
  colBandwidth: 'Bandwidth',
  now: 'now',
  joined: 'joined',
  joinedTip: 'Tuned in after this song began: measured on the part heard',
  cut: 'cut short',
  cutTip: 'The stream stopped or changed before this song ended',
  noneYet: 'No song has ended yet.',
  noTitles: 'This stream sends no song titles: the session covers it all.',
});

/** kHz as the radio's lines write them: 44.1, 48, 22.05. */
const _khz = hz => String(+(hz / 1000).toFixed(2));

/**
 * How far the stream's spectrum reaches (the stream JSON's `bw`):
 * "≈ 16.0 kHz", "full, 22.05 kHz", "measuring…".
 */
export function bandwidthShort(bw) {
  if (!bw || bw.state !== 'ok' || !Number.isFinite(bw.hz)) return 'measuring…';
  if (bw.full) return `full, ${_khz(bw.nyq)} kHz`;
  return `≈ ${(bw.hz / 1000).toFixed(1)} kHz`;
}

/** "Source bandwidth ≈ 16.0 kHz" / "Source bandwidth: full, 22.05 kHz" / "Source bandwidth: measuring…". */
export function bandwidthText(bw) {
  const s = bandwidthShort(bw);
  return s.startsWith('≈') ? `Source bandwidth ${s}` : `Source bandwidth: ${s}`;
}

/**
 * What a stream says it is (the status's `radio.info`): "MP3 · 128 kbps ·
 * 44.1 kHz", "FLAC · 44.1 kHz/16". `codecName`: the radio's word for a codec.
 */
export function streamTechText(info, codecName = c => String(c || '').toUpperCase()) {
  if (!info?.rate) return '';
  const rate = `${_khz(info.rate)} kHz${info.bits ? '/' + info.bits : ''}`;
  const kbps = parseInt(info.icyBr, 10);
  return [codecName(info.codec), kbps > 0 ? `${kbps} kbps` : '', rate].filter(Boolean).join(' · ');
}

export const PROV = Object.freeze({
  UNAVAIL:    0,
  ANALYTIC:   1,
  FORECAST:   2,
  MEASURED:   3,
  UNCHANGED:  4,
  HP_PENDING: 5,
  STALE:      6,
  COMPUTING:  7,
});

// Display rounding per metric id (decimal places)
const _ROUND = [
  2, 2, 2, 2, 2, 2, 4, 3,  // 0-7
  0, 2, 2, 2, 0, 0, 0, 0,  // 8-15
  3, 1, 1, 0, 1, 1, 2, 3,  // 16-23
  0, 1,                     // 24-25
];

// Unit suffixes per metric id (empty string = none shown)
const _UNIT = [
  ' LUFS', ' LUFS', ' LUFS', ' LU', ' dBTP', ' dBTP', '',  '',  // 0-7
  '', ' dBFS', ' dBFS', ' dB', '', '', '', '',               // 8-15
  '%', ' dBFS', ' dBFS', '', ' dBFS', ' dBFS', ' dB', '',   // 16-23
  ' Hz', '%',                                                  // 24-25
];

/**
 * Format a metric value for display.
 * @param {number} id  METRIC_ID constant
 * @param {number} v   Float value (may be NaN)
 * @param {number} prov  PROV constant
 * @param {object} [opt]  { omitUnit: bool }
 * @returns {string}
 */
export function formatMetric(id, v, prov, opt = {}) {
  if (isNaN(v)) {
    return prov === PROV.COMPUTING ? '···' : '—';
  }
  const dp = (id >= 0 && id < _ROUND.length) ? _ROUND[id] : 2;
  const unit = opt.omitUnit ? '' : (_UNIT[id] || '');
  let s = v.toFixed(dp);
  // Add '+' prefix for positive dBTP values (TP metrics)
  if ((id === METRIC_ID.TP_EBUR || id === METRIC_ID.TP_ENGINE) && v > 0) {
    s = '+' + s;
  }
  return s + unit;
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/**
 * Decode provenance nibbles: metric i → nibble i.
 * byte = prov_nibbles[i >> 1], bits = (i & 1) * 4 .. +3
 */
function _decodeProvNibbles(dv, byteOffset, n) {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    const byte = dv.getUint8(byteOffset + (i >> 1));
    out[i] = (byte >> ((i & 1) * 4)) & 0x0F;
  }
  return out;
}

/** Read an ASCII/UTF-8 4-byte magic string from a DataView. */
function _magic(dv, off) {
  return String.fromCharCode(
    dv.getUint8(off), dv.getUint8(off+1), dv.getUint8(off+2), dv.getUint8(off+3));
}

/** Read n f32 LE values starting at byteOffset; returns Float32Array. */
function _readF32Array(dv, byteOffset, n) {
  const arr = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    arr[i] = dv.getFloat32(byteOffset + i * 4, true);
  }
  return arr;
}

/** Read n f64 LE values starting at byteOffset; returns Float64Array. */
function _readF64Array(dv, byteOffset, n) {
  const arr = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    arr[i] = dv.getFloat64(byteOffset + i * 8, true);
  }
  return arr;
}

// ── AAN1 ──────────────────────────────────────────────────────────────────────

/**
 * Decode an AAN1 (Live Frame) binary buffer.
 * @param {ArrayBuffer|DataView} buf
 * @returns {AAN1Frame}
 */
export function decodeAAN1(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);

  const magic = _magic(dv, 0);
  if (magic !== 'AAN1') throw new TypeError(`decodeAAN1: bad magic "${magic}"`);
  const version = dv.getUint8(4);
  if (version < 2) throw new TypeError(`decodeAAN1: unsupported version ${version}`);

  const flags        = dv.getUint8(5);
  const hp_active    = !!(flags & 0x01);
  const b_ready      = !!(flags & 0x02);
  const conv_exists  = !!(flags & 0x04);
  const xtc_active   = !!(flags & 0x08);
  const stale        = !!(flags & 0x10);
  const isp_out_active = !!(flags & 0x20);

  const seq            = dv.getUint32(6, true);
  const track_id_lo    = dv.getUint32(10, true);
  const track_id_hi    = dv.getUint32(14, true);
  const chain_rev      = dv.getUint32(18, true);
  const coverage_pct   = dv.getUint8(22);
  const n_fft_bins_log2 = dv.getUint8(23);
  const spec_floor_neg  = dv.getUint8(24);
  // log2 of the spectrogram column hop in output samples (0 = not sent)
  const spec_hop_log2   = dv.getUint8(25);

  const live_metrics = _readF32Array(dv, 26, 26);
  const prov_codes   = _decodeProvNibbles(dv, 130, N_METRICS);
  // byte 143 reserved

  let off = 144;
  const n_loud_pts = dv.getUint16(off, true); off += 2;

  const loud_pts = [];
  for (let i = 0; i < n_loud_pts; i++) {
    const time_s = dv.getFloat32(off,   true); off += 4;
    const lufs_s = dv.getFloat32(off,   true); off += 4;
    const lufs_m = dv.getFloat32(off,   true); off += 4;
    loud_pts.push({ time_s, lufs_s, lufs_m });
  }

  const n_spec_cols   = dv.getUint16(off, true); off += 2;
  const n_fft_bins    = 1 << n_fft_bins_log2;
  const spec_floor_dbfs = -spec_floor_neg;
  const spec_range_dbfs = spec_floor_neg;  // range from floor to 0

  // Collect spectrogram columns as Uint8Array slices
  const spec_cols = [];
  for (let c = 0; c < n_spec_cols; c++) {
    spec_cols.push(new Uint8Array(dv.buffer, dv.byteOffset + off, n_fft_bins));
    off += n_fft_bins;
  }

  // analytics: SB-EXT — backward-compat S/B live metrics (PROTOCOL.md §SB-EXT)
  // 6 × f32 + 1 byte sb_prov = 25 bytes; older frames without this section get NaN.
  let s_lufs_m = NaN, s_lufs_s = NaN, s_tp = NaN;
  let b_lufs_m = NaN, b_lufs_s = NaN, b_tp = NaN;
  let sb_prov = 0;
  if (dv.byteLength - off >= 25) {
    s_lufs_m = dv.getFloat32(off, true); off += 4;
    s_lufs_s = dv.getFloat32(off, true); off += 4;
    s_tp     = dv.getFloat32(off, true); off += 4;
    b_lufs_m = dv.getFloat32(off, true); off += 4;
    b_lufs_s = dv.getFloat32(off, true); off += 4;
    b_tp     = dv.getFloat32(off, true); off += 4;
    sb_prov  = dv.getUint8(off);         off += 1;
  }
  const s_prov_code = sb_prov & 0x0f;
  const b_prov_code = (sb_prov >> 4) & 0x0f;
  // A stream's S beside O (stream.rs): only on a stream.
  const strm = dv.byteLength - off >= 44 ? decodeSTRM(dv, off) : null;

  return {
    magic: 'AAN1',
    version,
    flags,
    hp_active, b_ready, conv_exists, xtc_active, stale, isp_out_active,
    seq,
    track_id_lo, track_id_hi,
    chain_rev,
    coverage_pct,
    n_fft_bins_log2, n_fft_bins,
    spec_floor_neg, spec_hop_log2,
    spec_floor_dbfs, spec_range_dbfs,
    live_metrics,
    prov_codes,
    n_loud_pts,
    loud_pts,
    n_spec_cols,
    spec_cols,
    // SB-EXT fields (NaN if server does not yet send them)
    s_lufs_m, s_lufs_s, s_tp,
    b_lufs_m, b_lufs_s, b_tp,
    sb_prov, s_prov_code, b_prov_code,
    strm,
  };
}

/**
 * The STRM tail of a stream's live frame (stream.rs): S — the stream as it
 * came in — beside O, on the stream's clock. Columns: S's, and O's on S's
 * grid (band j the same frequency in both; O's go on above S's Nyquist);
 * column k begins at sample k·hop of its rate. Null when not a STRM tail.
 * @param {DataView} dv
 * @param {number} off
 */
export function decodeSTRM(dv, off) {
  const ver = dv.getUint8(off + 4);
  if (_magic(dv, off) !== 'STRM' || ver < 1 || ver > 2) return null;
  const flags = dv.getUint8(off + 5);
  const f32 = i => dv.getFloat32(off + i, true);
  const out = {
    flags, sOn: !!(flags & 1), bp: !!(flags & 2), missed: !!(flags & 4),
    l: dv.getUint16(off + 6, true),
    srcRate: dv.getUint32(off + 8, true),
    outRate: dv.getUint32(off + 12, true),
    clockS: dv.getFloat64(off + 16, true),
    sLufsM: f32(24), sLufsS: f32(28), sTp: f32(32), sRms: f32(36), oRms: f32(40),
    sPts: [],
  };
  let o = off + 44;
  const n = dv.getUint16(o, true); o += 2;
  for (let i = 0; i < n; i++, o += 12) {
    out.sPts.push({ time_s: dv.getFloat32(o, true), lufs_s: dv.getFloat32(o + 4, true), lufs_m: dv.getFloat32(o + 8, true) });
  }
  const strip = () => {
    const bins = dv.getUint16(o, true), hop = dv.getUint32(o + 2, true);
    const first = dv.getUint32(o + 6, true), k = dv.getUint16(o + 10, true);
    o += 12;
    const cols = [];
    for (let i = 0; i < k; i++, o += bins) cols.push(new Uint8Array(dv.buffer, dv.byteOffset + o, bins));
    return { bins, hop, first, cols };
  };
  out.sCols = strip();
  out.oCols = strip();
  // v2: the waveform's blocks of S, then of O — block k is the stream's
  // frames k·STRM_ENV_BLOCK.. (O's the same time), each min, max, RMS of L
  // then of R as i16 of full scale.
  const env = () => {
    const k = dv.getUint16(o, true); o += 2;
    const blocks = new Uint32Array(k), v = new Int16Array(k * 6);
    for (let i = 0; i < k; i++, o += 16) {
      blocks[i] = dv.getUint32(o, true);
      for (let j = 0; j < 6; j++) v[i * 6 + j] = dv.getInt16(o + 4 + 2 * j, true);
    }
    return { blocks, v };
  };
  if (ver >= 2) {
    out.sEnv = env();
    out.oEnv = env();
  }
  return out;
}

/** A stream's waveform block, in its own frames (stream.rs ENV_BLOCK). */
export const STRM_ENV_BLOCK = 256;

// ── AAN2 ──────────────────────────────────────────────────────────────────────

/**
 * Decode an AAN2 (Track Summary Frame) binary buffer.
 * Returns the short stub `{magic, rev}` if server signals no-change (8-byte response).
 * @param {ArrayBuffer|DataView} buf
 * @returns {AAN2Frame|AAN2Stub}
 */
export function decodeAAN2(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);

  const magic = _magic(dv, 0);
  if (magic !== 'AAN2') throw new TypeError(`decodeAAN2: bad magic "${magic}"`);

  if (dv.byteLength === 8) {
    // No-change stub: magic(4) + rev(u32)
    return { magic: 'AAN2', stub: true, rev: dv.getUint32(4, true) };
  }

  const version   = dv.getUint8(4);
  if (version < 1) throw new TypeError(`decodeAAN2: unsupported version ${version}`);
  // bytes 5-7 reserved
  const rev        = dv.getUint32(8, true);
  const chain_rev  = dv.getUint32(12, true);
  const duration_s = dv.getFloat32(16, true);

  const s_metrics  = _readF64Array(dv, 20,  32);
  const b_metrics  = _readF64Array(dv, 276, 32);
  const o_metrics  = _readF64Array(dv, 532, 32);

  const s_prov = _decodeProvNibbles(dv, 788, N_METRICS);
  const b_prov = _decodeProvNibbles(dv, 804, N_METRICS);
  const o_prov = _decodeProvNibbles(dv, 820, N_METRICS);

  let off = 836;

  // Per-channel DR: S tap
  const n_channels_s = dv.getUint8(off); off++;
  const dr_s = [];
  for (let c = 0; c < n_channels_s; c++) {
    const peak_dbfs = dv.getFloat32(off, true); off += 4;
    const rms_dbfs  = dv.getFloat32(off, true); off += 4;
    const dr        = dv.getFloat32(off, true); off += 4;
    /* _pad */                                   off += 4;
    dr_s.push({ peak_dbfs, rms_dbfs, dr });
  }

  // Per-channel DR: O tap
  const n_channels_o = dv.getUint8(off); off++;
  const dr_o = [];
  for (let c = 0; c < n_channels_o; c++) {
    const peak_dbfs = dv.getFloat32(off, true); off += 4;
    const rms_dbfs  = dv.getFloat32(off, true); off += 4;
    const dr        = dv.getFloat32(off, true); off += 4;
    /* _pad */                                   off += 4;
    dr_o.push({ peak_dbfs, rms_dbfs, dr });
  }

  // Histogram S
  const hist_n_bins = dv.getUint16(off, true); off += 2;
  const hist_s = _readF32Array(dv, off, hist_n_bins); off += hist_n_bins * 4;

  // Histogram B
  const _hist_b_n   = dv.getUint16(off, true); off += 2;
  const hist_b = _readF32Array(dv, off, _hist_b_n); off += _hist_b_n * 4;

  // DR block list S
  const n_dr_blocks_s = dv.getUint16(off, true); off += 2;
  const dr_blocks_s = [];
  for (let i = 0; i < n_dr_blocks_s; i++) {
    const rms_dbfs  = dv.getFloat32(off, true); off += 4;
    const peak_dbfs = dv.getFloat32(off, true); off += 4;
    dr_blocks_s.push({ rms_dbfs, peak_dbfs });
  }

  // Clip events S
  const n_clips_s = dv.getUint16(off, true); off += 2;
  const clips_s = [];
  for (let i = 0; i < n_clips_s; i++) {
    const start_sample = dv.getUint32(off, true); off += 4;
    const run_len      = dv.getUint16(off, true); off += 2;
    /* _pad */                                      off += 2;
    clips_s.push({ start_sample, run_len });
  }

  // Over/TP events O
  const n_over_o = dv.getUint16(off, true); off += 2;
  const over_o = [];
  for (let i = 0; i < n_over_o; i++) {
    const time_s       = dv.getFloat32(off, true); off += 4;
    const level_dbtp   = dv.getFloat32(off, true); off += 4;
    over_o.push({ time_s, level_dbtp });
  }

  // Chain token string
  const chain_str_len = dv.getUint16(off, true); off += 2;
  const chain_str_bytes = new Uint8Array(dv.buffer, dv.byteOffset + off, chain_str_len);
  const chain_str = new TextDecoder().decode(chain_str_bytes);
  off += chain_str_len;

  // Chain-change markers
  const n_chain_marks = dv.getUint16(off, true); off += 2;
  const chain_marks = [];
  for (let i = 0; i < n_chain_marks; i++) {
    const time_s = dv.getFloat32(off, true); off += 4;
    const from_bytes = new Uint8Array(dv.buffer, dv.byteOffset + off, 10);
    const from_token = new TextDecoder().decode(from_bytes).replace(/\0+$/, '');
    off += 10;
    const to_bytes = new Uint8Array(dv.buffer, dv.byteOffset + off, 10);
    const to_token = new TextDecoder().decode(to_bytes).replace(/\0+$/, '');
    off += 10;
    const stage_id = dv.getUint8(off); off++;
    /* _pad */                          off++;
    chain_marks.push({ time_s, from_token, to_token, stage_id });
  }

  // S loudness series
  const n_lufs_s_s = dv.getUint32(off, true); off += 4;
  const lufs_s_series_s = _readF32Array(dv, off, n_lufs_s_s); off += n_lufs_s_s * 4;
  const lufs_m_series_s = _readF32Array(dv, off, n_lufs_s_s); off += n_lufs_s_s * 4;

  // B loudness series
  const n_lufs_s_b = dv.getUint32(off, true); off += 4;
  const lufs_s_series_b = _readF32Array(dv, off, n_lufs_s_b); off += n_lufs_s_b * 4;
  const lufs_m_series_b = _readF32Array(dv, off, n_lufs_s_b); off += n_lufs_s_b * 4;

  // Mipmap metadata
  const n_lods         = dv.getUint8(off);        off++;
  const n_wave_channels = dv.getUint8(off);        off++;
  const tile_samples   = dv.getUint16(off, true);  off += 2;
  const wave_tile_counts_s = [];
  for (let l = 0; l < n_lods; l++) { wave_tile_counts_s.push(dv.getUint32(off, true)); off += 4; }
  const wave_tile_counts_o = [];
  for (let l = 0; l < n_lods; l++) { wave_tile_counts_o.push(dv.getUint32(off, true)); off += 4; }
  const spec_tile_counts_s = [];
  for (let l = 0; l < n_lods; l++) { spec_tile_counts_s.push(dv.getUint32(off, true)); off += 4; }
  // Optional tail: the analysed signal's sample rate, then the spectrogram
  // tiles' hop in its samples (0 = not sent).
  const src_rate = off + 4 <= dv.byteLength ? dv.getUint32(off, true) : 0;
  const spec_hop = off + 8 <= dv.byteLength ? dv.getUint32(off + 4, true) : 0;
  // Then the true peak (dBTP) of each 100 ms block of S and of B.
  let tp100_s = null, tp100_b = null;
  let t = off + 8;
  if (t + 4 <= dv.byteLength) {
    const read = () => {
      const n = dv.getUint32(t, true); t += 4;
      const a = new Float32Array(n);
      for (let i = 0; i < n; i++) { a[i] = dv.getFloat32(t, true); t += 4; }
      return a;
    };
    tp100_s = read();
    if (t + 4 <= dv.byteLength) tp100_b = read();
  }
  // Then the spectrogram tiles of S, B and O: the pass (its low byte is in
  // each tile), tiles done, tiles in all, bands (O's go on above S's Nyquist).
  let spec_info = null;
  let lufs_s_series_o = null, lufs_m_series_o = null;
  if (t + 48 <= dv.byteLength) {
    spec_info = [];
    for (let k = 0; k < 3; k++) {
      spec_info.push({
        gen: dv.getUint32(t, true), ready: dv.getUint32(t + 4, true),
        total: dv.getUint32(t + 8, true), n_bins: dv.getUint16(t + 12, true),
      });
      t += 16;
    }
    // Then O's loudness series from the O pass (as S's; empty while it runs).
    if (t + 4 <= dv.byteLength) {
      const n = dv.getUint32(t, true); t += 4;
      if (t + n * 8 <= dv.byteLength) {
        lufs_s_series_o = _readF32Array(dv, t, n); t += n * 4;
        lufs_m_series_o = _readF32Array(dv, t, n); t += n * 4;
      }
    }
  }

  return {
    magic: 'AAN2', stub: false, version,
    rev, chain_rev, duration_s,
    s_metrics, b_metrics, o_metrics,
    s_prov, b_prov, o_prov,
    n_channels_s, dr_s,
    n_channels_o, dr_o,
    hist_n_bins, hist_s, hist_b,
    n_dr_blocks_s, dr_blocks_s,
    n_clips_s, clips_s,
    n_over_o, over_o,
    chain_str, n_chain_marks, chain_marks,
    n_lufs_s_s, lufs_s_series_s, lufs_m_series_s,
    n_lufs_s_b, lufs_s_series_b, lufs_m_series_b,
    n_lods, n_wave_channels, tile_samples,
    wave_tile_counts_s, wave_tile_counts_o, spec_tile_counts_s, src_rate, spec_hop,
    tp100_s, tp100_b, spec_info, lufs_s_series_o, lufs_m_series_o,
  };
}

// ── AAN3 ──────────────────────────────────────────────────────────────────────

/**
 * Decode an AAN3 (Response/Chain Frame) binary buffer.
 * @param {ArrayBuffer|DataView} buf
 * @returns {AAN3Frame}
 */
export function decodeAAN3(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);

  const magic = _magic(dv, 0);
  if (magic !== 'AAN3') throw new TypeError(`decodeAAN3: bad magic "${magic}"`);
  const version    = dv.getUint8(4);
  if (version < 1) throw new TypeError(`decodeAAN3: unsupported version ${version}`);
  const scale_flag = dv.getUint8(5);
  const scale      = scale_flag === 0 ? 'log' : 'lin';
  const chain_rev  = dv.getUint32(6, true);
  const n_pts      = dv.getUint32(10, true);
  const f0_actual  = dv.getFloat32(14, true);
  const f1_actual  = dv.getFloat32(18, true);

  let off = 22;
  const h_pairs = [];
  for (let i = 0; i < n_pts; i++) {
    const h_min = dv.getFloat32(off, true); off += 4;
    const h_max = dv.getFloat32(off, true); off += 4;
    h_pairs.push({ min: h_min, max: h_max });
  }

  const o_pairs = [];
  for (let i = 0; i < n_pts; i++) {
    const o_min = dv.getFloat32(off, true); off += 4;
    const o_max = dv.getFloat32(off, true); off += 4;
    o_pairs.push({ min: o_min, max: o_max });
  }

  // Version 2+: S and B Welch PSD pairs (same n_pts)
  const s_pairs = [];
  const b_pairs = [];
  if (version >= 2 && dv.byteLength > off) {
    for (let i = 0; i < n_pts; i++) {
      const s_min = dv.getFloat32(off, true); off += 4;
      const s_max = dv.getFloat32(off, true); off += 4;
      s_pairs.push({ min: s_min, max: s_max });
    }
    for (let i = 0; i < n_pts; i++) {
      const b_min = dv.getFloat32(off, true); off += 4;
      const b_max = dv.getFloat32(off, true); off += 4;
      b_pairs.push({ min: b_min, max: b_max });
    }
  }

  return {
    magic: 'AAN3', version,
    scale, scale_flag,
    chain_rev, n_pts, f0_actual, f1_actual,
    h_pairs, o_pairs, s_pairs, b_pairs,
  };
}

// ── Wave tile (AAWT) ─────────────────────────────────────────────────────────

/**
 * Decode a wave tile (AAWT) binary buffer.
 * @param {ArrayBuffer|DataView} buf
 * @returns {AAWTFrame}
 */
export function decodeWaveTile(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);

  const magic = _magic(dv, 0);
  if (magic !== 'AAWT') throw new TypeError(`decodeWaveTile: bad magic "${magic}"`);
  const version    = dv.getUint8(4);
  if (version < 1) throw new TypeError(`decodeWaveTile: unsupported version ${version}`);
  const src_flag   = dv.getUint8(5);
  const src        = ['S', 'B', 'O'][src_flag] || '?';
  const lod        = dv.getUint8(6);
  const n_channels = dv.getUint8(7);
  const tile_idx   = dv.getUint32(8, true);
  const n_samples  = dv.getUint32(12, true);

  let off = 16;
  const samples = [];
  for (let s = 0; s < n_samples; s++) {
    const chans = [];
    for (let c = 0; c < n_channels; c++) {
      const min = dv.getFloat32(off, true); off += 4;
      const max = dv.getFloat32(off, true); off += 4;
      const rms = dv.getFloat32(off, true); off += 4;
      chans.push({ min, max, rms });
    }
    samples.push(chans);
  }

  return { magic: 'AAWT', version, src, lod, n_channels, tile_idx, n_samples, samples };
}

// ── Spec tile (AAST) ─────────────────────────────────────────────────────────

/**
 * Decode a spectrogram tile (AAST) binary buffer.
 * @param {ArrayBuffer|DataView} buf
 * @returns {AASTFrame}
 */
export function decodeSpecTile(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);

  const magic = _magic(dv, 0);
  if (magic !== 'AAST') throw new TypeError(`decodeSpecTile: bad magic "${magic}"`);
  const version  = dv.getUint8(4);
  if (version < 1) throw new TypeError(`decodeSpecTile: unsupported version ${version}`);
  const src_flag = dv.getUint8(5);
  const src      = ['S', 'B', 'O'][src_flag] || '?';
  const lod      = dv.getUint8(6);
  // byte 7 reserved
  const tile_idx = dv.getUint32(8, true);
  const dB_floor = dv.getFloat32(12, true);
  const dB_range = dv.getFloat32(16, true);
  const n_cols   = dv.getUint16(20, true);
  const n_bins   = dv.getUint16(22, true);

  const data = new Uint8Array(dv.buffer, dv.byteOffset + 24, n_cols * n_bins);

  return { magic: 'AAST', version, src, lod, tile_idx, dB_floor, dB_range, n_cols, n_bins, data };
}

// ── AASL ──────────────────────────────────────────────────────────────────────

/**
 * Decode a selection's statistics (AASL): the whole-track metrics of one signal
 * over source samples [s0, s1) and its spectrum there. status 0 = being computed,
 * 1 = done, 2 = not available (the frame then ends after s1).
 * @param {ArrayBuffer|DataView} buf
 */
export function decodeAASL(buf) {
  const dv = buf instanceof DataView ? buf : new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer);
  const magic = _magic(dv, 0);
  if (magic !== 'AASL') throw new TypeError(`decodeAASL: bad magic "${magic}"`);
  const version = dv.getUint8(4);
  if (version < 1) throw new TypeError(`decodeAASL: unsupported version ${version}`);
  const status = dv.getUint8(5);
  const src = ['S', 'B', 'O'][dv.getUint8(6)] || '?';
  // Kept in f32 though it has more (O): below about −180 dBFS is rounding.
  const f32_kept = (dv.getUint8(7) & 1) === 1;
  const gen = dv.getUint32(8, true);
  const s0 = Number(dv.getBigUint64(12, true));
  const s1 = Number(dv.getBigUint64(20, true));
  const head = { magic, version, status, src, f32_kept, gen, s0, s1 };
  if (status !== 1 || dv.byteLength < 28 + 256 + 16 + 16) return head;
  const metrics = _readF64Array(dv, 28, 32);
  const prov = _decodeProvNibbles(dv, 284, N_METRICS);
  let off = 300;
  const welch_n = dv.getUint32(off, true); off += 4;
  const n_pts = dv.getUint16(off, true); off += 4;
  const f0 = dv.getFloat32(off, true); off += 4;
  const f1 = dv.getFloat32(off, true); off += 4;
  const pairs = new Float32Array(n_pts * 2);
  for (let i = 0; i < n_pts * 2 && off + 4 <= dv.byteLength; i++) { pairs[i] = dv.getFloat32(off, true); off += 4; }
  return { ...head, metrics, prov, welch_n, f0, f1, pairs };
}
