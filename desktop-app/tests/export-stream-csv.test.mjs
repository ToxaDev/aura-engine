// The CSV of a live stream: its loudness as the page keeps it, a row per
// 100 ms on the stream's clock, S and O, the song's TP and title; a file's
// CSV as it was.
// Run: node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { buildCSV } from '../src/js/analytics/export.js';

const pts = (a, b, v) => {
  const out = [];
  for (let k = Math.round(a * 10); k < Math.round(b * 10); k++) out.push({ time_s: k / 10 + 0.003, value: v });
  return out;
};

test("a stream's CSV: S and O by the stream's clock, each row's song and its TP", () => {
  const store = {
    _stream: true,
    series: {
      lufs_s_s_live: pts(100, 103, -20), lufs_m_s_live: pts(100, 103, -21),
      lufs_s_o: pts(101, 104, -14), lufs_m_o: pts(101, 104, -15),
    },
    _streamTotals: {
      song: { title: 'B, "Two"', url: 'u1', startS: 102, s: { tp: -1.25 }, o: { tp: -0.5 } },
      songs: [
        { title: 'A - One', url: 'u1', startS: 50, s: { tp: -3 }, o: { tp: -2 } },
        { title: 'Z - Other station', url: 'u0', startS: 90, s: { tp: -9 }, o: { tp: -9 } },
      ],
    },
  };
  const lines = buildCSV(store).split('\n');
  assert.equal(lines[0], 'time_s,lufs_s_S[M],lufs_s_O[M],lufs_m_S[M],lufs_m_O[M],tp_S[M],tp_O[M],song');
  assert.equal(lines.length, 1 + 40, 'a row per 100 ms from 100.0 to 103.9');
  assert.equal(lines[1], '100.0,-20.00,,-21.00,,-3.00,-2.00,A - One', 'S only; the song before');
  assert.equal(lines[11], '101.0,-20.00,-14.00,-21.00,-15.00,-3.00,-2.00,A - One');
  assert.equal(lines[21], '102.0,-20.00,-14.00,-21.00,-15.00,-1.25,-0.50,"B, ""Two"""', 'the song now, its title quoted');
  assert.equal(lines.at(-1), '103.9,,-14.00,,-15.00,-1.25,-0.50,"B, ""Two"""', 'O only at the end');
  assert.ok(!lines.some(l => l.includes('Other station')), 'another station\'s song is on another clock');
});

test('a stream with no titles: the song column stays empty', () => {
  const store = { _stream: true, series: { lufs_s_s_live: pts(0, 0.3, -18) }, _streamTotals: { song: null, songs: [{ title: 'X', url: 'u', startS: 0 }] } };
  const lines = buildCSV(store).split('\n');
  assert.deepEqual(lines.slice(1), ['0.0,-18.00,,,,,,', '0.1,-18.00,,,,,,', '0.2,-18.00,,,,,,']);
});

test("a file's CSV as before", () => {
  const store = {
    metrics: { s: new Float64Array(26).fill(NaN), o: new Float64Array(26).fill(NaN), sProv: new Uint8Array(26), oProv: new Uint8Array(26) },
    series: { lufs_s_s: [-20, -19.5], lufs_m_s: [-21, NaN], lufs_s_o_whole: [-14, -13.5], lufs_m_o_whole: [-15, -14.5] },
  };
  const lines = buildCSV(store).split('\n');
  assert.equal(lines[0], 'time_s,lufs_s_S[M],lufs_s_O,lufs_m_S[M],lufs_m_O,tp_S[M],tp_O');
  assert.deepEqual(lines.slice(1), ['0.4,-20.00,-14.00,-21.00,-15.00,,', '0.5,-19.50,-13.50,,-14.50,,']);
});
