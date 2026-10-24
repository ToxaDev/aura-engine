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

import { rackSig, normalizeSig } from '../src/js/library.js';

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
