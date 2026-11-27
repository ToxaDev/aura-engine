// The analyzer's side of the simulated engine: what the analyzer window
// asks over /player/an/* (see the app's js/analytics/protocol.js), answered
// from the track's measured envelopes — peak and RMS per channel and 32 log
// bands 20 Hz–20 kHz at 20 frames a second. Everything shown is derived
// from those numbers: the spectra and spectrograms are the bands spread over
// the analyzer's grid, the loudness is the power of the frames weighted by
// the K curve over the bands, peaks are the frames' peaks. What the
// envelopes cannot tell (true peak, DR, correlation, bandwidth, …) is left
// unknown, as the analyzer shows anything not measured yet. The output side
// (O) is the source with the EQ's curve on it.

const SRC_RATE = 44100;
const SPEC_HOP = 2205;               // source samples per spectrogram column: one envelope frame (50 ms)
const NB = 512, F0 = 5;              // the analyzer's log bands from 5 Hz
const FLOOR = -120;
const TILE = 64;                     // spectrogram columns per tile
const LIVE_HOP_LOG2 = 14;            // live columns: 16384 output samples (46 ms at 352.8 kHz)
const WAVE_TS = 256;                 // waveform samples per entry, entries per tile
const LIVE_SID = 4294967294;

const enc = new TextEncoder();
class W {
  constructor(n = 1024) { this.b = new ArrayBuffer(n); this.v = new DataView(this.b); this.o = 0; }
  need(n) {
    if (this.o + n <= this.b.byteLength) return;
    const nb = new ArrayBuffer(Math.max(this.b.byteLength * 2, this.o + n));
    new Uint8Array(nb).set(new Uint8Array(this.b)); this.b = nb; this.v = new DataView(nb);
  }
  tag(s) { this.need(4); for (let i = 0; i < 4; i++) this.v.setUint8(this.o++, s.charCodeAt(i)); return this; }
  u8(x) { this.need(1); this.v.setUint8(this.o, x); this.o += 1; return this; }
  u16(x) { this.need(2); this.v.setUint16(this.o, x, true); this.o += 2; return this; }
  u32(x) { this.need(4); this.v.setUint32(this.o, x >>> 0, true); this.o += 4; return this; }
  u64(x) { this.need(8); this.v.setBigUint64(this.o, BigInt(Math.max(0, Math.floor(x))), true); this.o += 8; return this; }
  f32(x) { this.need(4); this.v.setFloat32(this.o, x, true); this.o += 4; return this; }
  f64(x) { this.need(8); this.v.setFloat64(this.o, x, true); this.o += 8; return this; }
  bytes(a) { this.need(a.length); new Uint8Array(this.b, this.o, a.length).set(a); this.o += a.length; return this; }
  out() { return new Uint8Array(this.b, 0, this.o); }
}
const db8 = v => v / 2 - 127.5;                       // the envelope files' byte scale
const lin = db => Math.pow(10, db / 20);
const byte = db => Math.max(0, Math.min(255, Math.round((db - FLOOR) / -FLOOR * 255)));

// The K curve (BS.1770's pre-filter and RLB high-pass), dB, near enough at
// a band's centre.
function kDb(f) {
  const hp = 10 * Math.log10(1 / (1 + Math.pow(38 / f, 4)));
  const shelf = 4 * (1 / (1 + Math.pow(1500 / f, 2)));
  return hp + shelf;
}

/// `env` gives the sim's state: track(), ready(slug), position(), state(),
/// eq() → { gainDb(f), preDb, rev } or null when the EQ is off, outRate.
export function createAnalyzer(env) {
  const cache = new Map();            // slug -> derived per-track numbers
  let seq = 0, live = { id: null, colT: 0, ptT: -1 };

  function derive(d) {
    const n = d.frames, nb = d.nb, edges = d.edges;
    const centres = new Float64Array(nb);
    for (let b = 0; b < nb; b++) centres[b] = Math.sqrt(edges[b] * edges[b + 1]);
    // per frame: mean square of L+R, and how the K curve changes the frame's power (from its bands)
    const ms = new Float64Array(n), pw = new Float64Array(n * nb);
    for (let f = 0; f < n; f++) {
      const rl = lin(db8(d.rms[2 * f])), rr = lin(db8(d.rms[2 * f + 1]));
      ms[f] = rl * rl + rr * rr;
      for (let b = 0; b < nb; b++) pw[f * nb + b] = Math.pow(10, db8(d.bands[f * nb + b]) / 10);
    }
    let pkL = -200, pkR = -200, sL = 0, sR = 0;
    for (let f = 0; f < n; f++) {
      pkL = Math.max(pkL, db8(d.pk[2 * f])); pkR = Math.max(pkR, db8(d.pk[2 * f + 1]));
      sL += lin(db8(d.rms[2 * f])) ** 2; sR += lin(db8(d.rms[2 * f + 1])) ** 2;
    }
    const meanBands = new Float64Array(nb);
    for (let b = 0; b < nb; b++) {
      let s = 0; for (let f = 0; f < n; f++) s += pw[f * nb + b];
      meanBands[b] = 10 * Math.log10(s / n + 1e-30);
    }
    return { d, n, centres, ms, pw, pkL, pkR, rmsL: 10 * Math.log10(sL / n + 1e-30), rmsR: 10 * Math.log10(sR / n + 1e-30), meanBands,
    };
  }
  function get() {
    const t = env.track(); if (!t) return null;
    const d = env.ready(t.slug); if (!d) return null;
    let x = cache.get(t.slug);
    if (!x) { x = derive(d); cache.set(t.slug, x); }
    return x;
  }

  // Bands spread over a log grid: dB at frequency f from frame f (or the
  // track's mean), as power in one of `perBand` grid bands per envelope
  // band; nothing below 20 Hz but a slope, nothing above 20 kHz.
  function at(x, frame, f, ratioDb) {
    const nb = x.d.nb, lo = Math.log(20), hi = Math.log(20000);
    if (f > 20000) return FLOOR;
    const pos = (Math.log(Math.max(f, 20)) - lo) / (hi - lo) * nb - 0.5;
    const b1 = Math.max(0, Math.min(nb - 1, Math.floor(pos))), k = Math.max(0, Math.min(1, pos - b1));
    const v = frame < 0 ? x.meanBands : null;
    const g = b => { b = Math.max(0, Math.min(nb - 1, b)); return v ? v[b] : db8(x.d.bands[frame * nb + b]); };
    // Catmull-Rom through the band centres: a smooth curve, as a fine FFT draws it
    const p0 = g(b1 - 1), p1 = g(b1), p2 = g(b1 + 1), p3 = g(b1 + 2);
    const k2 = k * k, k3 = k2 * k;
    const cr = 0.5 * (2 * p1 + (p2 - p0) * k + (2 * p0 - 5 * p1 + 4 * p2 - p3) * k2 + (3 * p1 - p0 - 3 * p2 + p3) * k3);
    let db = Math.min(Math.max(p1, p2), cr) - ratioDb;
    if (f < 20) db -= 24 * Math.log2(20 / f);
    return db;
  }
  // The envelope files keep each band's loudest FFT bin; the analyzer's
  // grid bands are narrower and read their mean: the bin's peak over the
  // band's mean (about 10 dB for music), and the narrower band.
  const MAX_OVER_MEAN_DB = 10;
  const ratioDb = (fTop) => MAX_OVER_MEAN_DB - 10 * Math.log10((Math.log(fTop / F0) / NB) / (Math.log(1000) / 32));
  const eqDb = (eq, f) => (eq ? eq.gainDb(f) + eq.preDb : 0);

  // Loudness from frame powers, K-weighted through the bands (and the EQ's
  // curve for O): momentary 400 ms, short-term 3 s, ending at t.
  function loud(x, t, win, eq) {
    const c = cum(x, eq), fps = x.d.fps, f1 = Math.min(x.n, Math.round(t * fps)), f0 = Math.max(0, f1 - Math.round(win * fps));
    if (f1 <= f0) return -Infinity;
    return -0.691 + 10 * Math.log10((c[f1] - c[f0]) / (f1 - f0) + 1e-30);
  }
  // running sums of the frames' weighted power, per EQ state
  function cum(x, eq) {
    const key = eq ? eq.rev : 'src';
    if (x.cumKey === key) return x.cum;
    const nb = x.d.nb, kq = new Float64Array(nb);
    for (let b = 0; b < nb; b++) kq[b] = Math.pow(10, (kDb(x.centres[b]) + eqDb(eq, x.centres[b])) / 10);
    const c = new Float64Array(x.n + 1);
    for (let f = 0; f < x.n; f++) {
      let p = 0, pk = 0;
      for (let b = 0; b < nb; b++) { const e = x.pw[f * nb + b]; p += e; pk += e * kq[b]; }
      c[f + 1] = c[f] + x.ms[f] * (p > 0 ? pk / p : 1);
    }
    x.cumKey = key; x.cum = c;
    return c;
  }
  function series(x, eq) {
    const n = Math.max(0, Math.ceil(x.d.frames / x.d.fps * 10) - 4), s = new Float32Array(n), m = new Float32Array(n);
    for (let i = 0; i < n; i++) { const t = (i + 4) * 0.1; s[i] = clampL(loud(x, t, 3, eq)); m[i] = clampL(loud(x, t, 0.4, eq)); }
    return { s, m };
  }
  const clampL = v => (Number.isFinite(v) ? Math.max(-120, v) : -120);
  // BS.1770 gating over the 400 ms blocks (75 % overlap ≈ the 10 Hz series)
  function integrated(m) {
    const abs = Array.from(m).filter(v => v > -70);
    if (!abs.length) return NaN;
    const mean = a => 10 * Math.log10(a.reduce((s, v) => s + Math.pow(10, v / 10), 0) / a.length);
    const rel = mean(abs) - 10;
    const g = abs.filter(v => v > rel);
    return g.length ? mean(g) : NaN;
  }
  function lra(s) {
    const a = Array.from(s).filter(v => v > -70);
    if (!a.length) return NaN;
    const rel = 10 * Math.log10(a.reduce((x, v) => x + Math.pow(10, v / 10), 0) / a.length) - 20;
    const g = a.filter(v => v > rel).sort((p, q) => p - q);
    if (g.length < 2) return NaN;
    return g[Math.floor(g.length * 0.95) - 1] - g[Math.floor(g.length * 0.1)];
  }

  // ── AAN2: the whole track ───────────────────────────────────────────────
  function trackFrame(x, rev) {
    const eq = env.eq(), dur = x.d.frames / x.d.fps, nS = Math.round(dur * SRC_RATE);
    const sS = x.sSer || (x.sSer = series(x, null));
    const sO = eq ? series(x, eq) : sS;
    const metrics = (ser) => {
      const m = new Float64Array(32).fill(NaN);
      const I = integrated(ser.m), sp = Math.max(x.pkL, x.pkR);
      m[0] = I; m[3] = lra(ser.s); m[6] = lin(sp);           // SP is linear: 1.0 = 0 dBFS
      m[9] = 10 * Math.log10((lin(x.rmsL) ** 2 + lin(x.rmsR) ** 2) / 2);
      m[22] = sp - I;
      return m;
    };
    const mS = metrics(sS), mO = metrics(sO);
    if (eq) { mO[6] = NaN; mO[9] = NaN; mO[22] = NaN; }     // the output's own peaks are not in the envelopes
    mO[25] = 100;
    const prov = (m) => { const p = new Uint8Array(16); for (let i = 0; i < 32; i++) if (!Number.isNaN(m[i])) p[i >> 1] |= 3 << ((i & 1) * 4); return p; };
    const w = new W(64 * 1024);
    w.tag('AAN2').u8(1).u8(0).u8(0).u8(0).u32(rev).u32(1).f32(dur);
    for (const m of [mS, mS, mO]) for (let i = 0; i < 32; i++) w.f64(m[i]);
    w.bytes(prov(mS)).bytes(prov(mS)).bytes(prov(mO));
    w.u8(2).f32(x.pkL).f32(x.rmsL).f32(NaN).f32(0).f32(x.pkR).f32(x.rmsR).f32(NaN).f32(0);
    w.u8(0);
    const hist = new Float32Array(256);
    for (const v of sS.s) if (v > -80) hist[Math.min(255, Math.floor((v + 80) / 80 * 256))]++;
    w.u16(256); for (const v of hist) w.f32(v);
    w.u16(256); for (const v of hist) w.f32(v);
    w.u16(0).u16(0).u16(0);                                  // DR blocks, clips, overs: not in the envelopes
    w.u16(0);                                                // chain string
    w.u16(0);                                                // marks
    for (const ser of [sS, sS]) { w.u32(ser.s.length); for (const v of ser.s) w.f32(v); for (const v of ser.m) w.f32(v); }
    const lods = waveLods(nS);
    w.u8(lods.length).u8(2).u16(WAVE_TS);
    for (const c of lods) w.u32(c);
    for (let i = 0; i < lods.length; i++) w.u32(0);          // no output waveform
    const st = specTiles(x);
    for (let i = 0; i < lods.length; i++) w.u32(i === 0 ? st : 0);
    w.u32(SRC_RATE).u32(SPEC_HOP);
    w.u32(0).u32(0);                                         // true peak per 100 ms: not in the envelopes
    const gO = eq ? (eq.rev + 2) : 1;
    for (const g of [1, 1, gO]) w.u32(g).u32(st).u32(st).u16(NB).u16(0);
    w.u32(sO.s.length); for (const v of sO.s) w.f32(v); for (const v of sO.m) w.f32(v);
    return w.out();
  }
  function waveLods(n) {
    const out = [];
    let entries = Math.ceil(n / WAVE_TS), blocks = n / WAVE_TS, count = 1;
    while (blocks > 256) { blocks /= 4; count++; }
    for (let i = 0; i < count; i++) { out.push(Math.ceil(entries / WAVE_TS)); entries = Math.ceil(entries / 4); }
    return out;
  }
  const specTiles = x => Math.ceil(x.d.frames / TILE);

  // ── tiles ───────────────────────────────────────────────────────────────
  function waveTile(x, src, lod, i) {
    const per = WAVE_TS * Math.pow(4, lod), nS = Math.round(x.d.frames / x.d.fps * SRC_RATE);
    const e0 = i * WAVE_TS, e1 = Math.min(Math.ceil(nS / per), e0 + WAVE_TS), n = Math.max(0, e1 - e0);
    const w = new W(16 + n * 24);
    w.tag('AAWT').u8(1).u8(src).u8(lod).u8(2).u32(i).u32(n);
    const fps = x.d.fps, d = x.d;
    for (let e = e0; e < e1; e++) {
      const f0 = Math.min(d.frames - 1, Math.floor(e * per / SRC_RATE * fps));
      const f1 = Math.min(d.frames - 1, Math.max(f0, Math.floor((e + 1) * per / SRC_RATE * fps)));
      for (let c = 0; c < 2; c++) {
        let pk = 0, s = 0;
        for (let f = f0; f <= f1; f++) { pk = Math.max(pk, lin(db8(d.pk[2 * f + c]))); s += lin(db8(d.rms[2 * f + c])) ** 2; }
        const r = Math.sqrt(s / (f1 - f0 + 1));
        // within a frame the entries differ: a frame's peak is the most any of them reaches
        const k = per < SRC_RATE / fps ? 0.55 + 0.45 * hash(e * 2 + c) : 1;
        const p = Math.max(r * 1.05, pk * k);
        w.f32(-p).f32(p).f32(r);
      }
    }
    return w.out();
  }
  function specTile(x, src, i) {
    const eq = src === 2 ? env.eq() : null, gen = eq ? eq.rev + 2 : 1;
    const c0 = i * TILE, c1 = Math.min(x.d.frames, c0 + TILE), n = Math.max(0, c1 - c0);
    const w = new W(24 + n * NB);
    w.tag('AAST').u8(1).u8(src).u8(0).u8(gen & 255).u32(i).f32(FLOOR).f32(-FLOOR).u16(n).u16(NB);
    const step = Math.log((SRC_RATE / 2) / F0) / NB, rd = ratioDb(SRC_RATE / 2);
    const fr = x.fr || (x.fr = Float64Array.from({ length: NB }, (_, b) => F0 * Math.exp((b + 0.5) * step)));
    const eqc = eq ? Float64Array.from(fr, f => eqDb(eq, f)) : null;
    const col = new Uint8Array(NB);
    for (let c = c0; c < c1; c++) {
      for (let b = 0; b < NB; b++) col[b] = byte(at(x, c, fr[b], rd) + (eqc ? eqc[b] : 0));
      w.bytes(col);
    }
    return w.out();
  }

  // ── AAN3: the response and the whole-track spectra ──────────────────────
  function resp(x, q) {
    const n = Math.max(2, Math.min(4096, +q.n || 1024)), f0 = Math.max(1, +q.f0 || F0), f1 = Math.max(f0 * 1.01, +q.f1 || SRC_RATE / 2);
    const eq = env.eq(), rd = ratioDb(SRC_RATE / 2);
    const w = new W(22 + n * 32);
    w.tag('AAN3').u8(2).u8(0).u32(1).u32(n).f32(f0).f32(f1);
    const fs = Array.from({ length: n }, (_, i) => f0 * Math.pow(f1 / f0, i / (n - 1)));
    const h = fs.map(f => eqDb(eq, f)), s = fs.map(f => Math.max(FLOOR, at(x, -1, f, rd)));
    for (const v of h) w.f32(v).f32(v);
    for (let i = 0; i < n; i++) { const v = Math.max(FLOOR, s[i] + h[i]); w.f32(v).f32(v); }
    for (const v of s) w.f32(v).f32(v);
    for (const v of s) w.f32(v).f32(v);
    return w.out();
  }

  // ── AAN1: live, every ~100 ms ───────────────────────────────────────────
  function liveFrame(x) {
    const t = env.track(), pos = env.position(), out = env.outRate;
    if (live.id !== t.id || pos < live.colT - 0.5 || pos > live.colT + 3) live = { id: t.id, colT: pos, ptT: -1 };
    const colDur = Math.pow(2, LIVE_HOP_LOG2) / out;
    let cols = Math.floor((pos - live.colT) / colDur);
    if (cols <= 0 && pos - live.ptT < 0.09) return new Uint8Array(0);
    cols = Math.max(0, Math.min(cols, 60));
    const eq = env.eq(), step = Math.log((out / 2) / F0) / NB, rd = ratioDb(out / 2);
    const lS = clampL(loud(x, pos, 3, eq)), lM = clampL(loud(x, pos, 0.4, eq));
    const w = new W(256 + cols * NB);
    w.tag('AAN1').u8(2).u8(0x02).u32(++seq).u32(t.id).u32(0).u32(1).u8(100).u8(9).u8(-FLOOR).u8(LIVE_HOP_LOG2);
    const lm = new Array(26).fill(NaN); lm[1] = lS; lm[2] = lM;
    for (const v of lm) w.f32(v);
    const pv = new Uint8Array(13); pv[0] |= 3 << 4; pv[1] |= 3;   // metrics 1 and 2 measured
    w.bytes(pv).u8(0);
    w.u16(1).f32(pos).f32(lS).f32(lM);
    live.ptT = pos;
    w.u16(cols);
    const col = new Uint8Array(NB);
    for (let c = 0; c < cols; c++) {
      live.colT += colDur;
      const fr = Math.max(0, Math.min(x.d.frames - 1, Math.round(live.colT * x.d.fps)));
      for (let b = 0; b < NB; b++) { const f = F0 * Math.exp((b + 0.5) * step); col[b] = byte(at(x, fr, f, rd) + eqDb(eq, f)); }
      w.bytes(col);
    }
    return w.out();
  }

  let trackRev = 0, revKey = '';
  return {
    sid: LIVE_SID,
    route(path, q) {
      const r = path.slice('/player/an/'.length);
      const x = get();
      switch (r) {
        case 'live': return env.state() === 'playing' && x ? liveFrame(x) : new Uint8Array(0);
        case 'track': {
          if (!x) return new Uint8Array(0);      // no data yet: an empty answer, the page waits
          const eq = env.eq(), key = env.track().id + '|' + (eq ? eq.rev : 'off');
          if (key !== revKey) { revKey = key; trackRev++; }
          if (+q.rev === trackRev) { const w = new W(8); w.tag('AAN2').u32(trackRev); return w.out(); }
          return trackFrame(x, trackRev);
        }
        case 'resp': return x ? resp(x, q) : new Uint8Array(0);
        case 'wave': return x ? waveTile(x, src(q.src), +q.lod || 0, +q.i || 0) : null;
        case 'spec': return x ? specTile(x, src(q.src), +q.i || 0) : null;
        case 'specz': { const w = new W(12); w.tag('AAZR').u8(1).u8(2).u16(0).u32(0); return w.out(); }
        case 'wavez': { const w = new W(36); w.tag('AAWZ').u8(2).u8(src(q.src)).u8(0).u8(0).u32(0).u64(+q.s0 || 0).u64(+q.s1 || 0).u32(0).u32(0); return w.out(); }
        case 'selstat': { const w = new W(28); w.tag('AASL').u8(1).u8(2).u8(src(q.src)).u8(0).u32(0).u64(+q.s0 || 0).u64(+q.s1 || 0); return w.out(); }
        default: return null;
      }
    },
  };
}
const src = s => (s === 'B' ? 1 : s === 'O' ? 2 : 0);
function hash(n) { let x = (n | 0) * 2654435761; x ^= x >>> 15; x = Math.imul(x, 2246822519); x ^= x >>> 13; return ((x >>> 0) % 1000) / 1000; }
