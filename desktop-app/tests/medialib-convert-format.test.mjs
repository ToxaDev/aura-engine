// The library's Convert makes what is heard now (medialib/convert-format.js):
// PCM out of the rack is a FLAC at its FS, DSD out of it the same DSD rate
// by the same modulator; another format is said to be not what plays.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    heardValue, targetLabel, targetLong, convertPlan, formatMenu, kHz, multipleOfLabel, flacFs, CONVERT_TEXTS,
} from '../src/js/medialib/convert-format.js';

const pcmPlaying = { state: 'playing', outRate: 705600, dsd: null };
const dsdPlaying = { state: 'playing', outRate: 0, dsd: { label: 'DSD256', modulator: 'sdm7La', running: 'sdm5' } };

test('rates read as the button says them', () => {
    assert.equal(kHz(705600), '705.6k');
    assert.equal(kHz(768000), '768k');
    assert.equal(kHz(352800), '352.8k');
    assert.equal(multipleOfLabel('DSD256'), 256);
    assert.equal(multipleOfLabel('DSD128 (48k)'), 128);
    assert.equal(multipleOfLabel('PCM'), 0);
    assert.equal(flacFs(32), 16, 'FLAC holds FS16 at most');
    assert.equal(flacFs(8), 8);
});

test('what is heard: the playing DSD by its chosen modulator, PCM as FLAC, the stored choice while nothing plays', () => {
    assert.equal(heardValue({ status: pcmPlaying, outFmt: '256:sdm7La' }), 'flac', 'PCM plays (the PC could not run DSD): FLAC');
    assert.equal(heardValue({ status: dsdPlaying }), '256:sdm7La', 'the modulator chosen, not the lighter one it stepped down to');
    assert.equal(heardValue({ status: { state: 'playing', dsd: { label: 'DSD128 (48k)', running: 'sdm5' } } }), '128:sdm5');
    assert.equal(heardValue({ status: { state: 'stopped' }, outFmt: '128:sdm9' }), '128:sdm9');
    assert.equal(heardValue({ status: null, outFmt: 'pcm' }), 'flac');
    assert.equal(heardValue({ status: null, outFmt: '256' }), '256:sdm5', 'a stored value from before the catalog');
    assert.equal(heardValue({ status: dsdPlaying, bitPerfect: true }), 'flac', 'bit-perfect: the rack is not heard');
    assert.equal(heardValue({ status: { state: 'playing', dsd: { label: 'DSD64', asIs: true } } }), 'flac', 'a DSD file as it is');
});

test('the button names the result: FLAC at the album family times the FS, or the DSD rate', () => {
    assert.equal(targetLabel('flac', { fs: 16, srcRate: 44100 }), 'FLAC 705.6k');
    assert.equal(targetLabel('flac', { fs: 16, srcRate: 96000 }), 'FLAC 768k');
    assert.equal(targetLabel('flac', { fs: 8, srcRate: 0 }), 'FLAC 352.8k', 'an unknown rate is the 44.1 family');
    assert.equal(targetLabel('flac', { fs: 32, srcRate: 44100 }), 'FLAC 705.6k');
    assert.equal(targetLabel('256:sdm7La', { fs: 16 }), 'DSD256');
    assert.equal(targetLong('256:sdm7La'), 'DSD256 · SDM7 LA');
    assert.equal(targetLong('flac', { fs: 16, srcRate: 48000 }), 'FLAC 24-bit · 768k');
});

test('the plan: as heard, no mark; another format picked, marked not now playing', () => {
    const heard = convertPlan({ status: dsdPlaying, fs: 16, srcRate: 44100 });
    assert.equal(heard.value, '256:sdm7La');
    assert.equal(heard.text, 'Convert → DSD256');
    assert.equal(heard.differs, false);
    assert.equal(heard.mark, '');
    const other = convertPlan({ status: dsdPlaying, fs: 16, srcRate: 44100, chosen: 'flac' });
    assert.equal(other.value, 'flac');
    assert.equal(other.text, 'Convert → FLAC 705.6k');
    assert.equal(other.differs, true);
    assert.equal(other.mark, CONVERT_TEXTS.notNow);
    assert.match(other.tip, /DSD256 · SDM7 LA/, 'the tip says what plays');
    const same = convertPlan({ status: pcmPlaying, fs: 16, srcRate: 44100, chosen: 'flac' });
    assert.equal(same.differs, false, 'picking what is heard is no difference');
    assert.equal(convertPlan({ status: pcmPlaying, fs: 32 }).differs, true, 'FLAC cannot hold ×32');
    assert.equal(convertPlan({ status: pcmPlaying, bitPerfect: true }).differs, true);
});

test('the menu: what is heard first, then FLAC and every DSD rate, highest first, no doubles', () => {
    const mods = {
        64: [{ kind: 'sdm5', label: 'SDM5', realtime: true }],
        256: [{ kind: 'sdm7La', label: 'SDM7 LA', realtime: true }, { kind: 'sdm9Trellis', label: 'SDM9 Trellis', realtime: false }],
    };
    const m = formatMenu(mods, '256:sdm7La', { fs: 16 });
    assert.equal(m[0].value, '256:sdm7La');
    assert.ok(m[0].heard);
    const values = m.filter(i => !i.sep).map(i => i.value);
    assert.deepEqual(values, ['256:sdm7La', 'flac', '256:sdm9Trellis', '64:sdm5']);
    assert.match(m.find(i => i.value === '256:sdm9Trellis').text, /offline/);
    assert.ok(m[1].sep, 'a separator after what is heard');
    assert.ok(!m[m.length - 1].sep, 'none at the end');
    const f = formatMenu(mods, 'flac', { fs: 16 });
    assert.deepEqual(f.filter(i => !i.sep).map(i => i.value), ['flac', '256:sdm7La', '256:sdm9Trellis', '64:sdm5']);
    for (let i = 1; i < f.length; i++) assert.ok(!(f[i].sep && f[i - 1].sep), 'no two separators together');
    assert.deepEqual(formatMenu(null, 'flac').map(i => i.value), ['flac']);
});

test('the words: English', () => {
    for (const s of Object.values(CONVERT_TEXTS)) assert.ok(!/[Ѐ-ӿ]/.test(s), s);
});
