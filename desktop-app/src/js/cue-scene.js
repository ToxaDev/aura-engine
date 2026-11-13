// Cue Consistency's stage, as the listener faces it: each source the stage
// finds stands on a floor as a column of its own wave, the newest moment at
// the floor and the last seconds rising above it, as wide as the source is
// loud. Its colour is where its sound lies, warm low, blue and violet high.
// Depth is how much of a place reads as diffuse, an estimate and not a
// distance in metres.

/// A place reading this share of direct sound or less stands at the far end.
const FARTHEST_DIRECT = 0.55;
/// The range of an image under the loudest band heard lately, dB.
const IMAGE_DB = 50;
/// How fast that loudest band is forgotten, dB a second.
const TOP_FALL_DB_S = 1;
const MONO = "ui-monospace,'Cascadia Mono','Segoe UI Mono',Consolas,monospace";
/// The wave scrolls smoothly at this many frames a second; more is not seen.
const FRAME_MS = 33;
/// Seconds of a source's wave a column shows, floor to top.
const COLUMN_S = 3;
/// Hops the picture stays behind the newest one the window has: the next
/// view (every 100 ms) brings the hops it is about to show before it gets there.
const LAG_HOPS = 10;
/// How long the playhead takes to catch up with where it should be, s.
const CHASE_S = 0.3;
/// Further off than this, s, the playhead jumps instead: a seek, a new track.
const SNAP_S = 1;
/// The programme's level sets the columns' scale: it climbs to a louder wave
/// this fast, dB a second, and is forgotten this fast, so a loud passage
/// does not leave the next quiet one too thin.
const REF_RISE_DB_S = 6;
const REF_FALL_DB_S = 1.5;
/// Quieter than this never fills a column: silence stays silence.
const PEAK_FLOOR = 1e-3;
/// A view whose newest hop is this many hops before the last one's comes from
/// another chain (a seek back, a new track): the waves start over. Less is the
/// heard clock's own jitter.
const BACK_HOPS = 2;
/// A column's width follows its wave in dB over this range under the
/// programme's level: each step down is the same step thinner, so a quiet
/// source beside a dense loud one is a thin column, a medium one clearly
/// thicker, not all of them hairlines beside one wide column.
const WIDTH_DB = 36;
/// Bent a little so the loud end keeps its steps too: 6 dB under the loudest
/// is clearly narrower than the loudest.
const WIDTH_CURVE = 1.3;
/// The narrowest a sounding column gets, a share of its widest; it fades to
/// nothing over the next `WIDTH_FADE_DB` below `WIDTH_DB`.
const WIDTH_MIN = 0.1;
const WIDTH_FADE_DB = 18;

/// A sample against the programme's level `ref`, as a share of the column's
/// widest half-width, with its sign.
export function widthOf(v, ref) {
    const m = Math.abs(v) / ref;
    if (!(m > 0)) return 0;
    const db = Math.min(0, 20 * Math.log10(m));
    const f = Math.pow(Math.max(0, (db + WIDTH_DB) / WIDTH_DB), WIDTH_CURVE);
    const gate = Math.min(1, Math.max(0, (db + WIDTH_DB + WIDTH_FADE_DB) / WIDTH_FADE_DB));
    return Math.sign(v) * (WIDTH_MIN * gate + (1 - WIDTH_MIN) * f);
}

export function depthOf(direct) {
    return Math.min(1, Math.max(0, (1 - direct) / (1 - FARTHEST_DIRECT)));
}

/// An image, 0–1 band by band, against the loudest band heard lately.
export function imageOf(specDb, topDb) {
    const n = specDb.length;
    const v = specDb.map(db => Math.min(1, Math.max(0, (db - (topDb - IMAGE_DB)) / IMAGE_DB)));
    return v.map((x, i) => (v[Math.max(i - 1, 0)] + 2 * x + v[Math.min(i + 1, n - 1)]) / 4);
}

/// The speakers' angle off centre from the listening geometry, degrees.
export function speakerDeg(g) {
    const dist = g ? (g.distL + g.distR) / 2 : 0;
    if (!(g?.span > 0) || !(dist > 0)) return 30;
    return Math.asin(Math.min(1, g.span / 2 / dist)) * 180 / Math.PI;
}

/// Where a place stands before the stage: between the speakers, a hard pan
/// at a speaker, read off the mix's angle (90° = hard pan).
export function originalDeg(deg, speakerAngle) {
    return deg * speakerAngle / 90;
}

/// The angle the stage plays a place at: the mix's angle narrowed by Stage
/// width, as `decide` does.
export function playedDeg(deg, stageWidth) {
    return deg * stageWidth / 90;
}

/// Woodworth's time difference for an angle, in head radii over the speed of sound.
const woodworth = (deg) => { const th = Math.abs(deg) * Math.PI / 180; return Math.sign(deg) * (th + Math.sin(th)); };

/// The angle whose time difference the stage gives a place at `deg` when ITD
/// scale is `itdScale` %: Woodworth inverted, capped at 90°.
export function itdDeg(deg, itdScale) {
    const want = Math.abs(woodworth(deg) * itdScale / 100);
    if (want >= woodworth(90)) return 90 * Math.sign(deg);
    let lo = 0, hi = 90;
    for (let i = 0; i < 40; i++) { const m = (lo + hi) / 2; if (woodworth(m) < want) lo = m; else hi = m; }
    return Math.sign(deg) * (lo + hi) / 2;
}

export function degText(deg) {
    const v = Math.round(Math.abs(deg));
    return v === 0 ? '0°' : `${deg > 0 ? 'L' : 'R'} ${v}°`;
}

/// Follows sources by the number the stage gives each from view to view:
/// `shown` keeps what is on screen, a number seen again takes its new
/// reading, a missing one fades out, a new one fades in with an empty wave.
export function followSources(shown, dots, make) {
    const byId = new Map(dots.map(d => [d.id, d]));
    for (const c of shown) {
        const d = byId.get(c.id);
        c.goal = d ? 1 : 0;
        if (d) { c.to = make(d); byId.delete(c.id); }
    }
    for (const d of byId.values()) {
        const to = make(d);
        shown.push({ ...to, id: d.id, sp: [...to.sp], a: 0, goal: 1, to, wave: newWave() });
    }
    return shown;
}

/// A source's wave as kept: the lowest and highest sample of each hop, the
/// last one hop number `last`.
export const newWave = () => ({ last: -1, lo: [], hi: [] });

/// Adds a view's wave to what is kept. The wave is [lo, hi, ...] oldest first,
/// its last pair hop `seq`; views overlap, so only hops newer than the last
/// kept are taken. A gap between them (the source was not read) is silence;
/// at most `keep` hops stay.
export function mergeWave(h, wave, seq, keep) {
    const n = (wave?.length ?? 0) >> 1;
    if (!n || seq <= h.last) return h;
    const first = seq - (n - 1);
    if (h.lo.length && first - h.last > keep) { h.lo.length = 0; h.hi.length = 0; }
    if (h.lo.length) for (let g = h.last + 1; g < first; g++) { h.lo.push(0); h.hi.push(0); }
    for (let i = h.lo.length ? Math.max(0, h.last + 1 - first) : 0; i < n; i++) {
        h.lo.push(wave[2 * i]); h.hi.push(wave[2 * i + 1]);
    }
    h.last = seq;
    const cut = h.lo.length - keep;
    if (cut > 0) { h.lo.splice(0, cut); h.hi.splice(0, cut); }
    return h;
}

/// The kept pair of hop `hop`, or silence where there is none.
export function waveAt(h, hop) {
    const j = hop - (h.last - h.lo.length + 1);
    return j >= 0 && j < h.lo.length ? [h.lo[j], h.hi[j]] : [0, 0];
}

/// Whether a view's newest hop went back from the last one's: another chain,
/// the waves kept belong to something no longer heard.
export function steppedBack(prevSeq, seq) {
    return prevSeq != null && seq < prevSeq - BACK_HOPS;
}

/// The hop the picture shows at the floor, moved on by `dt` seconds: it runs
/// at the hops' own rate, a little faster or slower to meet `target` within
/// about `CHASE_S`, never past `newest` (nothing is known beyond it, and a
/// pause holds the picture), and jumps when it is far off.
export function chasePlayhead(ph, target, newest, rate, dt) {
    if (ph == null || Math.abs(target - ph) > rate * SNAP_S) return Math.min(target, newest);
    const speed = Math.min(2, Math.max(0, 1 + (target - ph) / (rate * CHASE_S)));
    return Math.min(newest, ph + rate * dt * speed);
}

/// The columns' scale, the programme's level lately: it climbs to a louder
/// wave at `REF_RISE_DB_S` (at once from silence) and falls at
/// `REF_FALL_DB_S`, never below `PEAK_FLOOR`. A kick drum's hit alone
/// does not shrink every other column: the scale moves a dB or so a beat,
/// and the kick's own column fills out instead.
export function followRef(ref, dt, louder) {
    const fell = ref * Math.pow(10, -REF_FALL_DB_S * dt / 20);
    if (louder <= fell) return Math.max(PEAK_FLOOR, fell);
    const up = ref <= PEAK_FLOOR * 1.0001 ? louder : Math.min(louder, ref * Math.pow(10, REF_RISE_DB_S * dt / 20));
    return Math.max(PEAK_FLOOR, fell, up);
}

/// Where an image's weight lies across the bands, 0 (lowest) to 1 (highest).
export function centroidOf(sp) {
    const n = sp.length;
    if (n < 2) return .5;
    let s = 0, m = 0;
    sp.forEach((v, i) => { s += v; m += v * i; });
    return s > 0 ? m / s / (n - 1) : .5;
}

const STOPS = [[0, [233, 150, 92]], [.3, [217, 179, 140]], [.58, [56, 189, 248]], [.82, [167, 139, 250]], [1, [224, 231, 255]]];

/// The colour of a place in the range, 0 low (warm) to 1 high (blue, violet).
export function colourAt(t) {
    t = Math.min(1, Math.max(0, t));
    for (let i = 1; i < STOPS.length; i++) {
        if (t <= STOPS[i][0]) {
            const [a, c] = STOPS[i - 1];
            const [b, d] = STOPS[i];
            const u = (t - a) / (b - a);
            return c.map((x, j) => Math.round(x + (d[j] - x) * u));
        }
    }
    return STOPS[STOPS.length - 1][1];
}

const rgba = (c, a) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

/// `canvas` is drawn into; `tip` is the card over a source.
export function createScene(canvas, tip) {
    const x = canvas.getContext('2d');
    let W = 720, H = 350, HOR = 98, FLOOR = 320, DPR = 1;
    let shown = [];
    let hz = [];
    let topDb = -200;
    let bypassed = false, running = false, xtc = false, spk = 30;
    let t = 0, last = performance.now(), drawn = 0;
    let stage = { width: 90, itd: 100 };
    // The waves' clock: the newest hop the window has and when it came, the
    // hops a second, the hop shown at the floor now, the loudest wave lately.
    let seq = null, seqAt = 0, rate = 86, ph = null, peak = PEAK_FLOOR;
    const hits = [];
    const dust = Array.from({ length: 90 }, () => ({ x: Math.random(), y: Math.random(), r: .3 + Math.random() * .9, p: Math.random() * 6.28 }));

    const stops = (g, alpha) => {
        for (let i = 0; i <= 8; i++) g.addColorStop(i / 8, rgba(colourAt(i / 8), alpha));
        return g;
    };
    // A plain perspective of a flat floor: the angle is a place across it, the
    // depth a distance along it, and both shrink as one towards a single
    // vanishing point at the middle of the horizon.
    const proj = (deg, z) => {
        const s = 1 / (1 + 1.9 * z);
        return { s, x: W / 2 - (deg / 90) * (W * .46) * s, y: HOR + (FLOOR - HOR) * s };
    };

    function fit() {
        const r = canvas.parentElement.getBoundingClientRect();
        if (!r.width || !r.height) return;
        DPR = Math.min(2, window.devicePixelRatio || 1);
        W = r.width; H = r.height;
        HOR = H * .28; FLOOR = H - 30;
        canvas.width = Math.round(W * DPR);
        canvas.height = Math.round(H * DPR);
    }

    function sky() {
        const g = x.createLinearGradient(0, 0, 0, H);
        g.addColorStop(0, '#050a14'); g.addColorStop(HOR / H, '#0b1c33'); g.addColorStop((HOR + 1) / H, '#071222'); g.addColorStop(1, '#03070d');
        x.fillStyle = g; x.fillRect(0, 0, W, H);
        const h = x.createRadialGradient(W / 2, HOR, 0, W / 2, HOR, W * .47);
        h.addColorStop(0, 'rgba(56,189,248,.22)'); h.addColorStop(.4, 'rgba(56,189,248,.06)'); h.addColorStop(1, 'rgba(56,189,248,0)');
        x.fillStyle = h; x.fillRect(0, 0, W, H);
        for (const d of dust) {
            x.fillStyle = `rgba(186,230,253,${.10 + .22 * (.5 + .5 * Math.sin(d.p + t * .5))})`;
            x.beginPath(); x.arc(d.x * W, d.y * HOR, d.r, 0, 6.3); x.fill();
        }
    }

    function floor() {
        x.lineWidth = 1;
        for (let d = -90; d <= 90; d += 15) {
            const a = proj(d, 0), far = proj(d, 6);
            const g = x.createLinearGradient(a.x, a.y, far.x, far.y);
            g.addColorStop(0, `rgba(125,211,252,${d % 45 === 0 ? .30 : .15})`); g.addColorStop(1, 'rgba(125,211,252,.02)');
            x.strokeStyle = g;
            x.beginPath(); x.moveTo(a.x, a.y); x.lineTo(far.x, far.y); x.stroke();
        }
        for (const z of [0, .25, .5, .75, 1]) {
            const y = proj(0, z).y;
            x.strokeStyle = `rgba(125,211,252,${.26 - .17 * z})`;
            x.beginPath(); x.moveTo(proj(90, z).x, y); x.lineTo(proj(-90, z).x, y); x.stroke();
        }
        // A slow wave leaving the listener: the space is alive while nothing plays too.
        const wz = (t * .14) % 1, wy = proj(0, wz).y;
        x.strokeStyle = `rgba(217,179,140,${.20 * (1 - wz)})`;
        x.beginPath(); x.moveTo(proj(90, wz).x, wy); x.lineTo(proj(-90, wz).x, wy); x.stroke();
    }

    function marks() {
        x.font = `11px ${MONO}`;
        x.fillStyle = 'rgba(148,163,184,.75)';
        x.textAlign = 'left';
        for (const [z, s] of [[0, 'NEAR'], [.5, 'MID'], [1, 'FAR']]) {
            const p = proj(90, z);
            x.fillText(s, Math.max(8, p.x + 4), p.y - 3);
        }
        x.textAlign = 'center';
        for (const d of [90, 45, 0, -45, -90]) {
            const p = proj(d, 0);
            x.fillText(d === 0 ? '0°' : `${d > 0 ? 'L' : 'R'} ${Math.abs(d)}°`, Math.min(W - 22, Math.max(22, p.x)), FLOOR + 13);
        }
    }

    function speakers() {
        if (!running || !xtc) return;
        for (const side of [1, -1]) {
            const p = proj(spk * side, .62);
            const r = 4 + 3 * p.s;
            x.save(); x.translate(p.x, p.y); x.rotate(Math.PI / 4);
            x.strokeStyle = 'rgba(45,212,191,.55)'; x.fillStyle = 'rgba(45,212,191,.10)'; x.lineWidth = 1;
            x.fillRect(-r, -r, 2 * r, 2 * r); x.strokeRect(-r, -r, 2 * r, 2 * r);
            x.restore();
        }
    }

    /// A column's size where it stands: its height above the floor and its
    /// widest half-width, the loudest wave lately.
    const sizeAt = (p) => ({ hh: (H * .44) * p.s + 14, max: 40 * p.s + 4 });
    /// Hops a column shows, floor to top.
    const span = () => Math.round(COLUMN_S * rate);
    // The column's light: the source's colour, fading out as the wave gets older.
    const fading = (col, base, hh, alpha) => {
        const g = x.createLinearGradient(0, base, 0, base - hh);
        g.addColorStop(0, rgba(col, alpha)); g.addColorStop(.6, rgba(col, .5 * alpha)); g.addColorStop(1, rgba(col, 0));
        return g;
    };
    // Each hop of the wave from the floor up: its height in the column and its
    // lowest and highest sample in pixels across, against the loudest wave lately.
    function eachHop(c, base, hh, max, every, f) {
        const n = span(), top = Math.floor(ph);
        for (let i = 0; i <= n; i += every) {
            const hop = top - i;
            const [lo, hi] = waveAt(c.wave, hop);
            f(base - (ph - hop) / n * hh, max * widthOf(lo, peak), max * widthOf(hi, peak));
        }
    }

    // Where the stage plays it: the wave itself, one outline, its highest
    // samples up the right side and its lowest back down the left.
    function column(c, cx, base, hh, max, fog) {
        const col = colourAt(centroidOf(c.sp));
        const lo = [];
        x.beginPath(); x.moveTo(cx, base);
        eachHop(c, base, hh, max, 1, (y, l, h) => { x.lineTo(cx + h, y); lo.push(y, l); });
        for (let i = lo.length - 2; i >= 0; i -= 2) x.lineTo(cx + lo[i + 1], lo[i]);
        x.closePath();
        x.fillStyle = fading(col, base, hh, .9 * fog);
        x.shadowColor = rgba(col, .6); x.shadowBlur = (14 * hh / H + 4) * fog;
        x.fill();
        x.shadowBlur = 0;
    }
    // Where the speakers alone would put it: the same wave as a phantom, in
    // thin strips a few hops apart.
    function phantom(c, cx, base, hh, max, fog) {
        const col = colourAt(centroidOf(c.sp));
        x.beginPath();
        eachHop(c, base, hh, max, 3, (y, l, h) => x.rect(cx + l, y, Math.max(.6, h - l), 1));
        x.fillStyle = fading(col, base, hh, .9 * fog);
        x.fill();
    }
    // A column with nothing to show in the seconds it spans is not drawn at
    // all: no empty mark, no angle.
    function sounding(c) {
        let any = false;
        eachHop(c, 0, 1, 1, 2, (y, l, h) => { if (h - l > .02) any = true; });
        return any;
    }
    // The light on the floor under a column.
    function standing(p, max, fog) {
        const pool = x.createRadialGradient(p.x, p.y, 0, p.x, p.y, max * 1.5);
        pool.addColorStop(0, `rgba(217,179,140,${.30 * fog})`); pool.addColorStop(1, 'rgba(217,179,140,0)');
        x.fillStyle = pool; x.beginPath(); x.ellipse(p.x, p.y, max * 1.5, max * .34, 0, 0, 6.3); x.fill();
    }
    const labels = [];
    // Angles are written after every image, loudest first, each where it does
    // not cover one already written; the rest stay in the card on hover.
    function writeLabels() {
        x.font = `600 13px ${MONO}`; x.textAlign = 'center'; x.textBaseline = 'middle';
        const placed = [];
        labels.sort((a, b) => b.w - a.w);
        for (const l of labels) {
            const w = x.measureText(l.text).width + 10, h = 17;
            const r = { x: l.x - w / 2, y: l.y - h / 2, w, h };
            if (placed.some(q => r.x < q.x + q.w + 2 && q.x < r.x + r.w + 2 && r.y < q.y + q.h && q.y < r.y + r.h)) continue;
            placed.push(r);
            x.fillStyle = `rgba(5,12,24,${.78 * l.a})`;
            x.beginPath(); x.roundRect(r.x, r.y, r.w, r.h, 4); x.fill();
            x.fillStyle = `rgba(241,245,249,${l.a})`;
            x.fillText(l.text, l.x, l.y + .5);
        }
        x.textBaseline = 'alphabetic';
        labels.length = 0;
    }

    /// A source is drawn solid where the stage plays it now. Where the speakers
    /// alone would put it, between them, stays a phantom in strips, with a
    /// trail on the floor out to where it sounds: the wider the stage, the
    /// further they part.
    function figure(c) {
        if (ph == null || !sounding(c)) return;
        const fog = (1 - .5 * c.z) * c.a * (bypassed ? .35 : 1);
        const og = originalDeg(c.deg, spk);
        const away = Math.min(1, Math.max(0, (Math.abs(c.pd - og) - .5) / 4));
        const p = proj(c.pd, c.z), ps = sizeAt(p);
        x.save();
        if (c.z > .45) x.filter = `blur(${(c.z - .45) * 3}px)`;
        if (away > 0) {
            const g = proj(og, c.z), gs = sizeAt(g);
            phantom(c, g.x, g.y, gs.hh, gs.max, .45 * fog * away);
            x.setLineDash([3, 3]); x.lineWidth = 1;
            x.strokeStyle = `rgba(217,179,140,${.55 * fog * away})`;
            x.beginPath(); x.moveTo(g.x, g.y); x.lineTo(p.x, p.y); x.stroke();
            x.setLineDash([]);
            x.fillStyle = `rgba(217,179,140,${.7 * fog * away})`;
            const dir = Math.sign(g.x - p.x);
            x.beginPath(); x.moveTo(p.x + dir * 2, p.y); x.lineTo(p.x + dir * 8, p.y - 3); x.lineTo(p.x + dir * 8, p.y + 3); x.fill();
        }
        standing(p, ps.max, fog);
        column(c, p.x, p.y, ps.hh, ps.max, fog);
        x.restore();
        if (c.w >= .08) labels.push({ text: degText(c.pd), x: p.x, y: p.y + 13 + 6 * p.s, w: c.w, a: .9 * c.a * (bypassed ? .5 : 1) });
        hits.push({ c, x: p.x, y: p.y - ps.hh / 2, r: Math.max(16, ps.hh / 2) });
    }

    // The colours' key: where in the range a source's sound mostly lies.
    function legend() {
        if (hz.length < 2) return;
        const lx = W - 20, ty = 46, h = 108;
        x.fillStyle = stops(x.createLinearGradient(0, ty + h, 0, ty), .9);
        x.fillRect(lx, ty, 3, h);
        x.font = `9px ${MONO}`; x.fillStyle = 'rgba(148,163,184,.8)'; x.textAlign = 'right';
        const at = (f) => { let i = 0; while (i < hz.length - 1 && hz[i] < f) i++; return i / (hz.length - 1); };
        for (const [f, s] of [[50, '50'], [500, '500'], [5000, '5k'], [15000, '15k']]) x.fillText(s, lx - 4, ty + h - at(f) * h + 3);
        x.fillText('Hz', lx + 3, ty - 6);
    }

    function tick(now) {
        requestAnimationFrame(tick);
        if (now - drawn < FRAME_MS) return;
        drawn = now;
        const dt = Math.min(.1, (now - last) / 1000);
        last = now;
        t += dt;
        const k = 1 - Math.exp(-dt * 5);
        for (const c of shown) {
            const s = c.to;
            c.deg += (s.deg - c.deg) * k; c.pd += (playedDeg(s.deg, stage.width) - c.pd) * k; c.w += (s.w - c.w) * k; c.z += (s.z - c.z) * k;
            if (c.sp.length === s.sp.length) for (let i = 0; i < c.sp.length; i++) c.sp[i] += (s.sp[i] - c.sp[i]) * k;
            else c.sp = [...s.sp];
            c.a += (c.goal - c.a) * k;
            c.direct = s.direct; c.itd = s.itd;
        }
        shown = shown.filter(c => c.a > .02 || c.goal);
        if (seq != null) {
            // Between views the newest hop is guessed on at the hops' rate, a
            // little way only: a pause stops the hops and so the picture.
            const ahead = rate * Math.min(.25, (now - seqAt) / 1000);
            ph = chasePlayhead(ph, seq + ahead - LAG_HOPS, seq, rate, dt);
        }
        hits.length = 0;
        x.setTransform(DPR, 0, 0, DPR, 0, 0);
        x.filter = 'none';
        sky(); floor(); marks(); speakers();
        [...shown].sort((a, b) => b.z - a.z).forEach(figure);
        writeLabels();
        legend();
    }

    canvas.parentElement.addEventListener('mousemove', (e) => {
        if (!tip) return;
        const r = canvas.getBoundingClientRect();
        const mx = e.clientX - r.left, my = e.clientY - r.top;
        let hit = null, bd = 1e9;
        for (const h of hits) {
            const d = Math.hypot(mx - h.x, (my - h.y) * .6);
            if (d < Math.max(h.r, 24) && d < bd) { bd = d; hit = h; }
        }
        if (!hit) { tip.style.display = 'none'; return; }
        const c = hit.c;
        let top = 0, ti = 0;
        c.sp.forEach((v, i) => { if (v > top) { top = v; ti = i; } });
        const f = hz[ti] ?? 0;
        const og = originalDeg(c.deg, spk);
        const from = Math.abs(c.pd - og) > 1.5 ? `  (speakers alone ${degText(og)})` : '';
        const heard = itdDeg(c.pd, stage.itd);
        const time = Math.abs(heard - c.pd) > 1.5 ? `<br>time cue as of ${degText(heard)}` : '';
        // A source the mix panned by a delay between the channels, not by level alone.
        const lag = Math.abs(c.itd) >= .05 ? `<br>time pan ${Math.abs(c.itd).toFixed(2)} ms, ${c.itd > 0 ? 'right' : 'left'} later` : '';
        tip.innerHTML = `<b>${degText(c.pd)}</b>${from}  strength ${(c.w * 100).toFixed(0)} %<br>peak ${f >= 1000 ? `${(f / 1000).toFixed(1)} kHz` : `${Math.round(f)} Hz`}<br>direct ${(c.direct * 100).toFixed(0)} %${lag}${time}`;
        tip.style.display = 'block';
        tip.style.left = `${Math.min(W - 150, mx + 14)}px`;
        tip.style.top = `${Math.max(4, my - 30)}px`;
    });
    canvas.parentElement.addEventListener('mouseleave', () => { if (tip) tip.style.display = 'none'; });

    new ResizeObserver(fit).observe(canvas.parentElement);
    fit();
    requestAnimationFrame(tick);

    let viewAt = performance.now();
    return {
        /// One look at the stage, as `player_cue_view` gives it.
        update(v, isBypassed, speakerAngle, stageParams) {
            if (stageParams) stage = { width: stageParams.stageWidth ?? 90, itd: stageParams.itdScale ?? 100 };
            const now = performance.now();
            bypassed = isBypassed;
            running = !!v?.running;
            xtc = !!v?.xtc;
            spk = speakerAngle;
            const dots = running ? v.dots : [];
            if (running && v.bands.length) hz = v.bands.map(b => b.hz);
            const since = (now - viewAt) / 1000;
            topDb -= TOP_FALL_DB_S * since;
            viewAt = now;
            for (const d of dots) for (const db of d.specDb) if (db > topDb) topDb = db;
            shown = followSources(shown, dots, d => ({ deg: d.deg, pd: playedDeg(d.deg, stage.width), w: d.weight, z: depthOf(d.direct), sp: imageOf(d.specDb, topDb), direct: d.direct, itd: d.itdMs ?? 0 }));
            if (!running || !Number.isFinite(v.seq)) return;
            if (v.sliceHz > 0) rate = v.sliceHz;
            if (steppedBack(seq, v.seq)) { for (const c of shown) c.wave = newWave(); ph = null; }
            // Each wave takes the hops it did not have; the loudest of them
            // moves the columns' scale, slowly.
            let louder = 0;
            for (const d of dots) {
                const h = shown.find(e => e.id === d.id).wave;
                const had = h.last;
                mergeWave(h, d.wave, v.seq, span() + 2 * LAG_HOPS);
                for (let hop = Math.max(had + 1, h.last - h.lo.length + 1); hop <= h.last; hop++) {
                    const [lo, hi] = waveAt(h, hop);
                    louder = Math.max(louder, -lo, hi);
                }
            }
            peak = followRef(peak, since, louder);
            if (seq == null || v.seq !== seq) seqAt = now;
            seq = v.seq;
        },
    };
}
