// The instruments pack in the studio (Anton 28.09): the window that downloads
// it, the line beside the scene's book switch (what the pack is doing), and
// the way to delete it. The pack itself — the download, the checks, its
// updates with the app — is the app's (src-tauri/src/spatial/pack.rs); this
// page asks and shows.
//
// The book switch (studio.js) calls ensureInstruments() before a scene goes
// "With instruments"; the line is packLine($('spState'), specOf).

const T = window.__TAURI__;
const invoke = (c, a) => T.tauri.invoke(c, a);
const $ = id => document.getElementById(id);
const mb = v => (v / 1e6).toFixed(1);
const megs = v => `${Math.round(v / 1e6)} MB`;
const pct = s => (s.total ? Math.min(100, Math.floor((100 * s.got) / s.total)) : 0);
/// An install under way.
const RUNNING = new Set(['downloading', 'verifying', 'unpacking']);

export const packStatus = () => invoke('spatial_status').catch(() => null);

// ── the window ──
const up = () => !$('packModal').hidden;

// While the window is up nothing else takes keys (Tab / Enter / Space reach
// its buttons).
window.addEventListener('keydown', e => {
    if (!up()) return;
    if (e.key === 'Tab' || ((e.key === 'Enter' || e.key === ' ') && e.target.closest?.('#packModal'))) return;
    e.preventDefault();
    e.stopImmediatePropagation();
}, true);

/// The window set for one question.
function ask({ title, text, yes, no }) {
    const w = {
        m: $('packModal'), y: $('packYes'), n: $('packNo'), text: $('packText'),
        bar: $('packBar'), fill: $('packFill'), stat: $('packStat'),
    };
    $('packTitle').textContent = title;
    w.text.textContent = text;
    w.stat.textContent = '';
    w.stat.classList.remove('err');
    w.bar.hidden = true;
    w.fill.style.width = '0%';
    w.y.hidden = false;
    w.y.disabled = false;
    w.y.textContent = yes;
    w.n.textContent = no;
    return w;
}

/// Asks, then downloads in the window (nothing else can be done meanwhile);
/// resolves true when a pack is in. An install the app is running by itself
/// (an update) is followed at once.
function packWindow(st) {
    return new Promise(resolve => {
        const w = ask({
            title: 'Scenes with instruments need the instruments pack',
            text: 'Scenes with instruments show each instrument of the song on its own: where it sits, when it '
                + 'plays, its notes. To find them the player analyses the music with neural networks that do not '
                + `come with the app: ${st?.bytes ? megs(st.bytes) : 'about 280 MB'}, downloaded once. New versions `
                + 'of the pack then arrive by themselves with app updates.\n\nDownload it now?',
            yes: 'Download',
            no: 'Cancel',
        });
        let running = false, un = null;
        const close = ok => {
            w.m.hidden = true;
            un?.();
            w.y.onclick = w.n.onclick = null;
            resolve(ok);
        };
        const show = s => {
            if (!s || !RUNNING.has(s.state)) return;
            if (s.state === 'downloading') {
                w.fill.style.width = pct(s) + '%';
                w.stat.textContent = `${mb(s.got)} of ${mb(s.total)} MB`
                    + (s.from > 0 ? ` (continuing from ${mb(s.from)} MB downloaded before)` : '');
            } else {
                w.fill.style.width = '100%';
                w.stat.textContent = s.state === 'verifying' ? 'Checking…' : 'Setting it up…';
            }
        };
        const start = async () => {
            if (running) return;
            running = true;
            // the button stays, locked, while it downloads; Cancel stops it
            w.y.disabled = true;
            w.y.textContent = 'Downloading…';
            w.n.textContent = 'Cancel';
            w.bar.hidden = false;
            w.stat.classList.remove('err');
            w.text.textContent = 'Downloading the instruments pack. Scenes with instruments work as soon as it is in.';
            w.stat.textContent = 'Starting…';
            try { un = await T.event.listen('spatial:progress', e => show(e.payload)); } catch (_) {}
            try {
                await invoke('spatial_install');
                close(true);
            } catch (e) {
                running = false;
                un?.();
                un = null;
                if (String(e) === 'cancelled') { close(false); return; }
                // no room is a sentence of its own; anything else is most
                // likely the connection
                const room = /^Not enough free space/.test(String(e));
                w.text.textContent = room ? String(e)
                    : 'The download did not work. Check the internet connection and try again.';
                w.stat.textContent = room ? '' : String(e);
                w.stat.classList.add('err');
                w.bar.hidden = true;
                w.y.disabled = false;
                w.y.textContent = 'Try again';
                w.n.textContent = 'Close';
            }
        };
        w.y.onclick = start;
        w.n.onclick = () => { if (running) invoke('spatial_cancel').catch(() => {}); else close(false); };
        w.m.hidden = false;
        w.y.focus();
        if (st?.busy) start();
    });
}

/// Scenes with instruments can run: true when a pack is in — asks for the
/// download in the window when it is not. A pack in use means the user uses
/// the instruments: from now on it is kept up to date with the app.
export async function ensureInstruments() {
    const st = await packStatus();
    if (st?.installed) {
        if (!st.wanted) await invoke('spatial_set_wanted', { on: true }).catch(() => {});
        return true;
    }
    return packWindow(st);
}

/// The user deletes the pack (the line's "Remove…"), asked first: scenes
/// with instruments then show only the rough layer, and no more updates
/// come. Resolves true when it went (or goes at the next start).
export async function leaveInstruments() {
    const st = await packStatus();
    return new Promise(resolve => {
        const w = ask({
            title: 'Delete the instruments pack?',
            text: 'Scenes with instruments then show only the rough layer, and the pack stops updating with the app. '
                + `It takes ${megs(st?.diskBytes || 0)} on the disk; it can be downloaded again at any time.`,
            yes: 'Delete',
            no: 'Keep',
        });
        const close = ok => {
            w.m.hidden = true;
            w.y.onclick = w.n.onclick = null;
            resolve(ok);
        };
        w.y.onclick = async () => {
            w.y.disabled = true;
            try {
                const r = await invoke('spatial_remove');
                if (!r?.later) { close(true); return; }
                w.text.textContent = 'It is in use now — it goes at the next start.';
                w.y.hidden = true;
                w.n.textContent = 'Close';
                w.n.onclick = () => close(true);
            } catch (e) {
                w.stat.textContent = String(e);
                w.stat.classList.add('err');
                w.y.disabled = false;
            }
        };
        w.n.onclick = () => close(false);
        w.m.hidden = false;
        w.n.focus(); // the safe answer first
    });
}

// ── the line ──

/// Keeps `el` telling what the pack does: an update or a download under
/// way; a problem, with what to do about it; or, when all is well, the pack,
/// its size and "Remove…". A scene with instruments (`specOf() >= 2`) and no
/// pack says so, with "Download it". Returns a function that looks again
/// (for when the scene changes).
export function packLine(el, specOf) {
    let st = null;
    const link = (label, act) => {
        const a = document.createElement('a');
        a.href = '#';
        a.textContent = label;
        a.addEventListener('click', ev => {
            ev.preventDefault();
            ev.stopPropagation();
            act();
        });
        return a;
    };
    const render = () => {
        el.textContent = '';
        if (!st) return;
        if (st.busy && RUNNING.has(st.state)) {
            el.append(`${st.background ? 'Updating' : 'Downloading'} the instruments pack… ${pct(st)} %`);
            return;
        }
        if (st.installed) {
            if (!st.current && st.state === 'error') {
                el.append('The instruments pack did not update — the previous one works; it tries again at the next start. ');
                el.append(link('Try now', () => invoke('spatial_install').catch(() => {}).finally(look)));
                return;
            }
            const how = !st.wanted ? '' : st.current ? 'updates with the app · ' : 'an update is on its way · ';
            el.append(`Instruments pack · ${megs(st.diskBytes || 0)} · ${how}`);
            el.append(link('Remove…', async () => { if (await leaveInstruments()) look(); }));
            return;
        }
        if (specOf() >= 2 || st.wanted) {
            el.append('The instruments pack is not installed — scenes with instruments show only the rough layer. ');
            el.append(link('Download it', async () => { if (await packWindow(await packStatus())) look(); }));
        }
    };
    const look = async () => {
        st = await packStatus();
        render();
    };
    // Between full looks the events carry the running state; when an install
    // ends, a full look (the size on the disk, which pack is in).
    T.event.listen('spatial:progress', e => {
        const ended = st?.busy && !e.payload?.busy;
        st = { ...(st || {}), ...e.payload };
        render();
        if (ended) look();
    }).catch(() => {});
    look();
    return look;
}
