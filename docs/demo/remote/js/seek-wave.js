// The seek line as the track's amplitude (Anton 23.11): the Library look's
// foot, the streamer's wide look and a tablet's docked player. The compact
// window, a phone, the listening mode and a stream keep the plain line.
//
// The engine reads the track once (player_waveform: per slice of its length,
// the peak and the RMS as 0..255 over −60..0 dBFS) and keeps it; until it
// answers the bar is a dotted line. Played: bright, cyan into violet;
// ahead: dim. A white head where the sound is heard. Held and moved: a violet
// marker with the time, the stretch between the head and it tinted; the
// player goes on from where it was until the release (player.js finishSeek).
//
// Bars: 2 px wide, 1.5 px apart, at most the bar's height; each the loudest
// slices under it. Drawn on a canvas, again only when what shows changes.

const BW = 2, GAP = 1.5;
const MIN_H = 0.08;
const DIM = 'rgba(100, 140, 180, 0.30)';
const TINT = 'rgba(167, 139, 250, 0.55)';
const FROM = ['#22d3ee', '#a78bfa'];
const FROM_FILE = ['#10b981', '#6ee7b7'];

const lin = v => (v > 0 ? Math.pow(10, ((v / 255) * 60 - 60) / 20) : 0);

/// Heights 0..1 for `n` bars from the engine's slices: mostly the RMS (the
/// track's shape survives a loud master), some peak; each against the
/// track's own loudest.
export function barHeights(w, n) {
    const m = Math.min(w?.peak?.length || 0, w?.rms?.length || 0);
    if (!m || n <= 0) return null;
    const pk = w.peak.slice(0, m).map(lin), rm = w.rms.slice(0, m).map(lin);
    const maxP = Math.max(...pk) || 1, maxR = Math.max(...rm) || 1;
    const out = new Float32Array(n);
    for (let i = 0; i < n; i++) {
        const a = Math.floor((i * m) / n), b = Math.max(a + 1, Math.floor(((i + 1) * m) / n));
        let v = 0;
        for (let k = a; k < b && k < m; k++) v = Math.max(v, 0.7 * (rm[k] / maxR) + 0.3 * (pk[k] / maxP));
        out[i] = Math.max(MIN_H, Math.min(1, Math.pow(v, 0.85)));
    }
    return out;
}

/// Whether this layout shows the amplitude.
function layoutWants() {
    const b = document.body.classList;
    if (b.contains('mode-listening')) return false;
    if (b.contains('win-landscape')) return true;
    return b.contains('rm') && !b.contains('rm-dock-min') && matchMedia('(min-width: 760px)').matches;
}

export function seekWave(bar, invoke, fmtTime) {
    const cv = document.createElement('canvas');
    cv.className = 'pl-wave';
    cv.setAttribute('aria-hidden', 'true');
    const head = document.createElement('span');
    head.className = 'pl-wave-head';
    const ghost = document.createElement('span');
    ghost.className = 'pl-wave-ghost';
    const gt = document.createElement('b');
    ghost.appendChild(gt);
    bar.prepend(cv);
    bar.append(head, ghost);

    let id = null, asked = null, wave = null, failed = new Set();
    let played = 0, ghostPct = null, durS = 0;
    let heights = null, hn = 0, drawn = '';
    let on = false;

    function ask(tid) {
        asked = tid;
        wave = null; heights = null; drawn = '';
        invoke('player_waveform', { id: tid }).then(w => {
            if (tid !== id) return;
            wave = w; heights = null; drawn = '';
            draw();
        }).catch(e => {
            if (tid !== id) return;
            // Asked for meanwhile by another page of the same player: again.
            if (/another track/.test(String(e))) { setTimeout(() => { if (tid === id) ask(tid); }, 800); return; }
            failed.add(tid);
            setOn(false);
        });
    }

    function setOn(v) {
        if (on === v) return;
        on = v;
        bar.classList.toggle('pl-seek-wave', v);
        drawn = '';
        if (v) draw();
    }

    function draw() {
        if (!on) return;
        const W = bar.clientWidth, H = bar.clientHeight;
        if (W < 4 || H < 4) return;
        const n = Math.max(1, Math.floor((W + GAP) / (BW + GAP)));
        const fromFile = !!bar.closest('.pl-from-file');
        const px = Math.round(played * W), gx = ghostPct == null ? -1 : Math.round(ghostPct * W);
        const key = [W, H, n, px, gx, !!wave, fromFile].join('|');
        head.style.left = played * 100 + '%';
        if (ghostPct != null) {
            ghost.style.left = ghostPct * 100 + '%';
            ghost.classList.toggle('pl-wave-ghost--left', ghostPct > 0.8);
        }
        if (key === drawn) return;
        drawn = key;
        if (wave && (!heights || hn !== n)) { heights = barHeights(wave, n); hn = n; }
        const d = window.devicePixelRatio || 1;
        if (cv.width !== Math.round(W * d) || cv.height !== Math.round(H * d)) {
            cv.width = Math.round(W * d); cv.height = Math.round(H * d);
        }
        const g = cv.getContext('2d');
        g.setTransform(d, 0, 0, d, 0, 0);
        g.clearRect(0, 0, W, H);
        const c = fromFile ? FROM_FILE : FROM;
        const grad = g.createLinearGradient(0, 0, Math.max(px, 8), 0);
        grad.addColorStop(0, c[0]); grad.addColorStop(1, c[1]);
        const lo = ghostPct == null ? -1 : Math.min(played, ghostPct), hi = ghostPct == null ? -1 : Math.max(played, ghostPct);
        for (let i = 0; i < n; i++) {
            const x = i * (BW + GAP), t = (x + BW / 2) / W;
            const h = Math.max(2, (heights ? heights[i] : MIN_H) * (H - 2));
            g.fillStyle = t <= played ? grad : (t > lo && t <= hi) ? TINT : DIM;
            if (t <= played && t > lo && t <= hi) g.globalAlpha = 0.55; // dragged back: the stretch to come again
            g.fillRect(x, (H - h) / 2, BW, h);
            g.globalAlpha = 1;
        }
    }

    new ResizeObserver(() => { drawn = ''; draw(); }).observe(bar);

    return {
        /// Each poll: the track, the heard share of it, its length, a stream or not.
        sync(trackId, pct, dur, live) {
            durS = dur;
            if (trackId !== id) { id = trackId; asked = null; wave = null; heights = null; }
            const want = layoutWants() && !live && id != null && dur > 0 && !failed.has(id);
            setOn(want);
            if (want && asked !== id) ask(id);
            played = Math.max(0, Math.min(1, pct || 0));
            draw();
        },
        /// Held and moved: the marker at `pct`; null: let go.
        drag(pct) {
            ghostPct = pct;
            bar.classList.toggle('pl-wave-dragging', pct != null);
            if (pct != null) gt.textContent = fmtTime(pct * durS);
            draw();
        },
    };
}
