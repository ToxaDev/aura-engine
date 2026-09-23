
import { state } from './state.js';
import { startConversion } from './converter.js';
import { chainBadgesHtml } from './dsprack.js';

const { invoke } = window.__TAURI__.tauri;

export function syncEmptyState() {
    const isEmpty = state.convFileQueue.length === 0 && state.convNextQueue.length === 0;
    document.getElementById('dzEmpty')?.classList.toggle('hidden', !isEmpty);
}
window._dzSyncEmpty = syncEmptyState;

// changedIdxs: optional Set of queue indices whose state changed since the
// last render. When given (from the delta poll) and the DOM rows already
// exist, only those rows are updated — a 685-file queue polled at 5 Hz then
// touches a handful of DOM nodes per tick instead of all of them.
export function renderFileQueue(changedIdxs = null) {
    const container = document.getElementById('fileQueue');
    const allItems = [
        ...state.convFileQueue.map(f => ({ ...f, batch: 'current' })),
        ...state.convNextQueue.map(f  => ({ ...f, batch: 'next'    }))
    ];

    const getProps = (f) => {
        let icon = '○', cls = '', badge = '', action = 'remove', title = 'Remove', showClose = true;
        if (f.dismissed) {
            showClose = false; action = '';
            icon = f.status === 'active' ? '⏳' : '–';
            cls = f.status === 'active' ? 'dismissing' : 'cancelled';
        } else {
            if (f.batch === 'next') { icon = '⏳'; cls = 'queued'; }
            else if (f.status === 'done') { icon = '✓'; cls = 'done'; showClose = false; action = ''; title = 'File successfully converted.'; }
            else if (f.status === 'cancelled') { icon = '–'; cls = 'cancelled'; showClose = false; action = ''; }
            else if (f.status === 'active') { icon = '▶'; cls = 'active'; action = 'cancel-one'; title = 'Cancel'; }
            else if (f.status === 'error') {
                icon = '✗'; cls = 'error';
                // The backend message names the missing filter and where it
                // looked; a bare "error" would send the user to the console.
                title = f.errorMsg || 'Error during conversion';
            }
        }
        
        // The second line of the row is the chain: every stage that was
        // asked for, in the order the pipeline runs them, each saying whether
        // it ran, was reached and declined, or has not been reached yet.
        // Built in dsprack.js, which owns the chain order and the colours.
        if (f.badge === 1) { icon = '✗'; cls = 'error'; showClose = false; action = ''; }
        else if (f.badge === 2) { icon = '–'; cls = 'cancelled'; showClose = false; action = ''; }
        else if (f.badge === 3) { icon = '⚠'; showClose = false; action = ''; }

        const badgeHtml = f.batch === 'next'
            ? '<span class="cb" data-lab-why="Queued for the next batch — nothing has run on it yet." '
              + 'data-lab-tok="NEXT" data-lab-name="Queued">&rarr; next</span>'
            : chainBadgesHtml(f);
        const pct = f.filePct != null ? f.filePct : (cls === 'active' ? state.convCurrentFilePct : (cls === 'done' ? 100 : 0));
        return { icon, cls, badgeHtml, action, title, showClose, pct };
    };


    const existing = container.querySelectorAll('.file-item');

    if (existing.length === allItems.length && allItems.length > 0) {
        allItems.forEach((f, i) => {
            if (changedIdxs && !changedIdxs.has(i)) return;
            const el = existing[i];
            const { icon, cls, badgeHtml, action, title, showClose, pct } = getProps(f);
            el.className = `file-item ${cls}`;

            const progEl  = el.querySelector('.file-item-progress');
            if (progEl) progEl.style.width = pct.toFixed(1) + '%';

            const iconEl  = el.querySelector('.file-item-icon');
            const nameEl  = el.querySelector('.file-item-name');
            const badgeEl = el.querySelector('.badge-container');
            const closeEl = el.querySelector('.file-item-close');
            const divEl   = el.querySelector('.file-item-divider');
            
            if (iconEl) {
                iconEl.textContent = icon;
                iconEl.title = title;
            }
            if (nameEl) {
                nameEl.textContent = f.name;
                nameEl.title = f.path;
            }
            if (badgeEl) badgeEl.innerHTML = badgeHtml;
            if (closeEl) {
                closeEl.dataset.action = action;
                closeEl.dataset.idx    = i;
                closeEl.title          = title;
                closeEl.style.display  = showClose ? 'flex' : 'none';
            }
            if (divEl) {
                divEl.style.display    = showClose ? 'block' : 'none';
            }
        });
    } else {
        container.innerHTML = allItems.map((f, i) => {
            const { icon, cls, badgeHtml, action, title, showClose, pct } = getProps(f);
            // Two lines: the file on top, the chain underneath.
            return `<div class="file-item ${cls}">
                <div class="file-item-progress" style="width:${pct.toFixed(1)}%"></div>
                <div class="fi-head">
                    <span class="file-item-icon" title="${title}">${icon}</span>
                    <span class="file-item-name" title="${f.path}">${f.name}</span>
                    <div class="file-item-divider" style="display:${showClose ? 'block' : 'none'}"></div>
                    <button class="file-item-close" data-action="${action}" data-idx="${i}" title="${title}" tabindex="-1" style="display:${showClose ? 'flex' : 'none'}">
                        <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>
                    </button>
                </div>
                <div class="badge-container">${badgeHtml}</div>
            </div>`;
        }).join('');
    }
    
    syncEmptyState();
}

export function initDropZone() {
    const dropZone = document.getElementById('dropZone');
    
    dropZone.addEventListener('click', async (e) => {
        if (state.dzClickSuppressed || e.target.closest('.file-item-close')) return;
        try {
            const { open } = window.__TAURI__.dialog;
            const selected = await open({
                multiple: true,
                title: 'Select Audio Files',
                filters: [{ name: 'Audio Files', extensions: ['wav', 'flac', 'mp3', 'ogg', 'aac', 'm4a'] }]
            });
            if (!selected) return;
            const files = Array.isArray(selected) ? selected : [selected];
            if (files.length === 0) return;

            if (state.convIsConverting) {
                state.convNextQueue.push(...files.map(f => ({ path: f, name: f.split('\\').pop().split('/').pop(), status: 'queued', batch: 'next' })));
                renderFileQueue();
            } else {
                startConversion(files);
            }
        } catch(e) {}
    });

    if (window.__TAURI__.event) {
        // A file drop reaches every window that registered a global listener for
        // it, not only the window the file actually landed on. Without this guard
        // a file dropped on a second window - the XTC geometry window - would
        // also queue itself for conversion here.
        const myLabel = window.__TAURI__.window?.appWindow?.label ?? 'main';
        const forMe = (event) => !event.windowLabel || event.windowLabel === myLabel;

        window.__TAURI__.event.listen('tauri://file-drop', (event) => {
            if (!forMe(event)) return;
            dropZone.classList.remove('drag-over');
            const files = (event.payload || []).filter(f => ['wav', 'flac', 'mp3', 'ogg', 'aac', 'm4a'].includes(f.split('.').pop().toLowerCase()));
            if (files.length === 0) return;
            if (state.convIsConverting) {
                state.convNextQueue.push(...files.map(f => ({ path: f, name: f.split('\\').pop().split('/').pop(), status: 'queued', batch: 'next' })));
                renderFileQueue();
            } else startConversion(files);
        });
        window.__TAURI__.event.listen('tauri://file-drop-hover', (event) => { if (forMe(event)) dropZone.classList.add('drag-over'); });
        window.__TAURI__.event.listen('tauri://file-drop-cancelled', (event) => { if (forMe(event)) dropZone.classList.remove('drag-over'); });
    }

    // Cancel buttons inside the file queue
    document.getElementById('fileQueue').addEventListener('click', async (e) => {
        const btn = e.target.closest('.file-item-close');
        if (!btn) return;
        e.stopPropagation();
        const idx = parseInt(btn.dataset.idx);
        if (btn.dataset.action === 'cancel-one') {
            state.convFileQueue[idx].dismissed = true; renderFileQueue();
            await invoke('cancel_file', { idx });
        } else if (btn.dataset.action === 'remove') {
            if (idx < state.convFileQueue.length) state.convFileQueue.splice(idx, 1);
            else state.convNextQueue.splice(idx - state.convFileQueue.length, 1);
            renderFileQueue();
        }
    });

    // Hold to Cancel all
    const btn = document.getElementById('dzCancelBtn');
    const fill = document.getElementById('dzCancelFill');
    if (!btn) return;
    let holdStart = null, rafId = null;

    function startHold(e) {
        e.preventDefault(); e.stopPropagation();
        holdStart = performance.now(); fill.style.transition = 'none'; tick();
    }
    function tick() {
        if (!holdStart) return;
        const pct = Math.min(100, (performance.now() - holdStart) / 20);
        fill.style.width = pct + '%';
        if (pct >= 100) commitCancel(); else rafId = requestAnimationFrame(tick);
    }
    function stopHold(e) {
        if (e) { e.preventDefault(); e.stopPropagation(); }
        if (!holdStart) return; holdStart = null;
        if (rafId) { cancelAnimationFrame(rafId); rafId = null; }
        fill.style.transition = 'width 0.3s ease'; fill.style.width = '0%';
    }
    async function commitCancel() {
        holdStart = null; if (rafId) cancelAnimationFrame(rafId);
        state.dzClickSuppressed = true; setTimeout(() => state.dzClickSuppressed = false, 600);
        state.convNextQueue = [];
        state.convFileQueue.forEach(f => { if (!f.dismissed && f.status !== 'done' && f.status !== 'error') { f.status = 'cancelled'; f.dismissed = true; } });
        renderFileQueue(); btn.style.display = 'none';
        try { await invoke('cancel_conversion'); } catch(e) {}
    }
    btn.addEventListener('mousedown', startHold);
    btn.addEventListener('mouseup', stopHold);
    btn.addEventListener('mouseleave', stopHold);
    btn.addEventListener('click', e => e.stopPropagation());
}
