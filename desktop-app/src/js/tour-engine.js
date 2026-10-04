// ══════════════════════════════════════════════════════════════════════
// Drawing a tour. What it says and where the tip goes is tour-core.js;
// which elements a step points at is its window's (tour.js for the main
// one).
//
// Over the window, while a tour is up: a sheet that takes every click and
// every key but the tour's own, so nothing under it is pressed by
// accident; the window dimmed, with a hole round what the step points at
// and a thin lit ring on its edge; and the tip beside it, its arrow on the
// target, with Back, Next and Skip tour. The hole and the tip are measured
// again on every frame, so they stay on their element when the window
// changes size, the layout switches or the list is redrawn.
//
// A step can hand one thing to the listener (`live`): the sheet opens over
// what it points at, so a click there reaches it, and the step is over when
// the window shows it was pressed (`over`). The main tour ends that way:
// the listener folds the big player back with its own edge.
//
// When the tour ends, done or skipped, the tip flies left and up into the
// ? that brings it back (400–700 ms), and the ? lights and fades, so the
// way back in is seen. With reduced motion the tip fades where it stands
// and the ? only lights.
// ══════════════════════════════════════════════════════════════════════

import {
    placeTip, padRect, unionRect, lerpRect, easeOutCubic,
    flyTransform, flyDuration, nextIndex, stepCount, textParts,
} from './tour-core.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const EASE = 'cubic-bezier(.2,.7,.2,1)';

const reducedMotion = () => {
    try { return window.matchMedia('(prefers-reduced-motion: reduce)').matches; } catch (_) { return false; }
};
const wait = ms => new Promise(r => setTimeout(r, ms));
const rectOf = el => { const r = el.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; };
export const shown = el => {
    if (!el || !el.isConnected) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
};

function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
}

function svg(tag, attrs = {}) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
    return e;
}

/// A tour's text into its element: the words as text, a button named by its
/// sign in braces ("the {?} button") as a small key, so the sign never
/// stands alone in the line like a slip of the pen.
function tourText(node, s) {
    const parts = textParts(s);
    if (!parts.some(p => p.key != null)) { node.textContent = s || ''; return; }
    node.replaceChildren(...parts.map(p => (p.key != null ? el('span', 'tour-chip', p.key) : document.createTextNode(p.text))));
}

let layers = 0;

/// Light the ? for a moment: the way back into the tour, seen.
export function glowHelp(btn) {
    if (!btn) return;
    btn.classList.remove('tour-glow');
    void btn.offsetWidth;   // start the animation over
    btn.classList.add('tour-glow');
    const off = () => btn.classList.remove('tour-glow');
    btn.addEventListener('animationend', off, { once: true });
    setTimeout(off, 2600);
}

/// cfg:
///   name       a word for the layer's id ('main', 'analyzer')
///   texts      { offer, again, nav } (tour-core.js TOUR_TEXTS)
///   steps      [{ id, title, body(ctx) → string | Node, targets(ctx) → Element[],
///                 exists?() → bool, side, pad, rings?: 'first', needsPrep?(ctx),
///                 prepare?(ctx), enter?(ctx), leave?(ctx),
///                 live?: true, keys?: [code], over?(ctx) → bool }]
///              The arrow points at the first target; `rings: 'first'` lights
///              only it, the others are uncovered to be seen around it.
///              `live`: the first target can be pressed through the sheet;
///              `keys`: the window's own keys (KeyboardEvent.code) that reach
///              it on this step; `over`: asked on every frame — true, the
///              listener did what the step asks, and the tour is done.
///              A text may name a button by its sign in braces, `{?}`: it is
///              drawn as a small key (tourText).
///   help()     the ? button, or null
///   begin()    before the first step (hold the player's controls, remember the layout)
///   end(kind)  after the tour: 'done' | 'skipped' | 'abort'; put back what begin() changed;
///              true when that moves the layout (the hole then goes at once)
///   record(state)  keep 'offered' | 'done' | 'skipped' in the profile
///   frame?()   called on every frame while the tour is up (a tour's own furniture)
export function createTour(cfg) {
    const steps = cfg.steps;
    const T = cfg.texts;

    let layer = null, block = null, dim = null, holesG = null, ringsG = null;
    let tip = null, arrow = null, hEl = null, nEl = null, bEl = null;
    let bSkip = null, bBack = null, bNext = null;
    let phase = null;        // 'offer' | 'steps' | 'closing'
    let offerKind = null;    // 'first' | 'again'
    let index = -1;
    let avail = [];
    let gen = 0;             // generation: a later move cancels an earlier one still waiting
    let raf = 0;
    let switching = false;   // the layout is changing between two steps: nothing drawn
    let tween = null;        // { from: rect, t0, dur }
    let lastUnion = null;    // what was drawn last, for the next glide
    let cache = null;        // { index, els, radii }
    let begun = false;
    const ctx = {};

    // ── the layer ─────────────────────────────────────────────────────

    function build() {
        const id = ++layers;
        layer = el('div', 'tour-layer');
        layer.id = 'tourLayer';
        layer.dataset.tour = cfg.name;

        block = el('div', 'tour-block');
        block.addEventListener('wheel', e => e.preventDefault(), { passive: false });
        block.addEventListener('contextmenu', e => e.preventDefault());

        dim = svg('svg', { class: 'tour-dim', 'aria-hidden': 'true' });
        const defs = svg('defs');
        const mask = svg('mask', { id: `tourMask${id}`, maskUnits: 'userSpaceOnUse', x: 0, y: 0, width: '100%', height: '100%' });
        mask.appendChild(svg('rect', { x: 0, y: 0, width: '100%', height: '100%', fill: 'white' }));
        holesG = svg('g');
        mask.appendChild(holesG);
        defs.appendChild(mask);
        dim.appendChild(defs);
        dim.appendChild(svg('rect', { class: 'tour-shade', x: 0, y: 0, width: '100%', height: '100%', mask: `url(#tourMask${id})` }));
        ringsG = svg('g', { class: 'tour-rings' });
        dim.appendChild(ringsG);

        tip = el('div', 'tour-tip');
        tip.setAttribute('role', 'dialog');
        tip.setAttribute('aria-modal', 'true');
        arrow = el('div', 'tour-arrow');
        const head = el('div', 'tour-head');
        hEl = el('div', 'tour-h');
        hEl.id = `tourTitle${id}`;
        nEl = el('span', 'tour-n');
        head.append(hEl, nEl);
        bEl = el('div', 'tour-b');
        bEl.id = `tourBody${id}`;
        tip.setAttribute('aria-labelledby', hEl.id);
        tip.setAttribute('aria-describedby', bEl.id);
        const foot = el('div', 'tour-f');
        bSkip = el('button', 'tour-skip');
        bBack = el('button', 'tour-btn tour-back');
        bNext = el('button', 'tour-btn tour-next');
        for (const b of [bSkip, bBack, bNext]) b.type = 'button';
        foot.append(bSkip, bBack, bNext);
        tip.append(arrow, head, bEl, foot);

        bSkip.addEventListener('click', () => finish('skipped'));
        bBack.addEventListener('click', () => (phase === 'offer' ? offerNo() : back()));
        bNext.addEventListener('click', () => (phase === 'offer' ? offerYes() : next()));

        layer.append(block, dim, tip);
        document.body.appendChild(layer);
        // The window's own tooltips wait too: one of a thing a step hands to
        // the listener would stand over the tip (tour.css).
        document.body.classList.add('tour-up');
        window.addEventListener('keydown', onKey, true);
        window.addEventListener('keyup', onKeyUp, true);
        raf = requestAnimationFrame(frame);
        // In on the next frame, so the fade runs.
        requestAnimationFrame(() => layer?.classList.add('tour-in'));
    }

    function teardown() {
        cancelAnimationFrame(raf);
        raf = 0;
        window.removeEventListener('keydown', onKey, true);
        window.removeEventListener('keyup', onKeyUp, true);
        document.body.classList.remove('tour-up');
        layer?.remove();
        layer = block = dim = holesG = ringsG = tip = arrow = hEl = nEl = bEl = bSkip = bBack = bNext = null;
        phase = null;
        index = -1;
        tween = null;
        lastUnion = null;
        lastShapes = null;
        lastPlace = null;
        lastPass = null;
        cache = null;
        switching = false;
        const ae = document.activeElement;
        if (ae && ae !== document.body && !ae.isConnected) try { ae.blur(); } catch (_) { /* */ }
    }

    // ── keys: the tour's own, and nothing for the window under it ─────

    /// A dialog over the tour (the update offer, 9000 over the tour's 8000)
    /// keeps the keys: the tour neither acts on them nor holds them back.
    /// Only its own button under the dialog is kept from being pressed.
    function dialogOver(e) {
        const up = document.getElementById('upModal');
        if (up && !up.classList.contains('up-hidden')) return true;
        const t = e.target;
        return !!(t && t.nodeType === 1 && !layer.contains(t) && t.closest?.('[aria-modal="true"]'));
    }

    // The key that closed the tour: its release is the tour's, everything
    // after it the window's again (Space plays at once, flight or not).
    let lastDown = null, lastDownAt = 0, closingKey = null;

    function onKey(e) {
        if (!layer) return;
        if (dialogOver(e)) { if (layer.contains(e.target)) e.preventDefault(); return; }
        if (phase === 'closing') return;
        lastDown = e.key;
        lastDownAt = performance.now();
        const ae = document.activeElement;
        const onButton = !!ae && tip?.contains(ae) && ae.tagName === 'BUTTON';
        const own = () => { e.preventDefault(); e.stopImmediatePropagation(); };
        // A key held down steps once, not through the whole tour.
        if (e.repeat) { own(); return; }
        switch (e.key) {
        case 'Escape':
            own();
            if (phase === 'offer') offerNo(); else if (phase === 'steps') finish('skipped');
            return;
        case 'ArrowRight': case 'PageDown':
            own();
            if (phase === 'steps') next();
            return;
        case 'ArrowLeft': case 'PageUp':
            own();
            if (phase === 'steps') back();
            return;
        case 'Enter':
        case ' ':
            // The button with the focus is pressed here, on the key's way
            // down, not left to the page's own activation: that comes with
            // the key's character, which not every source of keys sends.
            own();
            if (onButton) ae.click();
            else if (e.key === 'Enter') { if (phase === 'offer') offerYes(); else if (phase === 'steps') next(); }
            return;
        case 'Tab': {
            own();
            const bs = [bSkip, bBack, bNext].filter(b => b && !b.hidden);
            if (!bs.length) return;
            const k = bs.indexOf(ae);
            const to = k < 0 ? bs.length - 1 : (k + (e.shiftKey ? -1 : 1) + bs.length) % bs.length;
            bs[to].focus();
            return;
        }
        default:
            // A key the step hands to the window (L, on the step that asks
            // for the fold) goes on to it.
            if (phase === 'steps' && !switching && steps[index]?.keys?.includes(e.code)) return;
            // The window's own shortcuts (L, Space on the player, C, A...)
            // wait until the tour is gone.
            e.stopImmediatePropagation();
        }
    }

    function onKeyUp(e) {
        if (!layer) return;
        if (dialogOver(e)) { if (layer.contains(e.target)) e.preventDefault(); return; }
        if (phase === 'closing') {
            if (e.key === closingKey) { closingKey = null; e.preventDefault(); e.stopImmediatePropagation(); }
            return;
        }
        // Space's own activation would come on its way up: pressed already.
        if (e.key === ' ' || e.key === 'Enter') e.preventDefault();
        e.stopImmediatePropagation();
    }

    // ── drawing, every frame ──────────────────────────────────────────

    function targetsNow() {
        const st = steps[index];
        if (!st || phase !== 'steps' || switching) return [];
        if (!cache || cache.index !== index || cache.els.some(e => !e.isConnected)) {
            const els = (st.targets?.(ctx) || []).filter(Boolean);
            const radii = els.map(e => {
                const r = parseFloat(getComputedStyle(e).borderTopLeftRadius) || 0;
                return Math.max(4, Math.min(14, r + (st.pad ?? 4)));
            });
            cache = { index, els, radii };
        }
        return cache.els;
    }

    let lastShapes = null;

    /// The holes, and a ring on the first `ringN` of them (a step can light
    /// one part of what it uncovers: a row of a menu shown whole).
    function drawShapes(rects, radii, ringN = rects.length) {
        // Written only when something moved: the mask repaints the window.
        const key = ringN + '|' + rects.map((r, k) => `${r.x.toFixed(1)},${r.y.toFixed(1)},${r.w.toFixed(1)},${r.h.toFixed(1)},${radii[k]}`).join(';');
        if (key === lastShapes) return;
        lastShapes = key;
        const sync = (g, n, make) => {
            while (g.childNodes.length > n) g.lastChild.remove();
            while (g.childNodes.length < n) g.appendChild(make());
        };
        sync(holesG, rects.length, () => svg('rect', { fill: 'black' }));
        sync(ringsG, Math.min(ringN, rects.length), () => svg('rect', { class: 'tour-ring' }));
        rects.forEach((r, k) => {
            for (const node of [holesG.childNodes[k], ringsG.childNodes[k]]) {
                if (!node) continue;
                node.setAttribute('x', r.x.toFixed(1));
                node.setAttribute('y', r.y.toFixed(1));
                node.setAttribute('width', Math.max(0, r.w).toFixed(1));
                node.setAttribute('height', Math.max(0, r.h).toFixed(1));
                node.setAttribute('rx', String(radii[k] ?? 8));
            }
        });
    }

    let lastPass = null;

    /// The sheet open over `r` (null: whole again), so a click there reaches
    /// what is under it: the sheet is clipped, and a clipped-out part of it
    /// takes no pointer.
    function passThrough(r) {
        const f = v => v.toFixed(1);
        const key = r ? `${f(r.x)},${f(r.y)},${f(r.w)},${f(r.h)}` : '';
        if (key === lastPass) return;
        lastPass = key;
        block.style.clipPath = r
            ? `path(evenodd, "M-9 -9H99999V99999H-9Z M${f(r.x)} ${f(r.y)}h${f(r.w)}v${f(r.h)}h${f(-r.w)}Z")`
            : '';
    }

    let lastPlace = null;
    let lastScroll = 0;

    function frame(now) {
        if (!layer) return;
        raf = requestAnimationFrame(frame);
        try { cfg.frame?.(ctx); } catch (_) { /* its own furniture; the tour goes on */ }
        if (phase === 'closing') return;
        const view = { w: window.innerWidth, h: window.innerHeight };
        const st = steps[index];
        // The listener did what the step handed them: the tour is done.
        if (phase === 'steps' && !switching && st?.over) {
            let over = false;
            try { over = !!st.over(ctx); } catch (_) { /* asked again on the next frame */ }
            if (over) { finish('done'); return; }
        }
        const els = targetsNow().filter(shown);
        const pad = st?.pad ?? 4;
        // The window made shorter under a step, and what it points at went
        // past the edge: bring it back into sight (not every frame).
        if (els.length && now - lastScroll > 400) {
            const r = els[0].getBoundingClientRect();
            if (r.bottom > view.h + 1 || r.top < -1) { lastScroll = now; els[0].scrollIntoView({ block: 'nearest' }); }
        }
        let rects = els.map(e => padRect(rectOf(e), pad));
        const radii = els.map(e => cache.radii[cache.els.indexOf(e)]);
        if (tween) {
            const t = (now - tween.t0) / tween.dur;
            if (t >= 1) tween = null;
            else if (rects.length) {
                const k = easeOutCubic(t);
                rects = rects.map(r => lerpRect(tween.from, r, k));
            }
        }
        drawShapes(rects, radii, st?.rings === 'first' ? 1 : rects.length);
        // Open over the thing itself, not the room the hole keeps round it.
        passThrough(st?.live && els.length ? rectOf(els[0]) : null);
        const u = unionRect(rects);
        if (u) lastUnion = u;

        const size = { w: tip.offsetWidth, h: tip.offsetHeight };
        // Several things shown: the arrow at the first.
        const aim = rects.length > 1 ? rects[0] : null;
        const p = placeTip(view, phase === 'steps' ? u : null, size, st?.side || ['bottom', 'top'], { aim });
        const key = `${p.x},${p.y},${p.side},${p.ax},${p.ay}`;
        if (key === lastPlace) return;
        lastPlace = key;
        tip.style.transform = `translate(${p.x}px, ${p.y}px)`;
        tip.dataset.side = p.side;
        if (p.ax == null) {
            arrow.style.display = 'none';
        } else {
            // The arrow's 12 px square, centred on the tip's outer edge: its
            // offsets count from inside the tip's 1 px border.
            arrow.style.display = '';
            arrow.style.left = (p.ax - 7) + 'px';
            arrow.style.top = (p.ay - 7) + 'px';
        }
    }

    // ── the offer ─────────────────────────────────────────────────────

    function renderOffer() {
        const t = offerKind === 'first' ? T.offer : T.again;
        hEl.textContent = t.title;
        nEl.textContent = '';
        tourText(bEl, t.body);
        bSkip.hidden = true;
        bBack.hidden = false;
        bBack.textContent = offerKind === 'first' ? t.skip : t.cancel;
        bNext.textContent = t.start;
        tip.classList.add('tour-offer');
        lastPlace = null;
    }

    /// `kind`: 'first' (a newcomer's first start) or 'again' (the ?).
    function offer(kind) {
        if (layer) return false;
        offerKind = kind;
        phase = 'offer';
        build();
        renderOffer();
        if (kind === 'first') cfg.record?.('offered');
        // From the ?: the card comes out of it.
        const help = cfg.help?.();
        if (kind === 'again' && shown(help) && !reducedMotion()) {
            const view = { w: window.innerWidth, h: window.innerHeight };
            const size = { w: tip.offsetWidth, h: tip.offsetHeight };
            const p = placeTip(view, null, size);
            const f = flyTransform({ x: p.x, y: p.y, w: size.w, h: size.h }, rectOf(help));
            tip.style.transformOrigin = '50% 50%';
            tip.animate([
                { transform: `translate(${p.x + f.dx}px, ${p.y + f.dy}px) scale(${f.s})`, opacity: 0.2 },
                { transform: `translate(${p.x}px, ${p.y}px) scale(1)`, opacity: 1 },
            ], { duration: 380, easing: EASE });
        }
        focusSoon(bNext);
        return true;
    }

    function offerYes() {
        if (phase !== 'offer') return;
        tip.classList.remove('tour-offer');
        lastUnion = rectOf(tip);    // the first hole opens out of the card
        startSteps();
    }

    function offerNo() {
        if (phase !== 'offer') return;
        if (offerKind === 'first') { finish('skipped'); return; }
        // Not now: back into the ?, nothing recorded.
        close(null);
    }

    // ── the steps ─────────────────────────────────────────────────────

    function startSteps() {
        avail = steps.map(s => { try { return s.exists ? !!s.exists() : true; } catch (_) { return false; } });
        const first = nextIndex(steps.length, -1, +1, k => avail[k]);
        if (first >= steps.length) { close('done'); return; }
        if (!begun) { begun = true; try { cfg.begin?.(ctx); } catch (_) { /* */ } }
        phase = 'steps';
        go(first);
    }

    /// Go to step `to`: leave the one shown, let the layout switch if the
    /// next one lives in the other (the dim lifts, so the switch is seen),
    /// then point at it.
    async function go(to) {
        const my = ++gen;
        const from = steps[index];
        if (from && index !== to) { try { from.leave?.(ctx, steps[to]); } catch (_) { /* */ } }
        index = to;
        cache = null;
        const st = steps[to];
        let prep = false;
        try { prep = !!st.needsPrep?.(ctx); } catch (_) { prep = false; }
        if (prep) {
            switching = true;
            layer.classList.add('tour-switching');
            drawShapes([], []);
            await wait(reducedMotion() ? 0 : 170);
            if (my !== gen || !layer) return;
            try { await st.prepare?.(ctx); } catch (_) { /* shown as it is */ }
            if (my !== gen || !layer) return;
        }
        // Whoever started a switch, the step now shown ends it: a move that
        // overtook an earlier one would otherwise leave the dim and the tip
        // lifted, and the sheet taking every click.
        if (switching) {
            switching = false;
            cache = null;
            layer.classList.remove('tour-switching');
        }
        try { st.enter?.(ctx); } catch (_) { /* */ }
        renderStep();
        const primary = (st.targets?.(ctx) || []).find(Boolean);
        if (primary && shown(primary)) {
            const r = primary.getBoundingClientRect();
            if (r.top < 0 || r.bottom > window.innerHeight) primary.scrollIntoView({ block: 'nearest' });
        }
        tween = (!prep && lastUnion && !reducedMotion())
            ? { from: lastUnion, t0: performance.now(), dur: 280 } : null;
        focusSoon(bNext);
    }

    function isLast() {
        return nextIndex(steps.length, index, +1, k => avail[k]) >= steps.length;
    }

    function renderStep() {
        const st = steps[index];
        hEl.textContent = st.title;
        const { at, total } = stepCount(steps.length, index, k => avail[k]);
        nEl.textContent = T.nav.count(at, total);
        const b = st.body?.(ctx);
        if (b == null || typeof b === 'string') tourText(bEl, b);
        else bEl.replaceChildren(b);
        const first = nextIndex(steps.length, index, -1, k => avail[k]) < 0;
        const last = isLast();
        bBack.hidden = first;
        bBack.textContent = T.nav.back;
        bNext.textContent = last ? T.nav.done : T.nav.next;
        bSkip.hidden = last;
        bSkip.textContent = T.nav.skip;
        tip.dataset.step = st.id;
        lastPlace = null;
    }

    // While the layout switches between two steps, Next and Back wait for
    // it (Skip and Esc do not).
    function next() {
        if (phase !== 'steps' || switching) return;
        const j = nextIndex(steps.length, index, +1, k => avail[k]);
        if (j >= steps.length) finish('done');
        else go(j);
    }

    function back() {
        if (phase !== 'steps' || switching) return;
        const j = nextIndex(steps.length, index, -1, k => avail[k]);
        if (j >= 0) go(j);
    }

    // ── the end: into the ? ───────────────────────────────────────────

    function finish(kind) {
        if (!layer || phase === 'closing') return;
        cfg.record?.(kind);
        close(kind);
    }

    /// Leave, flying into the ?. `kind`: 'done' / 'skipped' (the tour
    /// ended), or null (the ?'s card put away: nothing to put back).
    async function close(kind) {
        const my = ++gen;
        if (phase === 'steps') { try { steps[index]?.leave?.(ctx, null); } catch (_) { /* */ } }
        phase = 'closing';
        // The sheet whole again at once: a second click on the edge the
        // listener has just pressed would land on what lies there now.
        passThrough(null);
        closingKey = performance.now() - lastDownAt < 150 ? lastDown : null;
        // The hole stays where it is while the dim fades, unless the layout
        // is going to move under it.
        let moves = false;
        if (begun) { begun = false; try { moves = !!cfg.end?.(kind || 'done'); } catch (_) { /* */ } }
        if (moves) drawShapes([], []);
        const help = cfg.help?.();
        await flyInto(help);
        if (my !== gen) return;
        teardown();
        glowHelp(help);
    }

    async function flyInto(help) {
        if (!tip) return;
        arrow.style.display = 'none';
        layer.classList.add('tour-out');
        const from = rectOf(tip);
        if (!shown(help) || reducedMotion()) {
            const a = tip.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 180, fill: 'forwards' });
            dim.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 220, fill: 'forwards' });
            await a.finished.catch(() => {});
            return;
        }
        const to = rectOf(help);
        const f = flyTransform(from, to);
        const dur = flyDuration(from, to);
        tip.style.transformOrigin = '50% 50%';
        const a = tip.animate([
            { transform: `translate(${from.x}px, ${from.y}px) scale(1)`, opacity: 1 },
            { transform: `translate(${from.x + f.dx * 0.55}px, ${from.y + f.dy * 0.45}px) scale(${Math.max(f.s, 0.42)})`, opacity: 0.9, offset: 0.55 },
            { transform: `translate(${from.x + f.dx}px, ${from.y + f.dy}px) scale(${f.s})`, opacity: 0.12 },
        ], { duration: dur, easing: 'cubic-bezier(.45,.05,.55,.95)', fill: 'forwards' });
        dim.animate([{ opacity: 1 }, { opacity: 0 }], { duration: Math.round(dur * 0.8), easing: 'ease-out', fill: 'forwards' });
        await a.finished.catch(() => {});
    }

    /// Down at once, no animation, nothing recorded (the test tool's rw
    /// marks the profile itself and takes the tour off the screen).
    function abort() {
        if (!layer) return false;
        gen++;
        if (phase === 'steps') { try { steps[index]?.leave?.(ctx, null); } catch (_) { /* */ } }
        if (begun) { begun = false; try { cfg.end?.('abort'); } catch (_) { /* */ } }
        teardown();
        return true;
    }

    function focusSoon(b) {
        requestAnimationFrame(() => { if (b && b.isConnected && !b.hidden) b.focus({ preventScroll: true }); });
    }

    return {
        offer,
        abort,
        isOpen: () => !!layer,
        /// Where it stands, and what the step points at (for the test tool).
        state: () => ({
            phase, step: steps[index]?.id ?? null, index, switching, gliding: !!tween,
            live: !!steps[index]?.live,
            targets: phase === 'steps' && cache ? cache.els.filter(shown).map(rectOf) : [],
        }),
        next, back,
        skip: () => (phase === 'offer' ? offerNo() : finish('skipped')),
        start: () => (phase === 'offer' ? offerYes() : false),
    };
}
