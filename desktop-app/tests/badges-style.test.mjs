// One stage badge everywhere (Anton 19.10): the player's line, the big
// player over its picture and a row of the list draw the same markup
// (dsprack.js cb) in the same look (dsprack.css .cb) — no place sizes or
// colours it its own way. The load badge and the load window's header keep
// their words centred.
//
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { cb } from '../src/js/dsprack.js';

const src = rel => readFileSync(new URL('../src/' + rel, import.meta.url), 'utf8');
const noComments = css => css.replace(/\/\*[\s\S]*?\*\//g, '');

/// Every rule of a stylesheet: [selector, body].
const rules = css => [...noComments(css).matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .map(m => [m[1].trim(), m[2]]);

test('one badge markup: the rows and the player build it with dsprack.js cb()', () => {
    const html = cb('fired', 'αHP', 'Continuous Alpha', 'ahp', 'Ran & changed "it".', 'animation-delay:-0.2s');
    assert.match(html, /^<span class="cb fired" style="--h:var\(--lab-ahp\);animation-delay:-0\.2s"/);
    assert.match(html, /data-lab-why="Ran &amp; changed &quot;it&quot;\."/);
    assert.match(html, />αHP<\/span>$/);
    // No reason: no empty tooltip line.
    assert.doesNotMatch(cb('pl-preview', 'DC', 'Declip', 'dc', null), /data-lab-why/);
    assert.match(cb('', 'DC', 'Declip', 'dc', null), /^<span class="cb" /);
    // Nobody writes its own stage badge span any more (dropzone's plain
    // "→ next" chip has no stage and keeps its own words).
    for (const f of ['js/player.js', 'js/dropzone.js']) {
        assert.doesNotMatch(src(f), /<span class="cb[^>]*data-lab-hue/, `${f} writes a stage badge span by hand`);
    }
    assert.equal(src('js/dsprack.js').match(/<span class="cb(?!sep)/g).length, 1, 'dsprack.js: cb() is the only badge span');
    assert.match(src('js/player.js'), /import \{[^}]*\bcb\b[^}]*\} from '\.\/dsprack\.js'/);
});

test('one badge look: only dsprack.css sizes and colours .cb; nowhere else overrides it', () => {
    const base = rules(src('css/dsprack.css')).filter(([sel]) => /(^|,\s*)\.cb\b/.test(sel));
    assert.ok(base.length >= 4, 'the .cb rules are in dsprack.css');
    const cbRule = base.find(([sel]) => sel === '.cb')[1];
    assert.match(cbRule, /font-size:\s*0\.6rem/);
    assert.match(cbRule, /line-height:\s*11px/);
    assert.match(cbRule, /padding:\s*1px 4px/);
    // A solid dark ground: the big player's picture does not show through.
    assert.match(cbRule, /--cb-ground:\s*#[0-9a-f]{6}/i);
    assert.match(cbRule, /background:\s*color-mix\(in srgb, #[0-9a-f]{6} \d+%, var\(--cb-ground\)\)/i, 'opaque: mixed into the solid ground');
    assert.match(cbRule, /opacity:\s*1/);
    // Sharp letters: no glow on the text, a tight halo only on the lit box.
    assert.match(cbRule, /text-shadow:\s*none/);
    const fired = base.find(([sel]) => sel === '.cb.fired')[1];
    assert.doesNotMatch(fired, /text-shadow/);
    const halo = fired.match(/box-shadow:\s*0 0 (\d+)px/);
    assert.ok(halo && +halo[1] <= 2, 'the lit badge glows at most 2 px');
    assert.match(fired, /background:\s*color-mix\(in srgb, var\(--h\) \d+%, var\(--cb-ground\)\)/);
    // Off is the dashed "selected but not working" outline (Anton 19.10: a
    // dim glow still looked switched on): grey letters, no glow, no stage
    // colour; skipped is that same look (no rule of its own). Lit is solid,
    // the stage colour itself.
    assert.match(cbRule, /border:\s*1px dashed rgba\(148, 163, 184, 0\.6\)/);
    assert.match(cbRule, /box-shadow:\s*none/);
    assert.match(cbRule, /(^|[;\s])color:\s*#9aa8bc/);
    assert.doesNotMatch(cbRule, /var\(--h\)\s*\d+%/, 'no stage colour while off');
    assert.equal(base.find(([sel]) => sel === '.cb.skipped'), undefined, 'skipped takes the off look as it is');
    assert.match(fired, /border-style:\s*solid/);
    assert.match(fired, /(^|[;\s])color:\s*var\(--h\)/);
    // Pending keeps the stage's colour, dimmed and solid (player.css pulses it).
    const pending = base.find(([sel]) => sel === '.cb.pending')[1];
    assert.match(pending, /border-style:\s*solid/);
    assert.match(pending, /color:\s*color-mix\(in srgb, var\(--h\) 58%, var\(--cb-ground\)\)/);
    // Elsewhere only the switching states, which dim by brightness (not
    // opacity, which would let the picture through).
    for (const f of ['css/player.css', 'css/listening.css', 'css/list.css', 'css/style.css']) {
        for (const [sel, body] of rules(src(f))) {
            if (!/\.cb\b/.test(sel) || sel.startsWith('@')) continue;
            const ok = sel.split(',').every(s => /^\s*\.cb\.(pending|leaving)\s*$/.test(s));
            assert.ok(ok, `${f}: "${sel}" overrides the badge's look`);
            assert.doesNotMatch(body, /opacity/, `${f}: "${sel}" dims by opacity`);
        }
    }
});

test('the token chips beside the badges share their box and their solid ground', () => {
    const tok = rules(src('css/player.css')).find(([sel]) => sel === '.pl-tok')[1];
    assert.match(tok, /font-size:\s*0\.6rem/);
    assert.match(tok, /line-height:\s*11px/);
    assert.match(tok, /padding:\s*1px 4px/);
    assert.match(tok, /background:\s*#0a101b/);
    assert.doesNotMatch(src('css/listening.css'), /\.pl-tok\s*\{[^}]*font-size/);
});

test('the load badge is 14 px with its letters cap-centred; the load window header on one centre line', () => {
    const badge = rules(src('css/perf.css')).find(([sel]) => sel === '#perfBadge')[1];
    assert.match(badge, /height:\s*14px/);
    assert.match(badge, /align-items:\s*center/);
    assert.match(src('css/perf.css'), /#perfBadge \.pb-k,\s*#perfBadge \.pb-v \{[^}]*text-box:\s*trim-both cap alphabetic/);
    const head = src('perf.html');
    assert.match(head, /header \.tok, header \.t, \.range button\{[^}]*text-box:trim-both cap alphabetic/);
    assert.match(head, /header\{[^}]*align-items:center/);
});

test('the taps chip, off while stopped, takes the badges\' dashed off outline', () => {
    const prev = rules(src('css/player.css')).find(([sel]) => sel === '.pl-tok.pl-preview')[1];
    assert.match(prev, /border:\s*1px dashed rgba\(148, 163, 184, 0\.6\)/);
    assert.match(prev, /color:\s*#9aa8bc/);
});
