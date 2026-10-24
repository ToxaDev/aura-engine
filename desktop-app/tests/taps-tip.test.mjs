// The taps chip's tooltip names how the player upsamples: single stage
// word for word as before, multistage with its short chain.
import test from 'node:test';
import assert from 'node:assert/strict';

import { buildTapsTip } from '../src/js/taps-tip.js';

test('single stage keeps its tooltip', () => {
    assert.equal(
        buildTapsTip({ taps: '30M' }),
        'Filter size: 30M taps. Ratio: \xd78 (the player always renders through the polyphase path).'
    );
    assert.equal(
        buildTapsTip({ taps: '30M', upsampler: null }),
        'Filter size: 30M taps. Ratio: \xd78 (the player always renders through the polyphase path).'
    );
});

test('multistage names the filter at the source rate and its chain', () => {
    assert.equal(
        buildTapsTip({ taps: '30M', upsampler: '\xd72 \xb7 \xd74, 95 + 287 taps' }),
        'Filter size: 30M taps. Multistage: the filter at the source rate + short exact interpolator ' +
        '(\xd72 \xb7 \xd74, 95 + 287 taps); matches single-stage to −204…−213 dBFS.'
    );
});
