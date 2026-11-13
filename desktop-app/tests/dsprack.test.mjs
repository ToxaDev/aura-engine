// The subsonic corner the UI sends, and the corners the backend accepts.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
//
// Two lists name the corners: SUBSONIC_STEPS here and SUBSONIC_CORNERS_HZ in
// apodize.rs. The command refuses a corner that is not on its list, so a
// step added on one side only would be a batch that fails before its first
// file. The first case reads both and holds them together.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { SUBSONIC_STEPS, subsonicCornerHz } from '../src/js/dsprack.js';

test('the UI offers exactly the corners the backend accepts', () => {
    const rs = readFileSync(
        new URL('../../crates/aura-core/src/audio/converter/apodize.rs', import.meta.url), 'utf8');
    const m = rs.match(/pub const SUBSONIC_CORNERS_HZ:\s*\[u32;\s*\d+\]\s*=\s*\[([^\]]*)\]/);
    assert.ok(m, 'SUBSONIC_CORNERS_HZ not found in apodize.rs');
    const backend = m[1].split(',').map(s => parseInt(s, 10)).filter(Number.isFinite);
    assert.deepEqual([...SUBSONIC_STEPS].sort((a, b) => a - b), backend.sort((a, b) => a - b));
});

/// What a fresh install opens on, read from the markup rather than repeated
/// here: the hidden input is the only place that decides it.
const MARKUP_CORNER = Number(
    readFileSync(new URL('../src/components/converter.html', import.meta.url), 'utf8')
        .match(/id="convSubsonicHz"\s+value="(\d+)"/)?.[1]);

test('the corner a fresh install opens on is one the backend accepts', () => {
    assert.ok(Number.isFinite(MARKUP_CORNER), 'converter.html has no convSubsonicHz default');
    assert.ok(SUBSONIC_STEPS.includes(MARKUP_CORNER),
        `the markup opens on ${MARKUP_CORNER} Hz, which is not one of the steps`);
    page({ on: true, hz: String(MARKUP_CORNER) });
    assert.equal(subsonicCornerHz(), MARKUP_CORNER);
});

/// Just enough of a document for the two inputs the corner is read from.
function page({ on, hz }) {
    const els = {
        convSubsonic: { checked: on },
        convSubsonicHz: hz === undefined ? undefined : { value: hz },
    };
    globalThis.document = { getElementById: id => els[id] ?? null };
}

test('off sends 0 whatever the chip says', () => {
    page({ on: false, hz: '15' });
    assert.equal(subsonicCornerHz(), 0);
});

test('on sends the corner on the chip', () => {
    for (const hz of SUBSONIC_STEPS) {
        page({ on: true, hz: String(hz) });
        assert.equal(subsonicCornerHz(), hz);
    }
});

test('a corner the backend would refuse reads as the default', () => {
    for (const hz of ['12', '0', '', 'abc', '25']) {
        page({ on: true, hz });
        assert.equal(subsonicCornerHz(), MARKUP_CORNER, `value ${JSON.stringify(hz)}`);
    }
    page({ on: true, hz: undefined });
    assert.equal(subsonicCornerHz(), MARKUP_CORNER, 'missing input');
});

// ── The rack's own shape ──────────────────────────────────────────────
//
// The zones, the stage list, the hidden inputs behind the badges and the
// hues are four lists that have to agree, and they live in four files. Each
// case below holds one pair of them together, so a stage added to one and
// forgotten in another fails here rather than rendering as a dead row.

import { DSP_CHAIN, DSP_ZONES, FEAT_BY_ID } from '../src/js/dsprack.js';

const html = readFileSync(new URL('../src/components/converter.html', import.meta.url), 'utf8');
const css = readFileSync(new URL('../src/css/dsprack.css', import.meta.url), 'utf8');

test('the zones cover the chain exactly, in order', () => {
    const n = DSP_ZONES.reduce((a, z) => a + z.n, 0);
    assert.equal(n, DSP_CHAIN.length,
        'zone sizes must add up to the chain, or the last stages draw nowhere');
});

test('every stage in the chain has an entry, and every entry is in the chain', () => {
    assert.deepEqual([...DSP_CHAIN].sort(), Object.keys(FEAT_BY_ID).sort());
});

test('every dependency and conflict names a stage that exists', () => {
    for (const [id, f] of Object.entries(FEAT_BY_ID)) {
        if (f.needs) assert.ok(FEAT_BY_ID[f.needs], `${id} needs unknown stage ${f.needs}`);
        if (f.conflicts) assert.ok(FEAT_BY_ID[f.conflicts], `${id} conflicts with unknown ${f.conflicts}`);
    }
});

test('a conflict is symmetric in effect, so neither side can be declared twice', () => {
    const pairs = Object.values(FEAT_BY_ID).filter(f => f.conflicts)
        .map(f => [f.id, f.conflicts].sort().join('|'));
    assert.equal(new Set(pairs).size, pairs.length, 'the same pair is declared from both ends');
});

test('nothing depends on a stage that also conflicts with it', () => {
    for (const f of Object.values(FEAT_BY_ID)) {
        if (f.needs && f.conflicts === f.needs) {
            assert.fail(`${f.id} both needs and rules out ${f.needs} — it could never come up`);
        }
    }
});

test('every badge has the hidden input it switches', () => {
    for (const id of DSP_CHAIN) {
        assert.ok(html.includes(`id="${id}"`), `converter.html has no input #${id}`);
    }
});

test('the stage carrying a number has its hidden input too', () => {
    for (const f of Object.values(FEAT_BY_ID)) {
        if (!f.amount) continue;
        assert.ok(html.includes(`id="${f.amount.input}"`),
            `converter.html has no input #${f.amount.input} for ${f.tok}`);
    }
});

test('Adaptive Headroom leans on a control that exists, and on its real Off value', () => {
    const f = FEAT_BY_ID.convLabHeadroom;
    assert.ok(f.needsSelect, 'AHR must declare what it depends on');
    const { id, off } = f.needsSelect;
    // prepare.rs runs the whole AHR block inside `if settings.headroom_db < 0.0`,
    // so the value the rack treats as "nothing to decide" has to be the same
    // value the Headroom control calls Off.
    const sel = html.split(`id="${id}"`)[1]?.split('</select>')[0];
    assert.ok(sel, `converter.html has no <select id="${id}">`);
    const offOpt = sel.match(/<option value="([^"]+)"[^>]*>\s*Off/i);
    assert.ok(offOpt, `#${id} has no option labelled Off`);
    assert.equal(off, offOpt[1],
        `AHR treats "${off}" as Off but the control's Off is "${offOpt[1]}"`);
});

test('every stage hue is a colour the stylesheet defines', () => {
    for (const f of Object.values(FEAT_BY_ID)) {
        assert.ok(css.includes(`--lab-${f.hue}:`), `dsprack.css defines no --lab-${f.hue} (${f.tok})`);
    }
});

test('tokens are unique — the queue row looks stages up by token', () => {
    const toks = Object.values(FEAT_BY_ID).map(f => f.tok);
    assert.equal(new Set(toks).size, toks.length);
});

// ── What a fresh install opens on ─────────────────────────────────────
//
// The defaults are a product decision, not an accident of markup order, so
// they are written down here too. A stage added to the rack without a
// deliberate choice about its default fails this.

test('the rack opens on exactly the stages the release ships on', () => {
    // Named rather than derived, so adding a stage without deciding its default
    // fails here. Everything in the chain that is not on this list must open
    // off — which also keeps this honest in a build that carries extra stages.
    // Album Level on: decided with the stage (Discussions #5), because an album
    // converted track by track comes out with its balance changed.
    const on = new Set(['convLabDeclip', 'convLabIsp', 'convSubsonic', 'convLabHeadroom',
                        'convAdaptiveApodizer', 'convFirResampling', 'convLabTfs',
                        'convAlbumLevel']);
    for (const id of on) {
        assert.ok(DSP_CHAIN.includes(id), `${id} is listed as a default but is not in the chain`);
    }
    for (const id of DSP_CHAIN) {
        const tag = html.match(new RegExp(`<input[^>]*id="${id}"[^>]*>`));
        assert.ok(tag, `converter.html has no input #${id}`);
        const checked = / checked\b/.test(tag[0]);
        assert.equal(checked, on.has(id),
            `#${id} should open ${on.has(id) ? 'ON' : 'OFF'}`);
    }
});

test('Adaptive Headroom ships on, so Headroom must not ship on Off', () => {
    // AHR only runs inside `if settings.headroom_db < 0.0`. Shipping it on
    // with Headroom on Off would light a badge that can never do anything,
    // and the rack would switch it straight back off at boot.
    const ahrOn = / checked\b/.test(html.match(/<input[^>]*id="convLabHeadroom"[^>]*>/)[0]);
    const sel = html.split('id="convHeadroom"')[1].split('</select>')[0];
    const selected = sel.match(/<option value="([^"]+)"[^>]*\bselected\b/);
    assert.ok(selected, 'convHeadroom has no selected option');
    if (ahrOn) {
        assert.notEqual(selected[1], FEAT_BY_ID.convLabHeadroom.needsSelect.off,
            'AHR ships on but Headroom ships Off — AHR would never run');
    }
});

// ── EQ in the rack ──────────────────────────────────────────────────────

test('convEq is in the Output zone, after convLabXtc and before convAlbumLevel', () => {
    const xtcIdx = DSP_CHAIN.indexOf('convLabXtc');
    const eqIdx  = DSP_CHAIN.indexOf('convEq');
    const albIdx = DSP_CHAIN.indexOf('convAlbumLevel');
    assert.ok(eqIdx > xtcIdx, 'convEq must come after convLabXtc in the chain');
    assert.ok(eqIdx < albIdx, 'convEq must come before convAlbumLevel');
    // Verify zone: Output is zone 2 (0-indexed). Its stages start after the
    // sum of the first two zones' n values.
    const outputStart = DSP_ZONES.slice(0, 2).reduce((a, z) => a + z.n, 0);
    assert.ok(eqIdx >= outputStart, 'convEq is in the Output zone');
});

test('Output zone has n=4 covering CUE, XTC, EQ and Album Level', () => {
    const outputZone = DSP_ZONES[DSP_ZONES.length - 1];
    assert.equal(outputZone.title, 'Output');
    assert.equal(outputZone.n, 4);
});

test('convEq has token EQ and hue eq', () => {
    const f = FEAT_BY_ID.convEq;
    assert.ok(f, 'convEq must have a FEAT entry');
    assert.equal(f.tok, 'EQ');
    assert.equal(f.hue, 'eq');
    assert.ok(f.setup, 'setup:true so the SET key opens the EQ window');
});

test('convEq hidden input ships unchecked (off by default)', () => {
    const tag = html.match(/<input[^>]*id="convEq"[^>]*>/);
    assert.ok(tag, 'converter.html has no input #convEq');
    assert.ok(!/ checked\b/.test(tag[0]), 'convEq must ship unchecked — off means bit-identical');
});

test('--lab-eq colour is defined in dsprack.css', () => {
    assert.ok(css.includes('--lab-eq:'), 'dsprack.css must define --lab-eq');
});

// ── HP's one lamp: the switch while HP switches, α while α is on. The
// brightness is the meter's value itself, set as it comes — nothing eases it
// on the way, in the code or in the CSS.
test('HP\'s lamp shows the value it is given: the switch, or α', async () => {
    const { setPhaseLamps, lampLevel, lampIdleOf, PHASE_LAMP_IDLE } = await import('../src/js/dsprack.js');
    const props = {}, cls = new Set();
    const lamp = { style: { getPropertyValue: k => props[k] ?? '', setProperty: (k, v) => { props[k] = v; } }, props, cls,
                   classList: { toggle: (c, on) => { if (on) cls.add(c); else cls.delete(c); } } };
    const alpha = { checked: false };
    globalThis.document = {
        querySelector: sel => (sel.includes('data-id="convHybridPhase"') ? lamp : null),
        getElementById: id => (id === 'convLabAlpha' ? alpha : null),
    };
    setPhaseLamps(1, 0.37);
    assert.equal(props['--lamp'], '1.00', 'HP: the switch');
    assert.match(lamp.title, /minimum-phase path across an attack/);
    alpha.checked = true;
    setPhaseLamps(1, 0.37);
    assert.equal(props['--lamp'], '0.37', 'α on: α');
    assert.match(lamp.title, /Glows with α/);
    setPhaseLamps(0, 0);
    assert.equal(props['--lamp'], '0.00', 'dark is dark: no base');
    alpha.checked = false;
    // A converted file made without HP blends nothing: dark, and the lamp
    // says why; back to its own tooltip once a blend plays again.
    setPhaseLamps(0, 0, 'file');
    assert.match(lamp.title, /converted file/);
    assert.ok(cls.has('idle'), 'the socket shows it cannot light');
    setPhaseLamps(0.5, 0);
    assert.match(lamp.title, /minimum-phase path/);
    assert.equal(cls.has('idle'), false);
    setPhaseLamps(0, 0, lampIdleOf(2, true));
    assert.match(lamp.title, /being worked out/);
    setPhaseLamps(0, 0, lampIdleOf(4, true));
    assert.match(lamp.title, /free memory/);
    setPhaseLamps(0, 0, lampIdleOf(3, true));
    assert.match(lamp.title, /source is not there/);
    for (let st = 2; st <= 6; st++) assert.ok(PHASE_LAMP_IDLE[lampIdleOf(st, true)], `state ${st} has words`);
    assert.equal(lampIdleOf(0, false), null, 'a live blend');
    assert.equal(lampIdleOf(0, true), 'file', 'a file made without HP');
    setPhaseLamps(1, 0, lampIdleOf(1, true));
    assert.equal(cls.has('idle'), false);
    assert.equal(lampLevel(NaN), '0.00');
    assert.equal(lampLevel(2), '1.00');
    assert.equal(lampLevel(-1), '0.00');

    const css = readFileSync(new URL('../src/css/dsprack.css', import.meta.url), 'utf8');
    // A square bezel, on or off (variant C, 29.10); the lens inside takes
    // its fill and glow from lampLens(), nothing smoothed in CSS.
    const bezel = css.match(/\.hpu \.lamp\s*\{([^}]*)\}/);
    assert.match(bezel[1], /width: 11px; height: 11px/);
    assert.match(bezel[1], /linear-gradient\(180deg, #05080f, #1b2537\)/);
    assert.doesNotMatch(bezel[1], /border-radius: 50%/, 'no longer round');
    const lens = css.match(/\.hpu \.lamp \.lens\s*\{([^}]*)\}/);
    assert.match(lens[1], /width: 7px; height: 7px/);
    assert.match(lens[1], /var\(--lens-bg/);
    assert.match(lens[1], /var\(--lens-sh/);
    // Its level is never smoothed; only the whole lamp fades out and back
    // while Hybrid-Phase prepares a track (HL47).
    assert.doesNotMatch(css, /\.lamp[^{]*\{[^}]*(transition(?!: opacity )|animation)/, 'the lamp is not smoothed in CSS');
    assert.doesNotMatch(css, /\.lens[^{]*\{[^}]*(transition|animation)/, 'the lens is not smoothed in CSS');
    assert.doesNotMatch(css, /rgba\(196, 181, 253, \.9\)/, 'the v12 ring is gone');
    assert.doesNotMatch(css, /\.lab-badge[^{,]*\.lamp/, 'only the HP unit has a lamp: other stages do not change');
    assert.match(css, /\.hpu\.on \.lamp\.idle \.lens\s*\{[^}]*1px dashed rgba\(148, 163, 184, \.5\)/,
        'a lamp that cannot light: a dashed outline in the socket');
});

test('HP\'s lens: dark violet → lilac at 0.6 → near white, a glow only once lit', async () => {
    const { lampLens, setPhaseLamps } = await import('../src/js/dsprack.js');
    assert.deepEqual(lampLens(0).c, [38, 32, 60]);
    assert.deepEqual(lampLens(0.6).c, [167, 139, 250]);
    assert.deepEqual(lampLens(1).c, [245, 243, 255]);
    assert.deepEqual(lampLens(0.3).c, [103, 86, 155], 'halfway to lilac');
    assert.deepEqual(lampLens(NaN).c, [38, 32, 60]);
    assert.deepEqual(lampLens(5).c, [245, 243, 255]);
    assert.doesNotMatch(lampLens(0).shadow, /167,139,250/, 'dark: no glow');
    assert.doesNotMatch(lampLens(0.03).shadow, /167,139,250/, 'the cut-off');
    assert.match(lampLens(0.04).shadow, /, 0 0 2\.24px 0\.05px rgba\(167,139,250,0\.048\)$/);
    assert.match(lampLens(1).shadow, /rgba\(255,255,255,0\.440\).*0 0 8\.00px 1\.20px rgba\(167,139,250,1\.000\)$/);
    assert.match(lampLens(0).bg, /^radial-gradient\(circle at 50% 35%, rgb\(114,110,128\) 0%, rgb\(38,32,60\) 60%, rgb\(25,21,39\) 100%\)$/,
        'the CSS fallback for an unlit lens is lampLens(0)');

    // The lens is repainted only when the level changes.
    const props = {}, lensProps = {}; let lensSets = 0;
    const lens = { style: { setProperty: (k, v) => { lensProps[k] = v; lensSets++; } } };
    const lamp = { style: { getPropertyValue: k => props[k] ?? '', setProperty: (k, v) => { props[k] = v; } },
                   classList: { toggle() {} }, querySelector: s => (s === '.lens' ? lens : null) };
    globalThis.document = {
        querySelector: sel => (sel.includes('data-id="convHybridPhase"') ? lamp : null),
        getElementById: () => ({ checked: false }),
    };
    setPhaseLamps(1, 0);
    assert.equal(lensProps['--lens-bg'], lampLens(1).bg);
    assert.equal(lensProps['--lens-sh'], lampLens(1).shadow);
    const n = lensSets;
    setPhaseLamps(1, 0);
    assert.equal(lensSets, n, 'same level, no repaint');
});

test('the sensitivity slider: seven whole-pixel steps, centres on the ticks', async () => {
    const { hpSensPlaces, hpSensAt, hpSensText, HP_SENS_THRESHOLDS } = await import('../src/js/dsprack.js');
    for (const w of [100, 101, 117, 123]) {
        for (let step = -3; step <= 3; step++) {
            const p = hpSensPlaces(w, step);
            assert.ok(Number.isInteger(p.thumb) && p.ticks.every(Number.isInteger), 'whole pixels');
            // tick 2 px from x−1, thumb 10 px from x−5: the same centre.
            assert.equal(p.thumb + 5, p.ticks[step + 3] + 1, `w ${w} step ${step}`);
        }
        assert.equal(hpSensPlaces(w, -3).ticks[0], -1);
        assert.equal(hpSensPlaces(w, 3).ticks[6], w - 1);
    }
    assert.deepEqual(hpSensPlaces(54, 0).ticks, [-1, 8, 17, 26, 35, 44, 53], 'v17: 64 px slider, 54 px rail, 9 px apart');
    const css = readFileSync(new URL('../src/css/dsprack.css', import.meta.url), 'utf8');
    assert.match(css, /\.hpu \.rail \{[^}]*left: 5px; right: 5px/, 'rail = 64 - 10');
    assert.equal(hpSensAt(0, 10, 120), -3, 'left of the rail: the first step');
    assert.equal(hpSensAt(70, 10, 120), 0);
    assert.equal(hpSensAt(31, 10, 120), -2);
    assert.equal(hpSensAt(500, 10, 120), 3);
    assert.equal(hpSensText(-2), '−2');
    assert.equal(hpSensText(0), '0');
    assert.equal(hpSensText(3), '+3');
    assert.equal(HP_SENS_THRESHOLDS.length, 7);
    assert.equal(HP_SENS_THRESHOLDS[3], 0.30, 'the middle is today\'s threshold');
});

test('HP is one unit: αHP has no row, its switch lives on HP', async () => {
    const src = readFileSync(new URL('../src/js/dsprack.js', import.meta.url), 'utf8');
    assert.equal(FEAT_BY_ID.convLabAlpha.unit, 'convHybridPhase');
    assert.ok(DSP_CHAIN.includes('convLabAlpha'), 'αHP stays a stage: tokens, settings, rackSig unchanged');
    assert.doesNotMatch(src, /class="dep"/, 'no dependency dot');
    assert.match(src, /role="switch"/);
    assert.match(src, /role="slider"/);
    assert.doesNotMatch(src, /type="range"/, 'a div slider: host styles cannot reach it');
    assert.match(html, /<input type="hidden" id="convHpSens" value="0">/);
    assert.match(css, /\.hpu \.sk\s*\{[^}]*width: 64px; flex: none;[^}]*margin-left: -4px/, 'v17: a 64 px slider, 4 px left as v12');
    assert.match(css, /\.hpu \.rail\s*\{[^}]*left: 5px; right: 5px/, 'v14: the rail 5 px in');
    assert.match(css, /\.hpu \.sv\s*\{[^}]*text-align: left/, 'v14: the value right after the slider');
    // v14: the controls in one group on the left, in this order, no pad before them
    assert.doesNotMatch(src, /class="pad"/);
    assert.match(src, /<span class="r2"><span class="ctl">'[\s\S]*?class="sw"[\s\S]*?class="al"[\s\S]*?class="sk"[\s\S]*?class="sv">0<\/span><\/span><\/span>'/);
    assert.match(css, /\.hpu \.ctl\s*\{[^}]*gap: 5px;[^}]*opacity: \.4;[^}]*transition: opacity \.16s ease/, 'dim until hover');
    assert.match(css, /\.hpu:hover \.ctl, \.hpu\.drag \.ctl, \.hpu \.ctl:has\(:focus-visible\) \{ opacity: 1; \}/);
    assert.match(src, /b\.classList\.add\('drag'\)/, 'bright while the thumb is held');
    assert.match(src, /'pointerup', 'pointercancel', 'lostpointercapture'\][^\n]*classList\.remove\('drag'\)/);
    // HL12: the stripe 3 px in at both ends, like a 1U button's padding (4/4 px outer in Edge, was 4/2)
    assert.match(css, /\.hpu \.bar\s*\{[^}]*height: auto;[^}]*top: 3px; bottom: 3px/, 'the 2U stripe is symmetric');
});

test('the sensitivity setting restores a whole step, 0 when missing', async () => {
    const { hpSensOf } = await import('../src/js/settings.js');
    assert.equal(hpSensOf(undefined), 0);
    assert.equal(hpSensOf('2'), 2);
    assert.equal(hpSensOf(-3), -3);
    assert.equal(hpSensOf(4), 0);
    assert.equal(hpSensOf(1.5), 0);
    assert.equal(hpSensOf('x'), 0);
});

test('TFS past this machine\'s RAM reads dim but stays armed, and the rack signature does not see it', async () => {
    const { tfsLimitState } = await import('../src/js/dsprack.js');
    const why = 'TFS at 1G needs 146.9 GB of memory, this PC has 63.7 GB; TFS works up to 180M here';
    assert.equal(tfsLimitState(true, null), null, 'within RAM: the usual states');
    assert.equal(tfsLimitState(false, null), null);
    assert.deepEqual(tfsLimitState(true, why), { text: 'ON', cls: '', limited: true }, 'armed stays ON');
    assert.deepEqual(tfsLimitState(false, why), { text: 'RAM', cls: 'dim', limited: true });
    const src = readFileSync(new URL('../src/js/dsprack.js', import.meta.url), 'utf8');
    assert.match(src, /b\.classList\.toggle\('limited', !!st\.limited\)/);
    assert.match(css, /\.lab-badge\.limited \{ opacity: 0\.55; \}/);
    // A file converted with TFS skipped is still the file of these settings:
    // the signature reads the rack's switches, never the limit.
    const lib = readFileSync(new URL('../src/js/library.js', import.meta.url), 'utf8');
    const sig = lib.slice(lib.indexOf('export function rackSig()'), lib.indexOf('export function rackSigWithMOverride'));
    assert.ok(sig.length > 0 && !/limited|tfs_unavailable|tfsLimit/.test(sig));
});
