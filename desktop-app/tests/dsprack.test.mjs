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
        new URL('../src-tauri/src/audio/converter/apodize.rs', import.meta.url), 'utf8');
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
    const on = new Set(['convLabDeclip', 'convLabIsp', 'convSubsonic', 'convLabHeadroom',
                        'convAdaptiveApodizer', 'convFirResampling', 'convLabTfs']);
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
