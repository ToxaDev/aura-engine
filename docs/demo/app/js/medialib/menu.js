// ══════════════════════════════════════════════════════════════════════
// The library's menu: what can be done with an album or a track, by the
// right button or the row's "more" button. One at a time; a press outside,
// Escape, a scroll or the window losing focus shuts it.
//
// Items: { text, tap, cls?, hint? } | { sep: true } | { group: 'words' }.
// ══════════════════════════════════════════════════════════════════════

let open = null;

export function closeMenu() {
    if (!open) return;
    const { box, off } = open;
    open = null;
    off();
    box.remove();
}

/// Where a box of w × h goes for the point (x, y) in a window of vw × vh:
/// down and right of the point, turned back where it would leave the window.
export function menuPlace(x, y, w, h, vw, vh, margin = 6) {
    let left = x, top = y;
    if (left + w > vw - margin) left = Math.max(margin, x - w);
    if (top + h > vh - margin) top = Math.max(margin, Math.min(y - h, vh - margin - h));
    if (top < margin) top = margin;
    return { left: Math.round(left), top: Math.round(top) };
}

/// Opens the menu at a point ({ x, y }) or under an element.
export function openMenu(at, items) {
    closeMenu();
    const box = document.createElement('div');
    box.className = 'ml-menu';
    box.setAttribute('role', 'menu');
    for (const it of items) {
        if (!it) continue;
        if (it.sep) { box.append(Object.assign(document.createElement('div'), { className: 'ml-menu-sep' })); continue; }
        if (it.group != null) { box.append(Object.assign(document.createElement('div'), { className: 'ml-menu-group', textContent: it.group })); continue; }
        const b = document.createElement('button');
        b.type = 'button';
        b.className = `ml-menu-row ${it.cls || ''}`.trim();
        b.setAttribute('role', 'menuitem');
        const t = document.createElement('span');
        t.textContent = it.text;
        b.append(t);
        if (it.hint) b.append(Object.assign(document.createElement('i'), { textContent: it.hint }));
        b.addEventListener('click', e => { e.stopPropagation(); closeMenu(); it.tap?.(); });
        box.append(b);
    }
    document.body.append(box);
    let x, y;
    if (at instanceof Element) {
        const r = at.getBoundingClientRect();
        x = r.left; y = r.bottom + 4;
    } else { x = at.x; y = at.y; }
    const p = menuPlace(x, y, box.offsetWidth, box.offsetHeight, window.innerWidth, window.innerHeight);
    box.style.left = p.left + 'px';
    box.style.top = p.top + 'px';

    const down = e => { if (!box.contains(e.target)) closeMenu(); };
    const key = e => { if (e.key === 'Escape') { e.stopPropagation(); closeMenu(); } };
    const gone = () => closeMenu();
    // After this press is over: the press that opens it is not one outside.
    const t = setTimeout(() => {
        document.addEventListener('mousedown', down, true);
        document.addEventListener('wheel', gone, { capture: true, passive: true });
    }, 0);
    document.addEventListener('keydown', key, true);
    window.addEventListener('blur', gone);
    window.addEventListener('resize', gone);
    open = {
        box,
        off() {
            clearTimeout(t);
            document.removeEventListener('mousedown', down, true);
            document.removeEventListener('wheel', gone, { capture: true });
            document.removeEventListener('keydown', key, true);
            window.removeEventListener('blur', gone);
            window.removeEventListener('resize', gone);
        },
    };
    return box;
}
