// ══════════════════════════════════════════════════════════════════════
// The EQ window's own tour: how the curve is drawn and loaded, its tools,
// presets and level, then the speakers — balance, delay, the distances
// calculator and "Keep with EQ off".
//
// The first time the EQ window opens it offers itself, once, as the
// analyzer's does: someone coming from an earlier version meets it there
// too. Taken or skipped, it is not offered again until the main window's
// tour is taken again (tour.js clears its record). The ? in the title bar
// brings it back any time. Drawn by tour-engine.js; the texts are
// tour-core.js's.
// ══════════════════════════════════════════════════════════════════════

import { createTour } from '../tour-engine.js';
import { TOUR_TEXTS, EQ_STEPS, TOUR_KEY_EQ, tourRecord, shouldOffer, stepText } from '../tour-core.js';

const $ = id => document.getElementById(id);
const q = sel => document.querySelector(sel);
const E = TOUR_TEXTS.eq;

// No bands yet: the graph holds the welcome card (window.js), and the
// curve's step tells how to load one.
const hasBands = () => !!q('#chips .chip');

const TARGETS = {
    graph: () => [$('graphBox')],
    bands: () => [$('chips')],
    tools: () => [$('range')?.closest('.grp'), $('spec')?.closest('.grp'), $('undoB'), $('redoB'), $('addBand')],
    files: () => [$('btnImport'), $('btnExport')],
    presets: () => [$('presetSel')?.closest('.grp'), $('ab'), $('proOn')?.closest('.grp')],
    output: () => [$('gainMode')?.closest('.grp'), $('outGain')?.closest('.grp')],
    speakers: () => [$('spkBox')],
    balance: () => [$('balance')?.closest('.grp')],
    delay: () => [$('delay')?.closest('.grp')],
    distances: () => [$('btnSpk')],
    keep: () => [$('spkAlways')?.closest('.grp')],
    end: () => [$('eqTourHelp')],
};

function store(state) {
    try { localStorage.setItem(TOUR_KEY_EQ, tourRecord(state)); } catch (_) { /* */ }
}

function readStored() {
    try { return localStorage.getItem(TOUR_KEY_EQ); } catch (_) { return null; }
}

function makeHelp(onClick) {
    const bar = $('winHead');
    if (!bar || $('eqTourHelp')) return $('eqTourHelp');
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'eqTourHelp';
    b.className = 'tour-help eq-tour-help';
    b.textContent = '?';
    b.setAttribute('aria-label', E.help.aria);
    b.title = E.help.tip;
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();
        onClick();
    });
    // In the top left corner, as in the analyzer.
    bar.prepend(b);
    return b;
}

/// The drawers and popovers the window may have open: closed as the tour
/// begins (Take the tour), so every step shows the window as it stands.
function closeOpen() {
    if ($('importDrawer') && !$('importDrawer').hidden) $('importCancel')?.click();
    if ($('exportDrawer') && !$('exportDrawer').hidden) $('exportClose')?.click();
    if ($('spkCalc') && !$('spkCalc').hidden) $('btnSpk')?.click();
    if ($('specMenu') && !$('specMenu').hidden) $('specOpts')?.click();
}

function buildSteps() {
    return EQ_STEPS.map(s => {
        const t = E[s.id];
        return {
            id: s.id,
            side: s.side,
            pad: s.pad,
            title: t.title,
            body: () => stepText(t, hasBands()),
            targets: TARGETS[s.id],
            exists: () => TARGETS[s.id]().some(Boolean),
        };
    });
}

let tour = null;
let firstTimer = 0;

function init() {
    if (tour) return;
    tour = createTour({
        name: 'eq',
        texts: { offer: E.offer, again: E.again, nav: TOUR_TEXTS.nav },
        steps: buildSteps(),
        help: () => $('eqTourHelp'),
        begin: closeOpen,
        end: () => false,
        record: store,
    });
    // The ? pressed first: the first offer it would have made is not made.
    makeHelp(() => { clearTimeout(firstTimer); if (!tour.isOpen()) tour.offer('again'); });
    window.__auraTourEq = {
        abort: () => tour.abort(),
        offer: kind => tour.offer(kind || 'again'),
        start: () => tour.start(),
        next: () => tour.next(),
        back: () => tour.back(),
        skip: () => tour.skip(),
        state: () => tour.state(),
    };
    if (shouldOffer({ stored: readStored() }) === 'offer') firstTimer = setTimeout(offerFirst, 900);
    // The window shown again (the EQ button while it is open): the main
    // tour, taken meanwhile, may have cleared the record.
    window.addEventListener('focus', () => {
        if (!firstTimer && shouldOffer({ stored: readStored() }) === 'offer') firstTimer = setTimeout(offerFirst, 400);
    });
}

/// The offer, unless something came first: the tour already up (its ?).
/// A window not on the screen waits to be seen, so the offer is not spent
/// on nobody.
function offerFirst() {
    firstTimer = 0;
    if (!tour || tour.isOpen() || shouldOffer({ stored: readStored() }) !== 'offer') return;
    if (document.hidden) { firstTimer = setTimeout(offerFirst, 800); return; }
    tour.offer('first');
}

init();
