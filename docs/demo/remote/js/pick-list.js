// A list that drops down from a button, drawn by the page: its rows can
// carry their own tooltips (a system list's rows cannot, and no tooltip
// covers a system list), their own colours and marks. The list stands in the
// page's top layer (a manual popover) under the button, or over it when
// there is no room under; the tooltips are lifted above it (tip-place.js).
//
// Keys on the button: ↓ / ↑ / Enter / Space open it; while open ↓ ↑ Home End
// move over the rows that can be chosen, Enter / Space choose, Escape and
// Tab close. A press outside, a scroll outside, the window losing the focus
// or its size close it too.
//
// Items: { value, text, cls?, disabled?, tip? } or { sep: true }.

/// The index of the next row that can be chosen from `from`, `dir` +1 or −1,
/// not past either end: `from` itself when there is none. From −1 down (or
/// `items.length` up) it finds the first (or the last) one.
export function stepIndex(items, from, dir) {
    for (let i = from + dir; i >= 0 && i < items.length; i += dir) {
        const it = items[i];
        if (it && !it.sep && !it.disabled) return i;
    }
    return from;
}

/// Where the list goes for a button at `r` (its client rect), the list
/// `h` px tall, in a window `vh` px tall: under the button when it fits
/// there or there is more room under, else over it. { top, up }.
export function dropPlace(r, h, vh, gap = 3, margin = 6) {
    const under = vh - margin - (r.bottom + gap);
    const over = r.top - gap - margin;
    const up = h > under && over > under;
    return { top: up ? Math.max(margin, r.top - gap - h) : r.bottom + gap, up };
}

/**
 * Turns `button` into the opener of a list. `onPick(value)` gets a chosen
 * row's value; `canOpen()` false keeps it shut (the caller says why);
 * `onToggle(open)` hears it open and close. Returns { setItems(items,
 * value), open(), close(), isOpen(), el }.
 */
export function createPickList(button, { onPick, canOpen = () => true, onToggle = () => {}, className = '' } = {}) {
    const doc = button.ownerDocument;
    const list = doc.createElement('div');
    list.className = `pick-list ${className}`.trim();
    list.setAttribute('role', 'listbox');
    list.id = `${button.id || 'pick'}List`;
    const pop = typeof list.showPopover === 'function';
    if (pop) list.setAttribute('popover', 'manual');
    doc.body.appendChild(list);
    button.setAttribute('aria-haspopup', 'listbox');
    button.setAttribute('aria-expanded', 'false');
    button.setAttribute('aria-controls', list.id);

    let items = [];
    let value = null;
    let active = -1;
    let shown = false;

    const rows = () => list.querySelectorAll('[data-i]');

    function paintActive() {
        for (const row of rows()) row.classList.toggle('active', Number(row.dataset.i) === active);
        const row = list.querySelector(`[data-i="${active}"]`);
        row?.scrollIntoView?.({ block: 'nearest' });
        if (row) button.setAttribute('aria-activedescendant', row.id);
        else button.removeAttribute('aria-activedescendant');
    }

    function draw() {
        list.replaceChildren(...items.map((it, i) => {
            if (it.sep) {
                const hr = doc.createElement('div');
                hr.className = 'pick-sep';
                hr.setAttribute('role', 'separator');
                return hr;
            }
            const row = doc.createElement('div');
            row.className = `pick-row ${it.cls || ''}`.trim();
            row.id = `${list.id}-${i}`;
            row.dataset.i = String(i);
            row.setAttribute('role', 'option');
            row.setAttribute('aria-selected', it.value === value ? 'true' : 'false');
            if (it.disabled) row.setAttribute('aria-disabled', 'true');
            if (it.tip) row.dataset.tip = it.tip;
            row.textContent = it.text;
            return row;
        }));
        if (shown) paintActive();
    }

    function place() {
        const r = button.getBoundingClientRect();
        list.style.left = '0px';
        list.style.top = '0px';
        list.style.minWidth = `${Math.round(r.width)}px`;
        const h = list.getBoundingClientRect().height;
        const vw = doc.defaultView.innerWidth, vh = doc.defaultView.innerHeight;
        const w = list.getBoundingClientRect().width;
        const p = dropPlace(r, h, vh);
        list.style.left = `${Math.round(Math.max(6, Math.min(r.left, vw - w - 6)))}px`;
        list.style.top = `${Math.round(p.top)}px`;
        list.classList.toggle('up', p.up);
    }

    function open() {
        if (shown || !canOpen()) return;
        shown = true;
        if (pop) list.showPopover();
        list.classList.add('open');
        button.setAttribute('aria-expanded', 'true');
        button.classList.add('open');
        const at = items.findIndex(it => !it.sep && it.value === value);
        active = at >= 0 && !items[at].disabled ? at : stepIndex(items, -1, 1);
        place();
        paintActive();
        onToggle(true);
    }

    function close() {
        if (!shown) return;
        shown = false;
        list.classList.remove('open');
        if (pop) try { list.hidePopover(); } catch (_) { /* gone already */ }
        button.setAttribute('aria-expanded', 'false');
        button.classList.remove('open');
        button.removeAttribute('aria-activedescendant');
        onToggle(false);
    }

    function pick(i) {
        const it = items[i];
        if (!it || it.sep || it.disabled) return;
        close();
        onPick?.(it.value);
    }

    button.addEventListener('click', () => (shown ? close() : open()));
    button.addEventListener('keydown', e => {
        const k = e.key;
        if (!shown) {
            if (k === 'ArrowDown' || k === 'ArrowUp' || k === 'Enter' || k === ' ') {
                e.preventDefault();
                open();
            }
            return;
        }
        if (k === 'Escape' || k === 'Tab') {
            if (k === 'Escape') e.preventDefault();
            close();
        } else if (k === 'ArrowDown' || k === 'ArrowUp') {
            e.preventDefault();
            active = stepIndex(items, active, k === 'ArrowDown' ? 1 : -1);
            paintActive();
        } else if (k === 'Home' || k === 'End') {
            e.preventDefault();
            active = k === 'Home' ? stepIndex(items, -1, 1) : stepIndex(items, items.length, -1);
            paintActive();
        } else if (k === 'Enter' || k === ' ') {
            e.preventDefault();
            pick(active);
        }
    });
    list.addEventListener('mousedown', e => e.preventDefault());   // the focus stays on the button
    list.addEventListener('click', e => {
        const row = e.target.closest?.('[data-i]');
        if (row) pick(Number(row.dataset.i));
    });
    list.addEventListener('mousemove', e => {
        const row = e.target.closest?.('[data-i]');
        const i = row ? Number(row.dataset.i) : -1;
        if (i >= 0 && i !== active && !items[i]?.disabled) {
            active = i;
            paintActive();
        }
    });
    doc.addEventListener('pointerdown', e => {
        if (shown && !list.contains(e.target) && !button.contains(e.target)) close();
    }, true);
    doc.addEventListener('scroll', e => { if (shown && !list.contains(e.target)) close(); }, true);
    doc.defaultView.addEventListener('blur', close);
    doc.defaultView.addEventListener('resize', close);

    return {
        el: list,
        setItems(next, v) {
            items = next;
            value = v;
            draw();
        },
        open,
        close,
        isOpen: () => shown,
    };
}
