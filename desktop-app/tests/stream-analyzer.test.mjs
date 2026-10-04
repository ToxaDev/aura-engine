// The analyzer on a stream: the live frame's STRM tail (stream.rs) read back,
// and the words the page makes of a stream (its line, its bandwidth).
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  decodeAAN1, bandwidthText, bandwidthShort, streamTechText, STREAM_TEXTS, STRM_ENV_BLOCK,
} from '../src/js/analytics/protocol.js';

/** An AAN1 frame as live.rs/proto.rs write it (no points, no O columns),
 *  SB-EXT, then `tail`. */
function aan1With(tail) {
  const head = new ArrayBuffer(144 + 2 + 2 + 25);
  const dv = new DataView(head);
  [...'AAN1'].forEach((c, i) => dv.setUint8(i, c.charCodeAt(0)));
  dv.setUint8(4, 2);
  dv.setUint8(23, 9);
  dv.setUint8(24, 120);
  for (let i = 0; i < 26; i++) dv.setFloat32(26 + 4 * i, NaN, true);
  for (let i = 0; i < 6; i++) dv.setFloat32(148 + 4 * i, NaN, true);
  const out = new Uint8Array(head.byteLength + tail.byteLength);
  out.set(new Uint8Array(head), 0);
  out.set(new Uint8Array(tail), head.byteLength);
  return out.buffer;
}

/** A STRM tail laid out as stream.rs documents it. */
function strm({ flags = 1, l = 8, src = 44100, out = 352800, clock = 12.5, pts = [], s, o, env = null }) {
  const strip = st => 12 + st.cols.length * st.bins;
  const n = 46 + pts.length * 12 + strip(s) + strip(o) + (env ? 4 + (env.s.length + env.o.length) * 16 : 0);
  const buf = new ArrayBuffer(n);
  const dv = new DataView(buf);
  [...'STRM'].forEach((c, i) => dv.setUint8(i, c.charCodeAt(0)));
  dv.setUint8(4, env ? 2 : 1);
  dv.setUint8(5, flags);
  dv.setUint16(6, l, true);
  dv.setUint32(8, src, true);
  dv.setUint32(12, out, true);
  dv.setFloat64(16, clock, true);
  [-14.5, -15.25, -1.5, -18.0, -19.5].forEach((v, i) => dv.setFloat32(24 + 4 * i, v, true));
  dv.setUint16(44, pts.length, true);
  let off = 46;
  for (const p of pts) {
    dv.setFloat32(off, p[0], true); dv.setFloat32(off + 4, p[1], true); dv.setFloat32(off + 8, p[2], true);
    off += 12;
  }
  for (const st of [s, o]) {
    dv.setUint16(off, st.bins, true);
    dv.setUint32(off + 2, st.hop, true);
    dv.setUint32(off + 6, st.first, true);
    dv.setUint16(off + 10, st.cols.length, true);
    off += 12;
    for (const c of st.cols) { new Uint8Array(buf, off, st.bins).set(c); off += st.bins; }
  }
  // v2: the waveform's blocks of S, then of O: [block, [6 × i16]].
  for (const e of env ? [env.s, env.o] : []) {
    dv.setUint16(off, e.length, true); off += 2;
    for (const [k, v] of e) {
      dv.setUint32(off, k, true);
      v.forEach((x, j) => dv.setInt16(off + 4 + 2 * j, x, true));
      off += 16;
    }
  }
  return buf;
}

test('a stream frame carries S beside O in its STRM tail', () => {
  const s = { bins: 4, hop: 2048, first: 7, cols: [[1, 2, 3, 4], [5, 6, 7, 8]] };
  const o = { bins: 6, hop: 16384, first: 7, cols: [[9, 9, 9, 9, 9, 9], [1, 1, 1, 1, 1, 2]] };
  const f = decodeAAN1(aan1With(strm({ flags: 1 | 2, pts: [[12.5, -15.25, -14.5]], s, o })));
  const t = f.strm;
  assert.ok(t, 'the tail is read');
  assert.equal(t.sOn, true);
  assert.equal(t.bp, true);
  assert.equal(t.missed, false);
  assert.equal(t.l, 8);
  assert.equal(t.srcRate, 44100);
  assert.equal(t.outRate, 352800);
  assert.equal(t.clockS, 12.5);
  assert.equal(t.sLufsM, -14.5);
  assert.equal(t.sLufsS, -15.25);
  assert.equal(t.sTp, -1.5);
  assert.equal(t.sRms, -18);
  assert.equal(t.oRms, -19.5);
  assert.deepEqual(t.sPts, [{ time_s: 12.5, lufs_s: -15.25, lufs_m: -14.5 }]);
  assert.equal(t.sCols.bins, 4);
  assert.equal(t.sCols.hop, 2048);
  assert.equal(t.sCols.first, 7);
  assert.deepEqual(t.sCols.cols.map(c => [...c]), s.cols);
  assert.equal(t.oCols.bins, 6);
  assert.equal(t.oCols.hop, 16384);
  assert.deepEqual(t.oCols.cols.map(c => [...c]), o.cols);
});

test('a file frame has no STRM tail', () => {
  const f = decodeAAN1(aan1With(new ArrayBuffer(0)));
  assert.equal(f.strm, null);
  assert.ok(Number.isNaN(f.s_lufs_m));
});

test('the bandwidth line says a cut, a full band or that it is still measuring', () => {
  assert.equal(bandwidthText({ state: 'ok', hz: 16012.4, full: false, nyq: 22050 }), 'Source bandwidth ≈ 16.0 kHz');
  assert.equal(bandwidthText({ state: 'ok', hz: 22050, full: true, nyq: 22050 }), 'Source bandwidth: full, 22.05 kHz');
  assert.equal(bandwidthText({ state: 'ok', hz: 24000, full: true, nyq: 24000 }), 'Source bandwidth: full, 24 kHz');
  assert.equal(bandwidthText({ state: 'measuring', hz: null, full: false, nyq: 22050 }), 'Source bandwidth: measuring…');
  assert.equal(bandwidthText(undefined), 'Source bandwidth: measuring…');
  assert.equal(bandwidthShort({ state: 'ok', hz: 19480, full: false, nyq: 24000 }), '≈ 19.5 kHz');
});

test("the stream's line: codec, bitrate, rate and depth as the radio writes them", () => {
  const name = c => ({ mp3: 'MP3', flac: 'FLAC', aac: 'AAC' }[c] || String(c).toUpperCase());
  assert.equal(streamTechText({ codec: 'mp3', icyBr: '128', rate: 44100 }, name), 'MP3 · 128 kbps · 44.1 kHz');
  assert.equal(streamTechText({ codec: 'flac', icyBr: '', rate: 44100, bits: 16 }, name), 'FLAC · 44.1 kHz/16');
  assert.equal(streamTechText({ codec: 'aac', icyBr: '96,128', rate: 48000 }, name), 'AAC · 96 kbps · 48 kHz');
  assert.equal(streamTechText({ codec: 'mp3' }, name), '', 'no rate yet: nothing to say');
});

test('the stream words are the ones agreed', () => {
  assert.equal(STREAM_TEXTS.song, 'This song');
  assert.equal(STREAM_TEXTS.session, 'Session');
  assert.equal(STREAM_TEXTS.bp, '= S');
  assert.match(STREAM_TEXTS.songTip, /the edges are approximate\.$/);
  for (const v of Object.values(STREAM_TEXTS)) assert.doesNotMatch(v, /[Ѐ-ӿ]/, 'English only');
});

test('a v2 frame carries the waveform blocks of S and of O; a v1 frame none', () => {
  const s = { bins: 4, hop: 2048, first: 7, cols: [[1, 2, 3, 4]] };
  const o = { bins: 6, hop: 16384, first: 7, cols: [[9, 9, 9, 9, 9, 9]] };
  const env = {
    s: [[40, [-1000, 2000, 900, -32767, 32767, 12000]], [41, [-5, 5, 3, 0, 0, 0]]],
    o: [[40, [-990, 1990, 880, -32000, 32000, 11000]]],
  };
  const f = decodeAAN1(aan1With(strm({ s, o, env }))).strm;
  assert.deepEqual([...f.sEnv.blocks], [40, 41]);
  assert.deepEqual([...f.sEnv.v], [-1000, 2000, 900, -32767, 32767, 12000, -5, 5, 3, 0, 0, 0]);
  assert.deepEqual([...f.oEnv.blocks], [40]);
  assert.deepEqual([...f.oEnv.v.slice(0, 3)], [-990, 1990, 880]);
  assert.deepEqual(f.sCols.cols.map(c => [...c]), [[1, 2, 3, 4]], 'the columns read as before');
  const v1 = decodeAAN1(aan1With(strm({ s, o }))).strm;
  assert.equal(v1.sEnv, undefined);
  assert.equal(STRM_ENV_BLOCK, 256);
});
