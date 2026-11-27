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
    isAsioDevice, isAlsaDevice, takesNativeDsd, baseModulator, OUTFMT_DSD48K, OUTFMT_DSD48K_TIP, dsdBase,
    dsdWayLine, dsdWayTag, dsdNative, dsdPlaying,
    outFmtPanelModel, shownPreset, panelValueAtRate, dacMaxOf, dacMaxSet, dacModelOf, dacModelSet, plainKind, isWideKind, chainLine, loadLine, panelHint,
} from '../src/js/outfmt.js';
import { knownDac, KNOWN_DACS, dacMakerOf, dacChoices, dacByName, searchDacs, usbDacs, deviceDac, tidyDeviceName } from '../src/js/dac-db.js';
import { DAC_DATA } from '../src/js/dac-db-data.js';

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
    assert.deepEqual(outputFormatPayload('2048'), { kind: 'pcm' }, 'DSD2048 is not offered');
    assert.deepEqual(outputFormatPayload('512'), { kind: 'dsd', multiple: 512, modulator: 'sdm7' }, 'DSD512 (ASIO, natively): SDM7 the lightest');
    assert.deepEqual(outputFormatPayload('2048:sdm9'), { kind: 'pcm' });
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
    assert.deepEqual(shown(l48).slice(2, 7), ['DSD256 (SDM7 LA) · too heavy for this PC', 'DSD256 (SDM7) · too heavy for this PC', 'DSD256 (SDM7 Wide)', 'DSD256 (SDM5)', '--']);
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

test('try anyway: a too-heavy choice picked on purpose plays as chosen; any other still steps down', () => {
    const none = { kinds: [{ kind: 'sdm7', label: 'SDM7', rank: 8, rates: rows(() => ['tooHeavy', 0.95]) }, { kind: 'sdm5', label: 'SDM5', rank: 11, rates: rows(() => ['ok', 0.2]) }] };
    assert.deepEqual(resolveOutFmt('256:sdm7', none, 44100, '256:sdm7'), { value: '256:sdm7', note: '' });
    assert.equal(resolveOutFmt('256:sdm7', none, 44100, '').value, '256:sdm5');
    assert.equal(resolveOutFmt('256:sdm7', none, 44100, '128:sdm7').value, '256:sdm5');
    const l = buildOutFmtList(none, 44100, '256:sdm7');
    assert.ok(l.filter((i) => i.state === 'tooHeavy').every((i) => !i.disabled));
    assert.match(outFmtListLines(l, '256:sdm7').join('\n'), /tried anyway/);
});

test('the tip: the heavy choice\'s load, what is too heavy, the notes marked', () => {
    const l = buildOutFmtList(CAPS, 48000, 'pcm');
    assert.deepEqual(outFmtListLines(l, '256:sdm5'), ['Too heavy for this PC: DSD256 (SDM7 LA), DSD256 (SDM7) (pick one to try anyway).']);
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
    assert.equal(OUTFMT_LEGEND, 'High CPU: plays, but takes much of one core per channel on this PC. Grey: too heavy for this PC; pick it to try anyway, a busy PC steps down to a lighter modulator by itself.');
});

test('the marks: amber "high CPU" plays, grey "too heavy for this PC" cannot be chosen; each row\'s tip says how it fares', () => {
    assert.deepEqual(OUTFMT_MARK, { heavy: ' · high CPU', tooHeavy: ' · too heavy for this PC' });
    const l48 = buildOutFmtList(CAPS, 48000, 'pcm');
    const grey = l48.find((i) => i.value === '256:sdm7La');
    assert.deepEqual([grey.state, grey.disabled, grey.long], ['tooHeavy', false, 'DSD256 (SDM7 LA) · too heavy for this PC']);
    assert.equal(outFmtItemTip(grey), 'SDM7 LA at DSD256: needs about 85 % of a core per channel: too heavy for this PC. Pick it to try anyway: if this PC falls behind, a lighter modulator plays instead.');
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

test('player.js: the selector beside BIT-PERFECT, filled from the capabilities, sent as what plays, the strip names how DSD goes out', () => {
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    // One housing: the device, then the output format's button, then BIT-PERFECT.
    assert.match(src, /<span class="pl-devfmt" id="plDevFmt"><span class="pl-dev-pick" id="plDevPick">[^\n]*id="plDevice"[^\n]*<button type="button" id="plOutFmt" class="pl-outfmt"[^\n]*<\/button><\/span>\s*<button type="button" id="plBitPerfect"/);
    assert.match(src, /createOutFmtPanel\(\$\('plOutFmt'\), \{[\s\S]{0,400}canOpen: \(\) => !bitPerfect,\s*onPick: pickOutFmt,/);
    assert.match(src, /text\.textContent = label/);
    assert.match(src, /buildOutFmtList\(dsdCaps, base, outFmt, dsdChecking \? 'checking' : true, nativeDev, dsd48k\)/);
    assert.match(src, /const base = dsdBase\(status\?\.srcRate, dsd48k\);/, 'the DSD family: 44.1 kHz unless the 48 kHz switch is on');
    assert.match(src, /outFmtList\.setItems\(items, value, ctx\)/, 'the panel gets the list as it is, the device kind and the family');
    assert.match(src, /if \(v !== 'pcm'\) \{\s*outFmtLastDsd = v;\s*store\(KEY_OUTFMT_DSD, v\);/, 'the DSD tab goes back to the choice before PCM');
    assert.match(src, /invoke\('player_dsd_capabilities', \{ remeasure \}\)/);
    assert.match(src, /capsFromCache\(load\(KEY_DSDCAPS, ''\), v\)[\s\S]{0,120}\}\)\.finally\(\(\) => setTimeout\(\(\) => loadDsdCaps\(false\), dsdCaps \? DSD_CHECK_DELAY_MS : 0\)\);/, 'the kept list at once, then asked after the start (at once without a kept list)');
    assert.match(src, /const DSD_CHECK_DELAY_MS = 10000;/);
    assert.equal(src.match(/output: +outputFormatPayload\(outFmtPlays\(\)\)/g)?.length, 2, 'the rack and a track rack');
    assert.match(src, /s\.dsd\s*\? `\$\{src\} → \$\{dsdStripLabel\(s\.dsd\)\} · \$\{dsdWayLine\(\$\('plDevice'\)\?\.value, s\.format\)\}`/);
    assert.match(src, /store\(KEY_OUTFMT, outFmt\);[\s\S]{0,120}outFmtSent = outFmtPlays\(\);[\s\S]{0,240}publishOutFmt\(outFmtSent\);\s*sendNow\(\);/);
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

test('an ASIO device: DSD1024 and DSD512 listed above DoP\'s rates, SDM7 the lightest there; elsewhere only a kept choice shows them', () => {
    assert.equal(isAsioDevice('asio:ASIO Chord 44.1Khz 768Khz'), true);
    assert.equal(isAsioDevice('{0.0.0.00000000}.{abc}'), false);
    assert.equal(isAsioDevice('asio:'), false);
    assert.equal(isAsioDevice(null), false);
    assert.equal(baseModulator(512), 'sdm7');
    assert.equal(baseModulator(256), 'sdm5');
    assert.equal(outFmtValue('512'), '512:sdm7');
    assert.equal(outFmtValue('1024:sdm9La'), '1024:sdm9La');
    assert.equal(outFmtValue('2048:sdm7'), 'pcm');
    assert.deepEqual(outputFormatPayload('1024:sdm9'), { kind: 'dsd', multiple: 1024, modulator: 'sdm9' });
    assert.equal(outFmtClosedLabel('1024:sdm9'), 'DSD1024');
    // Before the payload: SDM7 at the native rates, SDM5 at DoP's.
    const asio = shown(buildOutFmtList(null, 44100, 'pcm', false, true));
    assert.deepEqual(asio, ['PCM', '--', 'DSD1024 (SDM7)', '--', 'DSD512 (SDM7)', '--', 'DSD256 (SDM5)', '--', 'DSD128 (SDM5)', '--', 'DSD64 (SDM5)']);
    // Not an ASIO device: DoP's rates only, unless the kept choice is native.
    assert.deepEqual(shown(buildOutFmtList(null, 44100, 'pcm')), ['PCM', '--', 'DSD256 (SDM5)', '--', 'DSD128 (SDM5)', '--', 'DSD64 (SDM5)']);
    assert.equal(shown(buildOutFmtList(null, 44100, '512:sdm7')).includes('DSD512 (SDM7)'), true);
    // From the payload: a kind not built for DSD512 is left out there.
    const native = (b, m) => (m > 256 ? ['notForRate'] : ['ok', 0.09]);
    const caps = { ...CAPS, kinds: [
        { kind: 'sdm5', label: 'SDM5', rank: 11, rates: [44100, 48000].flatMap((b) => [64, 128, 256, 512, 1024].map((m) => row(b, m, ...native(b, m)))) },
        { kind: 'sdm7', label: 'SDM7', rank: 8, rates: [44100, 48000].flatMap((b) => [64, 128, 256, 512, 1024].map((m) => row(b, m, 'ok', m / 2048))) },
    ] };
    const l = shown(buildOutFmtList(caps, 44100, 'pcm', false, true));
    assert.deepEqual(l.slice(0, 5), ['PCM', '--', 'DSD1024 (SDM7)', '--', 'DSD512 (SDM7)']);
    // A native choice this PC cannot run with its kind steps to what it can: never SDM5.
    assert.equal(resolveOutFmt('512:sdm5', caps, 44100).value, '512:sdm7');
    // player.js asks for the native rates with a device that may take them.
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(src, /takesNativeDsd\(\$\('plDevice'\)\?\.value, defaultDeviceId\)/);
});

test('an ALSA card (Linux) takes DSD natively too: DSD1024 and DSD512 offered, for the system default as well', () => {
    assert.equal(isAlsaDevice('hw:CARD=M2,DEV=0'), true);
    assert.equal(isAlsaDevice('hw:'), false);
    assert.equal(isAlsaDevice('asio:Chord'), false);
    assert.equal(takesNativeDsd('hw:CARD=M2,DEV=0'), true);
    assert.equal(takesNativeDsd('asio:ASIO Chord 44.1Khz 768Khz'), true);
    assert.equal(takesNativeDsd('{0.0.0.00000000}.{abc}'), false);
    // "System default": the default device's id decides.
    assert.equal(takesNativeDsd('', 'hw:CARD=PCH,DEV=0'), true);
    assert.equal(takesNativeDsd('', '{0.0.0.00000000}.{abc}'), false);
    assert.equal(takesNativeDsd(null), false);
    const l = shown(buildOutFmtList(null, 44100, 'pcm', false, takesNativeDsd('hw:CARD=M2,DEV=0')));
    assert.deepEqual(l.slice(0, 5), ['PCM', '--', 'DSD1024 (SDM7)', '--', 'DSD512 (SDM7)']);
});

test('the 48 kHz DSD switch: off by default, a tick in the list before the re-check; the 44.1 kHz rows unless on (issue #8)', () => {
    // Off: a 48 kHz family track plays the 44.1 kHz family's DSD rate.
    assert.equal(dsdBase(96000, false), 44100);
    assert.equal(dsdBase(96000, true), 48000);
    assert.equal(dsdBase(44100, true), 44100);
    assert.equal(dsdBase(0, true), 44100);
    // No switch asked for: no item (as before).
    assert.equal(buildOutFmtList(CAPS, 44100, 'pcm', true).some((i) => i.value === OUTFMT_DSD48K), false);
    const off = buildOutFmtList(CAPS, 44100, 'pcm', true, false, false);
    assert.deepEqual(off.slice(-4).map((i) => i.sep || i.long), [true, '48 kHz DSD for 48 kHz tracks', true, 'Re-check this PC…']);
    const on = buildOutFmtList(null, 44100, 'pcm', false, false, true);
    assert.deepEqual(on.slice(-2).map((i) => i.sep || [i.long, i.checked]), [true, ['✓ 48 kHz DSD for 48 kHz tracks', true]]);
    assert.equal(outFmtItemTip(on.at(-1)), OUTFMT_DSD48K_TIP);
    assert.match(OUTFMT_DSD48K_TIP, /every DSD DAC/);
    // Never a stored output format.
    assert.equal(outFmtValue(OUTFMT_DSD48K), 'pcm');
    // player.js: stored on its own, sent with the rack, the list's family from it.
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(src, /KEY_DSD48K\s*=\s*'auraPlayerDsd48k'/);
    assert.equal((src.match(/^\s*dsd48k,$/gm) || []).length, 2, 'both settings payloads carry it');
    assert.match(src, /dsdBase\(status\?\.srcRate, dsd48k\)/);
    assert.match(src, /if \(v === OUTFMT_DSD48K\) \{/);
});

test('the way DSD goes out: natively on a DAC\'s ASIO driver, DoP elsewhere (the line said DoP on ASIO too)', () => {
    assert.equal(dsdWayLine('asio:Chord Mojo 2'), 'ASIO native');
    assert.equal(dsdWayLine('{0.0.0.00000000}.{abc}'), 'DoP · exclusive');
    assert.equal(dsdWayLine(''), 'DoP · exclusive');
    assert.equal(dsdNative('asio:RME'), true);
    assert.equal(dsdNative(undefined), false);
    // Linux: an ALSA device that took DSD in its own format says so in the stream's format.
    assert.equal(dsdNative('hw:CARD=D90,DEV=0', 'DSD native (DSD_U32_BE, 22.5792 MHz)'), true);
    assert.equal(dsdNative('hw:CARD=D90,DEV=0', 'int32 (24-in-32)'), false);
    assert.equal(dsdWayLine('hw:CARD=D90,DEV=0', 'DSD native (DSD_U32_BE, 22.5792 MHz)'), 'ALSA native');
    assert.equal(dsdWayLine('hw:CARD=D90,DEV=0', 'int32'), 'DoP · exclusive');
    assert.equal(dsdWayTag('asio:RME', ''), 'ASIO');
    assert.equal(dsdWayTag('hw:CARD=D90,DEV=0', 'DSD native (DSD_U8, 11.2896 MHz)'), 'native');
    assert.equal(dsdWayTag('hw:CARD=D90,DEV=0', 'int32'), 'DoP');
});

test('the DSD output a status plays: its rate, family and label; none for a file as it is', () => {
    assert.deepEqual(dsdPlaying({ label: 'DSD256', rateHz: 11289600 }), { label: 'DSD256', multiple: 256, family: 44100 });
    assert.deepEqual(dsdPlaying({ label: 'DSD128 (48k)', rateHz: 6144000 }), { label: 'DSD128 (48k)', multiple: 128, family: 48000 });
    assert.deepEqual(dsdPlaying({ label: 'DSD512', rateHz: 22579200 }), { label: 'DSD512', multiple: 512, family: 44100 });
    assert.equal(dsdPlaying({ label: 'DSD64', rateHz: 2822400, asIs: true }), null);
    assert.equal(dsdPlaying(null), null);
});

// ── The panel's model ──────────────────────────────────────────────────────

// Every realtime kind at every rate, as an ASIO device lists them.
const prow = (multiple, state, load) => ({ base: 44100, multiple, state, load, reason: `about ${Math.round(load * 100)} % of a core per channel` });
const pkind = (kind, label, rank, f) => ({ kind, label, rank, rates: [64, 128, 256, 512, 1024].map(f).filter(Boolean) });
const PCAPS = {
    kinds: [
        pkind('sdm5', 'SDM5', 11, (m) => m <= 256 && prow(m, 'ok', 0.03)),
        pkind('sdm5La', 'SDM5 LA', 10, (m) => prow(m, 'ok', 0.15)),
        pkind('sdm7', 'SDM7', 8, (m) => prow(m, 'ok', 0.04)),
        pkind('sdm7Wide', 'SDM7 Wide', 9, (m) => m >= 128 && prow(m, 'ok', 0.04)),
        pkind('sdm7La', 'SDM7 LA', 6, (m) => prow(m, m >= 512 ? 'heavy' : 'ok', 0.19)),
        pkind('sdm7WideLa', 'SDM7 Wide LA', 7, (m) => m >= 128 && prow(m, 'ok', 0.19)),
        pkind('sdm7LaPlus', 'SDM7 LA+', 5, (m) => prow(m, m >= 256 ? 'heavy' : 'ok', 0.6)),
        pkind('sdm9La', 'SDM9 LA', 2, (m) => m >= 512 && prow(m, 'tooHeavy', 0.95)),
    ],
};
const DOP = buildOutFmtList(PCAPS, 44100, 'pcm', true, false);
const ASIO = buildOutFmtList(PCAPS, 44100, 'pcm', true, true);

test('the panel: the rates a device cannot take are locked, the modulators run from the lightest to the best, Wide is a switch', () => {
    const m = outFmtPanelModel(DOP, '256:sdm7WideLa');
    assert.equal(m.pcm, false);
    assert.deepEqual(m.rates.map((r) => `${r.multiple}${r.locked ? ' locked' : ''}`), ['64', '128', '256', '512 locked', '1024 locked']);
    assert.deepEqual(outFmtPanelModel(ASIO, '256:sdm5').rates.filter((r) => r.locked), [], 'an ASIO driver takes them all');
    assert.deepEqual(m.mods.map((x) => x.label), ['SDM5', 'SDM5 LA', 'SDM7', 'SDM7 LA', 'SDM7 LA+']);
    assert.deepEqual(m.mods.map((x) => x.wide?.label || ''), ['', '', 'SDM7 Wide', 'SDM7 Wide LA', '']);
    assert.deepEqual([m.multiple, m.index, m.wide, m.chosen.value], [256, 3, { can: true, on: true }, '256:sdm7WideLa']);
    // Wide is from DSD128 up, and not every modulator has it.
    assert.deepEqual(outFmtPanelModel(DOP, '64:sdm7La').wide, { can: false, on: false });
    assert.deepEqual(outFmtPanelModel(DOP, '256:sdm7LaPlus').wide, { can: false, on: false });
    assert.equal(plainKind('sdm7WideLa'), 'sdm7La');
    assert.equal(plainKind('sdm5Wide'), 'sdm5');
    assert.ok(isWideKind('sdm7Wide') && !isWideKind('sdm7La'));
});

test('the panel: Auto is the best that runs freely, Max the best this PC runs at all, Light the lightest; anything else is manual', () => {
    const m = outFmtPanelModel(DOP, '256:sdm7La');
    assert.deepEqual(m.presets, { auto: '256:sdm7La', max: '256:sdm7LaPlus', light: '256:sdm5' });
    assert.equal(m.preset, 'auto');
    assert.equal(outFmtPanelModel(DOP, '256:sdm7LaPlus').preset, 'max');
    assert.equal(outFmtPanelModel(DOP, '256:sdm5').preset, 'light');
    assert.equal(outFmtPanelModel(DOP, '256:sdm7').preset, 'manual');
    assert.equal(outFmtPanelModel(DOP, '256:sdm7WideLa').preset, 'manual');
    // Where everything runs freely Auto and Max are one choice: Auto is lit.
    const low = outFmtPanelModel(DOP, '64:sdm7LaPlus');
    assert.deepEqual([low.presets.auto, low.presets.max, low.preset], ['64:sdm7LaPlus', '64:sdm7LaPlus', 'auto']);
    // Nothing free or heavy above the too-heavy one: never the too-heavy kind.
    assert.deepEqual(outFmtPanelModel(ASIO, '512:sdm7').presets, { auto: '512:sdm7', max: '512:sdm7LaPlus', light: '512:sdm5La' });
});

test('the panel on PCM: the DSD side shows the choice before PCM, or the best free one at DSD256', () => {
    const back = outFmtPanelModel(DOP, 'pcm', '128:sdm5La');
    assert.deepEqual([back.pcm, back.multiple, back.chosen.value], [true, 128, '128:sdm5La']);
    const first = outFmtPanelModel(DOP, 'pcm');
    assert.deepEqual([first.pcm, first.multiple, first.chosen.value], [true, 256, '256:sdm7La']);
    // A native rate kept from an ASIO device, on a device without it.
    assert.equal(outFmtPanelModel(DOP, 'pcm', '1024:sdm7').multiple, 256);
    // Before the capabilities: SDM5 at every rate, never empty.
    const bare = outFmtPanelModel(buildOutFmtList(null, 44100, 'pcm'), '128:sdm5');
    assert.deepEqual([bare.mods.map((x) => x.label), bare.chosen.value, bare.recheck], [['SDM5'], '128:sdm5', null]);
    assert.equal(outFmtPanelModel(DOP, 'pcm').recheck.short, 'Re-check this PC…');
});

test('the panel: a new rate keeps the modulator (and Wide) where that rate has it, else takes the best free one; a locked rate gives nothing', () => {
    assert.equal(panelValueAtRate(DOP, '256:sdm7La', 128), '128:sdm7La');
    assert.equal(panelValueAtRate(DOP, '256:sdm7WideLa', 128), '128:sdm7WideLa');
    assert.equal(panelValueAtRate(DOP, '256:sdm7WideLa', 64), '64:sdm7La', 'no Wide at DSD64');
    assert.equal(panelValueAtRate(ASIO, '256:sdm5', 512), '512:sdm7', 'SDM5 stops at DSD256');
    assert.equal(panelValueAtRate(DOP, '256:sdm7La', 512), '');
});

test('the panel\'s map: every modulator at every rate, the best first; its meters and hint', () => {
    const m = outFmtPanelModel(ASIO, '512:sdm9La');
    assert.deepEqual(m.map.map((r) => `${r.label}: ${r.cells.map((c) => (c ? c.state[0] : '.')).join('')}`), [
        'SDM9 LA: ...tt', 'SDM7 LA+: oohhh', 'SDM7 LA: ooohh', 'SDM7 Wide LA: .oooo', 'SDM7: ooooo', 'SDM7 Wide: .oooo', 'SDM5 LA: ooooo', 'SDM5: ooo..',
    ]);
    assert.deepEqual(outFmtPanelModel(DOP, '256:sdm5').map[1].cells.slice(3), [null, null], 'a locked rate has no cells');
    assert.equal(chainLine(64, 44100), 'FS4 · 176.4 kHz');
    assert.equal(chainLine(128, 48000), 'FS8 · 384 kHz');
    assert.equal(chainLine(1024, 44100), 'FS16 · 705.6 kHz');
    assert.equal(loadLine(m.chosen), '95 % of a core per channel');
    assert.equal(loadLine({ state: 'ok', load: null }), 'not measured yet');
    assert.match(panelHint(m.chosen), /^Too heavy for this PC\. It is tried anyway/);
    assert.match(panelHint({ state: 'heavy' }), /^Heavy: it plays/);
    assert.equal(panelHint({ state: 'ok' }), 'Runs freely on this PC.');
});

test('outfmt-panel.js: a slider previews while dragged and picks when let go; the panel stays open after a pick', () => {
    const src = readFileSync(new URL('../src/js/outfmt-panel.js', import.meta.url), 'utf8');
    assert.match(src, /el\.rate\.addEventListener\('input', \(\) => \{ pending = /);
    assert.match(src, /el\.rate\.addEventListener\('change', \(\) => pick\(/);
    assert.match(src, /el\.mod\.addEventListener\('change', \(\) => pick\(/);
    assert.doesNotMatch(src.slice(src.indexOf('function pick(v)'), src.indexOf('function rateValue')), /close\(\)/);
    const css = readFileSync(new URL('../src/css/player.css', import.meta.url), 'utf8');
    assert.match(css, /\.pick-list\.ofp \{ width: 348px; max-height: calc\(100vh - 12px\);/);
    assert.match(css, /\.ofp-scroll \{ flex: 1 1 auto; min-height: 0; overflow-y: auto;/, 'the middle scrolls, the tabs and the foot stay');
    assert.match(src, /panel\.style\.maxHeight = `\$\{vh - 12\}px`;/, 'never taller than the window');
});

test('the panel: a rate the listener says the DAC does not play is locked for that device, with everything above it', () => {
    // A driver's canSampleRate cannot be trusted (backend_asio.rs): the listener's word is kept per device.
    const m = outFmtPanelModel(ASIO, '256:sdm7La', '', 256);
    assert.deepEqual(m.rates.map((r) => `${r.multiple}${r.why ? ` ${r.why}` : ''}`), ['64', '128', '256', '512 dac', '1024 dac']);
    assert.deepEqual(m.map[0].cells.slice(3), [null, null], 'no cells in the map either');
    assert.deepEqual(outFmtPanelModel(DOP, '256:sdm7La', '', 128).rates.map((r) => r.why), ['', '', 'dac', 'asio', 'asio']);
    assert.equal(outFmtPanelModel(ASIO, 'pcm', '1024:sdm7', 256).multiple, 256, 'the DSD tab never goes back to a locked rate');
    let raw = dacMaxSet('', 'asio:Chord', 256);
    raw = dacMaxSet(raw, 'asio:Other', 512);
    assert.deepEqual([dacMaxOf(raw, 'asio:Chord'), dacMaxOf(raw, 'asio:Other'), dacMaxOf(raw, 'wasapi:x'), dacMaxOf('junk', 'a')], [256, 512, 0, 0]);
    assert.equal(dacMaxOf(dacMaxSet(raw, 'asio:Chord', 0), 'asio:Chord'), 0, 'the word taken back');
    assert.equal(dacMaxOf(dacMaxSet(raw, 'asio:Chord', 1024), 'asio:Chord'), 1024, 'the top rate: it plays them all, kept over what is known of the DAC');
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(src, /dacMax: said \|\| known\?\.max \|\| 0, dacName: said \? '' : known\?\.name \|\| '',\s+dacSaid: said, dacKnown: known,/, 'the listener\'s word first, then the DAC known by its name');
});

test('the map lists the modulators by the catalog\'s rank, whatever rates each has', () => {
    // SDM7 LA+ only to DSD256 on this PC: still above SDM7 LA.
    const caps = { kinds: PCAPS.kinds.map((k) => (k.kind === 'sdm7LaPlus' ? { ...k, rates: k.rates.filter((r) => r.multiple <= 256) } : k)) };
    const m = outFmtPanelModel(buildOutFmtList(caps, 44100, 'pcm', true, true), '256:sdm5');
    assert.deepEqual(m.map.map((r) => r.label).slice(0, 3), ['SDM9 LA', 'SDM7 LA+', 'SDM7 LA']);
});

test('a known DAC: told by name, Chord\'s one driver by the other outputs Windows lists; its top rate locks the ones above', () => {
    const names = ['System default', 'Цифровой выход (Mojo2)', 'Speakers (Realtek)', 'ASIO: ASIO Chord 44.1Khz 768Khz'];
    assert.deepEqual(knownDac('asio:ASIO Chord 44.1Khz 768Khz', names), { name: 'Chord Mojo 2', max: 256 });
    assert.deepEqual(knownDac('asio:ASIO Chord 44.1Khz 768Khz', ['Digital Output (Hugo 2)']), { name: 'Chord Hugo 2', max: 512 });
    assert.equal(knownDac('asio:ASIO Chord 44.1Khz 768Khz', ['Speakers']), null, 'which Chord DAC is not known');
    assert.equal(knownDac('asio:RME Fireface', names), null, 'another maker\'s driver is not the Mojo');
    assert.equal(knownDac('{0.0.0.00000000}.{mojo2}', names), null, 'only an ASIO driver offers the native rates');
    const css = readFileSync(new URL('../src/css/player.css', import.meta.url), 'utf8');
    assert.match(css, /\.pick-list::backdrop \{ background: rgba\(2, 6, 23, 0\.6\); \}/, 'the app dims under the open panel');
});

test('the DAC database: a model by the driver\'s own name, or by the maker\'s driver and another output\'s name; the more exact first; two models are none', () => {
    const chord = 'asio:ASIO Chord 44.1Khz 768Khz';
    assert.deepEqual(knownDac('asio:RME ADI-2 DAC', []), { name: 'RME ADI-2 DAC FS', max: 256 }, 'the driver\'s own name');
    assert.deepEqual(knownDac('asio:TOPPING USB Audio', ['Speakers (Topping E70 Velvet)']), { name: 'Topping E70 Velvet', max: 512 }, 'maker + another output');
    assert.equal(knownDac('asio:Realtek ASIO', ['Speakers (Topping E70 Velvet)']), null, 'another maker\'s driver: the output is not its own');
    assert.deepEqual(knownDac(chord, ['Speakers (Mojo 2)']), { name: 'Chord Mojo 2', max: 256 });
    assert.deepEqual(knownDac(chord, ['Speakers (Mojo)']), { name: 'Chord Mojo', max: 256 }, 'Mojo 2 first, the plain Mojo still told');
    assert.deepEqual(knownDac(chord, ['Speakers (Hugo TT 2)']), { name: 'Chord Hugo TT 2', max: 512 });
    assert.equal(knownDac(chord, ['Speakers (Mojo2)', 'Speakers (Hugo 2)']), null, 'two models: unknown, not wrong');
    assert.deepEqual(knownDac(chord, ['Speakers (Mojo2)', 'Line (Mojo2)']), { name: 'Chord Mojo 2', max: 256 }, 'one model in two outputs');
    assert.equal(knownDac(chord, ['ASIO: Holo Spring 3']), null, 'another ASIO driver is no output of this one');
    assert.equal(knownDac('wasapi:Topping E70 Velvet', ['Speakers (Topping E70 Velvet)']), null, 'only an ASIO driver');
    // A shorter model that a more exact one contains, first in the list: the one that fits more counts.
    const db = [
        { maker: /m/i, model: /mojo/i, name: 'Mojo', max: 256 },
        { maker: /m/i, model: /mojo\s*2/i, name: 'Mojo 2', max: 256 },
    ];
    assert.equal(knownDac('asio:m', ['Mojo 2'], db).name, 'Mojo 2', 'the model that fits more of the name');
    assert.equal(knownDac('asio:Topping USB DAC', []), null, 'a driver\'s generic words name no model');
    for (const d of KNOWN_DACS) {
        assert.ok(d.maker instanceof RegExp && d.model instanceof RegExp, d.name);
        assert.ok([64, 128, 256, 512, 1024].includes(d.max), d.name);
        assert.ok(d.name, 'a name');
    }
    assert.ok(KNOWN_DACS.length > 300, 'hundreds of DACs');
    for (const d of DAC_DATA) assert.match(d.s, /^https?:\/\//, `${d.n}: the page its rate was read on`);
    assert.equal(new Set(DAC_DATA.map((d) => d.n)).size, DAC_DATA.length, 'one entry per name');
});

test('outfmt-panel.js: "Your DAC" is one search field, Auto first; it is not rebuilt while in use', () => {
    const src = readFileSync(new URL('../src/js/outfmt-panel.js', import.meta.url), 'utf8');
    assert.match(src, /<input type="text" data-el="dacIn" role="combobox"/);
    assert.match(src, /`Auto · \$\{a\.name\}\$\{a\.by === 'usb' \? ' \(USB\)' : ''\}: DSD\$\{a\.max\}`/);
    assert.match(src, /`Auto · \$\{ctx\.dacMaker\}, model not named: choose it`/);
    assert.match(src, /'Auto · not in the list: every rate'/);
    assert.match(src, /if \(doc\.activeElement === inp\) return;/);
    assert.doesNotMatch(src, /noSound|dacLine/, 'the old link and line are gone');
    const css = readFileSync(new URL('../src/css/player.css', import.meta.url), 'utf8');
    assert.match(css, /\.ofp-dacres \{[^}]*overflow-y: auto;/, 'the results scroll');
});

test('the set pressed stays lit while it gives what is chosen: Max over Auto when both give one modulator', () => {
    const m = { chosen: { value: '256:sdm7La' }, presets: { auto: '256:sdm7La', max: '256:sdm7La', light: '256:sdm5' }, preset: 'auto' };
    assert.equal(shownPreset(m, 'max'), 'max');
    assert.equal(shownPreset(m, 'auto'), 'auto');
    assert.equal(shownPreset(m, ''), 'auto', 'nothing pressed: the first set that gives it');
    assert.equal(shownPreset(m, 'light'), 'auto', 'a set that no longer gives it is not lit');
    assert.equal(shownPreset({ chosen: null, presets: {}, preset: 'manual' }, 'max'), 'manual');
    const src = readFileSync(new URL('../src/js/outfmt-panel.js', import.meta.url), 'utf8');
    assert.match(src, /pressedPreset = t\.dataset\.preset;/, 'a press on a set is remembered');
});

// Anton's log, 20.11 04:16: Windows names the Mojo 2's output "Цифровой
// выход (Chord Async USB 44.1kHz-768kHz)": Chord's driver serves every
// Chord, so the model is not found, the maker is, and the model is chosen
// from the list (kept by device).
test('Chord by its own driver: the maker is known, the model is chosen from the list of DACs', () => {
    const id = 'asio:ASIO Chord 44.1Khz 768Khz';
    const names = ['Наушники (Realtek USB2.0 Audio)', 'Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)',
        'Цифровой выход (Chord Async USB 44.1kHz-768kHz)', '38GL950G (NVIDIA High Definition Audio)',
        'ASIO: ASIO Chord 44.1Khz 768Khz', 'ASIO: ASIO4ALL v2'];
    assert.equal(knownDac(id, names), null, 'no model in any name');
    assert.equal(dacMakerOf(id), 'Chord');
    assert.equal(dacMakerOf('asio:ASIO4ALL v2'), '');
    assert.equal(dacMakerOf('{0.0.0.00000000}.{x}'), '', 'not an ASIO driver');
    const list = dacChoices('Chord');
    assert.equal(list[0].maker, 'Chord', "the driver's maker first");
    assert.ok(list[0].models.some((d) => d.name === 'Chord Mojo 2' && d.max === 256));
    assert.equal(list.reduce((n, g) => n + g.models.length, 0), KNOWN_DACS.length, 'every DAC of the list');
    assert.deepEqual(dacByName('Chord Mojo 2'), { name: 'Chord Mojo 2', max: 256 });
    assert.equal(dacByName('Nope'), null);
    let raw = dacModelSet('', id, 'Chord Mojo 2');
    assert.equal(dacModelOf(raw, id), 'Chord Mojo 2');
    assert.equal(dacModelOf(raw, 'asio:other'), '');
    raw = dacModelSet(raw, id, '');
    assert.equal(dacModelOf(raw, id), '', 'Auto takes it back');
    assert.equal(dacModelOf('not json', id), '');
    const src = readFileSync(new URL('../src/js/player.js', import.meta.url), 'utf8');
    assert.match(src, /const known = picked \|\| found;/, 'the DAC chosen is over the one found');
});

test('the DAC search finds as the EQ window does: every word, the name that starts with it first; nothing typed: the own maker first', () => {
    assert.equal(searchDacs('mojo 2')[0].name, 'Chord Mojo 2');
    assert.equal(searchDacs('MOJO')[0].maker, 'Chord');
    assert.ok(searchDacs('mojo').every((d) => /mojo/i.test(d.name)));
    assert.deepEqual(searchDacs('d90 sabre').map((d) => d.name), ['Topping D90 III Sabre']);
    assert.deepEqual(searchDacs('no such dac'), []);
    const all = searchDacs('', { maker: 'Chord' });
    assert.equal(all.length, KNOWN_DACS.length);
    assert.ok(all.slice(0, 6).every((d) => d.maker === 'Chord'), 'the driver\'s maker first');
    assert.equal(searchDacs('', { limit: 3 }).length, 3);
});

test('a DAC told by its USB product string: Chord\'s one driver and one USB id, the model in the product string', () => {
    const chord = 'asio:ASIO Chord 44.1Khz 768Khz';
    const usb = [{ usb: '0b05:1a53', product: 'USB Audio' }, { usb: '1686:03f5', product: 'H1n' }, { usb: '245f:0815', product: 'Mojo2' }];
    assert.deepEqual(knownDac(chord, ['Speakers (Chord Async USB 44.1kHz-768kHz)'], undefined, usb), { name: 'Chord Mojo 2', max: 256, by: 'usb' });
    assert.deepEqual(knownDac(chord, [], undefined, [{ usb: '245f:0815', product: 'Hugo2' }]), { name: 'Chord Hugo 2', max: 512, by: 'usb' });
    assert.equal(knownDac(chord, [], undefined, [{ usb: '245f:0815', product: '' }]), null, 'the shared id alone tells no model');
    assert.equal(knownDac('asio:RME Fireface', [], undefined, usb), null, 'another maker\'s driver is not the Mojo');
    assert.equal(knownDac('{0.0.0.00000000}.{x}', [], undefined, usb), null, 'only an ASIO driver');
    assert.deepEqual(usbDacs([{ usb: '245f:0815', product: 'Qutest' }]).map((d) => d.name), ['Chord Qutest']);
    assert.deepEqual(usbDacs([{ usb: '0b05:1a53', product: 'USB Audio' }]), [], 'a device the list does not know');
});

test('a device row: no "ASIO:", the rate range short, the maker before "ASIO"', () => {
    assert.equal(tidyDeviceName('ASIO: ASIO Chord 44.1Khz 768Khz'), 'Chord ASIO 44.1–768 kHz');
    assert.equal(tidyDeviceName('Цифровой выход (Chord Async USB 44.1KHz-768KHz)'), 'Цифровой выход (Chord Async USB 44.1–768 kHz)');
    assert.equal(tidyDeviceName('Динамики (Realtek USB2.0 Audio)'), 'Динамики (Realtek USB2.0 Audio)');
});

test('the DAC behind a device: USB for a driver or endpoint of its maker, never for another maker\'s endpoint', () => {
    const usb = [{ usb: '245f:0815', product: 'Mojo2' }];
    assert.equal(deviceDac('asio:ASIO Chord 44.1Khz 768Khz', 'ASIO: ASIO Chord 44.1Khz 768Khz', [], usb)?.name, 'Chord Mojo 2');
    assert.equal(deviceDac('{w1}', 'Цифровой выход (Chord Async USB 44.1KHz-768KHz)', [], usb)?.name, 'Chord Mojo 2');
    assert.equal(deviceDac('{w2}', 'Динамики (Realtek USB2.0 Audio)', [], usb), null);
    assert.equal(deviceDac('{w3}', 'Динамики (Mojo 2)', [], [])?.name, 'Chord Mojo 2');
});
