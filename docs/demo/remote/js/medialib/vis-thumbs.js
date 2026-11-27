// ══════════════════════════════════════════════════════════════════════
// The Visualizer page's gallery pictures: a scene has no picture of its
// own, so each is drawn once — a moment of it, still, with no music — on a
// small canvas of the page's own, off screen, one scene after another, and
// kept as an image for as long as the app runs (the scene's text and
// values are in the key: an edited scene is drawn again).
//
// The canvas lives only while pictures are being drawn: its video-card
// context is let go when the queue is empty and when the page is left, so
// it does not count against the player's picture (a page holds only so many
// contexts; the oldest is dropped).
// ══════════════════════════════════════════════════════════════════════

import { createMusic } from '../vis/music.js';
import { createRenderer } from '../vis/renderer.js';

const cache = new Map();    // key → data URL ('' — it could not be drawn)
const W = 256, H = 144;     // the image kept (16:9)
const FRAMES = 24;          // drawn before the one kept: scenes that build on the frame before need a few

const textHash = s => {
    let h = 2166136261;
    for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619) >>> 0; }
    return h.toString(36);
};
/// A scene's picture's key: its id, its text, its values.
export const thumbKey = (entry, values) => `${entry.id}|${textHash(String(entry.text || ''))}|${JSON.stringify(values || {})}`;
/// The picture already drawn: a data URL, '' (none can be drawn), undefined (not yet).
export const thumbCached = key => cache.get(key);

/// The page's drawer: `want(key, entry, values)` → a promise of the data URL
/// ('' — none; null — the page was left first). `stop()` ends it all.
export function createThumbs() {
    let cv = null, r = null, music = null, dead = false, stopped = false;
    let own = null;             // the context let go on purpose is not "lost"
    let queue = [], running = false;

    const lose = () => {
        if (!cv) return;
        if (own) own.goes = true;
        try { r?.clearScene(); } catch (_) {}
        try { cv.getContext('webgl2')?.getExtension('WEBGL_lose_context')?.loseContext(); } catch (_) {}
        cv.remove();
        cv = null; r = null; music = null;
    };
    const ready = () => {
        if (r || dead) return r;
        cv = document.createElement('canvas');
        cv.className = 'ml-vz-thumbcv';
        cv.setAttribute('aria-hidden', 'true');
        document.body.appendChild(cv);
        const me = own = { goes: false };
        r = createRenderer(cv, { onLost: () => { if (!me.goes) dead = true; } });
        if (!r) { dead = true; cv.remove(); cv = null; return null; }
        // No music: a stopped player's numbers (nothing is asked of the engine).
        music = createMusic({ status: () => ({ state: 'stopped' }), delayMs: () => 0 });
        music.attach(r.sink);
        return r;
    };
    async function draw(job) {
        const rr = ready();
        if (!rr) return '';
        const res = await rr.setScene(job.entry.text, 'thumb:' + job.entry.id, job.values);
        if (stopped) return null;
        if (!res.ok || !cv) return '';
        // The frames and the copy in one go: the canvas keeps what it drew
        // only until the page is painted.
        let ok = false;
        for (let i = 0; i < FRAMES; i++) {
            const m = music.frame(1 / 30, { spatial: false });
            ok = rr.frame(m, { clock: 3 + i / 30, dt: 1 / 30, fade: 0, full: true, budgetMs: 40 });
            if (!ok) break;
        }
        if (!ok) return '';
        const out = document.createElement('canvas');
        out.width = W;
        out.height = H;
        const g = out.getContext('2d');
        if (!g) return '';
        g.drawImage(cv, 0, 0, W, H);
        return out.toDataURL('image/jpeg', 0.82);
    }
    async function run() {
        if (running) return;
        running = true;
        while (queue.length && !stopped) {
            const job = queue.shift();
            let url = cache.get(job.key);
            if (url === undefined) {
                try { url = await draw(job); } catch (e) { console.warn('[vis] gallery picture:', e); url = ''; }
                if (stopped || url === null) { job.done(null); break; }
                cache.set(job.key, url);
            }
            job.done(url);
            // A breath between scenes: the page's own picture goes on.
            await new Promise(res => setTimeout(res, 40));
        }
        running = false;
        if (!queue.length || stopped) lose();
    }

    return {
        want(key, entry, values) {
            const have = cache.get(key);
            if (have !== undefined) return Promise.resolve(have);
            if (stopped) return Promise.resolve(null);
            return new Promise(done => {
                queue.push({ key, entry, values, done });
                run();
            });
        },
        stop() {
            stopped = true;
            for (const j of queue) j.done(null);
            queue = [];
            lose();
        },
    };
}
