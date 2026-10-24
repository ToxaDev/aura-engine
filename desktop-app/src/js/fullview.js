
// ══════════════════════════════════════════════════════════════════════
// The big player's picture on the whole screen (Anton 27.09, an
// experiment). A button at the left end of the look switch's row; the window goes full
// screen and shows what the player shows there — the waves, or the cover
// when Album shows one — with the title top left, the time top right, and
// the transport at the bottom in the middle. While a track plays and the
// pointer rests, all of that goes; a move brings it back. Paused or
// stopped, it stays. Escape or the same button: back.
//
// The waves are not drawn twice: their element moves into the view and
// back (waves.js sizes its canvas from it every frame). The transport here
// presses the player's own buttons.
// ══════════════════════════════════════════════════════════════════════

const $ = id => document.getElementById(id);
const IDLE_MS = 2500;

export const SVG_FULL = '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" '
    + 'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M7 1h4v4M11 1 7.2 4.8M5 11H1V7M1 11l3.8-3.8"/></svg>';
const SVG_BACK = '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" '
    + 'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M11 5H7V1M7 5l3.8-3.8M1 7h4v4M5 7 1.2 10.8"/></svg>';

/// bar: #plBar; wrap: the waves' element; button: the button in the
/// player; state(): the player's state; isListening(): the big player is up.
export function initFullView({ bar, wrap, button, state, isListening }) {
    if (!bar || !wrap || !button) return;
    const appWindow = window.__TAURI__?.window?.appWindow;
    let view = null, open = false, busy = false, home = null, idleTimer = null, tick = null;

    const build = () => {
        view = document.createElement('div');
        view.id = 'fvView';
        view.className = 'fv-view';
        view.innerHTML = `
            <img class="fv-cover-bg" alt="">
            <img class="fv-cover" alt="">
            <div class="fv-stage"></div>
            <div class="fv-ui">
                <div class="fv-title"><div class="fv-name"></div><div class="fv-tags"></div></div>
                <div class="fv-time"></div>
                <div class="fv-transport">
                    <button type="button" class="fv-btn" data-for="plPrev" aria-label="Previous"></button>
                    <button type="button" class="fv-btn fv-main" data-for="plPlay" aria-label="Play or pause"></button>
                    <button type="button" class="fv-btn" data-for="plNext" aria-label="Next"></button>
                </div>
                <button type="button" class="fv-exit" aria-label="Back from full screen" data-tip="Back (Esc)">${SVG_BACK}</button>
            </div>`;
        document.body.appendChild(view);
        view.querySelectorAll('.fv-btn').forEach(b => b.addEventListener('click', () => $(b.dataset.for)?.click()));
        view.querySelector('.fv-exit').addEventListener('click', () => close());
        view.addEventListener('mousemove', wake);
        view.addEventListener('pointerdown', wake);
    };

    const playing = () => state() === 'playing';
    // Everything up; while a track plays, gone again after the pointer rests.
    const wake = () => {
        if (!open) return;
        view.classList.add('fv-awake');
        clearTimeout(idleTimer);
        idleTimer = setTimeout(() => { if (playing()) view.classList.remove('fv-awake'); }, IDLE_MS);
    };

    // The texts, the transport's icons, cover or waves — four times a second.
    const refresh = () => {
        const q = sel => view.querySelector(sel);
        q('.fv-name').textContent = $('plTitle')?.textContent || '';
        const tags = [$('plTagsArtist')?.textContent, $('plTagsAlbum')?.textContent].filter(Boolean).join(' · ');
        q('.fv-tags').textContent = tags;
        q('.fv-time').textContent = $('plTime')?.textContent || '';
        view.querySelectorAll('.fv-btn').forEach(b => {
            const src = $(b.dataset.for);
            if (src && b._html !== src.innerHTML) { b._html = src.innerHTML; b.innerHTML = src.innerHTML; }
        });
        // What the player shows where the picture lies: the cover (Album on
        // a track that has one) or the waves.
        const art = $('plArtWrap'), img = $('plArt');
        const cover = !!art && !!art.offsetParent && getComputedStyle(art).display !== 'none' && !!img?.getAttribute('src');
        view.classList.toggle('fv-has-cover', cover);
        if (cover) {
            const src = img.src;
            for (const c of view.querySelectorAll('.fv-cover, .fv-cover-bg')) if (c.src !== src) c.src = src;
        }
        wrap.dataset.full = cover ? '' : '1';
        // Not playing: nothing hides.
        if (!playing()) { view.classList.add('fv-awake'); clearTimeout(idleTimer); idleTimer = null; }
        else if (!idleTimer && view.classList.contains('fv-awake')) wake();
        if (!isListening()) close();
    };

    const onKey = ev => {
        if (!open) return;
        if (ev.key === 'Escape') { ev.preventDefault(); ev.stopImmediatePropagation(); close(); return; }
        // The mode switch waits: the picture belongs to the big player.
        if (ev.code === 'KeyL') { ev.preventDefault(); ev.stopImmediatePropagation(); }
        wake();
    };

    async function openView() {
        if (open || busy) return;
        busy = true;
        if (!view) build();
        home = { parent: wrap.parentNode, next: wrap.nextSibling };
        view.querySelector('.fv-stage').appendChild(wrap);
        document.body.classList.add('fullview');
        view.classList.add('fv-open', 'fv-awake');
        open = true;
        refresh();
        tick = setInterval(refresh, 250);
        wake();
        try { await appWindow?.setFullscreen(true); } catch (_) {}
        busy = false;
    }

    async function close() {
        if (!open || busy) return;
        busy = true;
        open = false;
        clearInterval(tick);
        clearTimeout(idleTimer);
        idleTimer = null;
        try { await appWindow?.setFullscreen(false); } catch (_) {}
        view.classList.remove('fv-open', 'fv-awake', 'fv-has-cover');
        delete wrap.dataset.full;
        if (home?.parent) home.parent.insertBefore(wrap, home.next && home.next.parentNode === home.parent ? home.next : null);
        document.body.classList.remove('fullview');
        busy = false;
        window.fitWindowAfterFull?.();
    }

    button.addEventListener('click', ev => {
        if (ev.detail > 0) button.blur();   // Space stays play/pause
        if (open) close(); else openView();
    });
    window.addEventListener('keydown', onKey, true);

    // For our checks.
    button.__fullView = { open: openView, close, isOpen: () => open };
}
