// The EQ window's curve arithmetic (js/eq/math.js).
//
// The window must draw what the engine applies, so the curves are held to
// closed-form values of the analog prototypes, to the analog curve itself
// (within the matched sections' tolerance) and to the engine's own numbers
// at the same points (the golden file).

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
    sections, magDb, bandCurve, totalCurve, autoPreamp, safeLevel, autoGain, modeLevel, SAFE_MARGIN_DB, logGrid, analogDb, VIEW_FS,
} from '../src/js/eq/math.js';
import { eqActive, eqSig, normalizeState } from '../src/js/eq/state.js';

const at = (b, f, fs = VIEW_FS) => sections(b, fs).reduce((a, co) => a + magDb(co, f, fs), 0);
const band = (type, f, gain = 0, q = 0.71, slope = 12) => ({ type, f, gain, q, slope, on: true });
const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b}`);

test('a bell has its gain at the centre and nothing far away', () => {
    near(at(band('bell', 1000, 6, 1.4), 1000), 6, 1e-9, 'centre');
    near(at(band('bell', 1000, -9, 2), 1000), -9, 1e-9, 'cut centre');
    near(at(band('bell', 1000, 6, 1.4), 20), 0, 0.01, 'far below');
});

test('shelves reach their gain on their side and half of it at the corner', () => {
    near(at(band('lowShelf', 200, 6, 0.71), 10), 6, 0.05, 'low shelf floor');
    near(at(band('lowShelf', 200, 6, 0.71), 200), 3, 1e-6, 'low shelf corner');
    near(at(band('highShelf', 2000, -4, 0.71), 20000), -4, 0.1, 'high shelf top');
    near(at(band('highShelf', 2000, -4, 0.71), 2000), -2, 1e-6, 'high shelf corner');
});

test('tilt is +g/2 above and -g/2 below its centre', () => {
    const b = band('tilt', 1000, 6);
    near(at(b, 15), -3, 0.1, 'bottom');
    near(at(b, 18000), 3, 0.2, 'top');
    near(at(b, 1000), 0, 1e-6, 'centre');
});

test('Butterworth cuts are -3 dB at the corner and fall at slope dB/oct', () => {
    for (const slope of [6, 12, 18, 24, 36, 48, 72, 96]) {
        near(at(band('lowCut', 100, 0, Math.SQRT1_2, slope), 100), -3.0103, 1e-3, `low cut ${slope} corner`);
        near(at(band('highCut', 1000, 0, Math.SQRT1_2, slope), 1000), -3.0103, 1e-3, `high cut ${slope} corner`);
        const d = at(band('lowCut', 1000, 0, Math.SQRT1_2, slope), 125) - at(band('lowCut', 1000, 0, Math.SQRT1_2, slope), 250);
        near(d, -slope, slope * 0.06 + 0.3, `low cut ${slope} slope`);
        assert.equal(sections(band('lowCut', 100, 0, 0.71, slope)).length, Math.ceil(slope / 12), `${slope} sections`);
    }
});

test('a cut q above Butterworth lifts the corner', () => {
    assert.ok(at(band('highCut', 1000, 0, 2, 24), 1000) > 0);
});

test('notch and band pass', () => {
    assert.ok(at(band('notch', 1000, 0, 5), 1000) < -100);
    near(at(band('bandPass', 1000, 0, 2), 1000), 0, 1e-9, 'band pass peak');
    assert.ok(at(band('bandPass', 1000, 0, 2), 100) < -20);
});

test('a band above Nyquist keeps its analog skirt below it, not aliased', () => {
    // 30 kHz is held to the contract's 24 kHz; at 48k that is Nyquist itself.
    const b = band('bell', 30000, 6, 1);
    for (const f of [5000, 10000, 15000, 20000, 23000]) near(at(b, f, 48000), analogDb(b, f), 0.2, `${f} Hz`);
    near(at(b, 23000, 48000), at(band('bell', 24000, 6, 1), 23000, 48000), 1e-12, 'the band at 24 kHz');
});

test('the total skips bands that are off; curves are cached per call', () => {
    const fr = logGrid(64);
    const a = band('bell', 1000, 6, 1), b = { ...band('bell', 100, -6, 1), on: false };
    const t = totalCurve([a, b], fr), ca = bandCurve(a, fr);
    for (let i = 0; i < fr.length; i++) near(t[i], ca[i], 1e-12, 'only the on band');
});

test('auto preamp is minus the peak, never a boost', () => {
    near(autoPreamp([band('bell', 1000, 6, 1)]), -6, 0.02, 'one bell');
    assert.equal(autoPreamp([band('bell', 1000, -6, 1)]), 0);
    assert.equal(autoPreamp([]), 0);
    // A boost outside 20 Hz–20 kHz counts only by the skirt that reaches in.
    assert.ok(autoPreamp([band('bell', 10, 12, 8)]) > -1);
});

test('gain modes: manual 0, safe the peak plus the margin, auto as loud as the EQ off', () => {
    const b6 = [band('bell', 1000, 6, 1)];
    assert.equal(modeLevel({ gain: 'manual', bands: b6 }), 0);
    near(modeLevel({ gain: 'safe', bands: b6 }), -6 - SAFE_MARGIN_DB, 0.02, 'safe');
    assert.equal(safeLevel([band('bell', 1000, -6, 1)]), 0, 'safe never boosts');
    // A cut is made up for, a boost taken back; a flat curve stays at 0.
    assert.ok(autoGain([band('bell', 1000, -6, 1)]) > 0.5 && autoGain(b6) < -1);
    assert.equal(autoGain([]), 0);
    // Level-matched, not peak: a narrow +12 dB bell costs far less than 12.
    assert.ok(autoGain([band('bell', 3000, 12, 8)]) > -1.5);
    // K-weighting: a sub-bass boost counts for less than the same at 2 kHz.
    assert.ok(autoGain([band('lowShelf', 40, 6, 0.7)]) > autoGain([band('bell', 2000, 6, 0.7)]));
    near(autoGain([band('bell', 1000, 30, 0.1), band('lowShelf', 200, 30, 0.3)]), -30, 1e-9, 'clamped');
    assert.equal(modeLevel({ gain: 'auto', bands: [{ ...b6[0], on: false }] }), 0, 'only bands that are on');
});

// The engine's golden values (EQ engine tests write them, eq-golden.gen.mjs):
// the window's curve, auto preamp and signature must agree with the engine.
const golden = JSON.parse(readFileSync(new URL('./fixtures/eq-golden.json', import.meta.url), 'utf8'));

test('band magnitudes agree with the engine golden file from 44.1k to 768k', () => {
    assert.ok(golden.magnitudes.length > 0);
    for (const c of golden.magnitudes) {
        c.freqs.forEach((f, i) => near(at(c.band, f, c.fs), c.db[i], 0.01, `${c.band.type} ${c.band.f} @ ${f} Hz, fs ${c.fs}`));
        c.freqs.forEach((f, i) => near(analogDb(c.band, f), c.analogDb[i], 1e-9, `analog ${c.band.type} ${c.band.f} @ ${f} Hz`));
    }
});

// How far a matched band may part from its analog curve, dB, where the
// analog curve is above -30 dB up to 20 kHz: by rate, and by where the
// frequency sits (up to 0.25 / 0.5 / 0.8 / 1 of Nyquist). The engine's
// test holds the same table over a wide grid; cuts are bilinear (exact
// -3 dB corner) and tested on their own.
function tolerance(fs, type, zone) {
    if (fs >= 176400) return type === 'notch' || type === 'bandPass' ? 0.25 : 0.05;
    if (fs >= 88200) return type === 'bell' || type === 'tilt' ? 0.17 : type === 'bandPass' ? 0.25 : 0.8;
    return ({
        bell: [1.8, 2.0, 2.0, 1.6], lowShelf: [0.9, 1.4, 4.5, 4.5], highShelf: [0.9, 1.4, 4.5, 4.5],
        tilt: [1.0, 2.0, 2.0, 0.8], notch: [0.7, 2.4, 11.0, 21.0], bandPass: [3.2, 3.2, 2.6, 1.8],
    })[type][zone];
}

test('matched bands follow their analog curve within the tolerance table', () => {
    for (const c of golden.magnitudes) {
        if (c.band.type === 'lowCut' || c.band.type === 'highCut') continue;
        c.freqs.forEach((f, i) => {
            if (c.analogDb[i] < -30 || f > 20000) return;
            const r = f / (c.fs / 2), zone = r <= 0.25 ? 0 : r <= 0.5 ? 1 : r <= 0.8 ? 2 : 3;
            near(at(c.band, f, c.fs), c.analogDb[i], tolerance(c.fs, c.band.type, zone), `${c.band.type} ${c.band.f} q${c.band.q} @ ${f} Hz, fs ${c.fs}`);
        });
    }
});

test('gain levels agree with the engine golden file (version-1 states migrated)', () => {
    for (const c of golden.levels) {
        const msg = `fs ${c.fs} ${JSON.stringify(c.state).slice(0, 90)}`;
        const bands = c.state.bands.slice(0, 24).filter(b => b.on);
        near(autoPreamp(bands, c.fs), c.autoPreampDb, 1e-6, msg);
        near(safeLevel(bands, c.fs), c.safeDb, 1e-6, msg);
        near(autoGain(bands, c.fs), c.autoGainDb, 1e-6, msg);
        const n = normalizeState(c.state);
        assert.deepEqual([n.gain, n.output], [c.gain, c.output], msg);
        near(modeLevel(n, c.fs), c.levelDb, 1e-6, msg);
    }
});

test('eqActive and eqSig agree with the engine golden file', () => {
    for (const c of golden.active) {
        assert.equal(eqActive(c.state), c.active, JSON.stringify(c.state));
        assert.equal(eqSig(c.state), c.sig ?? null, JSON.stringify(c.state));
    }
});
