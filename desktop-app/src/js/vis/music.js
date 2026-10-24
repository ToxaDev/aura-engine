
// ══════════════════════════════════════════════════════════════════════
// The music as the visualization scenes see it (Anton 27.09; the contract
// is src/vis/AURA-VIS-SPEC.md §5).
//
// The player renders seconds ahead of what is heard; its ring holds that
// sound. From it, about seven times a second:
//   – slices of the spectrum every 20 ms (controller.rs spectrum_span, with
//     the slice's extras: peak, RMS of L/R/mid/side, correlation), from a
//     little behind now to ~3.9 s ahead. Each slice is turned once, when it
//     first comes, into the scene's rows (music texture, scene.js ROW):
//     levels against their own recent top, hits, loudness, a centred
//     energy, onsets, brightness, stereo;
//   – the waveform around now at ~24 kHz (controller.rs wave_span).
// From the onsets, the tempo and where its beats fall (past and future).
// From the cover, four colours.
// For a spatial scene (// @spatial), the sound objects (spatial/mod.rs span):
// frames of the TRACK's own time (86 a second), from 6 s behind to 3.5 s
// ahead of what is heard; the rough ones (from the mix) are asked again as
// the separation reaches them. Once the track is mapped (the instruments
// tier), span's head names the map; the map — the instruments' colours and
// every note of the track — is asked for once, and each frame that comes
// gets its column of notes (instrument × key: velocity, onset flash).
//
// Slices are kept by their index on the ring's own grid (a slice is the
// same slice in every answer) until a splice rewrites the ring from it.
// Each frame the engine says where "now" is on that grid, to the fraction,
// and the values for now.
// ══════════════════════════════════════════════════════════════════════

import { ROW, MROWS, OBJ_N, OBJ_VALS, OBJ_W, KEY_LOW, KEYS, NOTE_ROWS } from './scene.js';

const BANDS = 48;
const GROUPS = [[0, 17], [17, 36], [36, 48]];
const BURST_S = [0.3, 0.22, 0.15];
const STEP_MS = 20;
const ASK_BACK_MS = 300;
const ASK_AHEAD_MS = 3900;
const ASK_MS = 140;
const LAG_MS = 20;          // from asking to the frame on the screen
const ATTACK_MS = 20;       // a slice shows a hit about half its window after it
const KEEP_BACK_S = 6.5;    // the past kept
const ENERGY_H = 37;        // half the energy window, slices (≈0.74 s → 1.5 s centred)
const WAVE_RATE = 24000;
const WAVE_BACK_MS = 250, WAVE_AHEAD_MS = 600, WAVE_ASK_MS = 110;
const WAVE_RING = 32768;
const OBJ_BACK_MS = 6000, OBJ_AHEAD_MS = 3500, OBJ_ASK_MS = 150, OBJ_HEAD = 12;

const TILT = Float32Array.from({ length: BANDS }, (_, b) => {
    const f = 25 * Math.pow(800, (b + 0.5) / BANDS);
    return Math.max(-6, Math.min(24, 4.5 * Math.log2(f / 500)));
});
const clamp01 = v => v < 0 ? 0 : v > 1 ? 1 : v;
const fromDb = db => Math.pow(10, db / 20);

// The app's colours, for tracks without a cover.
export const DEFAULT_PALETTE = [[56, 189, 248], [168, 85, 247], [244, 114, 182], [250, 204, 21]].map(c => c.map(v => v / 255));

/// status(): the player's status (player.js keeps one; the studio polls).
/// delayMs(): the listener's delay for the device (right-click on the bars).
export function createMusic({ status, delayMs }) {
    const sp = {
        ring: null, gen: null, rate: 0, step: 0,
        slices: new Map(),          // k → Float32Array(MROWS)
        onset: new Map(),           // k → raw flux
        loud: new Map(),            // k → loudness 0…1 (for the centred energy)
        now: 0, at: 0, synced: false, state: 'none', write: 0,
        pending: false, retryAt: 0, lastAsk: 0,
        top: [-40, -40, -40], topAll: -30, topLoud: -30, fluxTop: 3,
        prev: null,                 // the last slice's dB (for the flux)
    };
    const wv = {
        ring: null, gen: null, d: 1, rate: 0, pending: false, retryAt: 0, lastAsk: 0,
        buf: new Float32Array(WAVE_RING * 2), lap: new Float64Array(WAVE_RING).fill(-1), last: -1,
    };
    const sinks = new Set();

    // ── slices ─────────────────────────────────────────────────────────
    function derive(k, b, e) {
        const col = new Float32Array(MROWS);
        const db = new Float32Array(BANDS);
        for (let i = 0; i < BANDS; i++) db[i] = -96 + b[i] * 96 / 255;
        const stepS = sp.step / sp.rate;
        const prevCol = sp.slices.get(k - 1);
        // The parts' levels, each against its own recent top; a hit is a
        // jump over the slices just before, and dies away.
        for (let g = 0; g < 3; g++) {
            const [a, z] = GROUPS[g];
            let p = 0;
            for (let i = a; i < z; i++) p += Math.pow(10, db[i] / 10);
            const L = 10 * Math.log10(p / (z - a) + 1e-12);
            sp.top[g] = L > sp.top[g] ? L : Math.max(sp.top[g] - 1.5 * stepS, L, -60);
            const lv = clamp01((L - (sp.top[g] - 30)) / 30);
            col[ROW.BASS + g] = lv;
            let m = 0, n = 0;
            for (let j = 1; j <= 3; j++) {
                const q = sp.slices.get(k - j);
                if (q) { m += q[ROW.BASS + g]; n++; }
            }
            const hit = n ? Math.max(0, lv - m / n - 0.04) : 0;
            const was = prevCol ? prevCol[ROW.HIT + g] : 0;
            col[ROW.HIT + g] = Math.min(1, Math.max(1.8 * hit, was * Math.exp(-stepS / BURST_S[g])));
        }
        // The spectrum: every band against the loudest (tilted), and as it is.
        let mx = -96;
        for (let i = 0; i < BANDS; i++) if (db[i] + TILT[i] > mx) mx = db[i] + TILT[i];
        sp.topAll = mx > sp.topAll ? mx : Math.max(sp.topAll - 1.5 * stepS, mx, -50);
        let wsum = 0, csum = 0;
        for (let i = 0; i < BANDS; i++) {
            const v = clamp01((db[i] + TILT[i] - (sp.topAll - 42)) / 42);
            col[ROW.SPEC + i] = v;
            col[ROW.SPEC_DB + i] = (db[i] + 96) / 96;
            wsum += v * v;
            csum += v * v * i;
        }
        col[ROW.BRIGHT] = wsum > 1e-6 ? csum / wsum / (BANDS - 1) : 0;
        // Loudness: the mix's RMS (the slice's mid), against its recent top.
        const rmsDb = e ? -96 + e[3] * 96 / 255 : -96;
        sp.topLoud = rmsDb > sp.topLoud ? rmsDb : Math.max(sp.topLoud - 1 * stepS, rmsDb, -60);
        const loud = clamp01((rmsDb - (sp.topLoud - 36)) / 36);
        col[ROW.LOUD] = loud;
        col[ROW.LOUD_DB] = (rmsDb + 96) / 96;
        sp.loud.set(k, loud);
        // Onsets: how much the bands rose since the slice before (dB).
        let flux = 0;
        const prevDb = sp.prev && sp.prev.k === k - 1 ? sp.prev.db : null;
        if (prevDb) for (let i = 0; i < BANDS; i++) { const d = db[i] - prevDb[i]; if (d > 0) flux += d; }
        flux /= BANDS;
        sp.prev = { k, db };
        sp.onset.set(k, flux);
        sp.fluxTop = Math.max(flux, sp.fluxTop * Math.exp(-stepS / 4), 1.5);
        col[ROW.ONSET] = clamp01(flux / sp.fluxTop);
        // Stereo and peak from the extras.
        if (e) {
            col[ROW.PEAK_DB] = e[0] / 255;
            const l = fromDb(-96 + e[1] * 96 / 255), r = fromDb(-96 + e[2] * 96 / 255);
            const m = fromDb(-96 + e[3] * 96 / 255), s = fromDb(-96 + e[4] * 96 / 255);
            col[ROW.WIDTH] = m > 1e-4 ? clamp01(s / m) : 0;
            col[ROW.BALANCE] = l + r > 1e-4 ? (r - l) / (r + l) : 0;
            col[ROW.CORR] = e[5] / 127.5 - 1;
        } else {
            col[ROW.CORR] = 1;
        }
        col[ROW.K] = k;
        // Energy: the loudness over 1.5 s. This slice gets it from what is
        // behind (for now), the one half a window back from both sides.
        col[ROW.ENERGY] = meanLoud(k - 2 * ENERGY_H, k);
        const mid = sp.slices.get(k - ENERGY_H);
        if (mid) {
            mid[ROW.ENERGY] = meanLoud(k - 2 * ENERGY_H, k);
            for (const s of sinks) s.setValue?.(k - ENERGY_H, ROW.ENERGY, mid[ROW.ENERGY]);
        }
        return col;
    }
    function meanLoud(a, b) {
        let s = 0, n = 0;
        for (let k = a; k <= b; k++) { const v = sp.loud.get(k); if (v != null) { s += v; n++; } }
        return n ? s / n : 0;
    }

    function takeSpan(buf) {
        if (!buf || buf.byteLength < 80) { sp.state = 'none'; return; }
        const dv = new DataView(buf);
        const f = i => dv.getFloat64(i * 8, true);
        const now = f(0), rate = f(1), step = f(2), k0 = f(3), n = f(4), ring = f(5), gen = f(6), spliceAt = f(7);
        sp.write = f(8);
        sp.state = ['playing', 'paused', 'held'][f(9)] || 'none';
        // A ring's frames only go on; a second and more back, it is another
        // ring under the same name (a new stream that took the old one's place).
        const anew = ring === sp.ring && now < sp.now - rate;
        if (ring !== sp.ring || rate !== sp.rate || step !== sp.step || anew) {
            sp.slices.clear(); sp.onset.clear(); sp.loud.clear(); sp.prev = null;
            for (const s of sinks) s.clearSlices();
            Object.assign(sp, { ring, rate, step, gen, synced: false });
            beat.reset();
            // A new stream (another rate, BIT-PERFECT on or off, a stop and a
            // start the scene did not see): the waveform kept is the old
            // one's, its last sample far from the new frames — asked for
            // from there, it would never be asked for again (Anton 3.10).
            if (wv.ring !== ring || anew) dropWave();
        } else if (gen !== sp.gen) {
            const first = spliceAt < 0 ? 0 : Math.floor((spliceAt - 0.05 * rate) / step);
            for (const k of [...sp.slices.keys()]) if (k >= first) {
                sp.slices.delete(k); sp.onset.delete(k); sp.loud.delete(k);
                for (const s of sinks) s.dropSlice(k);
            }
            if (sp.prev && sp.prev.k >= first) sp.prev = null;
            sp.gen = gen;
        }
        const t = performance.now();
        if (sp.state === 'playing') {
            const guess = sp.now + (t - sp.at) * rate / 1000;
            const err = now - guess;
            sp.now = sp.synced && Math.abs(err) < 0.25 * rate ? guess + err * 0.1 : now;
            sp.synced = true;
        } else {
            sp.now = now;
            sp.synced = false;
        }
        sp.at = t;
        const per = 1 + BANDS + 8;
        let o = 80;
        for (let j = 0; j < n; j++, o += per) {
            const k = k0 + j;
            if (dv.getUint8(o) !== 1 || sp.slices.has(k)) continue;
            const col = derive(k, new Uint8Array(buf, o + 1, BANDS), new Uint8Array(buf, o + 1 + BANDS, 8));
            sp.slices.set(k, col);
            for (const s of sinks) s.setSlice(k, col);
        }
        const old = Math.floor(sp.now / step) - Math.ceil(KEEP_BACK_S * rate / step);
        for (const k of [...sp.slices.keys()]) if (k < old) { sp.slices.delete(k); sp.onset.delete(k); sp.loud.delete(k); }
    }

    function askSpan() {
        const t = performance.now();
        if (sp.pending || t < sp.retryAt || t - sp.lastAsk < ASK_MS) return;
        sp.pending = true;
        sp.lastAsk = t;
        let from = -(sp.slices.size ? ASK_BACK_MS : 1500);
        if (sp.synced && sp.step) {
            const frame = sp.now + (t - sp.at) * sp.rate / 1000;
            let k = Math.ceil((frame - ASK_BACK_MS * sp.rate / 1000) / sp.step);
            while (sp.slices.has(k)) k++;
            from = Math.max(-ASK_BACK_MS, ((k - 1.5) * sp.step - frame) / sp.rate * 1000);
        }
        fetch(`https://aura.localhost/player/spectrum_span?bands=${BANDS}&from_ms=${from.toFixed(1)}&to_ms=${ASK_AHEAD_MS}&step_ms=${STEP_MS}&extra=1`)
            .then(r => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.arrayBuffer(); })
            .then(buf => { sp.pending = false; takeSpan(buf); })
            .catch(() => { sp.pending = false; sp.retryAt = performance.now() + 2000; });
    }

    // ── the waveform ───────────────────────────────────────────────────
    /// None kept: the next answer starts it afresh, whatever its ring.
    function dropWave() {
        wv.ring = null;
        wv.lap.fill(-1);
        wv.last = -1;
        for (const s of sinks) s.clearWave?.();
    }
    function takeWave(buf) {
        if (!buf || buf.byteLength < 80) return;
        const dv = new DataView(buf);
        const f = i => dv.getFloat64(i * 8, true);
        const rate = f(1), d = f(2), i0 = f(3), n = f(4), ring = f(5), gen = f(6), spliceAt = f(7);
        if (ring !== wv.ring || d !== wv.d || rate !== wv.rate) {
            wv.lap.fill(-1);
            wv.last = -1;
            for (const s of sinks) s.clearWave?.();
            Object.assign(wv, { ring, d, rate, gen });
        } else if (gen !== wv.gen) {
            // Rewritten from the splice on: those samples are gone.
            const first = spliceAt < 0 ? 0 : Math.floor(spliceAt / d);
            if (wv.last >= first) {
                const from = Math.max(first, wv.last - WAVE_RING + 1);
                for (let i = from; i <= wv.last; i++) wv.lap[i % WAVE_RING] = -1;
                for (const s of sinks) s.dropWave?.(from, wv.last);
                wv.last = first - 1;
            }
            wv.gen = gen;
        }
        if (!n) return;
        const lr = new Float32Array(n * 2);
        for (let j = 0; j < n * 2; j++) lr[j] = dv.getInt16(80 + j * 2, true) / 32767;
        for (let j = 0; j < n; j++) {
            const i = i0 + j, s = i % WAVE_RING;
            wv.buf[s * 2] = lr[j * 2];
            wv.buf[s * 2 + 1] = lr[j * 2 + 1];
            wv.lap[s] = Math.floor(i / WAVE_RING);
        }
        wv.last = Math.max(wv.last, i0 + n - 1);
        for (const s of sinks) s.setWave?.(i0, n, lr);
    }
    function askWave() {
        const t = performance.now();
        if (wv.pending || t < wv.retryAt || t - wv.lastAsk < WAVE_ASK_MS || !sp.synced || !sp.rate) return;
        wv.pending = true;
        wv.lastAsk = t;
        const frame = sp.now + (t - sp.at) * sp.rate / 1000;
        let from = -WAVE_BACK_MS;
        if (wv.last >= 0 && wv.d) {
            const lastMs = ((wv.last + 1) * wv.d - frame) / sp.rate * 1000;
            // Further ahead than it is ever asked for: not this ring's.
            if (lastMs > WAVE_AHEAD_MS + 1000) dropWave();
            else if (lastMs > -WAVE_BACK_MS) from = lastMs;
        }
        if (from >= WAVE_AHEAD_MS - 20) { wv.pending = false; return; }
        fetch(`https://aura.localhost/player/wave_span?from_ms=${from.toFixed(1)}&to_ms=${WAVE_AHEAD_MS}&rate=${WAVE_RATE}`)
            .then(r => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.arrayBuffer(); })
            .then(buf => { wv.pending = false; takeWave(buf); })
            .catch(() => { wv.pending = false; wv.retryAt = performance.now() + 2000; });
    }
    /// The waveform's sample at fractional index q (0 where there is none).
    function waveAt(q, ch) {
        const i = Math.floor(q), fr = q - i;
        const a = sampleAt(i, ch), b = sampleAt(i + 1, ch);
        return a + (b - a) * fr;
    }
    function sampleAt(i, ch) {
        if (i < 0) return 0;
        const s = i % WAVE_RING;
        if (wv.lap[s] !== Math.floor(i / WAVE_RING)) return 0;
        return ch < 2 ? wv.buf[s * 2 + ch] : 0.5 * (wv.buf[s * 2] + wv.buf[s * 2 + 1]);
    }

    // ── the sound objects (spatial scenes) ─────────────────────────────
    const ob = {
        on: false, track: null, fps: 0, atS: 0, at: 0, nowS: 0, synced: false, state: 'none',
        frames: new Map(),          // k → Float32Array(OBJ_N · OBJ_VALS): x y z energy width onset presence coherence
        progress: -1, gpu: -1, source: 0, total: 0,
        busy: 0,                    // the instruments are on their way (span's head)
        made: 0,                    // the frames' make (a live stream's: they were made again when it moves)
        pending: false, retryAt: 0, lastAsk: 0, refetch: false, refetchAt: 0,
    };
    // The track map (the instruments tier, SPEC2-CONTRACT §4): its id comes at
    // the end of span's head (0: none yet); the map itself — the instruments'
    // colours and every note of the track — is asked for once per id.
    const map = { id: 0, want: 0, pending: false, count: 0, colour: new Float32Array(OBJ_N * 3),
        on: null, off: null, obj: null, key: null, vel: null, maxLen: 0 };
    function dropMap() {
        Object.assign(map, { id: 0, want: 0, count: 0, on: null, off: null, obj: null, key: null, vel: null, maxLen: 0 });
        map.colour.fill(0);
        for (const s of sinks) s.clearNotes?.();
    }
    function dropObjects() {
        ob.frames.clear();
        for (const s of sinks) s.clearObjects?.();
        dropMap();
    }
    /// The note columns of frames k0 … k0+n−1: while a note sounds its row
    /// holds its velocity; from its start its onset flash, dying in 0.12 s.
    function noteColumns(k0, n) {
        const cols = [];
        for (let j = 0; j < n; j++) cols.push(new Float32Array(NOTE_ROWS * 2));
        if (!map.on || !ob.fps) return cols;
        const half = 0.5 / ob.fps, t0 = k0 / ob.fps - half, t1 = (k0 + n - 1) / ob.fps + half;
        // the first note that can still be sounding (or flashing) at t0
        let lo = 0, hi = map.on.length;
        const from = t0 - Math.max(map.maxLen, 0.6);
        while (lo < hi) { const m = (lo + hi) >> 1; if (map.on[m] < from) lo = m + 1; else hi = m; }
        for (let i = lo; i < map.on.length && map.on[i] <= t1; i++) {
            const row = map.obj[i] * KEYS + map.key[i] - KEY_LOW;
            if (row < 0 || row >= NOTE_ROWS) continue;
            for (let j = 0; j < n; j++) {
                const t = (k0 + j) / ob.fps;
                const c = cols[j];
                if (map.on[i] <= t + half && map.off[i] > t - half) c[row * 2] = Math.max(c[row * 2], map.vel[i]);
                if (t >= map.on[i] - half) c[row * 2 + 1] = Math.max(c[row * 2 + 1], Math.exp(-Math.max(0, t - map.on[i]) / 0.12));
            }
        }
        return cols;
    }
    function takeMap(j) {
        const objs = Array.isArray(j?.objects) ? j.objects : [];
        map.count = Math.min(OBJ_N, Number(j?.count) || objs.length);
        map.colour.fill(0);
        objs.slice(0, OBJ_N).forEach((o, i) => { const c = o.colour || [0, 0, 0]; map.colour.set([+c[0] || 0, +c[1] || 0, +c[2] || 0], i * 3); });
        const notes = (Array.isArray(j?.notes) ? j.notes : []).filter(x => Array.isArray(x) && x.length >= 5 && x[0] < map.count)
            .sort((a, b) => a[2] - b[2]);
        const N = notes.length;
        map.obj = new Uint8Array(N); map.key = new Int16Array(N); map.on = new Float64Array(N); map.off = new Float64Array(N); map.vel = new Float32Array(N);
        map.maxLen = 0;
        notes.forEach((x, i) => {
            map.obj[i] = x[0]; map.key[i] = Math.round(x[1]); map.on[i] = x[2]; map.off[i] = Math.max(x[3], x[2]);
            map.vel[i] = Math.max(0, Math.min(1, x[4]));
            map.maxLen = Math.max(map.maxLen, map.off[i] - map.on[i]);
        });
        // the frames already here get their notes
        const ks = [...ob.frames.keys()].sort((a, b) => a - b);
        for (let i = 0; i < ks.length;) {
            let e = i;
            while (e + 1 < ks.length && ks[e + 1] === ks[e] + 1) e++;
            const cols = noteColumns(ks[i], e - i + 1);
            for (const s of sinks) s.setNotes?.(ks[i], cols);
            i = e + 1;
        }
    }
    function askMap(id) {
        if (map.pending || id === map.id || performance.now() < (map.retryAt || 0)) return;
        map.pending = true;
        map.want = id;
        fetch(`https://aura.localhost/player/spatial_map?id=${id}`)
            .then(r => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
            .then(j => { map.pending = false; if (map.want !== id) return; map.id = id; takeMap(j); })
            .catch(() => { map.pending = false; map.want = 0; map.retryAt = performance.now() + 2000; });
    }
    /// Frames k0 … k0+n−1 as the objects texture's rows.
    function objectRows(k0, n) {
        const a = new Float32Array(n * OBJ_W * 4);
        for (let j = 0; j < n; j++) {
            const fr = ob.frames.get(k0 + j), o = j * OBJ_W * 4;
            if (fr) for (let s = 0; s < OBJ_N; s++) for (let v = 0; v < OBJ_VALS; v++) a[o + s * OBJ_VALS + v] = fr[s * OBJ_VALS + v];
            a[o + (OBJ_W - 1) * 4] = fr ? k0 + j : -1;
        }
        return a;
    }
    function takeObjects(buf) {
        if (!buf || buf.byteLength < OBJ_HEAD * 8) { ob.state = 'none'; return; }
        const dv = new DataView(buf);
        const f = i => dv.getFloat64(i * 8, true);
        const atS = f(0), fps = f(1), k0 = f(2), n = f(3), total = f(4), progress = f(5), gpu = f(6), source = f(7);
        const state = ['playing', 'paused', 'held'][f(8)] || 'none', track = f(9), slots = f(10), vals = f(11);
        // The head may be longer than it was (newer fields at its end): its length is what the frames leave.
        const per = slots * vals * 2;
        const head = Math.max(OBJ_HEAD, Math.round((buf.byteLength - Math.max(0, n) * per) / 8));
        const mapId = head > OBJ_HEAD ? f(OBJ_HEAD) : 0;
        // the instruments on their way (an older head: while the analysis has not reached its end)
        const busy = head > OBJ_HEAD + 1 ? f(OBJ_HEAD + 1) : progress >= 0 && progress < 1 ? 1 : 0;
        // A live stream's frames are made as it comes: when frames given out
        // before were made again (a segment's end made whole, the instruments
        // come for frames of the mix, another song heard), this moves.
        const made = head > OBJ_HEAD + 2 ? f(OBJ_HEAD + 2) : 0;
        if (track !== ob.track || fps !== ob.fps) {
            dropObjects();
            Object.assign(ob, { track, fps, synced: false, made });
        } else if ((progress !== ob.progress || made !== ob.made) && performance.now() - ob.refetchAt > 400) {
            // The separation moved on: frames that came from the mix alone may
            // have their instruments now — ask for the window again (the old
            // ones stay drawn until the new ones come).
            ob.refetch = true;
            ob.refetchAt = performance.now();
            ob.made = made;
        }
        const t = performance.now();
        if (state === 'playing') {
            const guess = ob.nowS + (t - ob.at) / 1000;
            const err = atS - guess;
            ob.nowS = ob.synced && Math.abs(err) < 0.25 ? guess + err * 0.1 : atS;
            ob.synced = true;
        } else {
            ob.nowS = atS;
            ob.synced = false;
        }
        ob.at = t;
        Object.assign(ob, { state, progress, gpu, source, total, busy });
        if (mapId !== map.id) { if (mapId > 0) askMap(mapId); else if (map.id) dropMap(); }
        if (n > 0) {
            for (let j = 0, o = head * 8; j < n; j++, o += per) {
                const fr = new Float32Array(OBJ_N * OBJ_VALS);
                for (let s = 0; s < Math.min(slots, OBJ_N); s++) for (let v = 0; v < Math.min(vals, OBJ_VALS); v++) {
                    const u = dv.getUint16(o + (s * vals + v) * 2, true) / 65535;
                    fr[s * OBJ_VALS + v] = v === 4 || v === 12 ? u * 2 - 1 : u;       // x and origin x are ±1
                }
                ob.frames.set(k0 + j, fr);
            }
            const rows = objectRows(k0, n);
            for (const s of sinks) s.setObjects?.(k0, n, rows);
            if (map.on) {
                const cols = noteColumns(k0, n);
                for (const s of sinks) s.setNotes?.(k0, cols);
            }
        }
        const old = Math.floor((ob.nowS - OBJ_BACK_MS / 1000 - 0.5) * fps);
        for (const k of [...ob.frames.keys()]) if (k < old) ob.frames.delete(k);
    }
    function askObjects(playing) {
        const t = performance.now();
        if (ob.pending || t < ob.retryAt || t - ob.lastAsk < (playing ? OBJ_ASK_MS : 500)) return;
        ob.pending = true;
        ob.lastAsk = t;
        let from = -OBJ_BACK_MS;
        const refetch = ob.refetch;
        if (ob.fps && ob.track != null && !refetch) {
            // from the first frame not had yet
            const nowS = ob.nowS + (ob.state === 'playing' ? (t - ob.at) / 1000 : 0);
            let k = Math.ceil((nowS - OBJ_BACK_MS / 1000) * ob.fps);
            while (ob.frames.has(k)) k++;
            from = Math.max(-OBJ_BACK_MS, ((k - 1) / ob.fps - nowS) * 1000);
        }
        ob.refetch = false;
        fetch(`https://aura.localhost/player/spatial_span?from_ms=${Math.min(from, OBJ_AHEAD_MS).toFixed(1)}&to_ms=${OBJ_AHEAD_MS}`)
            .then(r => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.arrayBuffer(); })
            .then(buf => { ob.pending = false; takeObjects(buf); })
            .catch(() => { ob.pending = false; ob.retryAt = performance.now() + 2000; if (refetch) ob.refetch = true; });
    }

    // ── tempo ──────────────────────────────────────────────────────────
    // The onsets of ~9 s (past and future) → the lag at which they repeat
    // best (a mild pull towards 120 bpm against doubled/halved tempi) → the
    // offset of the beats that lands on the most onsets.
    const beat = {
        bpm: 0, conf: 0, kRef: 0, period: 0, at: 0, cand: 0, candN: 0,
        reset() { Object.assign(this, { bpm: 0, conf: 0, kRef: 0, period: 0, cand: 0, candN: 0 }); },
        update(kNow) {
            const kA = Math.floor(kNow - 6 / (sp.step / sp.rate)), keys = [];
            for (const k of sp.onset.keys()) if (k >= kA) keys.push(k);
            if (keys.length < 150) { this.conf *= 0.9; return; }
            keys.sort((a, b) => a - b);
            const k0 = keys[0], N = keys[keys.length - 1] - k0 + 1;
            const o = new Float32Array(N);
            for (const k of keys) o[k - k0] = sp.onset.get(k);
            // Above the local mean (0.3 s), half-wave.
            const d = new Float32Array(N);
            let acc = 0;
            const W = 15;
            for (let i = 0; i < N; i++) {
                acc += o[i] - (i >= W ? o[i - W] : 0);
                const mean = acc / Math.min(i + 1, W);
                d[i] = Math.max(0, o[i] - mean);
            }
            const stepS = sp.step / sp.rate;
            const lagMin = Math.max(2, Math.floor(60 / 200 / stepS)), lagMax = Math.ceil(60 / 60 / stepS);
            let r0 = 0;
            for (let i = 0; i < N; i++) r0 += d[i] * d[i];
            if (r0 < 1e-6) { this.conf *= 0.9; return; }
            const r = new Float32Array(lagMax + 2);
            let best = -1, bestV = 0;
            for (let L = lagMin; L <= lagMax + 1; L++) {
                let s = 0;
                for (let i = 0; i + L < N; i++) s += d[i] * d[i + L];
                r[L] = s / (N - L) * N / r0;
                const bpm = 60 / (L * stepS);
                const prior = Math.exp(-0.5 * Math.pow(Math.log2(bpm / 120) / 0.9, 2));
                const v = r[L] * (0.5 + 0.5 * prior);
                if (L <= lagMax && v > bestV) { bestV = v; best = L; }
            }
            if (best < 0) return;
            // Between lags, by the parabola through the three.
            const a = r[best - 1] || 0, b = r[best], c = r[best + 1] || 0;
            const den = a - 2 * b + c;
            const P = best + (Math.abs(den) > 1e-9 ? Math.max(-0.5, Math.min(0.5, 0.5 * (a - c) / den)) : 0);
            // The offset: the most onset on the grid.
            let bestPh = 0, bestS = -1;
            for (let ph = 0; ph < P; ph += 0.5) {
                let s = 0;
                for (let x = ph; x < N; x += P) {
                    const i = Math.round(x);
                    s += (d[i] || 0) + 0.5 * ((d[i - 1] || 0) + (d[i + 1] || 0));
                }
                if (s > bestS) { bestS = s; bestPh = ph; }
            }
            const conf = clamp01((b - 0.1) / 0.5);
            const bpm = 60 / (P * stepS);
            // A new tempo only when it wins twice running; near the old one, glide.
            if (this.bpm && Math.abs(bpm / this.bpm - 1) < 0.04) {
                this.period += (P - this.period) * 0.3;
            } else if (this.cand && Math.abs(bpm / this.cand - 1) < 0.04) {
                if (++this.candN >= 2 || !this.bpm) { this.period = P; this.candN = 0; this.cand = 0; }
            } else {
                this.cand = bpm;
                this.candN = 1;
                if (!this.bpm) this.period = P;
            }
            if (!this.period) return;
            this.bpm = 60 / (this.period * stepS);
            // The beat nearest now on the found grid.
            const kPh = k0 + bestPh;
            this.kRef = kPh + Math.round((kNow - kPh) / this.period) * this.period;
            this.conf += (conf - this.conf) * 0.35;
        },
    };

    // ── the cover ──────────────────────────────────────────────────────
    const cover = { id: null, img: null, palette: DEFAULT_PALETTE, has: false, gen: 0 };
    function loadCover(id) {
        cover.id = id;
        const my = ++cover.gen;
        if (id == null) { Object.assign(cover, { img: null, palette: DEFAULT_PALETTE, has: false }); return; }
        const img = new Image();
        img.crossOrigin = 'anonymous';
        img.onload = () => {
            if (my !== cover.gen) return;
            try {
                cover.palette = paletteOf(img);
                cover.img = img;
                cover.has = true;
            } catch (_) { Object.assign(cover, { img: null, palette: DEFAULT_PALETTE, has: false }); }
            for (const s of sinks) s.setCover?.(cover.img);
        };
        img.onerror = () => {
            if (my !== cover.gen) return;
            Object.assign(cover, { img: null, palette: DEFAULT_PALETTE, has: false });
            for (const s of sinks) s.setCover?.(null);
        };
        img.src = `https://aura.localhost/player/cover?id=${id}&v=vis`;
    }

    // ── each frame ─────────────────────────────────────────────────────
    const out = {
        live: 0, playing: 0, kBase: 0, q: 0, qps: 50, wBase: 0, wq: 0, wRate: WAVE_RATE,
        trackTime: 0, trackLength: 0, progress: 0, trackAge: 0, sampleRate: 0, ahead: 0,
        bass: 0, mid: 0, treble: 0, hit: [0, 0, 0], loud: 0, energy: 0, onset: 0, bright: 0, width: 0,
        bpm: 0, beatConf: 0, beatRef: 0, palette: DEFAULT_PALETTE, hasCover: 0, cover: null,
        audio: new Uint8Array(512 * 2),
        // the sound objects: frame grid and where now is on it; the values now
        oBase: 0, oq: 0, ofps: 0, spatial: 0, spatialProgress: -1, spatialBusy: 0,
        objNow: new Float32Array(OBJ_N * OBJ_VALS),
        // the track map: its instruments and their colours (0 before the map)
        objCount: 0, objColour: map.colour,
    };
    let trackId = null, trackStart = 0, beatAt = 0, playGlide = 0;
    const NOWV = new Float32Array(MROWS);

    /// The objects as heard now (between the two frames around it).
    function objectsNow(state, delay) {
        out.objNow.fill(0);
        out.spatial = 0;
        out.objCount = ob.on && map.on ? map.count : 0;
        out.spatialProgress = ob.on ? ob.progress : -1;
        out.spatialBusy = ob.on && ob.track != null ? ob.busy : 0;
        if (!ob.on || !ob.fps || ob.track == null) { out.ofps = 0; return; }
        const run = ob.state === 'playing' && state === 'playing';
        const nowS = ob.nowS + ((run ? performance.now() - ob.at : 0) + LAG_MS - delay) / 1000;
        const q = nowS * ob.fps;
        out.oBase = Math.floor(q);
        out.oq = q - out.oBase;
        out.ofps = ob.fps;
        const a = ob.frames.get(out.oBase), b = ob.frames.get(out.oBase + 1);
        if (!a && !b) return;
        out.spatial = ob.source;
        const f = out.oq;
        for (let s = 0; s < OBJ_N; s++) {
            const o = s * OBJ_VALS;
            let A = a && a[o] > 0 ? a : null, B = b && b[o] > 0 ? b : null;
            if (!A && !B) continue;
            // presence fades between the two frames; the rest from the side that has it
            out.objNow[o] = (a ? a[o] : 0) * (1 - f) + (b ? b[o] : 0) * f;
            A = A || B; B = B || A;
            for (let v = 1; v < OBJ_VALS; v++) {
                out.objNow[o + v] = v === 2 || v === 3 ? (f < 0.5 ? A : B)[o + v] : A[o + v] + (B[o + v] - A[o + v]) * f;
            }
        }
    }

    /// opts.spatial: the scene shown draws the sound objects.
    function frame(dt, opts) {
        const st = status() || {};
        const state = st.state || 'stopped';
        if (st.trackId !== trackId) {
            trackId = st.trackId ?? null;
            trackStart = performance.now();
            loadCover(trackId);
        }
        const wantObjects = !!opts?.spatial;
        if (wantObjects !== ob.on) {
            ob.on = wantObjects;
            if (!ob.on) { dropObjects(); ob.track = null; ob.fps = 0; }
        }
        if (ob.on && (state === 'playing' || state === 'paused')) askObjects(state === 'playing');
        else if (ob.on && state === 'stopped' && ob.track != null) { dropObjects(); ob.track = null; ob.fps = 0; }
        objectsNow(state, delayMs?.() || 0);
        if (state === 'playing') { askSpan(); askWave(); }
        else if (state === 'stopped' && (sp.slices.size || sp.ring != null)) {
            sp.slices.clear(); sp.onset.clear(); sp.loud.clear(); sp.prev = null; sp.ring = null; sp.state = 'none';
            for (const s of sinks) s.clearSlices();
            dropWave();
            beat.reset();
        }
        const live = (state === 'playing' || state === 'paused') && sp.rate > 0 && sp.slices.size > 0;
        out.live = live ? 1 : 0;
        playGlide += ((state === 'playing' ? 1 : 0) - playGlide) * Math.min(1, dt / 0.1);
        out.playing = playGlide;
        out.trackLength = Number(st.durationS) || 0;
        out.sampleRate = Number(st.outRate) || 0;
        out.trackAge = trackId != null ? (performance.now() - trackStart) / 1000 : 0;
        out.palette = cover.palette;
        out.hasCover = cover.has ? 1 : 0;
        out.cover = cover.img;
        if (!live) {
            out.trackTime = Number(st.positionS) || 0;
            out.progress = out.trackLength ? clamp01(out.trackTime / out.trackLength) : 0;
            for (const k of ['bass', 'mid', 'treble', 'loud', 'energy', 'onset', 'bright', 'width', 'ahead', 'bpm', 'beatConf']) out[k] = 0;
            out.hit = [0, 0, 0];
            out.audio.fill(0, 0, 512);
            out.audio.fill(128, 512);
            return out;
        }
        const run = sp.state === 'playing' && state === 'playing';
        const qps = sp.rate / sp.step;
        const delay = delayMs?.() || 0;
        const heard = sp.now + ((run ? performance.now() - sp.at : 0) + LAG_MS - delay) * sp.rate / 1000;
        const qNow = (heard + ATTACK_MS * sp.rate / 1000) / sp.step;
        out.kBase = Math.floor(qNow);
        out.q = qNow - out.kBase;
        out.qps = qps;
        out.trackTime = heard / sp.rate;
        out.progress = out.trackLength ? clamp01(out.trackTime / out.trackLength) : 0;
        if (wv.d && wv.rate) {
            const wq = heard / wv.d;
            out.wBase = Math.floor(wq);
            out.wq = wq - out.wBase;
            out.wRate = wv.rate / wv.d;
        }
        // How far ahead the slices reach from now.
        let kf = out.kBase;
        while (sp.slices.has(kf + 1) && kf - out.kBase < 400) kf++;
        out.ahead = Math.max(0, (kf - qNow) / qps);
        // The values for now.
        const a = sp.slices.get(out.kBase), b = sp.slices.get(out.kBase + 1);
        for (let r = 0; r < MROWS; r++) NOWV[r] = a && b ? a[r] + (b[r] - a[r]) * out.q : a ? a[r] : b ? b[r] : 0;
        out.bass = NOWV[ROW.BASS]; out.mid = NOWV[ROW.MID]; out.treble = NOWV[ROW.TREBLE];
        out.hit = [NOWV[ROW.HIT], NOWV[ROW.HIT + 1], NOWV[ROW.HIT + 2]];
        out.loud = NOWV[ROW.LOUD]; out.energy = NOWV[ROW.ENERGY]; out.onset = NOWV[ROW.ONSET];
        out.bright = NOWV[ROW.BRIGHT]; out.width = NOWV[ROW.WIDTH];
        // Tempo, twice a second.
        if (performance.now() - beatAt > 500) { beatAt = performance.now(); beat.update(qNow); }
        out.bpm = beat.bpm * (beat.conf > 0.05 ? 1 : 0);
        out.beatConf = beat.conf;
        out.beatRef = (beat.kRef - qNow) / qps;
        // Shadertoy's iChannel0: the spectrum 0–11 kHz, linear (row 0), the waveform (row 1).
        for (let i = 0; i < 512; i++) {
            const hz = (i + 0.5) * 11025 / 512;
            const fb = Math.log(Math.max(hz, 25) / 25) / Math.log(800) * BANDS - 0.5;
            const i0 = Math.max(0, Math.min(BANDS - 1, Math.floor(fb))), i1 = Math.min(BANDS - 1, i0 + 1);
            const fr = Math.max(0, Math.min(1, fb - i0));
            const dbv = (NOWV[ROW.SPEC_DB + i0] * (1 - fr) + NOWV[ROW.SPEC_DB + i1] * fr) * 96 - 96;
            out.audio[i] = Math.round(255 * clamp01((dbv + 100) / 70));
        }
        if (wv.d) {
            const wq0 = heard / wv.d - 256;
            for (let i = 0; i < 512; i++) out.audio[512 + i] = Math.round(255 * clamp01(0.5 + 0.5 * waveAt(wq0 + i, 2)));
        }
        return out;
    }

    return {
        frame,
        /// A drawing that keeps the data on its card: given all there is now,
        /// then every change.
        attach(sink) {
            sinks.add(sink);
            sink.clearSlices();
            for (const [k, col] of sp.slices) sink.setSlice(k, col);
            sink.clearWave?.();
            if (wv.last >= 0) {
                // The ring's samples that are still this lap's, oldest first.
                const from = Math.max(0, wv.last - WAVE_RING + 1);
                let i = from;
                while (i <= wv.last) {
                    if (wv.lap[i % WAVE_RING] !== Math.floor(i / WAVE_RING)) { i++; continue; }
                    let j = i;
                    while (j <= wv.last && wv.lap[j % WAVE_RING] === Math.floor(j / WAVE_RING) && (j % WAVE_RING) !== WAVE_RING - 1) j++;
                    const n = Math.min(j, wv.last) - i + 1;
                    const lr = new Float32Array(n * 2);
                    for (let x = 0; x < n; x++) { const s = (i + x) % WAVE_RING; lr[x * 2] = wv.buf[s * 2]; lr[x * 2 + 1] = wv.buf[s * 2 + 1]; }
                    sink.setWave?.(i, n, lr);
                    i += n;
                }
            }
            sink.setCover?.(cover.img);
            sink.clearObjects?.();
            sink.clearNotes?.();
            const ks = [...ob.frames.keys()].sort((a, b) => a - b);
            for (let i = 0; i < ks.length;) {
                let j = i;
                while (j + 1 < ks.length && ks[j + 1] === ks[j] + 1) j++;
                sink.setObjects?.(ks[i], j - i + 1, objectRows(ks[i], j - i + 1));
                if (map.on) sink.setNotes?.(ks[i], noteColumns(ks[i], j - i + 1));
                i = j + 1;
            }
        },
        detach(sink) { sinks.delete(sink); },
        /// For our checks.
        debug: () => ({ state: sp.state, rate: sp.rate, step: sp.step, slices: sp.slices.size, synced: sp.synced, ahead: out.ahead,
            wave: { d: wv.d, rate: wv.rate, last: wv.last }, bpm: beat.bpm, conf: beat.conf, palette: cover.palette, hasCover: cover.has,
            objects: { on: ob.on, track: ob.track, fps: ob.fps, frames: ob.frames.size, nowS: ob.nowS, progress: ob.progress, gpu: ob.gpu,
                source: ob.source, state: ob.state, total: ob.total, busy: ob.busy, now: Array.from(out.objNow) },
            map: { id: map.id, count: map.count, notes: map.on ? map.on.length : 0 } }),
        waveAt,
    };
}

/// Four colours of a cover: hues binned by how vivid they are, the four
/// strongest distinct ones (brightened so they read on the dark ground);
/// grey covers get the app's colours after their own.
export function paletteOf(img) {
    const N = 40;
    const c = document.createElement('canvas');
    c.width = N; c.height = N;
    const g = c.getContext('2d', { willReadFrequently: true });
    g.drawImage(img, 0, 0, N, N);
    const px = g.getImageData(0, 0, N, N).data;
    const BINS = 18;
    const bins = Array.from({ length: BINS }, () => ({ w: 0, r: 0, g: 0, b: 0 }));
    for (let i = 0; i < px.length; i += 4) {
        const r = px[i] / 255, gg = px[i + 1] / 255, b = px[i + 2] / 255;
        const mx = Math.max(r, gg, b), mn = Math.min(r, gg, b);
        const s = mx > 0 ? (mx - mn) / mx : 0;
        if (mx < 0.12 || s < 0.18) continue;
        let h;
        if (mx === mn) h = 0;
        else if (mx === r) h = ((gg - b) / (mx - mn) + 6) % 6;
        else if (mx === gg) h = (b - r) / (mx - mn) + 2;
        else h = (r - gg) / (mx - mn) + 4;
        const bin = bins[Math.floor(h / 6 * BINS) % BINS];
        const w = s * s * mx;
        bin.w += w; bin.r += r * w; bin.g += gg * w; bin.b += b * w;
    }
    const picks = [];
    const order = bins.map((b, i) => ({ ...b, i })).filter(b => b.w > 0).sort((a, b) => b.w - a.w);
    for (const b of order) {
        if (picks.length >= 4) break;
        if (picks.some(p => Math.min(Math.abs(p.i - b.i), BINS - Math.abs(p.i - b.i)) < 2)) continue;
        if (b.w < order[0].w * 0.04) break;
        picks.push(b);
    }
    const out = picks.map(b => {
        let col = [b.r / b.w, b.g / b.w, b.b / b.w];
        const mx = Math.max(...col);
        const k = mx > 0 ? Math.max(1, 0.85 / mx) : 1;      // bright enough on a dark ground
        return col.map(v => Math.min(1, v * k));
    });
    for (let i = 0; out.length < 4; i++) out.push(DEFAULT_PALETTE[i % 4]);
    return out;
}
