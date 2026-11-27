// The report window and the offer after a bad end. The decisions are in
// crash-report-flow.js; this draws them.

import { createReportFlow, RETRY_EVERY_MS } from './crash-report-flow.js';
import { headCap } from './perf-badge.js';

// The head's report button: a bug, drawn in the line's colour.
const BUG_SVG = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M6 4.6a2 2 0 0 1 4 0"/><path d="M6.6 3.1 5.4 1.7M9.4 3.1l1.2-1.4"/>'
    + '<rect x="4.7" y="4.9" width="6.6" height="9" rx="3.3"/><path d="M8 7.2v6.4"/>'
    + '<path d="M4.7 8H2.4M4.7 10.6H2.2M5 12.9l-2 1.1M11.3 8h2.3M11.3 10.6h2.5M11 12.9l2 1.1"/></svg>';

const INCLUDED = [
    'Program version, OS, CPU, memory, video card',
    'Sound settings and output format (no device names)',
    'Tracks as numbers: format, rate, length, size',
    'What the program was doing, and the end of its log with private text replaced',
    'What you write below',
];
const NEVER = [
    'File names, folders, titles, artists, albums',
    'Your user and computer names, device names, radio addresses',
    'Any lasting ID of you or your computer: each report has only its own random code',
];
const PRIVACY_URL = 'https://aura-engine.app/privacy.html';
/// How the last session ended, in the window's first line.
const ENDED = {
    stall: 'It stopped responding.',
    panic: 'It crashed.',
    hw_fault: 'It crashed.',
    unclean_exit: 'It stopped without closing: a crash, a freeze or a power cut.',
};

function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
}

function button(text, onClick, cls = 'crp-btn') {
    const b = el('button', cls, text);
    b.type = 'button';
    b.addEventListener('click', onClick);
    return b;
}

function list(title, items) {
    const box = el('div', 'crp-list');
    box.appendChild(el('div', 'crp-list-title', title));
    const ul = el('ul');
    items.forEach(i => ul.appendChild(el('li', '', i)));
    box.appendChild(ul);
    return box;
}

async function copy(text) {
    try {
        await navigator.clipboard.writeText(text);
    } catch (_) {
        await window.__TAURI__?.clipboardManager?.writeText(text);
    }
}

function notSent(error, kept) {
    return `Could not send it: ${error}.${kept ? ` It waits, still encrypted, and will be tried again by itself for up to 14 days. To cancel, delete ${kept}.` : ''}`;
}

function makeView(actions) {
    const bar = el('div', 'crp-bar');
    bar.hidden = true;
    const barText = el('span', 'crp-bar-text', 'The last session ended unexpectedly — send a report?');
    const barLook = button('Send report…', () => actions().open('last'));
    const barNo = button('Not now', () => actions().decline());
    const barClose = button('Close', () => { bar.hidden = true; });
    barClose.hidden = true;
    bar.append(barText, barLook, barNo, barClose);

    const shade = el('div', 'crp-shade');
    shade.hidden = true;
    const win = el('div', 'crp-win');
    win.setAttribute('role', 'dialog');
    win.setAttribute('aria-modal', 'true');
    const title = el('h3', 'crp-title');
    const lead = el('p', 'crp-lead');
    win.append(title, lead);

    const lists = el('div', 'crp-lists');
    lists.append(list('Included', INCLUDED), list('Never included', NEVER));
    win.appendChild(lists);

    const privacy = el('p', 'crp-privacy', 'Sealed so that only the developer can read it, and deleted after 90 days. If it cannot go right away, it waits on this computer and is tried again for up to 14 days. ');
    const privacyLink = el('a', '', 'Privacy');
    privacyLink.href = PRIVACY_URL;
    privacyLink.addEventListener('click', e => {
        e.preventDefault();
        window.__TAURI__.shell.open(PRIVACY_URL);
    });
    privacy.appendChild(privacyLink);
    win.appendChild(privacy);

    const noteLabel = el('label', 'crp-label', 'What happened (optional)');
    const note = el('textarea', 'crp-note');
    note.rows = 3;
    note.placeholder = 'What you were doing. Please no personal information.';
    noteLabel.appendChild(note);
    win.appendChild(noteLabel);

    const preview = el('pre', 'crp-preview');
    preview.hidden = true;
    win.appendChild(preview);

    const autoLabel = el('label', 'crp-auto');
    const auto = el('input');
    auto.type = 'checkbox';
    auto.addEventListener('change', () => actions().setAuto(auto.checked));
    autoLabel.append(auto, document.createTextNode(' Always send automatically after a crash or a hang'));
    win.appendChild(autoLabel);

    const result = el('div', 'crp-result');
    win.appendChild(result);

    const row = el('div', 'crp-row');
    const showAll = button('Show everything', () => actions().show());
    const save = button('Save to file', () => actions().save());
    const send = button('Send', () => actions().send(), 'crp-btn crp-primary');
    const close = button('Close', () => actions().dismiss());
    let gone = false;   // this report has gone: Send has nothing more to do
    row.append(showAll, save, close, send);
    win.appendChild(row);
    shade.appendChild(win);
    document.body.append(bar, shade);

    const say = (text, code) => {
        result.textContent = text;
        if (code) {
            result.appendChild(document.createTextNode(' '));
            result.appendChild(el('b', '', code));
            result.appendChild(document.createTextNode(' '));
            result.appendChild(button('Copy', () => copy(code)));
        }
    };

    return {
        hideBar() { bar.hidden = true; },
        showWindow(kind) {
            gone = false;
            send.disabled = false;
            title.textContent = kind ? 'Last time Aura Engine ended with a critical error' : 'Report a problem';
            lead.textContent = kind ? `${ENDED[kind] || ENDED.unclean_exit} Send the report to the developer now?` : '';
            lead.hidden = !kind;
            close.textContent = kind ? 'Not now' : 'Close';
            win.setAttribute('aria-label', title.textContent);
            preview.hidden = true;
            result.textContent = '';
            shade.hidden = false;
        },
        hideWindow() { shade.hidden = true; },
        showPreview(json) {
            preview.textContent = json;
            preview.hidden = false;
        },
        busy(on) {
            [showAll, save].forEach(b => { b.disabled = on; });
            send.disabled = on || gone;
        },
        sent(code) {
            gone = true;
            close.textContent = 'Close';
            say('Sent, thank you. If you write to us about it, quote this code:', code);
        },
        saved(code, path) {
            close.textContent = 'Close';
            say(`Saved as ${path}. Only we can read it. Its code:`, code);
        },
        failed(error, kept) {
            shade.hidden = false;
            // It waits and goes by itself: "not now" would promise otherwise.
            if (kept) close.textContent = 'Close';
            say(notSent(error, kept));
        },
        autoFailed(error, kept) {
            barText.textContent = `The last session ended unexpectedly. ${notSent(error, kept)}`;
            barLook.hidden = !!kept;
            barNo.hidden = !!kept;
            barClose.hidden = !kept;
            bar.hidden = false;
        },
        autoSent(code) {
            barText.textContent = `The last session ended unexpectedly. A report was sent (you asked for this): ${code}`;
            barLook.hidden = true;
            barNo.hidden = true;
            barClose.hidden = false;
            bar.hidden = false;
        },
        setAuto(on) { auto.checked = on; },
        note() { return note.value; },
    };
}

function makeEntry(onClick) {
    // A drawn bug at the end of the head's capsule (perf-badge.js headCap;
    // Anton 23.11: it was a "!").
    const host = headCap();
    if (!host) return;
    const b = el('button', 'crp-entry hd-ib');
    b.innerHTML = BUG_SVG;
    b.type = 'button';
    b.id = 'reportProblem';
    b.setAttribute('aria-label', 'Report a problem');
    b.dataset.tip = 'Report a problem';
    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();   // Space stays the player's
        onClick();
    });
    host.appendChild(b);
}

export function initCrashReport() {
    let flow = null;
    const view = makeView(() => flow);
    flow = createReportFlow({
        invoke: window.__TAURI__.core.invoke,
        view,
        storage: window.localStorage,
    });
    makeEntry(() => flow.open('current'));
    setInterval(() => flow.retryOutbox(), RETRY_EVERY_MS);
    return flow.start().finally(() => flow.retryOutbox());
}
