/**
 * protocol.test.mjs — Round-trip tests for protocol.js against golden byte vectors.
 *
 * Run: node protocol.test.mjs
 *
 * Each golden .bin in src-tauri/src/player/analytics/golden/ (shared with proto.rs) is decoded and compared field-by-field
 * against the corresponding .json.  All five frame types are tested.
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

// Resolve protocol.js relative to this test file
import {
  METRIC_ID, PROV, N_METRICS, N_PROV_BYTES,
  decodeAAN1, decodeAAN2, decodeAAN3, decodeWaveTile, decodeSpecTile,
  formatMetric,
} from './protocol.js';

const __dir  = dirname(fileURLToPath(import.meta.url));
// The same vectors proto.rs's golden tests read: desktop-app/src-tauri/src/player/analytics/golden/.
const GOLDEN_DIR = resolve(__dir, '../../../src-tauri/src/player/analytics/golden');

// ── utilities ──────────────────────────────────────────────────────────────────

let passed = 0;
let failed = 0;

function ok(name, got, expected) {
  if (Object.is(got, expected) || (Number.isNaN(got) && Number.isNaN(expected))) {
    passed++;
  } else {
    failed++;
    console.error(`  FAIL [${name}]: got ${JSON.stringify(got)} expected ${JSON.stringify(expected)}`);
  }
}

function approx(name, got, expected, eps = 1e-5) {
  if (typeof got !== 'number' || typeof expected !== 'number') {
    ok(name, got, expected);
    return;
  }
  if (Number.isNaN(got) && Number.isNaN(expected)) { passed++; return; }
  if (Math.abs(got - expected) <= eps) { passed++; }
  else {
    failed++;
    console.error(`  FAIL [${name}]: got ${got} expected ${expected} (eps=${eps})`);
  }
}

function section(label) {
  console.log(`\n=== ${label} ===`);
}

function readBin(name) {
  const nodeBuf = readFileSync(resolve(GOLDEN_DIR, name));
  // Node Buffer.buffer may be a shared pool; slice to exact extent.
  return nodeBuf.buffer.slice(nodeBuf.byteOffset, nodeBuf.byteOffset + nodeBuf.byteLength);
}

function readJSON(name) {
  return JSON.parse(readFileSync(resolve(GOLDEN_DIR, name), 'utf8'));
}

// ── Constants sanity ───────────────────────────────────────────────────────────

section('Constants');
ok('N_METRICS',       N_METRICS, 26);
ok('N_PROV_BYTES',    N_PROV_BYTES, 13);
ok('METRIC_ID.LUFS_I',        METRIC_ID.LUFS_I,        0);
ok('METRIC_ID.COVERAGE_O',    METRIC_ID.COVERAGE_O,    25);
ok('PROV.UNAVAIL',    PROV.UNAVAIL, 0);
ok('PROV.ANALYTIC',   PROV.ANALYTIC, 1);
ok('PROV.FORECAST',   PROV.FORECAST, 2);
ok('PROV.MEASURED',   PROV.MEASURED, 3);
ok('PROV.UNCHANGED',  PROV.UNCHANGED, 4);
ok('PROV.HP_PENDING', PROV.HP_PENDING, 5);
ok('PROV.STALE',      PROV.STALE, 6);
ok('PROV.COMPUTING',  PROV.COMPUTING, 7);

// ── formatMetric ───────────────────────────────────────────────────────────────

section('formatMetric');
ok('LUFS_I NaN prov COMPUTING',   formatMetric(METRIC_ID.LUFS_I, NaN, PROV.COMPUTING), '···');
ok('LUFS_I NaN prov UNAVAIL',     formatMetric(METRIC_ID.LUFS_I, NaN, PROV.UNAVAIL),   '—');
ok('LUFS_I -9.71',                formatMetric(METRIC_ID.LUFS_I, -9.71, PROV.FORECAST), '-9.71 LUFS');
ok('LRA 2.51',                    formatMetric(METRIC_ID.LRA,    2.51,  PROV.MEASURED), '2.51 LU');
ok('TP_EBUR positive',            formatMetric(METRIC_ID.TP_EBUR, 1.36, PROV.ANALYTIC), '+1.36 dBTP');
ok('TP_EBUR negative',            formatMetric(METRIC_ID.TP_EBUR, -0.52, PROV.FORECAST), '-0.52 dBTP');
ok('COVERAGE_O 63',               formatMetric(METRIC_ID.COVERAGE_O, 63, PROV.MEASURED), '63.0%');
ok('omitUnit',                    formatMetric(METRIC_ID.LUFS_I, -9.71, PROV.FORECAST, { omitUnit: true }), '-9.71');

// ── AAN1 ───────────────────────────────────────────────────────────────────────

section('AAN1 (live frame)');
const aan1bin  = readBin('aan1.bin');
const aan1json = readJSON('aan1.json');
const aan1     = decodeAAN1(aan1bin);

ok('magic',         aan1.magic,   'AAN1');
ok('version',       aan1.version, aan1json.version);
ok('flags',         aan1.flags,   aan1json.flags);
ok('hp_active',     aan1.hp_active,   aan1json.hp_active);
ok('b_ready',       aan1.b_ready,     aan1json.b_ready);
ok('conv_exists',   aan1.conv_exists, aan1json.conv_exists);
ok('xtc_active',    aan1.xtc_active,  aan1json.xtc_active);
ok('stale',         aan1.stale,       aan1json.stale);
ok('seq',           aan1.seq,         aan1json.seq);
ok('track_id_lo',   aan1.track_id_lo, aan1json.track_id_lo);
ok('track_id_hi',   aan1.track_id_hi, aan1json.track_id_hi);
ok('chain_rev',     aan1.chain_rev,   aan1json.chain_rev);
ok('coverage_pct',  aan1.coverage_pct, aan1json.coverage_pct);
ok('n_fft_bins_log2', aan1.n_fft_bins_log2, aan1json.n_fft_bins_log2);
ok('spec_floor_neg',  aan1.spec_floor_neg,  aan1json.spec_floor_neg);

// Metrics: check defined ones (NaN for others)
for (let i = 0; i < 26; i++) {
  const got      = aan1.live_metrics[i];
  const expected = aan1json.live_metrics[i];
  if (expected === null) {
    ok(`live_metrics[${i}] NaN`, isNaN(got), true);
  } else {
    approx(`live_metrics[${i}]`, got, expected, 1e-4);
  }
}

// Provenance codes
for (let i = 0; i < 26; i++) {
  ok(`prov_codes[${i}]`, aan1.prov_codes[i], aan1json.prov_codes[i]);
}

ok('n_loud_pts',    aan1.n_loud_pts, aan1json.n_loud_pts);
approx('loud_pts[0].time_s', aan1.loud_pts[0].time_s, aan1json.loud_pts[0].time_s, 0.01);
approx('loud_pts[0].lufs_s', aan1.loud_pts[0].lufs_s, aan1json.loud_pts[0].lufs_s, 0.01);
approx('loud_pts[0].lufs_m', aan1.loud_pts[0].lufs_m, aan1json.loud_pts[0].lufs_m, 0.01);
ok('n_spec_cols',   aan1.n_spec_cols, aan1json.n_spec_cols);

const expected_spec_bytes = aan1json.spec_cols_total_bytes;
let got_spec_bytes = 0;
for (const col of aan1.spec_cols) got_spec_bytes += col.length;
ok('spec_cols_total_bytes', got_spec_bytes, expected_spec_bytes);

// ── AAN2 ───────────────────────────────────────────────────────────────────────

section('AAN2 (track summary)');
const aan2bin  = readBin('aan2.bin');
const aan2json = readJSON('aan2.json');
const aan2     = decodeAAN2(aan2bin);

ok('magic',       aan2.magic,   'AAN2');
ok('stub',        aan2.stub,    false);
ok('version',     aan2.version, aan2json.version);
ok('rev',         aan2.rev,     aan2json.rev);
ok('chain_rev',   aan2.chain_rev, aan2json.chain_rev);
approx('duration_s', aan2.duration_s, aan2json.duration_s, 1e-4);

// S metrics spot-check (LUFS_I=-6.35, LRA=2.51)
approx('s_metrics[LUFS_I]', aan2.s_metrics[0], aan2json.s_metrics[0], 1e-6);
approx('s_metrics[LRA]',    aan2.s_metrics[3], aan2json.s_metrics[3], 1e-6);
ok('s_metrics[12] CLIPS_GE2', aan2.s_metrics[12], aan2json.s_metrics[12]);
// B metrics spot-check
approx('b_metrics[0]', aan2.b_metrics[0], aan2json.b_metrics[0], 1e-6);
// O metrics spot-check
approx('o_metrics[0]', aan2.o_metrics[0], aan2json.o_metrics[0], 1e-6);
approx('o_metrics[25]', aan2.o_metrics[25], aan2json.o_metrics[25], 1e-6);

// Prov
ok('s_prov[0]',   aan2.s_prov[0],  aan2json.s_prov[0]);
ok('o_prov[0]',   aan2.o_prov[0],  aan2json.o_prov[0]);
ok('o_prov[3]',   aan2.o_prov[3],  aan2json.o_prov[3]);  // UNCHANGED

ok('n_channels_s', aan2.n_channels_s, aan2json.n_channels_s);
ok('n_channels_o', aan2.n_channels_o, aan2json.n_channels_o);
ok('hist_n_bins',  aan2.hist_n_bins,  aan2json.hist_n_bins);

ok('n_dr_blocks_s', aan2.n_dr_blocks_s, aan2json.n_dr_blocks_s);
ok('n_clips_s',    aan2.n_clips_s,    aan2json.n_clips_s);
ok('clips_s[0].start_sample', aan2.clips_s[0].start_sample, aan2json.clips_s[0].start);
ok('clips_s[0].run_len',      aan2.clips_s[0].run_len,      aan2json.clips_s[0].len);
ok('n_over_o',     aan2.n_over_o,     aan2json.n_over_o);

ok('chain_str',    aan2.chain_str,    aan2json.chain_str);
ok('n_chain_marks', aan2.n_chain_marks, aan2json.n_chain_marks);
approx('chain_marks[0].time_s', aan2.chain_marks[0].time_s, aan2json.chain_marks[0].time_s, 0.01);
ok('chain_marks[0].from_token', aan2.chain_marks[0].from_token, aan2json.chain_marks[0].from_token);
ok('chain_marks[0].to_token',   aan2.chain_marks[0].to_token,   aan2json.chain_marks[0].to_token);
ok('chain_marks[0].stage_id',   aan2.chain_marks[0].stage_id,   aan2json.chain_marks[0].stage_id);

ok('n_lufs_s_s', aan2.n_lufs_s_s, aan2json.n_lufs_s_s);
ok('n_lufs_s_b', aan2.n_lufs_s_b, aan2json.n_lufs_s_b);
ok('n_lods',     aan2.n_lods,     aan2json.n_lods);
ok('tile_samples', aan2.tile_samples, aan2json.tile_samples);

// AAN2 no-change stub
section('AAN2 no-change stub');
const stubBuf = new ArrayBuffer(8);
const stubDv  = new DataView(stubBuf);
stubDv.setUint8(0, 0x41); stubDv.setUint8(1, 0x41);
stubDv.setUint8(2, 0x4E); stubDv.setUint8(3, 0x32);
stubDv.setUint32(4, 42, true);
const stub2 = decodeAAN2(stubBuf);
ok('stub magic',  stub2.magic, 'AAN2');
ok('stub true',   stub2.stub,  true);
ok('stub rev',    stub2.rev,   42);

// ── AAN3 ───────────────────────────────────────────────────────────────────────

section('AAN3 (response frame)');
const aan3bin  = readBin('aan3.bin');
const aan3json = readJSON('aan3.json');
const aan3     = decodeAAN3(aan3bin);

ok('magic',     aan3.magic,   'AAN3');
ok('version',   aan3.version, aan3json.version);
ok('scale',     aan3.scale,   aan3json.scale);
ok('chain_rev', aan3.chain_rev, aan3json.chain_rev);
ok('n_pts',     aan3.n_pts,   aan3json.n_pts);
approx('f0_actual', aan3.f0_actual, aan3json.f0_actual, 0.01);
approx('f1_actual', aan3.f1_actual, aan3json.f1_actual, 0.01);

for (let i = 0; i < aan3json.h_pairs.length; i++) {
  approx(`h_pairs[${i}].min`, aan3.h_pairs[i].min, aan3json.h_pairs[i].min, 0.01);
  approx(`h_pairs[${i}].max`, aan3.h_pairs[i].max, aan3json.h_pairs[i].max, 0.01);
}
for (let i = 0; i < aan3json.o_pairs.length; i++) {
  approx(`o_pairs[${i}].min`, aan3.o_pairs[i].min, aan3json.o_pairs[i].min, 0.01);
  approx(`o_pairs[${i}].max`, aan3.o_pairs[i].max, aan3json.o_pairs[i].max, 0.01);
}
for (let i = 0; i < aan3json.s_pairs.length; i++) {
  approx(`s_pairs[${i}].min`, aan3.s_pairs[i].min, aan3json.s_pairs[i].min, 0.01);
  approx(`s_pairs[${i}].max`, aan3.s_pairs[i].max, aan3json.s_pairs[i].max, 0.01);
}
for (let i = 0; i < aan3json.b_pairs.length; i++) {
  approx(`b_pairs[${i}].min`, aan3.b_pairs[i].min, aan3json.b_pairs[i].min, 0.01);
  approx(`b_pairs[${i}].max`, aan3.b_pairs[i].max, aan3json.b_pairs[i].max, 0.01);
}

// ── Wave tile (AAWT) ──────────────────────────────────────────────────────────

section('Wave tile (AAWT)');
const wavebin  = readBin('wave_tile.bin');
const wavejson = readJSON('wave_tile.json');
const wave     = decodeWaveTile(wavebin);

ok('magic',       wave.magic,   'AAWT');
ok('version',     wave.version, wavejson.version);
ok('src',         wave.src,     wavejson.src);
ok('lod',         wave.lod,     wavejson.lod);
ok('n_channels',  wave.n_channels, wavejson.n_channels);
ok('tile_idx',    wave.tile_idx,   wavejson.tile_idx);
ok('n_samples',   wave.n_samples,  wavejson.n_samples);

for (let s = 0; s < wavejson.n_samples; s++) {
  for (let c = 0; c < wavejson.n_channels; c++) {
    const gi = s * wavejson.n_channels + c;
    const exp = wavejson.samples[gi];
    const got = wave.samples[s][c];
    approx(`samples[${s}][${c}].min`, got.min, exp.min, 1e-5);
    approx(`samples[${s}][${c}].max`, got.max, exp.max, 1e-5);
    approx(`samples[${s}][${c}].rms`, got.rms, exp.rms, 1e-5);
  }
}

// ── Spec tile (AAST) ──────────────────────────────────────────────────────────

section('Spec tile (AAST)');
const specbin  = readBin('spec_tile.bin');
const specjson = readJSON('spec_tile.json');
const spec     = decodeSpecTile(specbin);

ok('magic',    spec.magic,   'AAST');
ok('version',  spec.version, specjson.version);
ok('src',      spec.src,     specjson.src);
ok('lod',      spec.lod,     specjson.lod);
ok('tile_idx', spec.tile_idx,  specjson.tile_idx);
approx('dB_floor', spec.dB_floor, specjson.dB_floor, 0.01);
approx('dB_range', spec.dB_range, specjson.dB_range, 0.01);
ok('n_cols',   spec.n_cols,  specjson.n_cols);
ok('n_bins',   spec.n_bins,  specjson.n_bins);

for (let i = 0; i < specjson.data.length; i++) {
  ok(`data[${i}]`, spec.data[i], specjson.data[i]);
}

// ── Error handling ─────────────────────────────────────────────────────────────

section('Error handling');
let threw = false;
try { decodeAAN1(new ArrayBuffer(4)); } catch (e) { threw = true; }
ok('bad magic throws', threw, true);

threw = false;
try { decodeAAN2(new ArrayBuffer(8).slice(0,4)); } catch (e) { threw = true; }
ok('truncated AAN2 throws', threw, true);

// ── Summary ───────────────────────────────────────────────────────────────────

console.log(`\n✓ ${passed} passed   ${failed ? `✗ ${failed} FAILED` : '0 failed'}`);
if (failed > 0) process.exit(1);
