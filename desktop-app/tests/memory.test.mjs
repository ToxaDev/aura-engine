// Track memory (M) keeps Hybrid-Phase's sensitivity step with the rest of the
// rack: the snapshot reads it, applying puts it back, and the converter's
// override carries it only with HP on. A record made before the slider
// existed reads as step 0.
import test from 'node:test';
import assert from 'node:assert/strict';

const els = new Map();
const el = (id) => {
    if (!els.has(id)) els.set(id, { id, value: '', checked: false, dispatchEvent() {} });
    return els.get(id);
};
globalThis.document = {
    getElementById: el,
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener() {},
    createElement: () => ({ style: {}, classList: { add() {}, remove() {}, toggle() {} }, appendChild() {}, setAttribute() {} }),
};
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.window = { addEventListener() {} };

const { readMSnapshot, applyMSnapshot, snapToConvertOverride } = await import('../src/js/memory.js');

test('the snapshot reads the sensitivity step from the slider', () => {
    el('convHybridPhase').checked = true;
    el('convHpSens').value = '2';
    assert.equal(readMSnapshot().labFeatures.hpSens, 2);
    el('convHpSens').value = 'junk';
    assert.equal(readMSnapshot().labFeatures.hpSens, 0);
});

test('applying a snapshot puts its step back; an old record without one gives 0', () => {
    applyMSnapshot({ convHybridPhase: true, labFeatures: { hpSens: -2 } });
    assert.equal(el('convHpSens').value, '-2');
    applyMSnapshot({ convHybridPhase: true, labFeatures: { declip: true } });
    assert.equal(el('convHpSens').value, '0');
});

test('the converter override carries the step only with HP on', () => {
    assert.equal(snapToConvertOverride({ convHybridPhase: true, labFeatures: { hpSens: 3 } }).labFeatures.hpSens, 3);
    assert.equal(snapToConvertOverride({ convHybridPhase: false, labFeatures: { hpSens: 3 } }).labFeatures.hpSens, 0);
    assert.equal(snapToConvertOverride({ convHybridPhase: true, labFeatures: {} }).labFeatures.hpSens, 0);
});
