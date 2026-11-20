// ══════════════════════════════════════════════════════════════════════
// The Cue Consistency window's own tour: A/B and the delivery switch, the
// confidence meter, the stage and what its columns mean, then the
// controls under it.
//
// The first time the window opens it offers itself, once, as the EQ's
// does. Taken or skipped, it is not offered again until the main window's
// tour is taken again (tour.js clears its record). The ? in the title bar
// brings it back any time. Drawn by tour-engine.js; the texts are
// tour-core.js's.
// ══════════════════════════════════════════════════════════════════════

import { createTour } from './tour-engine.js';
import { TOUR_TEXTS, CUE_STEPS, TOUR_KEY_CUE, tourRecord, shouldOffer, stepText } from './tour-core.js';

const $ = id => document.getElementById(id);
const q = sel => document.querySelector(sel);
const C = TOUR_TEXTS.cue;

const TARGETS = {
    ab: () => [$('a'), $('b')],
    delivery: () => [$('dSpeakers'), $('dPhones')],
    confidence: () => [q('#scene .conf')],
    stage: () => [$('scene')],
    width: () => [$('s_stageWidth')?.closest('.sl')],
    cues: () => [$('zCues')?.closest('.zone')],
    shadow: () => [$('zShadow')?.closest('.zone')],
    read: () => [$('zRead')?.closest('.zone')],
    bands: () => [$('bandsToggle')?.closest('.zone')],
    end: () => [$('cueTourHelp')],
};

function store(state) {
    try { localStorage.setItem(TOUR_KEY_CUE, tourRecord(state)); } catch (_) { /* */ }
}

function readStored() {
    try { return localStorage.getItem(TOUR_KEY_CUE); } catch (_) { return null; }
}

function makeHelp(onClick) {
    const bar = $('drag');
    if (!bar || $('cueTourHelp')) return $('cueTourHelp');
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'cueTourHelp';
    b.className = 'tour-help eq-tour-help';
    b.textContent = '?';
    b.setAttribute('aria-label', C.help.aria);
    b.title = C.help.tip;
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();
        onClick();
    });
    // In the top left corner, as in the EQ and the analyzer.
    bar.prepend(b);
    return b;
}

/// The help bubble of an (i) may be up as the tour begins: put away.
function closeOpen() {
    const h = $('help');
    if (h) h.style.display = 'none';
}

function buildSteps() {
    return CUE_STEPS.map(s => {
        const t = C[s.id];
        return {
            id: s.id,
            side: s.side,
            pad: s.pad,
            title: t.title,
            body: () => stepText(t, true),
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
        name: 'cue',
        texts: { offer: C.offer, again: C.again, nav: TOUR_TEXTS.nav },
        steps: buildSteps(),
        help: () => $('cueTourHelp'),
        begin: closeOpen,
        end: () => false,
        record: store,
    });
    // The ? pressed first: the first offer it would have made is not made.
    makeHelp(() => { clearTimeout(firstTimer); firstTimer = 0; if (!tour.isOpen()) tour.offer('again'); });
    window.__auraTourCue = {
        abort: () => tour.abort(),
        offer: kind => tour.offer(kind || 'again'),
        start: () => tour.start(),
        next: () => tour.next(),
        back: () => tour.back(),
        skip: () => tour.skip(),
        state: () => tour.state(),
    };
    if (shouldOffer({ stored: readStored() }) === 'offer') firstTimer = setTimeout(offerFirst, 900);
    // The window shown again: the main tour, taken meanwhile, may have
    // cleared the record.
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
