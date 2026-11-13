// The EqState contract (js/eq/state.js): clamps, the signature rackSig
// carries, the undo history and the presets list.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
    flatState, normalizeState, normalizeBand, eqSig, eqActive, isBypass, badgeLabel, shortName,
    History, normalizePresets, savePreset, renamePreset, deletePreset, MAX_BANDS,
} from '../src/js/eq/state.js';

const st = (bands, extra = {}) => ({ v: 2, on: true, name: 'X', bands, gain: 'manual', output: 0, ...extra });
const bell = (f, gain, q = 1, on = true) => ({ type: 'bell', f, gain, q, slope: 12, on });

test('flat state is off and empty', () => {
    assert.deepEqual(flatState(), { v: 2, on: false, name: 'Flat', bands: [], gain: 'manual', output: 0 });
    assert.deepEqual(normalizeState(null), flatState());
    assert.deepEqual(normalizeState('junk'), flatState());
});

test('values are clamped to the contract', () => {
    const b = normalizeBand({ type: 'nope', f: 5, gain: 99, q: 0, slope: 7 });
    assert.deepEqual(b, { type: 'bell', f: 10, gain: 30, q: 0.1, slope: 6, on: true }, 'slope snaps to the nearest step');
    const s = normalizeState({ v: 2, on: true, bands: Array.from({ length: 40 }, () => bell(100, 1)), gain: 'auto', output: -40 });
    assert.equal(s.bands.length, MAX_BANDS);
    assert.deepEqual([s.v, s.gain, s.output, 'preamp' in s], [2, 'auto', -30, false]);
    assert.deepEqual([normalizeState({ v: 2, gain: 'x', output: 31 }).gain, normalizeState({ v: 2, output: 31 }).output], ['manual', 30]);
    assert.equal(normalizeBand({ type: 'lowCut', slope: 48 }).slope, 48);
});

test('a version-1 state migrates: auto preamp → safe, a manual preamp into the output', () => {
    const old = (extra) => normalizeState({ v: 1, on: true, bands: [bell(1000, 3)], preamp: null, output: 0, ...extra });
    assert.deepEqual([old({}).gain, old({}).output], ['safe', 0]);
    assert.deepEqual([old({ preamp: -2.5, output: 1 }).gain, old({ preamp: -2.5, output: 1 }).output], ['manual', -1.5]);
    assert.equal(old({ preamp: 5, output: 1 }).output, 1, 'a positive preamp counted as 0');
    assert.equal(old({ preamp: -50, output: -20 }).output, -30);
    assert.equal(old({ gain: 'auto', preamp: -3 }).gain, 'auto', 'a valid gain field wins (window builds before v2)');
    assert.equal(normalizeState({ on: true, bands: [] }).gain, 'safe', 'no version = version 1');
    assert.equal(eqSig(old({ preamp: -2 })), eqSig(st([bell(1000, 3)], { output: -2 })));
});

test('normalize is idempotent and JSON-stable', () => {
    const s = normalizeState(st([bell(1000, 3), { type: 'highCut', f: 15000, q: 0.71, slope: 24 }]));
    assert.deepEqual(normalizeState(JSON.parse(JSON.stringify(s))), s);
});

test('eqSig: null when inactive, the engine format, ignores name and inactive bands', () => {
    const a = st([bell(1000, 3)]);
    assert.equal(eqSig({ ...a, on: false }), null);
    assert.equal(eqSig(null), null);
    assert.equal(eqSig(st([bell(1000, 0.004)])), null, 'gain under 0.005 dB is inactive');
    assert.equal(eqSig(st([], { gain: 'auto' })), null);
    assert.equal(eqSig(a), '["manual",0,[["bell",1000,3,1,12]]]');
    assert.equal(eqSig(st([{ type: 'lowCut', f: 20, gain: 0, q: 0.71, slope: 24, on: true }], { gain: 'safe', output: 1 })),
        '["safe",1,[["lowCut",20,0,0.71,24]]]');
    assert.equal(eqSig(a), eqSig({ ...a, name: 'Other' }));
    assert.notEqual(eqSig(a), eqSig({ ...a, gain: 'auto' }));
    assert.equal(eqSig(a), eqSig(st([bell(1000, 3), bell(200, -4, 1, false), bell(300, 0)])));
    assert.notEqual(eqSig(a), eqSig(st([bell(1000.1, 3)])));
    assert.notEqual(eqSig(a), eqSig(st([{ ...bell(1000, 3), type: 'notch' }])));
    assert.ok(eqActive(st([], { output: -1 })));
    assert.ok(!eqActive(st([bell(1000, 3)], { on: false })));
});

test('bypass and badge label', () => {
    assert.ok(isBypass(flatState()));
    assert.ok(isBypass(st([bell(1000, 3, 1, false)])));
    assert.ok(!isBypass(st([bell(1000, 3)])));
    assert.ok(!isBypass(st([], { output: 2 })));
    assert.equal(badgeLabel(flatState()), 'FLAT');
    assert.equal(badgeLabel(st([bell(1000, 3)], { name: 'Sennheiser HD 600 ParametricEQ.txt' })), 'Sennheiser HD…');
    assert.equal(shortName('HD 600 ParametricEQ.txt'), 'HD 600');
});

test('history: 60 deep, a new edit drops redo', () => {
    const h = new History(60);
    for (let i = 0; i < 70; i++) h.push(i);
    assert.equal(h.undo.length, 60);
    assert.equal(h.back(70), 69);
    assert.equal(h.forward(69), 70);
    assert.equal(h.back(70), 69);
    assert.ok(h.canRedo);
    h.push(69);
    assert.ok(!h.canRedo);
    h.clear();
    assert.equal(h.back(1), null);
    assert.equal(h.forward(1), null);
});

test('presets: save, rename, delete; Flat is built in and reserved', () => {
    let ps = savePreset([], 'Mine', st([bell(1000, 3)]));
    assert.equal(ps.length, 1);
    assert.equal(ps[0].state.name, 'Mine');
    ps = savePreset(ps, 'mine', st([bell(500, 1)]));
    assert.equal(ps.length, 1, 'same name replaces');
    assert.equal(ps[0].state.bands[0].f, 500);
    assert.equal(savePreset(ps, 'Flat', st([])).length, 1, 'Flat is reserved');
    ps = savePreset(ps, 'Other', st([]));
    assert.equal(renamePreset(ps, 'Other', 'mine').length, 2, 'rename onto a taken name is refused');
    assert.deepEqual(renamePreset(ps, 'Other', 'mine').map(p => p.name), ['mine', 'Other']);
    ps = renamePreset(ps, 'Other', 'Third');
    assert.deepEqual(ps.map(p => p.name), ['mine', 'Third']);
    assert.equal(ps[1].state.name, 'Third');
    assert.deepEqual(deletePreset(ps, 'mine').map(p => p.name), ['Third']);
    assert.deepEqual(normalizePresets([null, { name: 'A', state: {} }, { name: 'a' }, { name: 3 }]).map(p => p.name), ['A']);
});

import { BUILTIN_PRESETS, isReservedName, allPresets } from '../src/js/eq/state.js';
import { totalCurve, logGrid, autoPreamp } from '../src/js/eq/math.js';

test('built-in presets: the agreed list, valid against the contract, gentle (peak ≤ +6 dB), manual gain at 0 dB', () => {
    assert.deepEqual(allPresets([]).map(p => p.name),
        ['Flat', 'Warm', 'Bright', 'Bass boost', 'Loudness', 'Vocal', 'De-harsh', 'Acoustic']);
    const grid = logGrid(400, 20, 20000);
    for (const p of BUILTIN_PRESETS) {
        // Already what normalizeState makes of it: nothing clamped away.
        const n = normalizeState(p.state);
        assert.deepEqual(n.bands, p.state.bands, p.name);
        assert.deepEqual([n.gain, n.output], ['manual', 0], p.name);
        assert.ok(eqActive(n), p.name);
        for (const b of n.bands) {
            assert.ok(Math.abs(b.gain) <= 5 && b.q >= 0.7 && b.q <= 2, `${p.name}: ${b.type} ${b.f}`);
        }
        const peak = Math.max(...totalCurve(n.bands, grid));
        assert.ok(peak <= 6 && peak > 0.5 || p.name === 'De-harsh', `${p.name}: peak ${peak}`);
        assert.ok(Math.abs(autoPreamp(n.bands) + Math.max(0, peak)) < 0.2, `${p.name}: auto preamp covers the peak`);
    }
});

test('user presets may not take a built-in name (any case); the list puts them after the built-ins', () => {
    assert.ok(isReservedName('warm') && isReservedName(' Flat ') && isReservedName('DE-HARSH'));
    assert.ok(!isReservedName('My warm'));
    const s = st([{ type: 'bell', f: 1000, gain: 1, q: 1 }]);
    assert.deepEqual(savePreset([], 'Warm', s), []);
    const one = savePreset([], 'Mine', s);
    assert.deepEqual(renamePreset(one, 'Mine', 'bright').map(p => p.name), ['Mine']);
    assert.deepEqual(normalizePresets([{ name: 'Loudness', state: s }, { name: 'Mine', state: s }]).map(p => p.name), ['Mine']);
    assert.equal(allPresets(one).at(-1).name, 'Mine');
    assert.ok(Object.isFrozen(BUILTIN_PRESETS[0].state.bands[0]));
});

import { balanceOf, BALANCE_MAX } from '../src/js/eq/state.js';

test('balance: ±30 dB in 0.01 dB steps, L/R unless ms; kept by normalize only off centre', () => {
    assert.deepEqual(balanceOf({}), { balance: 0, balanceMode: 'lr' });
    assert.deepEqual(balanceOf({ balance: 1.234, balanceMode: 'ms' }), { balance: 1.23, balanceMode: 'ms' });
    assert.deepEqual(balanceOf({ balance: -99, balanceMode: 'x' }), { balance: -BALANCE_MAX, balanceMode: 'lr' });
    assert.ok(Object.is(balanceOf({ balance: -0.004 }).balance, 0), 'no −0');
    assert.equal(balanceOf({ balance: NaN }).balance, 0);
    const plain = normalizeState(st([bell(1000, 3)]));
    assert.ok(!('balance' in plain) && !('balanceMode' in plain), 'a state without a balance stays as it was');
    const s = normalizeState(st([], { balance: -2.5, balanceMode: 'ms' }));
    assert.equal(s.balance, -2.5);
    assert.equal(s.balanceMode, 'ms');
    assert.deepEqual(normalizeState(JSON.parse(JSON.stringify(s))), s);
    assert.equal(normalizeState(st([], { balanceMode: 'ms' })).balanceMode, 'ms', 'the mode stays at the centre');
});

test('balance alone makes the EQ active; the signature gains it only off centre', () => {
    assert.ok(eqActive(st([], { balance: 0.5 })));
    assert.ok(!eqActive(st([], { balance: 0.5, on: false })));
    assert.ok(!eqActive(st([], { balanceMode: 'ms' })));
    assert.equal(eqSig(st([], { balance: 0.5 })), JSON.stringify(['manual', 0, [], 'lr', 0.5]));
    assert.equal(eqSig(st([bell(1000, 3)])), eqSig(st([bell(1000, 3)], { balanceMode: 'ms' })), 'mode at the centre is no new render');
});

import { delayOf, DELAY_MAX, speakerFields, withSpeakers, effectiveState, speakersActive } from '../src/js/eq/state.js';
import { fmtDelay, fmtDelayCm, parseDelay, delayFromPos, posFromDelay, speakerCalc, soundSpeed } from '../src/js/eq/controls.js';

test('delay: ±30 ms in 0.01 ms steps, + the right late; on with the EQ, kept by normalize only off 0', () => {
    assert.equal(delayOf({ delay: 2.044 }), 2.04);
    assert.equal(delayOf({ delay: -45 }), -DELAY_MAX);
    assert.equal(delayOf({ delay: 'x' }), 0);
    assert.ok(eqActive(st([], { delay: 0.5 })) && !eqActive(st([], { delay: 0.004 })));
    assert.ok(!eqActive(st([], { delay: 2, on: false })));
    assert.equal(normalizeState(st([], { delay: -1.5 })).delay, -1.5);
    assert.ok(!('delay' in normalizeState(st([]))));
});

test('the signature: balance-only renders keep theirs, a delay adds the mode, balance and delay', () => {
    assert.equal(eqSig(st([], { balance: 0.5 })), JSON.stringify(['manual', 0, [], 'lr', 0.5]));
    assert.equal(eqSig(st([], { delay: -2 })), JSON.stringify(['manual', 0, [], 'lr', 0, -2]));
    assert.equal(eqSig(st([bell(1000, 3)])), JSON.stringify(['manual', 0, [['bell', 1000, 3, 1, 12]]]));
});

test('keep with EQ off: the speakers alone play, no band, no gain; off by default', () => {
    const off = st([bell(1000, 3)], { on: false, output: 6, balance: 2, delay: 1 });
    assert.equal(effectiveState(off), null);
    assert.ok(!eqActive(off) && !speakersActive(off));
    const kept = { ...off, speakersAlways: true };
    assert.ok(eqActive(kept) && speakersActive(kept));
    assert.equal(eqSig(kept), JSON.stringify(['manual', 0, [], 'lr', 2, 1]));
    assert.ok(!eqActive({ ...kept, balance: 0, delay: 0 }), 'nothing to play with the EQ off');
    assert.equal(eqSig(st([bell(1000, 3)], { speakersAlways: true })), eqSig(st([bell(1000, 3)])), 'on, the switch changes nothing');
    assert.equal(normalizeState(kept).speakersAlways, true);
    assert.ok(!('speakersAlways' in normalizeState(off)));
});

test('the speakers are the room\'s: presets neither keep nor bring them', () => {
    const cur = st([bell(1000, 3)], { balance: -1.5, balanceMode: 'ms', delay: 2.04, speakersAlways: true });
    assert.deepEqual(speakerFields(cur), { balance: -1.5, balanceMode: 'ms', delay: 2.04, speakersAlways: true });
    const list = savePreset([], 'Mine', cur);
    for (const k of ['balance', 'balanceMode', 'delay', 'speakersAlways']) assert.ok(!(k in list[0].state), k);
    assert.ok(!('delay' in normalizePresets([{ name: 'Old', state: { ...cur } }])[0].state));
    const loaded = withSpeakers(st([bell(200, -2)], { balance: 9, delay: -9 }), cur);
    assert.equal(loaded.balance, -1.5); assert.equal(loaded.delay, 2.04); assert.equal(loaded.speakersAlways, true);
    assert.equal(loaded.bands[0].f, 200);
    const flat = withSpeakers(flatState(), cur);
    assert.equal(flat.bands.length, 0); assert.equal(flat.delay, 2.04);
});

test('the delay field: L/R and ms, distances in cm or m, the cm hint', () => {
    assert.equal(fmtDelay(0), '0 ms');
    assert.equal(fmtDelay(2.04), 'R 2.04 ms');
    assert.equal(fmtDelay(-0.5), 'L 0.5 ms');
    assert.equal(fmtDelayCm(2.04), '70 cm');
    assert.equal(fmtDelayCm(0.1), '3.4 cm');
    assert.equal(parseDelay('L2.04'), -2.04);
    assert.equal(parseDelay('r 1.5 ms'), 1.5);
    assert.equal(parseDelay('-2'), -2);
    assert.equal(parseDelay('off'), 0);
    assert.ok(Math.abs(parseDelay('L70cm') + 70 / 34.3) < 1e-12);
    assert.ok(Math.abs(parseDelay('0.343m R') - 1) < 1e-12);
    assert.equal(parseDelay('L2R'), null);
    assert.equal(parseDelay('abc'), null);
    for (const ms of [0, 0.25, -1.5, 10]) assert.equal(delayFromPos(posFromDelay(ms)), ms);
});

test('distances: the nearer speaker is delayed by the difference over the speed of sound', () => {
    assert.ok(Math.abs(soundSpeed(20) - 343.2) < 0.1);
    // right 70 cm nearer: the right is late by 0.7 / 343.2 s
    const r = speakerCalc(300, 230, 'cm', 20);
    assert.equal(r.delay, 2.04);
    assert.equal(r.balance, Math.round(20 * Math.log10(230 / 300) * 100) / 100);
    assert.ok(r.balance < 0, 'the nearer right is lowered: the balance leans left');
    const l = speakerCalc(2.3, 3, 'm');
    assert.equal(l.delay, -2.04);
    assert.ok(l.balance > 0);
    assert.deepEqual(speakerCalc(100, 100), { delay: 0, balance: 0, clamped: false });
    assert.equal(speakerCalc(10, 2000).clamped, true);
    assert.equal(speakerCalc(10, 2000).delay, -30);
    assert.equal(speakerCalc(0, 100), null);
    assert.equal(speakerCalc('', 100), null);
});
