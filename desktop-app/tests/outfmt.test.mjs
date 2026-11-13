// The player's output format (outfmt.js): the list from the capabilities,
// the payload the backend takes, the stored value and what plays of it, the
// selector's tip and note; and player.js wiring it in.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
    OUTFMT_LEGEND, OUTFMT_MARK, OUTFMT_RECHECK, OUTFMT_TIP, buildOutFmtList, capsCacheEntry, capsFromCache, dsdAsIsLine, dsdStripLabel, kindLabel,
    outFmtClosedLabel, outFmtItemTip, outFmtListLines, outFmtTip,
    outFmtValue, outputFormatPayload, parseOutFmt, rateBase, rememberKindNames, resolveOutFmt,
} from '../src/js/outfmt.js';

// A capabilities payload (player_dsd_capabilities) as the backend sends it:
// every realtime kind, rows for 44.1 and 48 kHz at DSD64/128/256.
const row = (base, multiple, state, load = 0.1) => ({
    base, multiple, label: base === 48000 ? `DSD${multiple} (48k)` : `DSD${multiple}`, state,
    load: ['ok', 'heavy', 'tooHeavy'].includes(state) ? load : null,
    reason: state === 'ok' ? `about ${Math.round(load * 100)} % of a core per channel`
        : state === 'heavy' ? `about ${Math.round(load * 100)} % of a core per channel: a busy PC may make it step down`
        : state === 'tooHeavy' ? `needs about ${Math.round(load * 100)} % of a core per channel: too heavy for this PC`
        : state === 'notAvailable' ? 'not available in this version' : 'built for DSD128 and up',
});
const rows = (f) => [44100, 48000].flatMap((b) => [64, 128, 256].map((m) => row(b, m, ...f(b, m))));
const CAPS = {
    measured: true, cores: 16, limits: { ok: 0.5, heavy: 0.8 },
    // Sent best first, but the list sorts by rank itself: shuffled here.
    kinds: [
        { kind: 'sdm5', label: 'SDM5', rank: 11, rates: rows(() => ['ok', 0.09]) },
        { kind: 'sdm9La', label: 'SDM9 LA', rank: 3, rates: rows(() => ['notForRate']) },
        { kind: 'sdm7La', label: 'SDM7 LA', rank: 5, rates: rows((b, m) => (m === 256 ? [b === 48000 ? 'tooHeavy' : 'heavy', b === 48000 ? 0.85 : 0.7] : ['ok', 0.3])) },
        { kind: 'sdm7LaPlus', label: 'SDM7 LA+', rank: 4, rates: rows(() => ['notAvailable']) },
        { kind: 'sdm7', label: 'SDM7', rank: 8, rates: rows((b, m) => (m === 256 && b === 48000 ? ['tooHeavy', 0.9] : ['ok', 0.2])) },
        { kind: 'sdm7Wide', label: 'SDM7 Wide', rank: 9, rates: rows((b, m) => (m === 64 ? ['notForRate'] : ['ok', 0.2])) },
    ],
};

test('the payload: PCM, or DSD64/128/256 with its modulator; a value from before the catalog is SDM5', () => {
    assert.deepEqual(outputFormatPayload('pcm'), { kind: 'pcm' });
    assert.deepEqual(outputFormatPayload('64'), { kind: 'dsd', multiple: 64, modulator: 'sdm5' });
    assert.deepEqual(outputFormatPayload('128'), { kind: 'dsd', multiple: 128, modulator: 'sdm5' });
    assert.deepEqual(outputFormatPayload('256'), { kind: 'dsd', multiple: 256, modulator: 'sdm5' });
    assert.deepEqual(outputFormatPayload('256:sdm7La'), { kind: 'dsd', multiple: 256, modulator: 'sdm7La' });
    assert.deepEqual(outputFormatPayload('128:sdm7WideLa'), { kind: 'dsd', multiple: 128, modulator: 'sdm7WideLa' });
    assert.deepEqual(outputFormatPayload('512'), { kind: 'pcm' }, 'DSD512 is no DoP rate');
    assert.deepEqual(outputFormatPayload('512:sdm9'), { kind: 'pcm' });
    assert.deepEqual(outputFormatPayload(undefined), { kind: 'pcm' });
});

test('a stored value: the old ones load as SDM5, anything unknown is PCM', () => {
    assert.equal(outFmtValue('256'), '256:sdm5');
    assert.equal(outFmtValue('64'), '64:sdm5');
    assert.equal(outFmtValue('256:sdm7La'), '256:sdm7La');
    assert.equal(outFmtValue('pcm'), 'pcm');
    assert.equal(outFmtValue('dsd'), 'pcm');
    assert.equal(outFmtValue('256:'), 'pcm');
    assert.equal(outFmtValue('256:<b>'), 'pcm');
    assert.equal(outFmtValue(OUTFMT_RECHECK), 'pcm', 'the re-check item is never a choice');
    assert.equal(outFmtValue(null), 'pcm');
    assert.deepEqual(parseOutFmt('128'), { multiple: 128, kind: 'sdm5' });
    assert.equal(parseOutFmt('pcm'), null);
});

test('the family: 48 kHz for its multiples, 44.1 kHz otherwise and before a track', () => {
    assert.equal(rateBase(48000), 48000);
    assert.equal(rateBase(192000), 48000);
    assert.equal(rateBase(44100), 44100);
    assert.equal(rateBase(88200), 44100);
    assert.equal(rateBase(0), 44100);
    assert.equal(rateBase(undefined), 44100);
});

test('a modulator\'s name: the catalog\'s once heard, read from its kind before', () => {
    assert.equal(kindLabel('sdm5'), 'SDM5');
    assert.equal(kindLabel('sdm7WideLa'), 'SDM7 Wide LA');
    assert.equal(kindLabel('sdm7LaPlus'), 'SDM7 LA+');
    assert.equal(kindLabel('sdm9Trellis'), 'SDM9 Trellis');
    assert.equal(kindLabel('other'), 'other');
    rememberKindNames({ kinds: [{ kind: 'sdm11', label: 'SDM11 Gold', rank: 1, rates: [] }] });
    assert.equal(kindLabel('sdm11'), 'SDM11 Gold');
    rememberKindNames(null);
});

// The tip: what it does, the lines in between, the marks' legend last.
const tipOf = (...lines) => [OUTFMT_TIP, ...lines, OUTFMT_LEGEND].join('\n');

const shown = (items) => items.map((i) => (i.sep ? '--' : `${i.long}${i.disabled ? ' [off]' : ''}`));

test('the list: PCM, then DSD256 → 128 → 64, best first; not built and not for the rate left out, too heavy greyed', () => {
    assert.deepEqual(shown(buildOutFmtList(CAPS, 44100, 'pcm')), [
        'PCM', '--',
        'DSD256 (SDM7 LA) · high CPU', 'DSD256 (SDM7)', 'DSD256 (SDM7 Wide)', 'DSD256 (SDM5)', '--',
        'DSD128 (SDM7 LA)', 'DSD128 (SDM7)', 'DSD128 (SDM7 Wide)', 'DSD128 (SDM5)', '--',
        'DSD64 (SDM7 LA)', 'DSD64 (SDM7)', 'DSD64 (SDM5)',
    ]);
    // The 48 kHz family has its own rows.
    const l48 = buildOutFmtList(CAPS, 48000, 'pcm');
    assert.deepEqual(shown(l48).slice(2, 7), ['DSD256 (SDM7 LA) · too heavy for this PC [off]', 'DSD256 (SDM7) · too heavy for this PC [off]', 'DSD256 (SDM7 Wide)', 'DSD256 (SDM5)', '--']);
    const heavy = buildOutFmtList(CAPS, 44100, 'pcm').find((i) => i.state === 'heavy');
    assert.deepEqual([heavy.value, heavy.short, heavy.disabled], ['256:sdm7La', 'DSD256 (SDM7 LA)', false]);
    assert.match(heavy.reason, /a busy PC may make it step down/);
    // Every value is one the selector takes back unchanged.
    for (const i of l48.filter((x) => !x.sep)) assert.equal(outFmtValue(i.value), i.value);
});

test('the list grows from the payload alone: a new kind needs no change here', () => {
    const caps = { ...CAPS, kinds: [...CAPS.kinds, { kind: 'sdm7WideLa', label: 'SDM7 Wide LA', rank: 7, rates: rows((b, m) => (m === 64 ? ['notForRate'] : ['ok', 0.4])) }] };
    const l = shown(buildOutFmtList(caps, 44100, 'pcm'));
    assert.deepEqual(l.slice(2, 8), ['DSD256 (SDM7 LA) · high CPU', 'DSD256 (SDM7 Wide LA)', 'DSD256 (SDM7)', 'DSD256 (SDM7 Wide)', 'DSD256 (SDM5)', '--']);
});

test('before the payload: SDM5 at every rate and the stored choice at its own, never empty, nothing greyed', () => {
    assert.deepEqual(shown(buildOutFmtList(null, 44100, 'pcm')), ['PCM', '--', 'DSD256 (SDM5)', '--', 'DSD128 (SDM5)', '--', 'DSD64 (SDM5)']);
    assert.deepEqual(shown(buildOutFmtList(null, 44100, '256:sdm7LaPlus')).slice(2, 4), ['DSD256 (SDM7 LA+)', 'DSD256 (SDM5)']);
    assert.equal(buildOutFmtList(null, 44100, 'pcm', true).some((i) => i.value === OUTFMT_RECHECK), false, 'nothing to re-check yet');
    const l = buildOutFmtList(CAPS, 44100, 'pcm', true);
    assert.deepEqual(l.slice(-2).map((i) => i.sep || i.long), [true, 'Re-check this PC…']);
    const busy = buildOutFmtList(CAPS, 44100, 'pcm', 'checking');
    assert.deepEqual(busy.slice(-1).map((i) => [i.value, i.long, i.disabled]), [[OUTFMT_RECHECK, 'Checking this PC…', false]], 'still usable while it checks');
});

test('what plays: the choice while this PC runs it, else the best it can run at that rate, said in a note', () => {
    assert.deepEqual(resolveOutFmt('pcm', CAPS, 44100), { value: 'pcm', note: '' });
    assert.deepEqual(resolveOutFmt('256:sdm7La', CAPS, 44100), { value: '256:sdm7La', note: '' }, 'heavy still plays');
    assert.deepEqual(resolveOutFmt('256:sdm7La', CAPS, 48000),
        { value: '256:sdm7Wide', note: 'SDM7 LA at DSD256 is too heavy for this PC — SDM7 Wide plays instead.' });
    assert.deepEqual(resolveOutFmt('256:sdm7LaPlus', CAPS, 44100),
        { value: '256:sdm7', note: 'SDM7 LA+ is not in this version — SDM7 plays instead.' },
        'the best that runs comfortably before a heavy one');
    assert.deepEqual(resolveOutFmt('128:sdm9Trellis', CAPS, 44100),
        { value: '128:sdm7La', note: 'SDM9 Trellis is not in this version — SDM7 LA plays instead.' });
    assert.deepEqual(resolveOutFmt('64:sdm7Wide', CAPS, 44100),
        { value: '64:sdm7La', note: 'SDM7 Wide is built for DSD128 and up — SDM7 LA plays instead.' });
    assert.deepEqual(resolveOutFmt('256', CAPS, 44100), { value: '256:sdm5', note: '' }, 'an old value plays SDM5');
    assert.deepEqual(resolveOutFmt('256:sdm7LaPlus', null, 44100), { value: '256:sdm7LaPlus', note: '' }, 'no judgement before the payload');
    // Nothing runs at the rate: SDM5 all the same (the stream steps down or says gaps).
    const none = { kinds: [{ kind: 'sdm5', label: 'SDM5', rank: 11, rates: rows(() => ['tooHeavy', 0.9]) }, { kind: 'sdm7', label: 'SDM7', rank: 8, rates: rows(() => ['tooHeavy', 0.95]) }] };
    assert.deepEqual(resolveOutFmt('256:sdm7', none, 44100), { value: '256:sdm5', note: 'SDM7 at DSD256 is too heavy for this PC — SDM5 plays instead.' });
    assert.deepEqual(resolveOutFmt('256:sdm5', none, 44100), { value: '256:sdm5', note: '' });
});

test('the tip: the heavy choice\'s load, what is too heavy, the notes marked', () => {
    const l = buildOutFmtList(CAPS, 48000, 'pcm');
    assert.deepEqual(outFmtListLines(l, '256:sdm5'), ['Too heavy for this PC: DSD256 (SDM7 LA), DSD256 (SDM7).']);
    const l44 = buildOutFmtList(CAPS, 44100, 'pcm');
    assert.deepEqual(outFmtListLines(l44, '256:sdm7La'), ['SDM7 LA at DSD256 takes about 70 % of a core per channel — a busy PC may make it step down to a lighter modulator.']);
    assert.deepEqual(outFmtListLines(l44, '128:sdm7'), []);
    const note = 'SDM7 LA at DSD256 is too heavy for this PC — SDM7 Wide plays instead.';
    assert.deepEqual(outFmtTip(null, false, { note, lines: ['x'] }), { tip: tipOf(note, 'x'), note: true });
    // The stream's own note: a step down after a lag.
    const lag = 'Stepped down to SDM7: this PC fell behind with SDM7 LA at DSD256.';
    assert.deepEqual(outFmtTip({ dsdNote: lag }, false, {}), { tip: tipOf(lag), note: true });
});

test('the tip: why PCM plays (a DSD file under BIT-PERFECT too), the overloads while DSD output plays', () => {
    assert.deepEqual(outFmtTip(null, false), { tip: tipOf(), note: false });
    const note = 'DSD: this device offers no 24-bit format here — playing PCM.';
    assert.deepEqual(outFmtTip({ dsdNote: note }, false), { tip: tipOf(note), note: true });
    const asIs = 'DSD64: this device takes no DoP at 176.4 kHz — playing PCM.';
    assert.deepEqual(outFmtTip({ dsdNote: asIs }, true), { tip: tipOf(asIs), note: true });
    const dsd = { label: 'DSD256', rateHz: 11289600, modulator: 'sdm5', overloads: 3 };
    assert.equal(outFmtTip({ dsd }, false).tip, tipOf('Modulator overloads: 3'));
    assert.equal(outFmtTip({ dsd: { ...dsd, overloads: 0 } }, false).tip, tipOf());
    assert.equal(outFmtTip({ dsd }, true).tip, tipOf(), 'no modulator under BIT-PERFECT');
    assert.match(OUTFMT_TIP, /^Output format\. .*Bit-perfect plays every file as it is: PCM as PCM, a DSD file as DoP at its own rate\. SDM5, SDM7 and SDM9 are .*Wide keeps the ultrasonic noise lower\.$/);
    assert.equal(OUTFMT_LEGEND, 'High CPU: plays, but takes much of one core per channel on this PC. Grey: too heavy for this PC.');
});

test('the marks: amber "high CPU" plays, grey "too heavy for this PC" cannot be chosen; each row\'s tip says how it fares', () => {
    assert.deepEqual(OUTFMT_MARK, { heavy: ' · high CPU', tooHeavy: ' · too heavy for this PC' });
    const l48 = buildOutFmtList(CAPS, 48000, 'pcm');
    const grey = l48.find((i) => i.value === '256:sdm7La');
    assert.deepEqual([grey.state, grey.disabled, grey.long], ['tooHeavy', true, 'DSD256 (SDM7 LA) · too heavy for this PC']);
    assert.equal(outFmtItemTip(grey), 'SDM7 LA at DSD256: needs about 85 % of a core per channel: too heavy for this PC');
    const amber = buildOutFmtList(CAPS, 44100, 'pcm').find((i) => i.value === '256:sdm7La');
    assert.deepEqual([amber.state, amber.disabled], ['heavy', false]);
    assert.equal(outFmtItemTip(amber), 'SDM7 LA at DSD256: about 70 % of a core per channel: a busy PC may make it step down');
    assert.equal(outFmtItemTip(l48[0]), '', 'PCM has none');
    assert.equal(outFmtItemTip({ sep: true }), '');
    assert.equal(outFmtItemTip(buildOutFmtList(null, 44100, 'pcm')[2]), '', 'none before the capabilities');
    assert.match(outFmtItemTip(buildOutFmtList(CAPS, 44100, 'pcm', true).at(-1)), /^Measures again/);
});

test('a kind that is another\'s very modulator at a rate (SDM7 LA+ at DSD256 = SDM7 LA): one row, a saved choice plays it silently', () => {
    const plus = { kind: 'sdm7LaPlus', label: 'SDM7 LA+', rank: 4, rates: rows((b, m) => (m === 256 ? ['ok', 0.4] : ['heavy', 0.6])) };
    for (const r of plus.rates) r.sameAs = r.multiple === 256 ? 'sdm7La' : null;
    const caps = { ...CAPS, kinds: CAPS.kinds.map((k) => (k.kind === 'sdm7LaPlus' ? plus : k)) };
    const l = buildOutFmtList(caps, 44100, 'pcm');
    assert.equal(l.some((i) => i.value === '256:sdm7LaPlus'), false, 'no LA+ row at DSD256');
    assert.equal(l.find((i) => i.value === '128:sdm7LaPlus')?.long, 'DSD128 (SDM7 LA+) · high CPU', 'kept where it differs');
    assert.equal(l.filter((i) => i.value === '256:sdm7La').length, 1);
    assert.deepEqual(resolveOutFmt('256:sdm7LaPlus', caps, 44100), { value: '256:sdm7La', note: '' });
    assert.deepEqual(resolveOutFmt('128:sdm7LaPlus', caps, 44100), { value: '128:sdm7LaPlus', note: '' });
    // Its twin too heavy here: the note names the twin, what plays is the next best.
    assert.deepEqual(resolveOutFmt('256:sdm7LaPlus', caps, 48000),
        { value: '256:sdm7Wide', note: 'SDM7 LA at DSD256 is too heavy for this PC — SDM7 Wide plays instead.' });
    assert.equal(outFmtClosedLabel(resolveOutFmt('256:sdm7LaPlus', caps, 44100).value), 'DSD256');
});

test('closed, the selector says the rate alone', () => {
    assert.equal(outFmtClosedLabel('pcm'), 'PCM');
    assert.equal(outFmtClosedLabel('256:sdm7LaPlus'), 'DSD256');
    assert.equal(outFmtClosedLabel('128:sdm5'), 'DSD128');
    assert.equal(outFmtClosedLabel('64'), 'DSD64', 'a value from before the catalog');
    assert.equal(outFmtClosedLabel('junk'), 'PCM');
    assert.equal(outFmtClosedLabel(OUTFMT_RECHECK), 'PCM');
});

test('a DSD file as it is: its own line; any other stream none', () => {
    assert.equal(dsdAsIsLine({ dsd: { label: 'DSD64', rateHz: 2822400, modulator: null, overloads: 0, asIs: true } }), 'DSD64 · DoP · bit-perfect · exclusive');
    assert.equal(dsdAsIsLine({ dsd: { label: 'DSD256', rateHz: 11289600, modulator: 'sdm5', overloads: 0 } }), '');
    assert.equal(dsdAsIsLine({}), '');
    assert.equal(dsdAsIsLine(null), '');
    for (const f of ['player.js', 'listening.js']) {
        const src = readFileSync(new URL(`../src/js/${f}`, import.meta.url), 'utf8');
        assert.match(src, /sub = dsdAsIsLine\(s\) \|\| \(/, f);
    }
});

test('player.js: the selector beside BIT-PERFECT, filled from the capabilities, sent as what plays, the strip names DoP', () => {
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    // One housing: the device, then the output format's button, then BIT-PERFECT.
    assert.match(src, /<span class="pl-devfmt" id="plDevFmt"><span class="pl-dev-pick" id="plDevPick">[^\n]*id="plDevice"[^\n]*<button type="button" id="plOutFmt" class="pl-outfmt"[^\n]*<\/button><\/span>\s*<button type="button" id="plBitPerfect"/);
    assert.match(src, /createPickList\(\$\('plOutFmt'\), \{[\s\S]{0,200}canOpen: \(\) => !bitPerfect,\s*onPick: pickOutFmt,/);
    assert.match(src, /text\.textContent = label/);
    assert.match(src, /buildOutFmtList\(dsdCaps, rateBase\(status\?\.srcRate\), outFmt, dsdChecking \? 'checking' : true\)/);
    assert.match(src, /invoke\('player_dsd_capabilities', \{ remeasure \}\)/);
    assert.match(src, /capsFromCache\(load\(KEY_DSDCAPS, ''\), v\)[\s\S]{0,120}\}\);\s*loadDsdCaps\(false\);/, 'the kept list at once, then asked without waiting');
    assert.equal(src.match(/output: +outputFormatPayload\(outFmtPlays\(\)\)/g)?.length, 2, 'the rack and a track rack');
    assert.match(src, /s\.dsd\s*\? `\$\{src\} → \$\{dsdStripLabel\(s\.dsd\)\} · DoP · exclusive`/);
    assert.match(src, /store\(KEY_OUTFMT, outFmt\);\s*outFmtSent = outFmtPlays\(\);\s*sendNow\(\);/);
    assert.match(src, /if \(v === OUTFMT_RECHECK\) \{\s*loadDsdCaps\(true\);\s*return;\s*\}/);
    assert.match(src, /function render\(\) \{[\s\S]*renderOutFmt\(s\)/);
});

test('player.css: the device and the output format in one housing, the format as narrow as "DSD256"', () => {
    const css = readFileSync(new URL('../src/css/player.css', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
    const block = (sel) => {
        const i = css.indexOf(`\n${sel} {\n`);
        assert.ok(i >= 0, `no "${sel}" rule`);
        return css.slice(i, css.indexOf('\n}', i + 1));
    };
    const prop = (b, p) => b.match(new RegExp(`\\n\\s*${p}: ([^;]+);`))?.[1];
    const house = block('.pl-devfmt');
    assert.equal(prop(house, 'height'), undefined, 'the row\'s shared height (topbar-controls.test.mjs)');
    assert.ok(prop(house, 'box-shadow'), 'it glows by itself');
    assert.match(css, /\n\.pl-devfmt:hover \{[^}]*box-shadow: 0 0 9px/, 'more under the pointer');
    const fmt = block('.pl-outfmt');
    assert.equal(prop(fmt, 'height'), '100%', 'the device\'s height');
    assert.match(prop(fmt, 'border-left'), /^1px solid/, 'a thin rule between the parts');
    assert.equal(prop(fmt, 'flex'), 'none', 'it never grows: the closed text is the rate alone');
    for (const p of ['font-size', 'font-weight', 'letter-spacing']) assert.equal(prop(fmt, p), prop(block('.pl-bp'), p), `${p}: BIT-PERFECT's letters`);
    assert.equal(prop(fmt, 'align-items'), undefined);
    assert.match(fmt, /display: inline-flex; align-items: center;/, 'its text centred');
    assert.ok(css.includes('.pl-outfmt-ghost { visibility: hidden; }'), 'as wide as "DSD256" whatever is chosen');
    // BIT-PERFECT idle: plainly a button, never faded out.
    assert.equal(prop(block('.pl-bp'), 'opacity'), undefined);
});

test('the strip names the modulator that plays: after a step down the lighter one', () => {
    assert.equal(dsdStripLabel({ label: 'DSD256', modulator: 'sdm5' }), 'DSD256 · SDM5');
    assert.equal(dsdStripLabel({ label: 'DSD256 (48k)', modulator: 'sdm5' }), 'DSD256 (48k) · SDM5');
    assert.equal(dsdStripLabel({ label: 'DSD256', modulator: 'sdm7La', running: 'sdm7La' }), 'DSD256 · SDM7 LA');
    assert.equal(dsdStripLabel({ label: 'DSD256', modulator: 'sdm7La', running: 'sdm7' }), 'DSD256 · SDM7');
    assert.equal(dsdStripLabel({ label: 'DSD64', modulator: null, asIs: true }), 'DSD64');
});

test('the kept capabilities: only for the version that kept them, anything broken is none', () => {
    const raw = capsCacheEntry(CAPS, '1.6.2');
    assert.deepEqual(capsFromCache(raw, '1.6.2'), CAPS);
    assert.equal(capsFromCache(raw, '1.6.3'), null, 'an update never shows the old list');
    assert.equal(capsFromCache(raw, ''), null);
    assert.equal(capsFromCache('', '1.6.2'), null);
    assert.equal(capsFromCache('{', '1.6.2'), null);
    assert.equal(capsFromCache(JSON.stringify(CAPS), '1.6.2'), null, 'the bare payload of an older page');
    assert.equal(capsFromCache(capsCacheEntry({ kinds: 3 }, '1.6.2'), '1.6.2'), null);
});
