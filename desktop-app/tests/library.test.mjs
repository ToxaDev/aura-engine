// Conversion records written before 1.5.0, opened by it.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// A record keeps the signature of the rack it was made with, and a row
// offers its converted file (✓, play from disk) only while the rack's
// signature matches. Until 1.5.0 a Window list named the file's window —
// Hann, Blackman, Nuttall — over a filter that was Kaiser all the same; the
// list is gone, and the records it named must still match.

import test from 'node:test';
import assert from 'node:assert/strict';

import { readFileSync } from 'node:fs';

import { rackSig, rackSigWithMOverride, normalizeSig, playedFile, cueForcesLive } from '../src/js/library.js';
import { setEqState } from '../src/js/eq-bridge.js';
import { flatState } from '../src/js/eq/state.js';

/// Enough of a rack for rackSig(): its sliders, selects and boxes.
function rack() {
    const els = {
        convTapSlider: { value: '1' }, convFsSlider: { value: '2' }, convApodizing: { value: '0' },
        convHeadroom: { value: '-0.5' }, convSubsonicHz: { value: '15' },
        convFirResampling: { checked: true }, convAdaptiveApodizer: { checked: true },
        convLabTfs: { checked: true }, convLabDeclip: { checked: true }, convLabIsp: { checked: true },
        convLabHeadroom: { checked: true }, convSubsonic: { checked: true },
    };
    globalThis.document = { getElementById: id => els[id] ?? null };
}

test('a record made under Hann, Blackman or Nuttall matches the rack again', () => {
    rack();
    const now = rackSig();
    assert.equal(JSON.parse(now).winType, 4);
    for (const w of [1, 2, 3]) {
        const old = now.replace('"winType":4', `"winType":${w}`);
        assert.notEqual(old, now);
        assert.equal(normalizeSig(old), now, `winType ${w}`);
    }
});

test('what needs no change is left as it was', () => {
    rack();
    const now = rackSig();
    assert.equal(normalizeSig(now), now);
    const custom = JSON.stringify({ fs: 8, taps: null, customFilterPath: 'x.npy', winType: null });
    assert.equal(normalizeSig(custom), custom);
    assert.equal(normalizeSig('not json'), 'not json');
});

test('with CUE on a converted row plays live under the same signature; off, the file again', () => {
    rack();
    const get = globalThis.document.getElementById;
    const box = { checked: false };
    globalThis.document = { getElementById: id => (id === 'convLabCue' ? box : get(id)) };
    const e = { id: 1, convs: [{ sig: rackSig(), out: 'a [aura].flac' }] };

    assert.equal(cueForcesLive(), false);
    assert.equal(playedFile(e), 'a [aura].flac');

    box.checked = true;
    assert.equal(rackSig(), e.convs[0].sig, 'no file holds CUE: its switch moves no signature');
    assert.equal(playedFile(e), null);

    box.checked = false;
    assert.equal(playedFile(e), 'a [aura].flac');
});

test('the row stays converted with CUE on and does not promise the disk', () => {
    const row = readFileSync(new URL('../src/js/dropzone.js', import.meta.url), 'utf8');
    assert.match(row, /const fromDisk = isDisk && !cueForcesLive\(\)/);
    assert.match(row, /fromDisk \? ' · plays from disk' : ' · plays live/);
    assert.match(row, /p\.fromDisk \? 'Play the converted file from disk' : 'Listen'/);
});

// DSF export (1.7.0): the output is part of what a record was made with.
// A FLAC rack keeps the signature it always had, so records made before
// stay converted; a DSF one names the rate and the modulator.
function dsfRack(format, rate, kind) {
    rack();
    const get = globalThis.document.getElementById;
    const out = { convOutFormat: { value: format }, convDsdRate: { value: rate }, convDsdModulator: { value: kind } };
    globalThis.document = { getElementById: id => out[id] ?? get(id) };
}

test('FLAC keeps the signature records were made with', () => {
    rack();
    const before = rackSig();
    dsfRack('flac', '256', 'sdm7');
    assert.equal(rackSig(), before);
    assert.equal(JSON.parse(before).out, undefined);
});

test('a change to DSF, its rate or its modulator is a file not made yet', () => {
    dsfRack('flac', '256', 'sdm7');
    const flac = rackSig();
    dsfRack('dsf', '256', 'sdm7');
    const d256 = rackSig();
    dsfRack('dsf', '128', 'sdm7');
    const d128 = rackSig();
    dsfRack('dsf', '256', 'sdm5');
    const d256b = rackSig();
    assert.equal(new Set([flac, d256, d128, d256b]).size, 4);
    assert.deepEqual(
        (({ out, dsdRate, modulator }) => ({ out, dsdRate, modulator }))(JSON.parse(d256)),
        { out: 'dsf', dsdRate: 256, modulator: 'sdm7' });
});

// Hybrid-Phase's sensitivity (HP's slider): in the signature only with HP
// on and off step 0, so records made before the slider stay converted.
function hpRack(hp, step) {
    rack();
    const get = globalThis.document.getElementById;
    const more = { convHybridPhase: { checked: hp }, convHpSens: { value: String(step) } };
    globalThis.document = { getElementById: id => more[id] ?? get(id) };
}

test('step 0 keeps the signature; another step with HP on is a file not made yet', () => {
    hpRack(true, 0);
    const zero = rackSig();
    assert.equal(JSON.parse(zero).hpSens, undefined);
    hpRack(true, 2);
    const up = rackSig();
    assert.equal(JSON.parse(up).hpSens, 2);
    assert.equal(up.replace(',"hpSens":2', ''), zero, 'only the field is added');
    hpRack(true, -1);
    assert.equal(JSON.parse(rackSig()).hpSens, -1);
    hpRack(false, 2);
    assert.equal(JSON.parse(rackSig()).hpSens, undefined, 'HP off: the step does nothing');
});

// ── EQ in rackSig ────────────────────────────────────────────────────────

function eqRack(eqState) {
    rack();
    setEqState(eqState || flatState());
    // convEq checkbox not in the basic rack() stub — add it
    const get = globalThis.document.getElementById;
    const more = { convEq: { checked: eqState?.on || false } };
    globalThis.document = { getElementById: id => more[id] ?? get(id) };
}

test('rackSig is unchanged when EQ is off (backward compat)', () => {
    rack();
    setEqState(flatState());  // off, no bands
    const without = rackSig();
    eqRack(flatState());
    const withEqOff = rackSig();
    assert.equal(withEqOff, without, 'EQ off must not change the sig');
    assert.equal(JSON.parse(withEqOff).eq, undefined, 'eq field absent when off');
});

test('rackSig changes when EQ is on with a band', () => {
    eqRack(flatState());
    const off = rackSig();
    eqRack({ v: 1, on: true, name: 'Test', bands: [
        { type: 'bell', f: 1000, gain: -3, q: 0.71, slope: 12, on: true }
    ], preamp: null, output: 0 });
    const on = rackSig();
    assert.notEqual(on, off, 'EQ on must change the sig');
    assert.ok(JSON.parse(on).eq, 'eq field present when on');
});

test('rackSig changes when a band parameter changes, not when the name changes', () => {
    const base = { v: 1, on: true, name: 'Original', bands: [
        { type: 'bell', f: 1000, gain: -3, q: 0.71, slope: 12, on: true }
    ], preamp: null, output: 0 };
    eqRack(base);
    const sig1 = rackSig();
    // Change the name only — sig must not change
    eqRack({ ...base, name: 'Renamed' });
    const sig2 = rackSig();
    assert.equal(sig1, sig2, 'renaming a preset must not change the sig');
    // Change a band parameter — sig must change
    eqRack({ ...base, bands: [{ ...base.bands[0], gain: -5 }] });
    const sig3 = rackSig();
    assert.notEqual(sig1, sig3, 'changing a band gain must change the sig');
});

test('rackSigWithMOverride: EQ from M override (never stored) returns null → no eq field', () => {
    eqRack(flatState());
    const mo = { apodizing: 0, adaptiveApodizer: true, headroomDb: -0.5, hybridPhase: false, subsonicHz: 15, labFeatures: {} };
    const s = JSON.parse(rackSigWithMOverride(mo));
    assert.equal(s.eq, undefined, 'M override without eq in labFeatures must not add eq field');
});

test('an M override reads the step from its lab features', () => {
    hpRack(false, 0);
    const mo = { apodizing: 0, adaptiveApodizer: true, headroomDb: -0.5, hybridPhase: true, subsonicHz: 15, labFeatures: {} };
    assert.equal(JSON.parse(rackSigWithMOverride(mo)).hpSens, undefined);
    assert.equal(JSON.parse(rackSigWithMOverride({ ...mo, labFeatures: { hpSens: 0 } })).hpSens, undefined);
    assert.equal(JSON.parse(rackSigWithMOverride({ ...mo, labFeatures: { hpSens: -2 } })).hpSens, -2);
    assert.equal(JSON.parse(rackSigWithMOverride({ ...mo, hybridPhase: false, labFeatures: { hpSens: -2 } })).hpSens, undefined);
});
