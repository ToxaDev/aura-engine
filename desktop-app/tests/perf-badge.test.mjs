// The load badge's numbers: the processor and the card averaged over a few
// seconds, so that background work in bursts does not make them jump.
import test from 'node:test';
import assert from 'node:assert/strict';
import { BADGE_AVG_S, mean } from '../src/js/perf-badge.js';

test('the badge averages the processor and the card over three seconds', () => {
    assert.equal(BADGE_AVG_S, 3);
    const s = [{ cpu: 2, gpu: 0 }, { cpu: 38, gpu: 6 }, { cpu: 20, gpu: 3 }];
    assert.equal(mean(s, 'cpu'), 20);
    assert.equal(mean(s, 'gpu'), 3);
});

test('no samples or a missing value read 0, not NaN', () => {
    assert.equal(mean([], 'cpu'), 0);
    assert.equal(mean([{ cpu: 4 }, {}], 'cpu'), 2);
});
