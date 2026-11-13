// Aura EQ profiles (src/js/eq-profile.js): what we read from AutoEq, Equalizer
// APO, REW and squig.link texts, and that our own export reads back to the
// same response. All samples are made up in the formats those tools write.

import test from 'node:test';
import assert from 'node:assert/strict';

import { parseProfile, exportApo, describe, MAX_BANDS } from '../src/js/eq-profile.js';

// --- response math for the round-trip check (RBJ, as in SPEC) ---
const FS = 48000;
function rbj(type, f, g, q) {
    const A = Math.pow(10, g / 40), w = 2 * Math.PI * f / FS, c = Math.cos(w), s = Math.sin(w), al = s / (2 * q);
    switch (type) {
        case 'bell': return [1 + al * A, -2 * c, 1 - al * A, 1 + al / A, -2 * c, 1 - al / A];
        case 'lowShelf': { const k = 2 * Math.sqrt(A) * al; return [A * ((A + 1) - (A - 1) * c + k), 2 * A * ((A - 1) - (A + 1) * c), A * ((A + 1) - (A - 1) * c - k), (A + 1) + (A - 1) * c + k, -2 * ((A - 1) + (A + 1) * c), (A + 1) + (A - 1) * c - k]; }
        case 'highShelf': { const k = 2 * Math.sqrt(A) * al; return [A * ((A + 1) + (A - 1) * c + k), -2 * A * ((A - 1) + (A + 1) * c), A * ((A + 1) + (A - 1) * c - k), (A + 1) - (A - 1) * c + k, 2 * ((A - 1) - (A + 1) * c), (A + 1) - (A - 1) * c - k]; }
        case 'highCut': return [(1 - c) / 2, 1 - c, (1 - c) / 2, 1 + al, -2 * c, 1 - al];
        case 'lowCut': return [(1 + c) / 2, -(1 + c), (1 + c) / 2, 1 + al, -2 * c, 1 - al];
        case 'notch': return [1, -2 * c, 1, 1 + al, -2 * c, 1 - al];
        case 'bandPass': return [al, 0, -al, 1 + al, -2 * c, 1 - al];
    }
}
function sections(b) {
    if (b.type === 'tilt') return [rbj('highShelf', b.f, b.gain / 2, 0.6), rbj('lowShelf', b.f, -b.gain / 2, 0.6)];
    if (b.type !== 'lowCut' && b.type !== 'highCut') return [rbj(b.type, b.f, b.gain, b.q)];
    const N = Math.round(b.slope / 6), out = [];
    assert.equal(N % 2, 0, 'odd cuts are not compared here');
    for (let k = 0; k < N / 2; k++) {
        let q = 1 / (2 * Math.cos(Math.PI * (2 * k + 1) / (2 * N)));
        if (k === N / 2 - 1) q *= b.q / Math.SQRT1_2;
        out.push(rbj(b.type, b.f, 0, q));
    }
    return out;
}
function magDb(co, f) {
    const w = 2 * Math.PI * f / FS, c1 = Math.cos(w), s1 = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w);
    const nr = co[0] + co[1] * c1 + co[2] * c2, ni = -(co[1] * s1 + co[2] * s2), dr = co[3] + co[4] * c1 + co[5] * c2, di = -(co[4] * s1 + co[5] * s2);
    return 10 * Math.log10(Math.max(1e-30, (nr * nr + ni * ni) / (dr * dr + di * di)));
}
const GRID = Array.from({ length: 400 }, (_, i) => 20 * Math.pow(1000, i / 399));
const response = (bands, pre = 0) => GRID.map(f => bands.filter(b => b.on !== false)
    .reduce((a, b) => a + sections(b).reduce((s, co) => s + magDb(co, f), 0), pre));

// --- samples ---
const AUTOEQ = `Preamp: -6.0 dB
Filter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.70
Filter 2: ON PK Fc 180 Hz Gain -3.3 dB Q 0.57
Filter 3: ON PK Fc 9140 Hz Gain 5.6 dB Q 3.46
Filter 4: ON PK Fc 3842 Hz Gain 6.4 dB Q 3.05
Filter 5: ON PK Fc 2458 Hz Gain -5.3 dB Q 4.04
Filter 6: ON HSC Fc 10000 Hz Gain -1.1 dB Q 0.70
Filter 7: ON PK Fc 1244 Hz Gain 1.4 dB Q 1.41
Filter 8: ON PK Fc 5590 Hz Gain -2.0 dB Q 5.12
Filter 9: ON PK Fc 41 Hz Gain 0.8 dB Q 1.00
Filter 10: ON PK Fc 7000 Hz Gain 1.2 dB Q 2.00
`;

test('AutoEq ParametricEQ: ten filters and the preamp', () => {
    const r = parseProfile(AUTOEQ);
    assert.equal(r.kind, 'apo');
    assert.equal(r.preamp, -6);
    assert.equal(r.bands.length, 10);
    assert.deepEqual(r.bands[0], { type: 'lowShelf', f: 105, gain: 5.5, q: 0.7, slope: 12, on: true });
    assert.deepEqual(r.bands[1], { type: 'bell', f: 180, gain: -3.3, q: 0.57, slope: 12, on: true });
    assert.equal(r.bands[5].type, 'highShelf');
    assert.deepEqual(r.skipped, []);
    assert.equal(describe(r), '10 filters · preamp -6.0 dB');
});

test('APO config: Device, Include and channel lines are skipped, filters kept', () => {
    const r = parseProfile(`# my config
Device: Speakers Realtek; all
Include: other.txt
Channel: all
Preamp: -3 dB
Preamp: -1.5 dB
Filter: ON PK Fc 1000 Hz Gain 2 dB Q 1
If: sampleRate == 44100
Filter 2: OFF HS Fc 8 kHz Gain -4 dB
EndIf:
Convolution: room.wav
`);
    assert.equal(r.preamp, -4.5, 'Preamp lines add up, as in APO');
    assert.equal(r.bands.length, 2);
    assert.equal(r.bands[1].type, 'highShelf');
    assert.equal(r.bands[1].f, 8000);
    assert.equal(r.bands[1].on, false);
    assert.equal(r.skipped.length, 6);
    assert.match(describe(r), /2 filters \(1 off\) · preamp -4\.5 dB · 6 lines skipped/);
});

test('APO: a second channel set is another channel, not ours', () => {
    const r = parseProfile(`Channel: L
Filter 1: ON PK Fc 100 Hz Gain 3 dB Q 1
Channel: R
Filter 2: ON PK Fc 200 Hz Gain 3 dB Q 1
Channel: L
Filter 3: ON PK Fc 300 Hz Gain 3 dB Q 1
`);
    assert.deepEqual(r.bands.map(b => b.f), [100, 300]);
    assert.ok(r.skipped.some(s => s.includes('200 Hz')));
});

test('REW export: header lines, double spaces and None filters', () => {
    const r = parseProfile(`Filter Settings file

Room EQ V5.31.3
Dated: 1 Nov 2026 10:00:00

Notes:Left speaker

Equaliser: Generic
Average 1
Filter  1: ON  PK       Fc    63.0 Hz  Gain  -5.0 dB  Q  4.00
Filter  2: ON  PK       Fc   1,250 Hz  Gain   2.5 dB  Q  1.20
Filter  3: ON  None
Filter  4: ON  None
`);
    assert.equal(r.kind, 'rew');
    assert.equal(r.bands.length, 2);
    assert.deepEqual(r.bands[0], { type: 'bell', f: 63, gain: -5, q: 4, slope: 12, on: true });
    assert.equal(r.bands[1].f, 1250);
    assert.deepEqual(r.skipped, []);
    assert.match(describe(r), /^REW: 2 filters$/);
});

test('squig.link export: LSQ/HSQ names', () => {
    const r = parseProfile(`Preamp: -4.2 dB
Filter 1: ON LSQ Fc 105 Hz Gain 4.0 dB Q 0.700
Filter 2: ON PK Fc 2500 Hz Gain -2.0 dB Q 2.000
Filter 3: ON HSQ Fc 10000 Hz Gain -3.0 dB Q 0.700`);
    assert.deepEqual(r.bands.map(b => b.type), ['lowShelf', 'bell', 'highShelf']);
});

test('GraphicEQ is recognized as a graphic profile, sorted and deduplicated', () => {
    const r = parseProfile('GraphicEQ: 20 -1.2; 40 0.5; 21 -1.0; 40 0.6; 20000 -3');
    assert.equal(r.kind, 'graphic');
    assert.deepEqual(r.graphic, [[20, -1.2], [21, -1], [40, 0.5], [20000, -3]]);
    assert.equal(r.bands.length, 0);
    assert.equal(describe(r), 'graphic EQ, 4 points');
});

test('numbers: kHz, thousands comma, decimal comma, BW Oct', () => {
    const r = parseProfile(`Filter 1: ON PK Fc 2.5 kHz Gain 1 dB Q 2
Filter 2: ON PK Fc 1,250 Hz Gain 1 dB Q 2
Filter 3: ON PK Fc 300 Hz Gain -2,5 dB Q 1,5
Filter 4: ON PEQ Fc 100 Hz Gain 1.0 dB BW Oct 1`);
    assert.deepEqual(r.bands.map(b => b.f), [2500, 1250, 300, 100]);
    assert.equal(r.bands[2].gain, -2.5);
    assert.equal(r.bands[2].q, 1.5);
    assert.equal(r.bands[3].q, 1.4142, 'one octave is Q √2');
});

test('shelf variants and defaults', () => {
    const r = parseProfile(`Filter: ON LS 6dB Fc 50 Hz Gain 7.2 dB
Filter: ON LS 12dB Fc 2000 Hz Gain -5 dB
Filter: ON LSC 12 dB Fc 300 Hz Gain 5 dB
Filter: ON LP Fc 8000 Hz
Filter: ON NO Fc 800 Hz
Filter: ON AP Fc 900 Hz Q 0.707
Filter: ON IIR Order 2 Coefficients 1 0 0 1 0 0`);
    assert.deepEqual(r.bands.map(b => b.type), ['lowShelf', 'lowShelf', 'lowShelf', 'highCut', 'notch']);
    assert.equal(r.bands[0].q, 0.5);
    assert.equal(r.bands[2].q, 0.7071, 'a 12 dB/oct shelf slope is S = 1');
    assert.equal(r.bands[3].slope, 12);
    assert.equal(r.bands[4].gain, 0);
    assert.equal(r.skipped.length, 2, 'all-pass and raw IIR are not ours');
});

test('values are clamped to the band limits', () => {
    const r = parseProfile('Filter: ON PK Fc 30000 Hz Gain 45 dB Q 100\nFilter: ON PK Fc 2 Hz Gain -40 dB Q 0.01');
    assert.deepEqual(r.bands.map(b => [b.f, b.gain, b.q]), [[24000, 30, 24], [10, -30, 0.1]]);
});

test('more than 24 filters: the first 24 are kept and it says so', () => {
    const text = Array.from({ length: 30 }, (_, i) => `Filter ${i + 1}: ON PK Fc ${100 + i * 10} Hz Gain 1 dB Q 1`).join('\n');
    const r = parseProfile(text);
    assert.equal(r.bands.length, MAX_BANDS);
    assert.equal(r.truncated, true);
    assert.match(describe(r), /only the first 24 kept/);
});

test('garbage gives an empty result', () => {
    for (const t of ['', 'hello world\nFilter of coffee', '\u0000\u0001', null, undefined, 42, '{"bands":[]}']) {
        const r = parseProfile(t);
        assert.equal(r.bands.length, 0);
        assert.equal(r.graphic, null);
    }
    assert.equal(describe(parseProfile('nothing')), 'Nothing to import: no filters found');
});

test('two LR4 halves at Q 0.707 stay two bands; a Butterworth set merges', () => {
    const lr = parseProfile('Filter: ON HPQ Fc 20 Hz Q 0.707\nFilter: ON HPQ Fc 20 Hz Q 0.707');
    assert.equal(lr.bands.length, 2);
    const bw = parseProfile('Filter: ON HPQ Fc 20 Hz Q 0.5412\nFilter: ON HPQ Fc 20 Hz Q 1.3066');
    assert.equal(bw.bands.length, 1);
    assert.equal(bw.bands[0].slope, 24);
    assert.ok(Math.abs(bw.bands[0].q - 0.7071) < 0.001);
});

test('round trip: exportApo then parseProfile gives the same response', () => {
    const state = {
        name: 'Test', preamp: null, output: 1.5,
        bands: [
            { type: 'bell', f: 1234.5, gain: 3.25, q: 2.2, slope: 12, on: true },
            { type: 'lowShelf', f: 90, gain: 4, q: 0.6, slope: 12, on: true },
            { type: 'highShelf', f: 9000, gain: -2.5, q: 0.8, slope: 12, on: true },
            { type: 'tilt', f: 1000, gain: 3, q: 0.71, slope: 12, on: true },
            { type: 'lowCut', f: 25, gain: 0, q: 0.71, slope: 24, on: true },
            { type: 'highCut', f: 18000, gain: 0, q: 1.2, slope: 48, on: true },
            { type: 'lowCut', f: 40, gain: 0, q: 0.7071, slope: 36, on: true },
            { type: 'notch', f: 6000, gain: 0, q: 8, slope: 12, on: true },
            { type: 'bell', f: 300, gain: -6, q: 1, slope: 12, on: false },
        ],
    };
    const text = exportApo(state, { autoPreamp: -4.3 });
    assert.match(text, /^# Aura EQ · Test\nPreamp: -2\.8 dB\n/);
    const r = parseProfile(text);
    assert.equal(r.preamp, -2.8);
    const a = response(state.bands), b = response(r.bands);
    const worst = Math.max(...a.map((v, i) => Math.abs(v - b[i])));
    assert.ok(worst < 0.01, `worst difference ${worst.toFixed(4)} dB`);
    // The cuts came back as single bands, the tilt as its two shelves.
    assert.equal(r.bands.filter(x => x.type === 'lowCut').length, 2);
    assert.equal(r.bands.find(x => x.type === 'highCut').slope, 48);
    assert.equal(r.bands.filter(x => x.on === false).length, 1);
});

test('odd cut slopes are written one order steeper, with a comment', () => {
    const text = exportApo({ preamp: -1, bands: [{ type: 'lowCut', f: 30, gain: 0, q: 0.71, slope: 18, on: true }] });
    assert.match(text, /# 18 dB\/oct low cut written as 24 dB\/oct/);
    const r = parseProfile(text);
    assert.equal(r.bands.length, 1);
    assert.equal(r.bands[0].slope, 24);
});
