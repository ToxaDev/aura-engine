// The interface pack in the page: the version line, and when the update
// dialog offers an interface update and in which words.
// Run with:  node --test "desktop-app/tests/**/*.test.mjs"
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { versionLabel, uiOffer, uiSkipKey, uiOfferTexts } from '../src/js/ui-pack.js';

const check = (over = {}) => ({
    enabled: true, available: false, current: '1.6.2', tag: 'v1.6.2', ui_current: 0,
    ui_pack: { patch: 1, notes: '- Fixed: the scale.', label: '1.6.2 · UI 1' }, ...over,
});

test('the version reads with the interface update served', () => {
    assert.equal(versionLabel('1.6.2', 0), '1.6.2');
    assert.equal(versionLabel('1.6.2', 1), '1.6.2 · UI 1');
    assert.equal(versionLabel('1.6.2', 12), '1.6.2 · UI 12');
});

test('an interface update is offered only without an app update, and only a newer one', () => {
    assert.deepEqual(uiOffer(check()), { patch: 1, notes: '- Fixed: the scale.', version: '1.6.2' });
    assert.equal(uiOffer(check({ available: true })), null, 'an app update wins');
    assert.equal(uiOffer(check({ enabled: false })), null);
    assert.equal(uiOffer(check({ ui_pack: null })), null);
    assert.equal(uiOffer(check({ ui_current: 1 })), null, 'the one served now');
    assert.equal(uiOffer(check({ ui_current: 2 })), null, 'an older one');
    assert.equal(uiOffer(check({ ui_current: 1, ui_pack: { patch: 2, notes: '' } })).patch, 2);
    assert.equal(uiOffer(null), null);
});

test('the dialog says it in its own words, and Skip remembers this update only', () => {
    const o = uiOffer(check());
    const t = uiOfferTexts(o);
    assert.equal(t.badge, 'Interface update 1 for 1.6.2 is available — click to update');
    assert.equal(t.title, 'Interface update available');
    assert.equal(t.line, 'Interface update 1 for Aura Engine 1.6.2 is ready to install. Only the interface changes; the app restarts.');
    assert.equal(t.skip, 'Skip this update');
    assert.equal(uiSkipKey(o), 'ui:v1.6.2-p1');
    assert.notEqual(uiSkipKey(o), 'v1.6.2', 'skipping it never skips the app release');
});

test('the update dialog installs an interface update with its own command, and shows notes as text', () => {
    const src = readFileSync(new URL('../src/js/update.js', import.meta.url), 'utf8');
    assert.match(src, /invoke\(_ui \? 'ui_pack_install' : 'update_install'\)/);
    const notes = src.slice(src.indexOf('function renderUiNotes'));
    assert.match(notes.slice(0, notes.indexOf('\n}')), /textContent/);
    assert.doesNotMatch(notes.slice(0, notes.indexOf('\n}')), /innerHTML/);
});
