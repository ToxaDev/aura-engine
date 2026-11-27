// A version found by its stamp, with no record of the conversion.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// A render keeps the hash of the rack it was made with (AURA_SETTINGS), so a
// row plays the version that fits its rack even where nothing recorded it
// (the streamer, another machine): the engine is asked (`conv_matching`) a
// few rows at a time, and the answer is kept per signature.

import test from 'node:test';
import assert from 'node:assert/strict';

import { rackSig, playedFile, forgetStampMatches } from '../src/js/library.js';
import { state } from '../src/js/state.js';

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

const settle = () => new Promise(r => setTimeout(r, 260));

test('a row without a record plays the version its stamp says fits the rack', async () => {
    rack();
    const asked = [];
    globalThis.window = {
        __TAURI__: {
            core: {
                invoke: async (cmd, args) => {
                    asked.push([cmd, args]);
                    assert.equal(cmd, 'conv_matching');
                    return args.sources
                        .filter(s => /a\.flac$/i.test(s))
                        .flatMap(s => [
                            { source: s.toUpperCase(), out: 'D:/m/a [AE · 44.1k→DSD256 · SDM5].dsf', kind: 'dsd', dsd: 256 },
                            { source: s, out: 'D:/m/a [AE · 44.1k→352.8k].flac', kind: 'pcm', dsd: null },
                        ]);
                },
            },
        },
    };
    forgetStampMatches();
    state.playerOutFmt = 'flac';
    const a = { id: 1, path: 'D:/m/a.flac' };
    const b = { id: 2, path: 'D:/m/b.flac' };

    // Not known yet: live, and both rows asked in one go.
    assert.equal(playedFile(a), null);
    assert.equal(playedFile(b), null);
    await settle();
    assert.equal(asked.length, 1);
    assert.equal(asked[0][1].sig, rackSig());
    assert.deepEqual(asked[0][1].sources, ['D:/m/a.flac', 'D:/m/b.flac']);

    // The version of the output's kind plays; the other row stays live and
    // is not asked again.
    assert.equal(playedFile(a), 'D:/m/a [AE · 44.1k→352.8k].flac');
    state.playerOutFmt = '256:native';
    assert.equal(playedFile(a), 'D:/m/a [AE · 44.1k→DSD256 · SDM5].dsf');
    state.playerOutFmt = 'flac';
    assert.equal(playedFile(b), null);
    await settle();
    assert.equal(asked.length, 1);

    // A record still wins over the stamps.
    a.convs = [{ sig: rackSig(), out: 'D:/m/a [own].flac' }];
    assert.equal(playedFile(a), 'D:/m/a [own].flac');

    // Forgotten (versions written or deleted): asked again.
    delete a.convs;
    forgetStampMatches();
    assert.equal(playedFile(a), null);
    await settle();
    assert.equal(asked.length, 2);
    assert.equal(playedFile(a), 'D:/m/a [AE · 44.1k→352.8k].flac');

    delete globalThis.window;
    delete state.playerOutFmt;
});
