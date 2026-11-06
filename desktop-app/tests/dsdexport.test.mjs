// The converter's output format (dsdexport.js): FLAC or DSF, the chain's FS
// for a DSD rate, the engine's modulator list kept in its order.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    DSD_TEXTS, DSD_RATES, rateAside, effectiveFs, modulatorChoices, pickModulator, dsdOutput, specsOutput,
    FLAC_MAX_FS, FLAC_FS_HINT, flacFsRefusal,
} from '../src/js/dsdexport.js';

// As the engine sends it: best first.
const LIST = [
    { kind: 'sdm7Trellis', label: 'SDM7 Trellis', rank: 2, realtime: false },
    { kind: 'sdm7', label: 'SDM7', rank: 8, realtime: true },
    { kind: 'sdm5', label: 'SDM5', rank: 11, realtime: true },
];

test('the rates and both families', () => {
    assert.deepEqual(DSD_RATES, [64, 128, 256, 512, 1024]);
    assert.equal(rateAside(64), '2.8224 / 3.072 MHz');
    assert.equal(rateAside(256), '11.2896 / 12.288 MHz');
    assert.equal(rateAside(512), '22.5792 / 24.576 MHz');
});

test('the chain FS: DSD64 takes FS8 at most, the rest any FS', () => {
    assert.equal(effectiveFs(16, 64), 8);
    assert.equal(effectiveFs(8, 64), 8);
    assert.equal(effectiveFs(2, 64), 2);
    for (const m of [128, 256, 512, 1024]) assert.equal(effectiveFs(16, m), 16);
    // Bits per PCM sample: a power of two, at least 8, for every pair.
    for (const m of DSD_RATES) for (const fs of [2, 4, 8, 16]) {
        const ratio = m / effectiveFs(fs, m);
        assert.ok(ratio >= 8 && Number.isInteger(Math.log2(ratio)), `FS${fs} DSD${m}`);
    }
});

test('the modulator list keeps the engine\'s order; offline kinds say so', () => {
    assert.deepEqual(modulatorChoices(LIST), [
        ['sdm7Trellis', 'SDM7 Trellis', 'offline'],
        ['sdm7', 'SDM7', ''],
        ['sdm5', 'SDM5', ''],
    ]);
    assert.deepEqual(modulatorChoices(null), []);
});

test('the default is the best built kind; a saved one stays while the rate has it', () => {
    assert.equal(pickModulator(LIST, undefined), 'sdm7Trellis');
    assert.equal(pickModulator(LIST, 'sdm5'), 'sdm5');
    assert.equal(pickModulator(LIST, 'sdm9'), 'sdm7Trellis');
    assert.equal(pickModulator([], 'sdm5'), null);
});

test('dsdOutput: null for FLAC, the rate and kind for DSF', () => {
    assert.equal(dsdOutput('flac', '256', 'sdm5'), null);
    assert.deepEqual(dsdOutput('dsf', '256', 'sdm5'), { multiple: 256, kind: 'sdm5' });
    assert.equal(dsdOutput('dsf', '256', ''), null);
    assert.equal(dsdOutput('dsf', '32', 'sdm5'), null);
    assert.equal(specsOutput(null, ''), 'FLAC');
    assert.equal(specsOutput({ multiple: 256, kind: 'sdm5' }, 'SDM5'), 'DSF DSD256 &bull; SDM5');
});

test('the words: English', () => {
    for (const v of Object.values(DSD_TEXTS)) {
        const s = typeof v === 'function' ? v('x') : v;
        assert.ok(!/[Ѐ-ӿ]/.test(s), s);
    }
    assert.equal(DSD_TEXTS.none, 'No modulator is built for this rate yet.');
    assert.equal(DSD_TEXTS.offlineTip, 'Trellis modulators are far slower than real time: a track may take minutes.');
});

// ── the format part of Convert all ──────────────────────────────────────

import { convFmtValue, parseConvFmt, convFmtClosedLabel, convFmtItems, convFmtTip } from '../src/js/dsdexport.js';

test('the part names FLAC, or the DSD rate alone', () => {
    assert.equal(convFmtValue('flac', '256', 'sdm7'), 'flac');
    assert.equal(convFmtValue('dsf', '256', 'sdm7'), '256:sdm7');
    assert.equal(convFmtValue('dsf', '256', ''), 'flac', 'no modulator: nothing to write DSF with');
    assert.deepEqual(parseConvFmt('1024:sdm9'), { multiple: 1024, kind: 'sdm9' });
    assert.equal(parseConvFmt('flac'), null);
    assert.equal(parseConvFmt('96:sdm7'), null, 'not a DSD rate');
    assert.equal(convFmtClosedLabel('flac'), 'FLAC');
    assert.equal(convFmtClosedLabel('512:sdm9'), 'DSD512');
});

test('the list: FLAC, then the rates from the highest down, each with its modulators, the slow ones marked', () => {
    const items = convFmtItems({ 64: LIST, 256: LIST.slice(1), 1024: [] });
    assert.deepEqual(items.map(i => i.sep ? '—' : i.value),
        ['flac', '—', '256:sdm7', '256:sdm5', '—', '64:sdm7Trellis', '64:sdm7', '64:sdm5']);
    assert.equal(items[0].text, 'FLAC 24-bit');
    const trellis = items[5];
    assert.equal(trellis.text, 'DSD64 (SDM7 Trellis) · offline');
    assert.equal(trellis.cls, 'heavy');
    assert.equal(trellis.tip, DSD_TEXTS.offlineTip);
    assert.equal(items[6].text, 'DSD64 (SDM7)');
    assert.equal(items[6].cls, '');
    const all = convFmtItems({ 64: LIST, 128: LIST, 256: LIST, 512: LIST, 1024: LIST });
    assert.deepEqual(all.filter(i => !i.sep && i.value !== 'flac' && i.value.endsWith(':sdm7Trellis')).map(i => i.value),
        ['1024:sdm7Trellis', '512:sdm7Trellis', '256:sdm7Trellis', '128:sdm7Trellis', '64:sdm7Trellis'], 'DSD1024 on top, DSD64 at the bottom');
    assert.deepEqual(convFmtItems(null).map(i => i.value), ['flac'], 'before the engine answers');
});

test('the part\'s tip names the output', () => {
    assert.equal(convFmtTip('flac'), 'Output: FLAC 24-bit. Click to change.');
    assert.equal(convFmtTip('256:sdm7La', 'SDM7 LA'), 'Output: DSF DSD256 · SDM7 LA. Click to change.');
    assert.equal(convFmtTip('256:sdm7La', ''), 'Output: DSF DSD256 · sdm7La. Click to change.');
});

test('FLAC refuses x32 (20-bit rate); DSF and every FS up to 16 pass', () => {
    assert.equal(FLAC_MAX_FS, 16);
    for (const fs of [2, 4, 8, 16]) assert.equal(flacFsRefusal(fs, null), null);
    assert.equal(flacFsRefusal(32, null), FLAC_FS_HINT);
    assert.match(FLAC_FS_HINT, /768 kHz/);
    for (const m of DSD_RATES) assert.equal(flacFsRefusal(32, { multiple: m, kind: 'sdm7' }), null);
});
