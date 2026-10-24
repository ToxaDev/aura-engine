// A quiet line in the player while the app updates the instruments pack by
// itself (after an app update, for a user who took the pack): at the right
// end of the status line, gone when the update is done. Everything else
// about the pack lives in the visualization studio (vis/pack.js).

const T = window.__TAURI__;
const RUNNING = new Set(['downloading', 'verifying', 'unpacking']);

/// The line's span, put in the player's status line once that is built.
function span() {
    let el = document.getElementById('plPackNote');
    if (el) return el;
    const line = document.getElementById('plStatus');
    if (!line) return null;
    el = document.createElement('span');
    el.id = 'plPackNote';
    el.className = 'pl-pack-note';
    el.hidden = true;
    line.appendChild(el);
    return el;
}

/// The words give way (…) before the percent does: a narrow window still
/// shows how far it is.
function show(s) {
    const el = span();
    if (!el) return;
    const on = !!s?.busy && !!s.background && RUNNING.has(s.state);
    const p = on && s.total ? Math.min(100, Math.floor((100 * s.got) / s.total)) : 0;
    el.textContent = '';
    if (on) {
        const words = document.createElement('span');
        words.className = 'w';
        words.textContent = 'Updating the instruments pack…';
        const pct = document.createElement('span');
        pct.className = 'p';
        pct.textContent = ` ${p} %`;
        el.append(words, pct);
        el.title = `Updating the instruments pack… ${p} %`;
    }
    el.hidden = !on;
}

if (T?.event) {
    T.event.listen('spatial:progress', e => show(e.payload)).catch(() => {});
    T.tauri.invoke('spatial_status').then(show).catch(() => {});
}
