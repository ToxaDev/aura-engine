// Settings saved by one version, opened by the next.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// A saved blob never mentions a stage that did not exist when it was written,
// so that stage keeps whatever the markup ships it as. For most new stages
// that is exactly right. For one it was not: TFS came up on beside a
// Hybrid-Phase the user had switched on in 1.2.x, a pair the rack calls
// broken and the engine settles by running TFS in place of Hybrid-Phase.
//
// And a blob does mention one setting it never chose: Polyphase FIR shipped
// unticked until 1.3.0, so every 1.2.x blob says "off", and every save since
// carried that forward. Only a blob written by 1.3.4 or later is believed.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { loadSettings } from '../src/js/settings.js';

/// The checkboxes as a fresh install has them, read from the markup rather
/// than repeated here.
const html = readFileSync(new URL('../src/components/converter.html', import.meta.url), 'utf8');
function markupChecked(id) {
    const m = html.match(new RegExp(`<input[^>]*id="${id}"[^>]*>`));
    assert.ok(m, `converter.html has no #${id}`);
    return /\bchecked\b/.test(m[0]);
}

const STAGES = ['convHybridPhase', 'convLabTfs', 'convLabDeclip', 'convLabIsp',
    'convLabHeadroom', 'convLabAlpha', 'convLabXtc', 'convFirResampling'];

/// Just enough of a document and a store to open one saved blob.
function open(saved) {
    const els = Object.fromEntries(STAGES.map(id => [id, { checked: markupChecked(id) }]));
    globalThis.document = { getElementById: id => els[id] ?? null };
    globalThis.localStorage = { getItem: () => JSON.stringify(saved) };
    const restored = loadSettings();
    // A blob that throws part-way is treated as a first run and reports
    // false; every case here has to have been read to the end.
    assert.equal(restored, true, 'loadSettings gave up on the blob');
    return id => els[id].checked;
}

/// What a 1.2.9 installation wrote: no labFeatures at all.
const v129 = hp => ({ convFs: '2', convTapCount: 1000000, convHybridPhase: hp,
    convAdaptiveApodizer: true, convFirResampling: true, convSubsonic: false });

test('a 1.2.x setup with Hybrid-Phase on keeps it, and TFS stays off', () => {
    const on = open(v129(true));
    assert.equal(on('convHybridPhase'), true);
    assert.equal(on('convLabTfs'), false);
});

test('the stages 1.3.0 added still come up as a fresh install has them', () => {
    for (const hp of [true, false]) {
        const on = open(v129(hp));
        for (const id of ['convLabDeclip', 'convLabIsp', 'convLabHeadroom'])
            assert.equal(on(id), markupChecked(id), `${id} with Hybrid-Phase ${hp}`);
    }
});

test('a 1.2.x setup without Hybrid-Phase opens on TFS, like a fresh install', () => {
    const on = open(v129(false));
    assert.equal(on('convHybridPhase'), false);
    assert.equal(on('convLabTfs'), markupChecked('convLabTfs'));
});

test('no old setup opens with Hybrid-Phase and TFS both on', () => {
    for (const hp of [true, false]) {
        const on = open(v129(hp));
        assert.ok(!(on('convHybridPhase') && on('convLabTfs')), `Hybrid-Phase ${hp}`);
    }
});

test('a setup saved since 1.3.0 is restored as it was saved', () => {
    const on = open({ ...v129(true), labFeatures: {
        declip: false, isp: true, tfsPhase: false, continuousAlpha: true,
        adaptiveHeadroom: false, xtc: false } });
    assert.equal(on('convHybridPhase'), true);
    assert.equal(on('convLabTfs'), false);
    assert.equal(on('convLabDeclip'), false);
    assert.equal(on('convLabIsp'), true);
    assert.equal(on('convLabAlpha'), true);
    assert.equal(on('convLabHeadroom'), false);
});

/// What 1.3.0–1.3.3 wrote on the first change after an update from 1.2.x:
/// labFeatures now present, but the pair and the PFR box as they were
/// inherited, not chosen.
const v133 = (hp, tfs, pfr) => ({ ...v129(hp), convFirResampling: pfr, labFeatures: {
    declip: true, isp: true, tfsPhase: tfs, continuousAlpha: false,
    adaptiveHeadroom: true, xtc: false } });

test('Hybrid-Phase and TFS both on, as a 1.3.x save kept them, comes back as Hybrid-Phase', () => {
    const on = open(v133(true, true, true));
    assert.equal(on('convHybridPhase'), true);
    assert.equal(on('convLabTfs'), false);
});

test('TFS chosen without Hybrid-Phase is left alone', () => {
    const on = open(v133(false, true, true));
    assert.equal(on('convHybridPhase'), false);
    assert.equal(on('convLabTfs'), true);
});

test('Polyphase FIR comes up on from any blob older than 1.3.4', () => {
    for (const blob of [{ ...v129(true), convFirResampling: false },
                        { ...v129(false), convFirResampling: false },
                        v133(true, false, false), v133(false, true, false)]) {
        assert.equal(open(blob)('convFirResampling'), true, JSON.stringify(blob));
    }
});

test('Polyphase FIR switched off in 1.3.4 or later stays off', () => {
    const on = open({ ...v133(false, true, false), settingsRev: 2 });
    assert.equal(on('convFirResampling'), false);
});

test('the markup ships Polyphase FIR on', () => {
    assert.equal(markupChecked('convFirResampling'), true);
});
