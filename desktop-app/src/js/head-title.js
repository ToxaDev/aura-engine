// The app's name in the head (Anton 23.11, mock aura-title-intro3, 3a).
//
// At start the letters come together one by one through a chromatic lens —
// each splits into cyan and violet and meets itself — and the version follows
// on the same wave. Then the name rests as "echo of the aura": copies falling
// below it, cyan going to violet, a soft halo under the letters. The echo
// breathes only while music plays (body.ht-play, player.js); paused, stopped
// or hidden it stands still. prefers-reduced-motion: no intro, no breathing.
//
// Only transform and opacity move after the intro, so the compositor does
// the work; the lens layers are the letters' own ::before/::after and are
// hidden (opacity 0) once the intro is over.

const STEP_MS = 65;      // one letter after the other (css/head-title.css)
const INTRO_MS = 2700;   // the intro's whole length, then the name rests
const ECHOES = 4;

const reduced = () => !!window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;

/// Letters as inline blocks: each carries its own index for the wave and its
/// character for the lens layers (data-c, read by ::before/::after).
function letters(text, from = 0) {
    const f = document.createDocumentFragment();
    [...text].forEach((ch, i) => {
        const s = document.createElement('span');
        s.className = 'ht-l';
        s.dataset.c = ch;
        s.style.setProperty('--i', String(from + i));
        s.textContent = ch;
        f.appendChild(s);
    });
    return f;
}

let nameLen = 0;

/// Replaces the heading's text with the layered name. Called by player.js
/// renameHeading once the text says "Aura Engine"; anything else in the
/// heading (the version's anchor) stays where it is.
export function mountHeadTitle(h2) {
    if (!h2 || h2.querySelector('.ht')) return;
    const text = [...h2.childNodes].find(n => n.nodeType === Node.TEXT_NODE && n.textContent.trim());
    if (!text) return;
    const name = text.textContent.trim();
    nameLen = name.length;

    const wrap = document.createElement('span');
    wrap.className = 'ht';
    const halo = wrap.appendChild(document.createElement('span'));
    halo.className = 'ht-halo';
    halo.setAttribute('aria-hidden', 'true');
    for (let k = 1; k <= ECHOES; k++) {
        const e = wrap.appendChild(document.createElement('span'));
        e.className = 'ht-e';
        e.setAttribute('aria-hidden', 'true');
        e.style.setProperty('--k', String(k));
        e.appendChild(letters(name));
    }
    const main = wrap.appendChild(document.createElement('span'));
    main.className = 'ht-main';
    main.appendChild(letters(name));

    h2.setAttribute('aria-label', name);
    text.replaceWith(wrap);
    h2.classList.add('ht-on');

    if (!reduced()) {
        h2.classList.add('ht-in');
        setTimeout(() => h2.classList.remove('ht-in'), INTRO_MS);
    }
    document.addEventListener('visibilitychange', () =>
        document.body.classList.toggle('ht-hidden', document.hidden));
}

/// The version's text (main.js headVersion), as letters that follow the
/// name's wave when it arrives during the intro.
export function setHeadVersionText(el, v) {
    if (!el) return;
    el.textContent = '';
    el.appendChild(letters(v, nameLen + 1));
}
