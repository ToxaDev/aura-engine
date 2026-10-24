// ══════════════════════════════════════════════════════════════════════
// The analyzer's own tour: what each part of this window shows — the
// chain, the numbers (S, B, O), each graph on its tab, the tools and the
// reports.
//
// The first time the analyzer opens it offers itself, once, as the main
// window's tour does (Anton 1.10: how the analyzer works was nowhere to be
// seen); taken or skipped, it is not offered again. The ? in the title bar
// brings it back: a small card, and the tour, done or skipped, flies back
// into it.
// It picks each graph's tab to show it, and puts back the one that was
// chosen. Drawn by tour-engine.js; the texts are tour-core.js's.
// ══════════════════════════════════════════════════════════════════════

import { createTour } from '../tour-engine.js';
import { TOUR_TEXTS, ANALYZER_STEPS, TOUR_KEY_ANALYZER, tourRecord, shouldOffer } from '../tour-core.js';

const $ = id => document.getElementById(id);
const A = TOUR_TEXTS.analyzer;

/// The tab button of a graph's panel (window.js builds them in the panels'
/// order, without ids).
function tabOf(panelId) {
    const panels = [...document.querySelectorAll('#anPanels > .an-panel-collapsible')];
    const k = panels.findIndex(p => p.id === panelId);
    return k < 0 ? null : document.querySelectorAll('#anTabs .an-tab')[k] || null;
}

const activePanel = () => document.querySelector('#anPanels > .an-panel-collapsible.an-tab-active')?.id || null;

// window.js keeps the chosen tab for the next window (TAB_KEY there). A tab
// the tour shows is not the listener's choice: what was kept is put back
// at once, so a window closed in the middle of the tour opens on their tab.
const TAB_KEY = 'analyzer-tab';
let tabKept;

function showTab(panelId) {
    if (activePanel() === panelId) return;
    tabOf(panelId)?.click();
    try {
        if (tabKept == null) localStorage.removeItem(TAB_KEY);
        else localStorage.setItem(TAB_KEY, tabKept);
    } catch (_) { /* */ }
}

const TARGETS = {
    chain: () => [$('anChainWrap')],
    metrics: () => [$('anMetricsSection')],
    graphs: () => [$('anTabs')],
    tools: () => [$('anToolbar')],
    reports: () => [$('anCardStrip')],
    end: () => [$('anTourHelp')],
};

function store(state) {
    try { localStorage.setItem(TOUR_KEY_ANALYZER, tourRecord(state)); } catch (_) { /* */ }
}

function makeHelp(onClick) {
    const bar = $('anTitleBar');
    if (!bar || $('anTourHelp')) return $('anTourHelp');
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'anTourHelp';
    b.className = 'tour-help an-tour-help';
    b.textContent = '?';
    b.setAttribute('aria-label', A.help.aria);
    b.setAttribute('data-tip', A.help.tip);
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();
        onClick();
    });
    // In the top left corner, where the main window keeps its own.
    bar.prepend(b);
    return b;
}

function buildSteps() {
    return ANALYZER_STEPS.map(s => {
        const t = A[s.id];
        const step = { id: s.id, side: s.side, pad: s.pad, title: t.title, body: () => t.text };
        if (s.tab) {
            step.targets = () => [tabOf(s.tab), $(s.tab)];
            step.exists = () => !!$(s.tab) && !!tabOf(s.tab);
            step.enter = () => showTab(s.tab);
        } else {
            step.targets = TARGETS[s.id];
            step.exists = () => TARGETS[s.id]().some(Boolean);
        }
        return step;
    });
}

let tour = null;
let tabBefore = null;

function init() {
    if (tour || !$('anTabs')) return !!tour;
    tour = createTour({
        name: 'analyzer',
        texts: { offer: A.offer, again: A.again, nav: TOUR_TEXTS.nav },
        steps: buildSteps(),
        help: () => $('anTourHelp'),
        begin() {
            tabBefore = activePanel();
            try { tabKept = localStorage.getItem(TAB_KEY); } catch (_) { tabKept = null; }
        },
        end() {
            if (tabBefore) showTab(tabBefore);
            return false;
        },
        record: store,
    });
    // The ? pressed first: the first offer it would have made is not made.
    makeHelp(() => { clearTimeout(firstTimer); if (!tour.isOpen()) tour.offer('again'); });
    window.__auraTourAnalyzer = {
        abort: () => tour.abort(),
        offer: kind => tour.offer(kind || 'again'),
        start: () => tour.start(),
        next: () => tour.next(),
        back: () => tour.back(),
        skip: () => tour.skip(),
        state: () => tour.state(),
    };
    // The first analyzer: its tour offers itself, once (an earlier build
    // only lit the ?, 'hinted': that was no offer either).
    if (shouldOffer({ stored: readStored() }) === 'offer') firstTimer = setTimeout(offerFirst, 1200);
    return true;
}

let firstTimer = 0;

function readStored() {
    try { return localStorage.getItem(TOUR_KEY_ANALYZER); } catch (_) { return null; }
}

/// The offer, unless something came first: the tour already up (its ?), or
/// another analyzer window that offered it meanwhile. A window not on the
/// screen (minimized at once) waits to be seen, so the offer is not spent
/// on nobody.
function offerFirst() {
    if (!tour || tour.isOpen() || shouldOffer({ stored: readStored() }) !== 'offer') return;
    if (document.hidden) { firstTimer = setTimeout(offerFirst, 800); return; }
    tour.offer('first');
}

// window.js builds the tabs as it starts; wait for them.
if (!init()) {
    const mo = new MutationObserver(() => { if (init()) mo.disconnect(); });
    mo.observe(document.documentElement, { childList: true, subtree: true });
    setTimeout(() => mo.disconnect(), 15000);
}
