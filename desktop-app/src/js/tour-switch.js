// ══════════════════════════════════════════════════════════════════════
// The COMPACT | LIBRARY switch's own short tour.
//
// The window opens in Library after a first install and after the update
// that brought it (landscape.js, the intro). Before anything else — before
// the main tour's offer to a newcomer — two steps point at the switch in
// the head: what the two looks are, and that the look left is the look
// that opens. No card asks first: the steps come at once, Skip or Esc
// ends them. Shown once (TOUR_KEY_SWITCH); taken or skipped, it goes into
// the ? like every tour.
//
// It is the one tour shown in the Library look: the landscape window does
// not step aside for it (landscape.js asks the layer's data-tour).
// Drawn by tour-engine.js; the texts are tour-core.js's.
// ══════════════════════════════════════════════════════════════════════

import { createTour, shown } from './tour-engine.js';
import { TOUR_TEXTS, SWITCH_STEPS, TOUR_KEY_SWITCH, tourRecord, shouldOffer, stepText } from './tour-core.js';
import { hide as hideTip } from './tooltip.js';

const $ = id => document.getElementById(id);
const wait = ms => new Promise(r => setTimeout(r, ms));
const S = TOUR_TEXTS.switch;

/// The name the tour's layer carries (data-tour): landscape.js leaves the
/// Library look standing for it.
export const SWITCH_TOUR = 'switch';

const TARGETS = {
    library: () => [$('winBtnOrient')],
    remember: () => [$('winBtnOrient')],
};

function readStored() {
    try { return localStorage.getItem(TOUR_KEY_SWITCH); } catch (_) { return null; }
}

function store(state) {
    try { localStorage.setItem(TOUR_KEY_SWITCH, tourRecord(state)); } catch (_) { /* shown twice is no harm */ }
}

function buildSteps() {
    return SWITCH_STEPS.map(s => ({
        id: s.id,
        side: s.side,
        pad: s.pad,
        title: S[s.id].title,
        body: () => stepText(S[s.id], true),
        targets: TARGETS[s.id],
        exists: () => TARGETS[s.id]().some(Boolean),
    }));
}

let tour = null;
let doneWith = () => {};

/// Called by main.js once the window is up. `after`: the update check — the
/// tour waits for it (up to 15 s), so it never comes up under the update
/// dialog. Resolves when the tour is over, or at once when it is not due:
/// the main tour's offer waits for it.
export function initSwitchTour({ after = null } = {}) {
    if (tour || shouldOffer({ stored: readStored() }) !== 'offer' || !$('winBtnOrient')) return Promise.resolve();
    tour = createTour({
        name: SWITCH_TOUR,
        texts: { offer: S.offer, again: S.again, nav: TOUR_TEXTS.nav },
        steps: buildSteps(),
        help: () => $('tourHelp'),
        begin: hideTip,
        end: () => { setTimeout(() => doneWith(), 0); return false; },
        record: store,
    });
    window.__auraTourSwitch = {
        abort: () => { const r = tour.abort(); doneWith(); return r; },
        next: () => tour.next(),
        back: () => tour.back(),
        skip: () => tour.skip(),
        state: () => tour.state(),
    };
    return new Promise(resolve => {
        doneWith = () => { doneWith = () => {}; resolve(); };
        Promise.race([Promise.resolve(after), wait(15000)])
            .catch(() => {})
            .then(() => setTimeout(showWhenFree, 600));
    });
}

/// At once, once the window is free for it: not over the update dialog,
/// another tour or full screen, and with the switch on the screen.
function showWhenFree() {
    if (shouldOffer({ stored: readStored() }) !== 'offer') { doneWith(); return; }
    const up = $('upModal');
    const b = document.body.classList;
    const busy = b.contains('fullview') || b.contains('tour-up') || b.contains('lm-anim')
        || (up && !up.classList.contains('up-hidden') && shown(up))
        || !shown($('winBtnOrient'));
    if (busy) { setTimeout(showWhenFree, 800); return; }
    // No card first: the offer opens the layer, the steps start at once.
    if (!tour.offer('first')) { doneWith(); return; }
    tour.start();
}
