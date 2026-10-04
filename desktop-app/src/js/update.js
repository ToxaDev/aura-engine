// In-app update UI.
//
// Initialised from main.js after boot (~2 s delay). On a non-enabled build
// (no AURA_RELEASE_BUILD and no AURA_UPDATE_TEST_FEED) the check returns
// { enabled: false } and this module does nothing.
//
// Flow:
//   1. invoke('update_check')  → show badge + modal (if available, not skipped)
//   2. "Update now"            → invoke('update_install') + poll /update/status
//   3. App relaunches itself   → the modal is never seen again

import { state } from './state.js';

const { invoke } = window.__TAURI__.tauri;
const { shell } = window.__TAURI__;

// ── state ─────────────────────────────────────────────────────────────────

let _checkResult  = null;   // the last update_check() response
let _pollTimer    = null;   // setInterval for /update/status
let _busyTimer    = null;   // setInterval re-reading the conversion flag while the modal is open
let _installing   = false;  // from "Update now" until an error: the modal cannot be closed
let _modalShownThisLaunch = false;

// ── public entry point ────────────────────────────────────────────────────

/// Called from main.js after boot with a ~2 s delay.
export async function initUpdateChecker() {
    let result;
    try {
        result = await invoke('update_check');
    } catch (e) {
        // Network errors are silent; do nothing.
        return;
    }

    if (!result || !result.enabled) return;
    _checkResult = result;
    if (!result.available) return;

    // Show the update badge.
    showBadge(result);

    // Show the one-per-launch modal unless the user already skipped this tag.
    if (!_modalShownThisLaunch && !isSkipped(result.tag)) {
        _modalShownThisLaunch = true;
        openModal(result);
    }
}

// ── badge ─────────────────────────────────────────────────────────────────

function showBadge(info) {
    const versionEl = document.getElementById('appVersion');
    if (!versionEl) return;

    // Remove any previous badge.
    const prev = document.getElementById('upBadge');
    if (prev) prev.remove();

    const badge = document.createElement('span');
    badge.id = 'upBadge';
    badge.title = `Aura Engine ${info.version} is available — click to update`;
    badge.setAttribute('aria-label', badge.title);
    badge.setAttribute('role', 'button');
    badge.setAttribute('tabindex', '0');
    // Up-arrow SVG
    badge.innerHTML =
        '<svg width="8" height="9" viewBox="0 0 8 9" fill="none"' +
        ' xmlns="http://www.w3.org/2000/svg" aria-hidden="true">' +
        '<path d="M4 1v7M1 4l3-3 3 3" stroke="currentColor"' +
        ' stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>' +
        '</svg>';

    badge.addEventListener('click', () => openModal(_checkResult));
    badge.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            openModal(_checkResult);
        }
    });

    // Inside the version line, right after the number: the line spans the
    // panel's width, so a badge placed after the line lands at the far right.
    versionEl.appendChild(badge);
}

// ── modal ─────────────────────────────────────────────────────────────────

function buildModal() {
    if (document.getElementById('upModal')) return;

    const overlay = document.createElement('div');
    overlay.id = 'upModal';
    overlay.className = 'up-hidden';
    // Block drag-to-move so the frameless title bar doesn't respond.
    overlay.style.webkitAppRegion = 'no-drag';

    overlay.innerHTML =
        '<div id="upDialog" role="dialog" aria-modal="true"' +
        ' aria-labelledby="upDialogTitle">' +
        '  <p class="up-title" id="upDialogTitle">Update available</p>' +
        '  <p class="up-versions" id="upVersionLine"></p>' +
        '  <div id="upNotes" class="up-notes"></div>' +
        '  <div id="upProgress" style="display:none">' +
        '    <div id="upProgressBar"><div id="upProgressFill"></div></div>' +
        '    <div id="upProgressLabel"></div>' +
        '  </div>' +
        '  <div id="upErrorArea" style="display:none">' +
        '    <p class="up-title" id="upDialogTitle2">Update failed</p>' +
        '    <div id="upErrorMsg"></div>' +
        '    <div class="up-btns">' +
        '      <button id="upBtnOpenPage">Open download page</button>' +
        '      <button id="upBtnClose">Close</button>' +
        '    </div>' +
        '  </div>' +
        '  <div class="up-btns" id="upMainBtns">' +
        '    <button id="upBtnSkip">Skip this version</button>' +
        '    <button id="upBtnLater">Later</button>' +
        '    <button id="upBtnNow">Update now</button>' +
        '  </div>' +
        '</div>';

    document.body.appendChild(overlay);

    document.getElementById('upBtnLater').addEventListener('click', closeModal);
    document.getElementById('upBtnSkip').addEventListener('click', () => {
        if (_checkResult) {
            try {
                localStorage.setItem('auraUpdateSkip', _checkResult.tag);
            } catch (_) {}
        }
        closeModal();
    });
    document.getElementById('upBtnNow').addEventListener('click', startInstall);
    document.getElementById('upBtnClose').addEventListener('click', closeModal);
    document.getElementById('upBtnOpenPage').addEventListener('click', () => {
        if (_checkResult && _checkResult.url) {
            try { shell.open(_checkResult.url); } catch (_) {}
        }
    });

    // Esc = Later
    overlay.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') { e.stopPropagation(); closeModal(); }
    });
    // Clicks on the overlay backdrop (not the dialog) = Later
    overlay.addEventListener('click', (e) => {
        if (e.target === overlay) closeModal();
    });
}

function openModal(info) {
    if (!info || !info.available) return;
    buildModal();
    resetModalToInfo(info);
    const modal = document.getElementById('upModal');
    modal.classList.remove('up-hidden');
    // A conversion can start or finish while the dialog is open: keep the
    // button in step (a JS flag read, no backend call).
    if (!_busyTimer) _busyTimer = setInterval(refreshInstallButtonState, 500);
    // Focus the dialog for keyboard navigation.
    const btnNow = document.getElementById('upBtnNow');
    if (btnNow) btnNow.focus();
}

function closeModal() {
    // Mid-install the app is about to restart: Esc, the backdrop and every
    // other way out are refused, so nothing can be started behind the dialog.
    if (_installing) return;
    stopPoll();
    if (_busyTimer) { clearInterval(_busyTimer); _busyTimer = null; }
    const modal = document.getElementById('upModal');
    if (modal) modal.classList.add('up-hidden');
}

function resetModalToInfo(info) {
    const titleEl = document.getElementById('upDialogTitle');
    const vl = document.getElementById('upVersionLine');
    const notes = document.getElementById('upNotes');
    const mainBtns = document.getElementById('upMainBtns');
    const progressArea = document.getElementById('upProgress');
    const errorArea = document.getElementById('upErrorArea');

    // Restore visibility of elements that showError() may have hidden.
    if (titleEl)    titleEl.style.display = '';
    if (vl)         { vl.textContent = `Aura Engine ${info.version} is ready to install. You have ${info.current}.`; vl.style.display = ''; }
    if (notes)      renderNotes(info, notes);
    if (mainBtns)   mainBtns.style.display = '';
    if (progressArea) progressArea.style.display = 'none';
    if (errorArea)  errorArea.style.display = 'none';
    if (notes)      notes.style.display = '';

    // While a conversion runs, disable "Update now" with a reason.
    refreshInstallButtonState();
}

/// If a conversion is running, disable "Update now".
function refreshInstallButtonState() {
    const btn = document.getElementById('upBtnNow');
    if (!btn) return;
    // converter.js keeps state.convIsConverting for the whole batch. (The
    // backend refuses an install during a conversion anyway; this makes the
    // button say so before the click.)
    const busy = !!state.convIsConverting;
    btn.disabled = busy;
    btn.title = busy
        ? 'A conversion is in progress — please wait for it to finish before updating'
        : '';
}

// ── install flow ──────────────────────────────────────────────────────────

async function startInstall() {
    refreshInstallButtonState();
    const btn = document.getElementById('upBtnNow');
    if (btn && btn.disabled) return;

    _installing = true;
    showProgressUI('Connecting…', 0, 0);

    try {
        await invoke('update_install');
    } catch (e) {
        _installing = false;
        showError(String(e));
        return;
    }

    // The backend returned OK; the worker is running. Start polling.
    startPoll();
}

function startPoll() {
    stopPoll();
    _pollTimer = setInterval(pollStatus, 250);
}

function stopPoll() {
    if (_pollTimer !== null) {
        clearInterval(_pollTimer);
        _pollTimer = null;
    }
}

async function pollStatus() {
    let status;
    try {
        const resp = await fetch('https://aura.localhost/update/status');
        if (!resp.ok) return;
        status = await resp.json();
    } catch (_) {
        return;
    }

    const { phase, done, total, error } = status;

    switch (phase) {
        case 'checking':
            setProgressLabel('Checking…', 0, 0);
            break;
        case 'downloading':
            setProgressLabel(formatDownload(done, total), done, total);
            break;
        case 'verifying':
            setProgressLabel('Checking the signature…', 0, 0);
            break;
        case 'installing':
            setProgressLabel('Installing…', 0, 0);
            break;
        case 'restarting':
            setProgressLabel('Restarting…', 0, 0);
            break;
        case 'error':
            stopPoll();
            _installing = false;
            showError(error || 'Unknown error');
            break;
        default:
            break;
    }
}

function formatDownload(done, total) {
    const mb = (n) => (n / (1024 * 1024)).toFixed(1);
    if (total > 0) {
        return `Downloading ${mb(done)} of ${mb(total)} MB`;
    }
    return `Downloading… ${done > 0 ? mb(done) + ' MB' : ''}`;
}

function showProgressUI(label, done, total) {
    const mainBtns = document.getElementById('upMainBtns');
    const notes = document.getElementById('upNotes');
    const progressArea = document.getElementById('upProgress');
    const errorArea = document.getElementById('upErrorArea');

    if (mainBtns) mainBtns.style.display = 'none';
    if (notes)    notes.style.display = 'none';
    if (errorArea) errorArea.style.display = 'none';
    if (progressArea) {
        progressArea.style.display = '';
        // Why the dialog does not close now (see closeModal).
        let note = document.getElementById('upProgressNote');
        if (!note) {
            note = document.createElement('div');
            note.id = 'upProgressNote';
            note.style.cssText = 'margin-top:8px;font-size:0.8em;opacity:0.7';
            progressArea.appendChild(note);
        }
        note.textContent = 'Installing the update. Aura Engine will restart by itself; this window stays open until then.';
    }

    setProgressLabel(label, done, total);
}

function setProgressLabel(label, done, total) {
    const fill = document.getElementById('upProgressFill');
    const lbl  = document.getElementById('upProgressLabel');

    if (lbl) lbl.textContent = label;

    if (fill) {
        if (total > 0) {
            fill.classList.remove('up-indeterminate');
            fill.style.width = Math.round((done / total) * 100) + '%';
        } else {
            fill.classList.add('up-indeterminate');
            fill.style.width = '';
        }
    }
}

function showError(msg) {
    const mainBtns = document.getElementById('upMainBtns');
    const notes = document.getElementById('upNotes');
    const progressArea = document.getElementById('upProgress');
    const errorArea = document.getElementById('upErrorArea');
    const errorMsg = document.getElementById('upErrorMsg');
    const titleEl = document.getElementById('upDialogTitle');
    const vl = document.getElementById('upVersionLine');

    // Strip the trailing technical suffix (e.g. ': signature mismatch') and
    // capitalise so the message reads naturally. Only strip if the tail after
    // the last ': ' is short enough to be a one-phrase technical tag.
    msg = msg.trim();
    const ci = msg.lastIndexOf(': ');
    if (ci > 0 && msg.length - ci < 40) msg = msg.slice(0, ci).trim();
    if (msg.length > 0) msg = msg[0].toUpperCase() + msg.slice(1);
    if (msg && msg[msg.length - 1] !== '.') msg += '.';

    if (mainBtns)    mainBtns.style.display = 'none';
    if (notes)       notes.style.display = 'none';
    if (progressArea) progressArea.style.display = 'none';
    // The "Update available" title and version line belong to the ready state.
    if (titleEl)     titleEl.style.display = 'none';
    if (vl)          vl.style.display = 'none';
    if (errorArea)   errorArea.style.display = '';
    if (errorMsg)    errorMsg.textContent = msg;
}

// ── helpers ───────────────────────────────────────────────────────────────

function isSkipped(tag) {
    try {
        return localStorage.getItem('auraUpdateSkip') === tag;
    } catch (_) {
        return false;
    }
}

// ── structured notes rendering ─────────────────────────────────────────────

// Maximum bullets shown per group before the "and N more" link. With both
// groups present each one gets fewer, so the fixes are on screen without
// scrolling the notes.
const NOTES_MAX_PER_GROUP = 6;
const NOTES_MAX_PER_GROUP_BOTH = 4;

/// Render structured release notes into `el`.
/// Uses `info.release_notes` (array of {version, notes}) when available;
/// falls back to plain-text rendering of `info.notes`.
function renderNotes(info, el) {
    // Clear previous content safely.
    while (el.firstChild) el.removeChild(el.firstChild);
    el.className = 'up-notes';

    const rn = info.release_notes;
    if (Array.isArray(rn) && rn.length > 0) {
        renderStructured(rn, el, info.url || '');
    } else {
        renderPlain(info.notes || '', el, info.url || '');
    }
}

/// Parse a Markdown release body into { added: string[], fixed: string[] }.
/// Recognised headings (case-insensitive):
///   Added / New / Features  → added
///   Changed / Improved      → added  (improvements treated as new capabilities)
///   Fixed / Fixes / Bug fixes → fixed
/// Bullet formats: "- text", "* text", "1. text" (leading whitespace allowed).
function parseReleaseSections(md) {
    const added = [];
    const fixed = [];
    let current = null; // 'added' | 'fixed' | null

    for (const raw of md.split(/\r?\n/)) {
        const line = raw.trim();
        const heading = line.match(/^#{1,4}\s+(.+)$/);
        if (heading) {
            const title = heading[1].trim().toLowerCase();
            if (/^(added|new|features?)/.test(title) || /^(changed|improved)/.test(title)) {
                current = 'added';
            } else if (/^(fixed|fixes|bug\s*fixes?)/.test(title)) {
                current = 'fixed';
            } else {
                current = null;
            }
            continue;
        }
        if (current === null) continue;
        const bullet = line.match(/^(?:[-*+]|\d+\.)\s+(.+)$/);
        if (bullet) {
            const text = stripInlineMarkdown(bullet[1].trim());
            if (text) {
                if (current === 'added') added.push(text);
                else fixed.push(text);
            }
        }
    }
    return { added, fixed };
}

/// Remove HTML from release-note text: script and style blocks with their
/// content, comments, and every tag, keeping the text between them. A tag
/// must start with a letter (or "/" and a letter), so a plain "a < b" stays.
/// The result only ever goes into textContent.
function stripHtml(text) {
    return text
        .replace(/<(script|style)\b[^>]*>[\s\S]*?<\/\1\s*>/gi, '')
        .replace(/<!--[\s\S]*?-->/g, '')
        .replace(/<\/?[a-zA-Z][^<>]*>/g, '')
        .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"')
        .replace(/&#39;/g, "'").replace(/&nbsp;/g, ' ').replace(/&amp;/g, '&');
}

/// Strip inline Markdown (links, bold, italic, code) and HTML from a string.
function stripInlineMarkdown(text) {
    return stripHtml(text)
        .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1')
        .replace(/\*\*(.+?)\*\*/g, '$1')
        .replace(/\*(.+?)\*/g, '$1')
        .replace(/`([^`]+)`/g, '$1')
        .trim();
}

/// Build and append structured Added / Fixed sections to `el`.
function renderStructured(releases, el, releaseUrl) {
    const allAdded = [];
    const allFixed = [];

    for (const rel of releases) {
        const { added, fixed } = parseReleaseSections(rel.notes || '');
        for (const item of added) allAdded.push({ version: rel.version, text: item });
        for (const item of fixed) allFixed.push({ version: rel.version, text: item });
    }

    // If no headings matched at all, fall back to plain text of the first release.
    if (allAdded.length === 0 && allFixed.length === 0) {
        renderPlain(releases[0].notes || '', el, releaseUrl);
        return;
    }

    // Version tags only say something when the items come from more than one
    // release; for a single release they would repeat the same number.
    const tagged = new Set([...allAdded, ...allFixed].map(i => i.version)).size > 1;
    const max = allAdded.length > 0 && allFixed.length > 0
        ? NOTES_MAX_PER_GROUP_BOTH : NOTES_MAX_PER_GROUP;

    if (allAdded.length > 0) {
        el.appendChild(buildSection('What\'s new', allAdded, releaseUrl, false, max, tagged));
    }
    if (allFixed.length > 0) {
        el.appendChild(buildSection('Fixes', allFixed, releaseUrl, true, max, tagged));
    }
}

/// Build a section DOM node: title + bullet rows + optional "and N more" link.
/// `isLight` applies the dimmed style used for the Fixes group; `max` is how
/// many bullets are shown; `tagged` puts the release version before each one.
function buildSection(title, items, releaseUrl, isLight, max, tagged) {
    const section = document.createElement('div');
    section.className = 'up-notes-section' + (isLight ? ' up-notes-section--fixes' : '');

    const titleEl = document.createElement('div');
    titleEl.className = 'up-notes-section-title';
    titleEl.textContent = title;
    section.appendChild(titleEl);

    const shown = items.slice(0, max);
    const rest = items.length - shown.length;

    for (const item of shown) {
        const row = document.createElement('div');
        row.className = 'up-notes-item';

        let ver = null;
        if (tagged) {
            ver = document.createElement('span');
            ver.className = 'up-notes-version';
            ver.textContent = item.version;
        }

        const txt = document.createTextNode((ver ? ' ' : '') + item.text);

        if (ver) row.appendChild(ver);
        row.appendChild(txt);
        section.appendChild(row);
    }

    if (rest > 0) {
        const more = document.createElement('div');
        more.className = 'up-notes-more';

        if (releaseUrl) {
            const link = document.createElement('a');
            link.href = '#';
            link.textContent = 'and ' + rest + ' more';
            link.addEventListener('click', (e) => {
                e.preventDefault();
                try { shell.open(releaseUrl); } catch (_) {}
            });
            more.appendChild(link);
        } else {
            more.textContent = 'and ' + rest + ' more';
        }
        section.appendChild(more);
    }

    return section;
}

/// Render plain-text fallback (no recognised headings in the body).
/// Shows the stripped text in a scrollable block with a link to full notes.
function renderPlain(md, el, releaseUrl) {
    el.classList.add('up-notes--plain');
    const text = stripMarkdown(md);
    el.textContent = text || '(no release notes)';

    if (releaseUrl && text) {
        const link = document.createElement('a');
        link.href = '#';
        link.className = 'up-notes-full-link';
        link.textContent = 'Full release notes';
        link.addEventListener('click', (e) => {
            e.preventDefault();
            try { shell.open(releaseUrl); } catch (_) {}
        });
        el.appendChild(link);
    }
}

/// Convert a GitHub release body (Markdown) to readable plain text.
/// Only does the most common transforms; the result is shown in a div.
function stripMarkdown(md) {
    return stripHtml(md)
        .replace(/^#{1,6}\s+/gm, '')           // headings
        .replace(/\*\*(.+?)\*\*/g, '$1')        // bold
        .replace(/\*(.+?)\*/g, '$1')            // italic
        .replace(/`([^`]+)`/g, '$1')            // inline code
        .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1') // links
        .replace(/^\s*[-*+]\s+/gm, '• ')        // bullet lists
        .replace(/\r\n/g, '\n')
        .trim();
}
