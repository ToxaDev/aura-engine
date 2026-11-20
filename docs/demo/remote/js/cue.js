// Cue Consistency, main-window side: the saved parameters reach the stage at
// startup (the SET window may never open), and the window's Speakers /
// Headphones switch reaches the rack, which owns XTC's badge.
//
// The parameters live under a key of their own: the SET window writes them,
// so `auraSettings` keeps its one author.

export const CUE_KEY = 'auraCue';

export const CUE_DEFAULTS = {
    bypass: false, strength: 100, f1Hz: 700, f2Hz: 1500, lfIld: 100, itdScale: 100, stageWidth: 90,
    headShadow: true, shadowBlend: 50, envItd: true, panLaw: 'sin3', snapDb: [],
    tauSMs: 50, onsetReset: true,
};

/// Whom the stage's cues are for: XTC after it delivers them to speakers,
/// without it they go straight to the ears. The app cannot tell what is
/// plugged in, so this is what the rack says, and the tip says what it needs.
export function cueMode(xtcOn) {
    return xtcOn
        ? { label: 'Speakers', tip: 'Cue Consistency for speakers: XTC delivers its cues to each ear.' }
        : { label: 'Headphones', tip: 'Cue Consistency for headphones: XTC is off. On speakers its cues mix in the air, switch XTC on.' };
}

export function loadCueParams() {
    try {
        // A/B starts on the stage, whatever was left pressed.
        return { ...CUE_DEFAULTS, ...JSON.parse(localStorage.getItem(CUE_KEY) || '{}'), bypass: false };
    } catch (e) {
        return { ...CUE_DEFAULTS };
    }
}

export function saveCueParams(p) {
    try { localStorage.setItem(CUE_KEY, JSON.stringify(p)); } catch (e) { /* the stage has them anyway */ }
}

export function openCueWindow() {
    window.__TAURI__?.core?.invoke('cue_window_open').catch((e) => console.error('[cue] window would not open:', e));
}

/// `setXtc(on)` is the rack's: XTC follows the switch in the SET window.
/// `isCueOn()` and `setCue(on)` are CUE's row, for the line in XTC's window.
export function initCueBridge(setXtc, isCueOn, setCue) {
    const t = window.__TAURI__;
    if (!t) return;
    t.core?.invoke('player_set_cue', { params: loadCueParams() }).catch(() => {});
    t.event?.listen('cue:delivery', (msg) => setXtc(msg?.payload?.xtc === true)).catch(() => {});
    t.event?.listen('cue:ask', () => announceCue(isCueOn(), true)).catch(() => {});
    t.event?.listen('cue:set', (msg) => {
        setCue(msg?.payload?.on === true);
        // A held rack refuses: the line goes back to what the row says.
        announceCue(isCueOn(), true);
    }).catch(() => {});
}

let announced = null;

/// Tells XTC's window where CUE's row stands, when that changed.
export function announceCue(on, always = false) {
    if (on === announced && !always) return;
    announced = on;
    window.__TAURI__?.event?.emit?.('cue:state', { on })?.catch(() => {});
}

/// The line in XTC's window: the same stage as the rack's row, and its SET.
export function initCueLine(onBtn, setBtn) {
    const ev = window.__TAURI__?.event;
    let on = false;
    const paint = () => {
        onBtn.textContent = on ? 'On' : 'Off';
        onBtn.classList.toggle('on', on);
        onBtn.setAttribute('aria-pressed', String(on));
    };
    paint();
    onBtn.addEventListener('click', () => ev?.emit('cue:set', { on: !on }).catch(() => {}));
    setBtn.addEventListener('click', openCueWindow);
    ev?.listen('cue:state', (msg) => { on = msg?.payload?.on === true; paint(); })
        .then(() => ev.emit('cue:ask'))
        .catch(() => {});
}
