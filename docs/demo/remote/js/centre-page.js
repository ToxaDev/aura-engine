// ══════════════════════════════════════════════════════════════════════
// One Library mode for the app and the streamer (Anton 22.11): in the
// Library window a window of the app — Cue Consistency, Crosstalk Cancel,
// the Equalizer — opens in the centre pane, over the library, the way the
// Analyzer page does: a bar with Back and the window's name, the window's
// own page in a frame under it. The compact window keeps its windows; the
// streamer's phone layout keeps its full-screen pages (remote.js).
//
// The frame is the window's page with `embedded` in its address: no title
// bar of its own (css/embedded.css), and in the app it takes the main
// window's bridge (js/vis/embed-shim.js), its Close coming back here.
// Back, Esc, the window's own Close, or a section picked at the side put
// it away.
// ══════════════════════════════════════════════════════════════════════

// UI texts.
export const PAGE_TEXTS = {
    back: 'Back',
    backTip: 'Back to the library (Esc)',
};

/// The windows that open as a page, by the command that opens one.
/// `page(args)`: the page's address.
export const WINDOW_PAGES = {
    cue_window_open: { title: 'Cue Consistency', page: () => 'cue.html' },
    xtc_geometry_open: { title: 'Crosstalk Cancel', page: () => 'xtc-geometry.html' },
    eq_window_open: { title: 'Equalizer', page: a => 'eq.html' + (a && a.tab ? '?tab=' + encodeURIComponent(a.tab) : '') },
};

/// The page's address in its frame: `embedded` added to its query.
export function embeddedUrl(page) {
    const p = String(page || '');
    return p + (p.includes('?') ? '&' : '?') + 'embedded';
}

const doc = () => globalThis.document;

/// The centre pane of the Library window, or null (the compact window, a
/// page without one).
export function centreHost() {
    const d = doc();
    if (!d?.body?.classList?.contains('win-landscape')) return null;
    return d.getElementById('lsCentre');
}

let open = null;   // { box, page, key, frame }

/// The page shown in the centre, or null.
export const centrePageNow = () => (open ? { page: open.page, title: open.title } : null);

/// Puts the page away (the library under it is as it was left).
export function closeCentrePage() {
    if (!open) return;
    const o = open;
    open = null;
    try { o.frame.contentWindow?.__auraOff?.(); } catch (_) {}
    o.box.remove();
    o.host.classList.remove('ls-paged');
}

/// Opens `page` with `title` in the centre pane. False where there is no
/// centre pane (the caller opens its window then). The same page again
/// stays as it is; another one takes its place.
export function openCentrePage(page, title) {
    const host = centreHost();
    if (!host) return false;
    const key = String(page).replace(/[?].*$/, '');
    if (open && open.key === key && open.box.isConnected) {
        if (open.page !== page) { open.page = page; open.frame.src = embeddedUrl(page); }
        return true;
    }
    closeCentrePage();
    const d = doc();
    const box = d.createElement('section');
    box.className = 'ls-page';
    box.dataset.page = key.replace(/\.html$/, '');
    const bar = d.createElement('header');
    bar.className = 'ls-page-bar';
    const back = d.createElement('button');
    back.type = 'button';
    back.className = 'ls-page-back';
    back.dataset.tip = PAGE_TEXTS.backTip;
    back.innerHTML = '<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 3L5 8l5 5"/></svg>';
    back.append(d.createTextNode(PAGE_TEXTS.back));
    back.addEventListener('click', closeCentrePage);
    const name = d.createElement('div');
    name.className = 'ls-page-title';
    name.textContent = title || '';
    bar.append(back, name);
    const frame = d.createElement('iframe');
    frame.className = 'ls-page-frame';
    frame.title = title || '';
    frame.src = embeddedUrl(page);
    frame.addEventListener('load', () => {
        // Esc inside the frame comes back here too.
        try {
            frame.contentWindow.addEventListener('keydown', e => { if (e.key === 'Escape' && !e.defaultPrevented) closeCentrePage(); });
            frame.contentWindow.focus();
        } catch (_) {}
    });
    box.append(bar, frame);
    host.append(box);
    host.classList.add('ls-paged');
    open = { box, page, key, frame, host, title };
    return true;
}

/// Opens the app's window `cmd` (WINDOW_PAGES) — in the centre pane in the
/// Library window, as its own window otherwise. Resolves as the command's
/// invoke does.
export function openAppWindow(cmd, args) {
    const w = WINDOW_PAGES[cmd];
    if (w && openCentrePage(w.page(args), w.title)) return Promise.resolve(null);
    const invoke = globalThis.window?.__TAURI__?.core?.invoke;
    return invoke ? invoke(cmd, args) : Promise.reject('no bridge');
}

let wired = false;
/// The page's ways back: the frame's Close (a message), Esc, a section
/// picked at the side, the window leaving the Library mode. Once.
export function wireCentrePages() {
    const w = globalThis.window;
    if (wired || !w) return;
    wired = true;
    w.addEventListener('message', m => {
        if (!open || !m.data) return;
        if ((m.data.remoteClose || m.data.auraClosePage) && m.source === open.frame.contentWindow) closeCentrePage();
    });
    w.addEventListener('keydown', e => { if (open && e.key === 'Escape' && !e.defaultPrevented) closeCentrePage(); });
    doc()?.addEventListener('click', e => {
        if (open && e.target?.closest?.('#lsSide button, #lsSide [data-tab], #lsSide a')) closeCentrePage();
    }, true);
}
