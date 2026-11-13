// AutoEq search for the EQ window (src/js/eq-autoeq.js): one recommended
// result per headphone first, the other measurements after, and the backend
// calls asked once. Made-up list, no network.

import test from 'node:test';
import assert from 'node:assert/strict';

import { norm, searchAutoEq, autoEqCredit, autoEqApi } from '../src/js/eq-autoeq.js';

const E = (name, source, rec = false, rig = '') => ({ name, source, rig, path: `${source}/x/${name}`, rec });
const LIST = [
    E('Sennheiser HD 600', 'Innerfidelity'),
    E('Sennheiser HD 600', 'oratory1990', true),
    E('Sennheiser HD 600', 'crinacle', false, 'GRAS 43AG-7'),
    E('Sennheiser HD 6XX', 'oratory1990', true),
    E('Sennheiser HD 660 S', 'crinacle', true),
    E('Sennheiser HD 650', 'Rtings'),
    E('Moondrop Blessing 2', 'crinacle', true, '711'),
    E('écoute TH1', 'Kuulokenurkka', true),
];

test('norm drops case, accents and punctuation', () => {
    assert.equal(norm('Sennheiser HD-600'), 'sennheiser hd 600');
    assert.equal(norm('Écoute  TH1'), 'ecoute th1');
});

test('one recommended result per headphone first, then the other measurements', () => {
    const r = searchAutoEq(LIST, 'hd 600');
    assert.deepEqual(r.map(x => [x.name, x.source, x.best]), [
        ['Sennheiser HD 600', 'oratory1990', true],
        ['Sennheiser HD 600', 'Innerfidelity', false],
        ['Sennheiser HD 600', 'crinacle', false],
    ]);
    assert.equal(r[0].others, 2);
});

test('closer names first; a model without a recommendation shows its first result', () => {
    const r = searchAutoEq(LIST, 'sennheiser hd 6');
    const best = r.filter(x => x.best).map(x => x.name);
    assert.deepEqual(best, ['Sennheiser HD 600', 'Sennheiser HD 650', 'Sennheiser HD 6XX', 'Sennheiser HD 660 S']);
    assert.equal(r.find(x => x.name === 'Sennheiser HD 650').source, 'Rtings');
});

test('words in any order, accents ignored, nothing for no match or empty query', () => {
    assert.equal(searchAutoEq(LIST, 'blessing moondrop')[0].name, 'Moondrop Blessing 2');
    assert.equal(searchAutoEq(LIST, 'ecoute')[0].name, 'écoute TH1');
    assert.deepEqual(searchAutoEq(LIST, 'zzz'), []);
    assert.deepEqual(searchAutoEq(LIST, '  '), []);
    assert.deepEqual(searchAutoEq(null, 'hd'), []);
    assert.equal(searchAutoEq(LIST, 'sennheiser', { limit: 2 }).length, 2);
});

test('credit line names the measurer and the rig', () => {
    assert.equal(autoEqCredit(LIST[6]), 'AutoEq · measured by crinacle on 711');
    assert.equal(autoEqCredit(LIST[1]), 'AutoEq · measured by oratory1990');
});

test('the list is asked once; a failure can be retried; errors are text', async () => {
    const calls = [];
    let fail = true;
    const invoke = async (cmd, args) => {
        calls.push([cmd, args]);
        if (cmd === 'eq_autoeq_index') {
            if (fail) throw "Can't reach AutoEq. Check the internet connection and try again.";
            return { entries: LIST, fetched: 1, stale: false };
        }
        return 'Preamp: -1 dB';
    };
    const api = autoEqApi(invoke);
    await assert.rejects(api.index(), /Can't reach AutoEq/);
    fail = false;
    const a = await api.index();
    const b = await api.index();
    assert.equal(a, b);
    assert.equal(calls.filter(c => c[0] === 'eq_autoeq_index').length, 2);
    assert.equal(await api.fetch(LIST[1]), 'Preamp: -1 dB');
    assert.deepEqual(calls.at(-1), ['eq_autoeq_fetch', { path: 'oratory1990/x/Sennheiser HD 600' }]);
});
