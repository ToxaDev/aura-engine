
// ══════════════════════════════════════════════════════════════════════
// A visualization scene drawn by the video card (WebGL2; Anton 27.09).
//
// The scene (scene.js) runs once per pixel into a picture of its own — at
// a lower resolution when the card cannot keep up — and that picture is
// laid on the canvas: premultiplied, and in the player's band faded under
// the title and at the seek bar like the waves. A scene asking for its
// previous frame (@feedback, Shadertoy's iChannel1) gets two pictures used
// in turn. A scene of several passes (// @pass: Shadertoy's buffers A–D) draws
// each buffer into a picture of its own at the same size — two for one read
// before it is drawn in a frame — then its image pass into the scene's.
//
// What the scene reads lives in textures kept up to date by the music
// engine (music.js): the slices (a column each, by grid index modulo 512;
// row 127 holds the index, so a column of another index reads as "not
// there"), the waveform (32768 samples by index, with the lap it belongs
// to), the cover, Shadertoy's 512×2 audio picture, and a noise texture.
//
// The card's time per frame is measured where the browser allows it
// (EXT_disjoint_timer_query_webgl2), else the frames' spacing: over the
// budget, the resolution comes down (to a quarter at most), and a scene
// that is still far over it is stopped and reported.
// ══════════════════════════════════════════════════════════════════════

import { SLOTS, MROWS, ROW, WAVE_W, WAVE_H, OBJ_W, OBJ_ROWS, OBJ_N, NOTE_ROWS, buildScene, mapErrors } from './scene.js';

const VS = `#version 300 es
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;
const BLIT = `#version 300 es
precision highp float;
uniform sampler2D uSrc;
uniform vec2 uSize;
uniform float uFade;     // 0 none, 1 the player's band, 2 the whole player (its bars off)
uniform float uBandK;    // the whole player: the band's height over the canvas's
uniform float uLower;    // the whole player: the level under the controls (1 while they are away)
out vec4 o;
// The player's band: the title's corner and the seek bar's edge stay quiet
// (the same as the waves; top 0 … bottom 1).
float fade(float y) {
    if (y < 0.28) return mix(0.1, 0.2, y / 0.28);
    if (y < 0.5) return mix(0.2, 0.75, (y - 0.28) / 0.22);
    if (y < 0.62) return mix(0.75, 1.0, (y - 0.5) / 0.12);
    if (y < 0.86) return 1.0;
    return mix(1.0, 0.0, (y - 0.86) / 0.14);
}
// The whole player: the band's own profile under the text, then the picture
// goes on to the bottom edge, at uLower under the seek bar and the buttons.
float fadeWhole(float y) {
    float yb = y / max(uBandK, 0.05);
    if (yb < 0.86) return fade(yb);
    return mix(1.0, uLower, clamp((yb - 0.86) / 0.14, 0.0, 1.0));
}
void main() {
    vec2 uv = gl_FragCoord.xy / uSize;
    vec4 c = texture(uSrc, uv);
    float a = clamp(c.a, 0.0, 1.0) * (uFade > 1.5 ? fadeWhole(1.0 - uv.y) : uFade > 0.5 ? fade(1.0 - uv.y) : 1.0);
    o = vec4(clamp(c.rgb, 0.0, 1.0) * a, a);
}`;

// (a scene of several passes: its iChannel0…3 on units CH … CH+3)
const TEX = { MUSIC: 0, WAVE: 1, PREV: 2, COVER: 3, AUDIO: 4, NOISE: 5, SRC: 6, OBJECTS: 7, OBJNOW: 8, NOTES: 9, CH: 10, NOISE64: 14 };

/// Shadertoy's noise pictures: random red and blue, and green and alpha the
/// same moved by (37, 17) texels — its classic 3-D noise reads two layers in
/// one fetch that way (`texture(ch, (uv + 0.5) / 256.0).yx`).
function noisePicture(n) {
    const r = new Uint8Array(n * n), b = new Uint8Array(n * n), out = new Uint8Array(n * n * 4);
    for (let i = 0; i < n * n; i++) { r[i] = Math.floor(Math.random() * 256); b[i] = Math.floor(Math.random() * 256); }
    for (let y = 0; y < n; y++) for (let x = 0; x < n; x++) {
        const i = y * n + x, j = ((y - 17) & (n - 1)) * n + ((x - 37) & (n - 1));
        out[i * 4] = r[i]; out[i * 4 + 1] = r[j]; out[i * 4 + 2] = b[i]; out[i * 4 + 3] = b[j];
    }
    return out;
}
const NO_COLOURS = new Float32Array(OBJ_N * 3);

/// canvas: the element drawn on. opts.onLost(): the card's context went
/// away (the caller may fall back); opts.onRestored(): it came back.
export function createRenderer(canvas, opts = {}) {
    let gl;
    try {
        gl = canvas.getContext('webgl2', { alpha: true, premultipliedAlpha: true, antialias: false, depth: false, stencil: false, preserveDrawingBuffer: false });
    } catch (_) { gl = null; }
    if (!gl) return null;

    let dead = false;
    let R = null;                 // what lives on the card (rebuilt after a loss)
    let scene = null;             // { built, prog, u, ready, feedback, quality, values, id }
    let pending = null;           // a scene compiling (KHR_parallel_shader_compile)
    const timer = { ext: null, q: [], avg: 0, n: 0, over: 0, under: 0, last: 0, pass: {} };
    let scale = 1, lastAdjust = 0, frameNo = 0, startWall = performance.now();
    let heavy = null;             // why the scene was stopped

    function setup() {
        const parallel = gl.getExtension('KHR_parallel_shader_compile');
        const floatRT = !!gl.getExtension('EXT_color_buffer_float');
        timer.ext = gl.getExtension('EXT_disjoint_timer_query_webgl2');
        const tex = (unit, w, h, ifmt, fmt, type, data, filter, wrap) => {
            const t = gl.createTexture();
            gl.activeTexture(gl.TEXTURE0 + unit);
            gl.bindTexture(gl.TEXTURE_2D, t);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter === gl.LINEAR_MIPMAP_LINEAR ? gl.LINEAR : filter);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, wrap);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, wrap);
            gl.texImage2D(gl.TEXTURE_2D, 0, ifmt, w, h, 0, fmt, type, data);
            return t;
        };
        const noise = noisePicture(256);
        // Sampler objects: how a pass of several reads each of its channels (they override the picture's own).
        const samplers = {};
        for (const f of ['nearest', 'linear', 'mipmap']) for (const w of ['clamp', 'repeat']) {
            const s = gl.createSampler();
            gl.samplerParameteri(s, gl.TEXTURE_MIN_FILTER, f === 'nearest' ? gl.NEAREST : f === 'linear' ? gl.LINEAR : gl.LINEAR_MIPMAP_LINEAR);
            gl.samplerParameteri(s, gl.TEXTURE_MAG_FILTER, f === 'nearest' ? gl.NEAREST : gl.LINEAR);
            gl.samplerParameteri(s, gl.TEXTURE_WRAP_S, w === 'clamp' ? gl.CLAMP_TO_EDGE : gl.REPEAT);
            gl.samplerParameteri(s, gl.TEXTURE_WRAP_T, w === 'clamp' ? gl.CLAMP_TO_EDGE : gl.REPEAT);
            samplers[f + ':' + w] = s;
        }
        R = {
            parallel, floatRT,
            music: tex(TEX.MUSIC, SLOTS, MROWS, gl.R32F, gl.RED, gl.FLOAT, emptyMusic(), gl.NEAREST, gl.CLAMP_TO_EDGE),
            wave: tex(TEX.WAVE, WAVE_W, WAVE_H, gl.RGBA32F, gl.RGBA, gl.FLOAT, emptyWave(), gl.NEAREST, gl.CLAMP_TO_EDGE),
            cover: tex(TEX.COVER, 1, 1, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array(4), gl.LINEAR, gl.CLAMP_TO_EDGE),
            audio: tex(TEX.AUDIO, 512, 2, gl.R8, gl.RED, gl.UNSIGNED_BYTE, new Uint8Array(1024), gl.LINEAR, gl.CLAMP_TO_EDGE),
            noise: tex(TEX.NOISE, 256, 256, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, noise, gl.LINEAR, gl.REPEAT),
            noise64: tex(TEX.NOISE64, 64, 64, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, noisePicture(64), gl.LINEAR, gl.REPEAT),
            samplers, bufs: null,
            dummy: tex(TEX.PREV, 1, 1, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array(4), gl.NEAREST, gl.CLAMP_TO_EDGE),
            objects: tex(TEX.OBJECTS, OBJ_W, OBJ_ROWS, gl.RGBA32F, gl.RGBA, gl.FLOAT, emptyObjects(), gl.NEAREST, gl.CLAMP_TO_EDGE),
            objNow: tex(TEX.OBJNOW, OBJ_N * 4, 1, gl.RGBA32F, gl.RGBA, gl.FLOAT, new Float32Array(OBJ_N * 16), gl.NEAREST, gl.CLAMP_TO_EDGE),
            // (WebGL starts a texture given no data at zero: no notes)
            notes: tex(TEX.NOTES, OBJ_ROWS, NOTE_ROWS, gl.RG16F, gl.RG, gl.HALF_FLOAT, null, gl.NEAREST, gl.CLAMP_TO_EDGE),
            coverWH: [1, 1],
            targets: [], tw: 0, th: 0, tFeedback: false, cur: 0,
            blit: link(VS, BLIT),
            vao: gl.createVertexArray(),
        };
        gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
        // the noise pictures also in smaller sizes (a channel may read them `mipmap`)
        for (const [unit, t] of [[TEX.NOISE, R.noise], [TEX.NOISE64, R.noise64]]) {
            gl.activeTexture(gl.TEXTURE0 + unit);
            gl.bindTexture(gl.TEXTURE_2D, t);
            gl.generateMipmap(gl.TEXTURE_2D);
        }
    }
    function emptyMusic() {
        const a = new Float32Array(SLOTS * MROWS);
        a.fill(-1, ROW.K * SLOTS, (ROW.K + 1) * SLOTS);
        return a;
    }
    function emptyObjects() {
        const a = new Float32Array(OBJ_W * OBJ_ROWS * 4);
        for (let r = 0; r < OBJ_ROWS; r++) a[(r * OBJ_W + OBJ_W - 1) * 4] = -1;
        return a;
    }
    function emptyWave() {
        const a = new Float32Array(WAVE_W * WAVE_H * 4);
        for (let i = 0; i < WAVE_W * WAVE_H; i++) a[i * 4 + 2] = -1;
        return a;
    }
    function shader(type, src) {
        const s = gl.createShader(type);
        gl.shaderSource(s, src);
        gl.compileShader(s);
        return s;
    }
    function link(vs, fs) {
        const p = gl.createProgram();
        const a = shader(gl.VERTEX_SHADER, vs), b = shader(gl.FRAGMENT_SHADER, fs);
        gl.attachShader(p, a);
        gl.attachShader(p, b);
        gl.linkProgram(p);
        if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getShaderInfoLog(b) || gl.getProgramInfoLog(p) || 'link');
        return { p, u: uniforms(p) };
    }
    function uniforms(p) {
        const u = {};
        const n = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS);
        for (let i = 0; i < n; i++) {
            const name = gl.getActiveUniform(p, i).name.replace(/\[0\]$/, '');
            u[name] = gl.getUniformLocation(p, name);
        }
        return u;
    }

    // The scene's picture(s), at the drawing size.
    function targets(w, h, feedback) {
        if (R.tw === w && R.th === h && R.tFeedback === feedback && R.targets.length) return;
        for (const t of R.targets) { gl.deleteFramebuffer(t.fb); gl.deleteTexture(t.tex); }
        R.targets = [];
        const n = feedback ? 2 : 1;
        for (let i = 0; i < n; i++) {
            const t = gl.createTexture();
            gl.activeTexture(gl.TEXTURE0 + TEX.SRC);
            gl.bindTexture(gl.TEXTURE_2D, t);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
            if (R.floatRT) gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA16F, w, h, 0, gl.RGBA, gl.HALF_FLOAT, null);
            else gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
            const fb = gl.createFramebuffer();
            gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
            gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, t, 0);
            gl.clearColor(0, 0, 0, 0);
            gl.clear(gl.COLOR_BUFFER_BIT);
            R.targets.push({ tex: t, fb });
        }
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        Object.assign(R, { tw: w, th: h, tFeedback: feedback, cur: 0 });
    }

    // A scene of several passes: its buffers at the drawing size. Two pictures only for a buffer that is
    // read before it is drawn in a frame (by itself or an earlier pass); smaller sizes for one read
    // `mipmap`. When the resolution changes (a slow card, a new window size) what they hold is carried over,
    // stretched to the new size — a simulation goes on instead of starting over.
    function buffers(w, h, S) {
        const key = `${S.since}:${w}x${h}`;
        if (R.bufs?.key === key) return R.bufs.map;
        const old = R.bufs?.since === S.since ? R.bufs : null;
        if (!old) dropBuffers();
        const map = {};
        S.passes.forEach((P, at) => {
            if (P.name === 'image') return;
            const readers = S.passes.map((Q, qi) => ({ qi, chs: Q.channels.filter(c => c.src === P.name) })).filter(r => r.chs.length);
            const pp = readers.some(r => r.qi <= at);
            const mip = readers.some(r => r.chs.some(c => c.filter === 'mipmap'));
            const b = { tex: [], fb: [], cur: 0, pp, mip };
            for (let i = 0; i < (pp ? 2 : 1); i++) {
                const t = gl.createTexture();
                gl.activeTexture(gl.TEXTURE0 + TEX.SRC);
                gl.bindTexture(gl.TEXTURE_2D, t);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
                if (R.floatRT) gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA16F, w, h, 0, gl.RGBA, gl.HALF_FLOAT, null);
                else gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
                if (mip) gl.generateMipmap(gl.TEXTURE_2D);
                const fb = gl.createFramebuffer();
                gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
                gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, t, 0);
                gl.clearColor(0, 0, 0, 0);
                gl.clear(gl.COLOR_BUFFER_BIT);
                b.tex.push(t);
                b.fb.push(fb);
            }
            map[P.name] = b;
            const was = old?.map[P.name];
            if (was) {
                // the old pictures, stretched onto the new ones (the one read next stays the one read next)
                b.cur = Math.min(was.cur, b.tex.length - 1);
                for (let i = 0; i < b.tex.length; i++) {
                    const from = was.fb[Math.min(i, was.fb.length - 1)];
                    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, from);
                    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, b.fb[i]);
                    gl.blitFramebuffer(0, 0, old.w, old.h, 0, 0, w, h, gl.COLOR_BUFFER_BIT, gl.LINEAR);
                    if (mip) { gl.bindTexture(gl.TEXTURE_2D, b.tex[i]); gl.generateMipmap(gl.TEXTURE_2D); }
                }
                gl.bindFramebuffer(gl.READ_FRAMEBUFFER, null);
                gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, null);
            }
        });
        if (old) dropBuffers();
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        R.bufs = { key, map, since: S.since, w, h };
        return map;
    }
    function dropBuffers() {
        if (!R?.bufs) return;
        for (const b of Object.values(R.bufs.map)) {
            b.fb.forEach(f => gl.deleteFramebuffer(f));
            b.tex.forEach(t => gl.deleteTexture(t));
        }
        R.bufs = null;
    }

    setup();
    canvas.addEventListener('webglcontextlost', ev => {
        ev.preventDefault();
        dead = true;
        pending = null;
        const since = performance.now() - (scene?.since || 0);
        opts.onLost?.({ scene: scene?.id, soon: since < 5000 });
    });
    canvas.addEventListener('webglcontextrestored', () => {
        dead = false;
        try {
            setup();
            const s = scene;
            scene = null;
            if (s) api.setScene(s.text, s.id, s.values);
            opts.onRestored?.();
        } catch (e) { console.warn('[vis] restore:', e); }
    });

    // ── the data (music.js sinks here) ─────────────────────────────────
    const one = new Float32Array([-1]);
    const sink = {
        clearSlices() {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.MUSIC);
            gl.bindTexture(gl.TEXTURE_2D, R.music);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.R32F, SLOTS, MROWS, 0, gl.RED, gl.FLOAT, emptyMusic());
        },
        setSlice(k, col) {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.MUSIC);
            gl.bindTexture(gl.TEXTURE_2D, R.music);
            gl.texSubImage2D(gl.TEXTURE_2D, 0, k & (SLOTS - 1), 0, 1, MROWS, gl.RED, gl.FLOAT, col);
        },
        dropSlice(k) {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.MUSIC);
            gl.bindTexture(gl.TEXTURE_2D, R.music);
            gl.texSubImage2D(gl.TEXTURE_2D, 0, k & (SLOTS - 1), ROW.K, 1, 1, gl.RED, gl.FLOAT, one);
        },
        setValue(k, row, v) {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.MUSIC);
            gl.bindTexture(gl.TEXTURE_2D, R.music);
            gl.texSubImage2D(gl.TEXTURE_2D, 0, k & (SLOTS - 1), row, 1, 1, gl.RED, gl.FLOAT, new Float32Array([v]));
        },
        clearWave() {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.WAVE);
            gl.bindTexture(gl.TEXTURE_2D, R.wave);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA32F, WAVE_W, WAVE_H, 0, gl.RGBA, gl.FLOAT, emptyWave());
        },
        /// Samples i0 … i0+n−1 (lr: left, right interleaved), a texel each.
        setWave(i0, n, lr) {
            if (dead || !n) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.WAVE);
            gl.bindTexture(gl.TEXTURE_2D, R.wave);
            let j = 0;
            while (j < n) {
                const i = i0 + j, x = i & (WAVE_W - 1), y = (i >> 8) & (WAVE_H - 1);
                const run = Math.min(n - j, WAVE_W - x);
                const a = new Float32Array(run * 4);
                for (let m = 0; m < run; m++) {
                    const ii = i + m;
                    a[m * 4] = lr[(j + m) * 2];
                    a[m * 4 + 1] = lr[(j + m) * 2 + 1];
                    a[m * 4 + 2] = Math.floor(ii / 32768);
                    a[m * 4 + 3] = 1;
                }
                gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, run, 1, gl.RGBA, gl.FLOAT, a);
                j += run;
            }
        },
        dropWave(i0, i1) {
            if (dead || i1 < i0) return;
            const n = i1 - i0 + 1;
            const lr = new Float32Array(n * 2);
            this.setWave(i0, n, lr);
            // …and mark them as another lap's.
            gl.activeTexture(gl.TEXTURE0 + TEX.WAVE);
            let j = 0;
            while (j < n) {
                const i = i0 + j, x = i & (WAVE_W - 1), y = (i >> 8) & (WAVE_H - 1);
                const run = Math.min(n - j, WAVE_W - x);
                const a = new Float32Array(run * 4);
                for (let m = 0; m < run; m++) a[m * 4 + 2] = -1;
                gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, run, 1, gl.RGBA, gl.FLOAT, a);
                j += run;
            }
        },
        clearNotes() {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.NOTES);
            gl.bindTexture(gl.TEXTURE_2D, R.notes);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RG16F, OBJ_ROWS, NOTE_ROWS, 0, gl.RG, gl.HALF_FLOAT, null);
        },
        /// The note columns of frames k0 … k0+n−1 (cols[j]: NOTE_ROWS × (velocity, onset)).
        setNotes(k0, cols) {
            if (dead || !cols.length) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.NOTES);
            gl.bindTexture(gl.TEXTURE_2D, R.notes);
            for (let j = 0; j < cols.length; j++) {
                gl.texSubImage2D(gl.TEXTURE_2D, 0, (k0 + j) & (OBJ_ROWS - 1), 0, 1, NOTE_ROWS, gl.RG, gl.FLOAT, cols[j]);
            }
        },
        clearObjects() {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.OBJECTS);
            gl.bindTexture(gl.TEXTURE_2D, R.objects);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA32F, OBJ_W, OBJ_ROWS, 0, gl.RGBA, gl.FLOAT, emptyObjects());
        },
        /// Frames k0 … k0+n−1 (rows: OBJ_W texels each), by index modulo OBJ_ROWS.
        setObjects(k0, n, rows) {
            if (dead || !n) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.OBJECTS);
            gl.bindTexture(gl.TEXTURE_2D, R.objects);
            let j = 0;
            // the newest OBJ_ROWS frames at most, in runs that do not wrap
            if (n > OBJ_ROWS) j = n - OBJ_ROWS;
            while (j < n) {
                const y = (k0 + j) & (OBJ_ROWS - 1);
                const run = Math.min(n - j, OBJ_ROWS - y);
                gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, y, OBJ_W, run, gl.RGBA, gl.FLOAT, rows.subarray(j * OBJ_W * 4, (j + run) * OBJ_W * 4));
                j += run;
            }
        },
        setCover(img) {
            if (dead) return;
            gl.activeTexture(gl.TEXTURE0 + TEX.COVER);
            gl.bindTexture(gl.TEXTURE_2D, R.cover);
            try {
                if (img) {
                    // bottom row first, as Shadertoy's pictures (their "vflip"): v = 0 is the cover's bottom edge,
                    // so a pasted shader that reads it through a channel sees it upright (auraCover() too)
                    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
                    try { gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, img); }
                    finally { gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false); }
                    gl.generateMipmap(gl.TEXTURE_2D);
                    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
                    R.coverWH = [img.naturalWidth || 1, img.naturalHeight || 1];
                } else {
                    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array(4));
                    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
                    R.coverWH = [1, 1];
                }
            } catch (e) { console.warn('[vis] cover:', e); }
        },
    };

    // ── scenes ─────────────────────────────────────────────────────────
    /// Compile a scene. Resolves { ok, errors, header }; while it compiles
    /// (and when it fails) the scene before it goes on being drawn.
    function compile(text, id, values) {
        const built = buildScene(text);
        if (built.errors.length) return Promise.resolve({ ok: false, errors: built.errors, header: built.header });
        if (dead) return Promise.resolve({ ok: false, errors: [{ line: 0, msg: 'the video card is not available' }], header: built.header });
        // one program, or one a pass (a scene of several passes), all compiled side by side
        const jobs = (built.passes || [{ name: 'image', fs: built.fs, channels: null }]).map(P => {
            const p = gl.createProgram();
            const vs = shader(gl.VERTEX_SHADER, VS), fs = shader(gl.FRAGMENT_SHADER, P.fs);
            gl.attachShader(p, vs);
            gl.attachShader(p, fs);
            gl.linkProgram(p);
            return { P, p, vs, fs };
        });
        const job = { jobs, built, text, id, values, t0: performance.now() };
        pending = job;
        return new Promise(resolve => {
            const drop = () => { for (const j of jobs) { gl.deleteProgram(j.p); gl.deleteShader(j.vs); gl.deleteShader(j.fs); } };
            const check = () => {
                if (pending !== job || dead) {
                    if (!dead) drop();
                    return resolve({ ok: false, superseded: true, errors: [], header: built.header });
                }
                if (R.parallel && jobs.some(j => !gl.getProgramParameter(j.p, R.parallel.COMPLETION_STATUS_KHR))) { setTimeout(check, 16); return; }
                pending = null;
                const failed = jobs.filter(j => !gl.getProgramParameter(j.p, gl.LINK_STATUS));
                if (failed.length) {
                    const logs = failed.map(j => gl.getShaderInfoLog(j.fs) || gl.getProgramInfoLog(j.p) || 'the scene did not compile');
                    const errors = [];
                    const seen = new Set();
                    failed.forEach((j, k) => {
                        for (const e of mapErrors(logs[k], built.offset, built.userLines)) {
                            // the same line fails in every pass that has it (the common code): say it once
                            const key = e.line + '|' + e.msg;
                            if (seen.has(key)) continue;
                            seen.add(key);
                            errors.push(built.passes && e.line === 0 ? { line: 0, msg: `(pass ${j.P.name}) ${e.msg}` } : e);
                        }
                    });
                    drop();
                    return resolve({ ok: false, errors, header: built.header, log: logs.join('\n') });
                }
                for (const j of jobs) { gl.deleteShader(j.vs); gl.deleteShader(j.fs); }
                const old = scene;
                const image = jobs.find(j => j.P.name === 'image');
                const passes = built.passes ? jobs.map(j => ({ name: j.P.name, prog: j.p, u: uniforms(j.p), channels: j.P.channels })) : null;
                scene = { id, text, built, prog: image.p, u: passes ? passes.find(q => q.name === 'image').u : uniforms(image.p),
                    passes, feedback: built.feedback, quality: built.header.quality,
                    values: values || {}, since: performance.now(), compileMs: performance.now() - job.t0 };
                if (old) dropScene(old);
                heavy = null;
                scale = scene.quality;
                timer.avg = 0; timer.n = 0; timer.over = 0; timer.under = 0; timer.pass = {};
                // the old scene's frames still being timed would count as the new one's (its passes too)
                for (const t of timer.q) t.qs.forEach(q => gl.deleteQuery(q));
                timer.q = [];
                frameNo = 0;
                startWall = performance.now();
                resolve({ ok: true, errors: [], header: built.header, compileMs: scene.compileMs });
            };
            check();
        });
    }

    function dropScene(s) {
        if (s.passes) { for (const P of s.passes) gl.deleteProgram(P.prog); dropBuffers(); }
        else if (s.prog) gl.deleteProgram(s.prog);
    }

    function setParams(u, header, values) {
        for (const p of header.params) {
            const loc = u[p.name];
            if (loc == null) continue;
            const v = values?.[p.name] ?? p.def;
            if (p.type === 'color') gl.uniform3f(loc, v[0], v[1], v[2]);
            else if (p.type === 'int' || p.type === 'bool' || p.type === 'choice') gl.uniform1i(loc, Math.round(v));
            else gl.uniform1f(loc, v);
        }
    }

    /// One frame. f: the music's numbers (music.js frame()), and
    /// { clock, dt, fade (1: the band's quiet corners; 2: the whole player,
    /// with bandK and lower), budgetMs }.
    function frame(m, f) {
        if (dead || !scene || heavy) return false;
        const dpr = window.devicePixelRatio || 1;
        const W = Math.max(1, Math.round(canvas.clientWidth * dpr)), H = Math.max(1, Math.round(canvas.clientHeight * dpr));
        if (canvas.width !== W || canvas.height !== H) { canvas.width = W; canvas.height = H; }
        measure(f.budgetMs || 6);
        const w = Math.max(8, Math.round(W * scale)), h = Math.max(8, Math.round(H * scale));
        targets(w, h, scene.feedback);
        const S = scene;
        const cur = R.targets[R.cur], prev = S.feedback ? R.targets[1 - R.cur] : null;

        gl.bindVertexArray(R.vao);
        gl.disable(gl.BLEND);
        // Textures on their units.
        const bind = (unit, t) => { gl.activeTexture(gl.TEXTURE0 + unit); gl.bindTexture(gl.TEXTURE_2D, t); };
        bind(TEX.MUSIC, R.music); bind(TEX.WAVE, R.wave); bind(TEX.COVER, R.cover);
        bind(TEX.NOISE, R.noise); bind(TEX.NOISE64, R.noise64); bind(TEX.PREV, prev ? prev.tex : R.dummy);
        bind(TEX.OBJECTS, R.objects);
        bind(TEX.NOTES, R.notes);
        bind(TEX.OBJNOW, R.objNow);
        if (m.objNow) gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, OBJ_N * 4, 1, gl.RGBA, gl.FLOAT, m.objNow);
        bind(TEX.AUDIO, R.audio);
        gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, 512, 2, gl.RED, gl.UNSIGNED_BYTE, m.audio);

        // the card's time: a query around each draw (only one may run at a time); the frame is their sum
        const timed = { qs: [], names: [] };
        const tick = name => {
            if (!timer.ext) return () => {};
            const q = gl.createQuery();
            gl.beginQuery(timer.ext.TIME_ELAPSED_EXT, q);
            return () => { gl.endQuery(timer.ext.TIME_ELAPSED_EXT); timed.qs.push(q); timed.names.push(name); };
        };
        const iFrame = frameNo++;
        if (!S.passes) {
            gl.bindFramebuffer(gl.FRAMEBUFFER, cur.fb);
            gl.viewport(0, 0, w, h);
            gl.useProgram(S.prog);
            setUniforms(S.u, m, f, w, h, iFrame, prev);
            if (S.u.iChannelResolution != null) gl.uniform3fv(S.u.iChannelResolution, [512, 2, 1, w, h, 1, R.coverWH[0], R.coverWH[1], 1, 256, 256, 1]);
            const done = tick('image');
            gl.drawArrays(gl.TRIANGLES, 0, 3);
            done();
        } else {
            // A, B, C, D, then the picture: each pass reads its channels, writes its buffer.
            const bufs = buffers(w, h, S);
            for (const P of S.passes) {
                const b = bufs[P.name];
                gl.bindFramebuffer(gl.FRAMEBUFFER, b ? b.fb[b.pp ? 1 - b.cur : 0] : cur.fb);
                gl.viewport(0, 0, w, h);
                gl.useProgram(P.prog);
                const res = [];
                P.channels.forEach((c, i) => {
                    let t = R.dummy, size = [1, 1], filter = c.filter;
                    const from = bufs[c.src];
                    if (from) { t = from.tex[from.cur]; size = [w, h]; if (filter === 'mipmap' && !from.mip) filter = 'linear'; }
                    else if (c.src === 'noise') { t = R.noise; size = [256, 256]; }
                    else if (c.src === 'noise64') { t = R.noise64; size = [64, 64]; }
                    else if (c.src === 'cover') { t = R.cover; size = R.coverWH; }
                    else if (c.src === 'music') { t = R.audio; size = [512, 2]; if (filter === 'mipmap') filter = 'linear'; }
                    bind(TEX.CH + i, t);
                    gl.bindSampler(TEX.CH + i, R.samplers[filter + ':' + c.wrap]);
                    if (P.u['iChannel' + i] != null) gl.uniform1i(P.u['iChannel' + i], TEX.CH + i);
                    res.push(size[0], size[1], 1);
                });
                setUniforms(P.u, m, f, w, h, iFrame, null);
                if (P.u.iChannelResolution != null) gl.uniform3fv(P.u.iChannelResolution, res);
                const done = tick(P.name);
                gl.drawArrays(gl.TRIANGLES, 0, 3);
                if (b) {
                    if (b.pp) b.cur = 1 - b.cur;
                    if (b.mip) { bind(TEX.SRC, b.tex[b.cur]); gl.generateMipmap(gl.TEXTURE_2D); }
                }
                done();
            }
            for (let i = 0; i < 4; i++) { gl.bindSampler(TEX.CH + i, null); bind(TEX.CH + i, R.dummy); }
        }
        if (timed.qs.length) timer.q.push(timed);

        // Onto the canvas.
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        gl.viewport(0, 0, W, H);
        gl.clearColor(0, 0, 0, 0);
        gl.clear(gl.COLOR_BUFFER_BIT);
        gl.useProgram(R.blit.p);
        bind(TEX.SRC, cur.tex);
        gl.uniform1i(R.blit.u.uSrc, TEX.SRC);
        gl.uniform2f(R.blit.u.uSize, W, H);
        gl.uniform1f(R.blit.u.uFade, f.fade === 2 ? 2 : f.fade ? 1 : 0);
        if (R.blit.u.uBandK != null) gl.uniform1f(R.blit.u.uBandK, f.bandK || 1);
        if (R.blit.u.uLower != null) gl.uniform1f(R.blit.u.uLower, f.lower ?? 1);
        gl.drawArrays(gl.TRIANGLES, 0, 3);
        if (S.feedback) R.cur = 1 - R.cur;
        return true;
    }

    /// The uniforms every program of a scene reads (a pass of several: each its own).
    function setUniforms(u, m, f, w, h, iFrame, prev) {
        const i1 = (n, v) => { if (u[n] != null) gl.uniform1i(u[n], v); };
        const f1 = (n, v) => { if (u[n] != null) gl.uniform1f(u[n], v); };
        i1('uMusic', TEX.MUSIC); i1('uWaveTex', TEX.WAVE); i1('uPrevTex', TEX.PREV); i1('uCoverTex', TEX.COVER);
        i1('uAudio', TEX.AUDIO); i1('uNoise', TEX.NOISE);
        i1('uKBase', m.kBase); f1('uQ', m.q); f1('uQps', m.qps);
        i1('uWBase', m.wBase); f1('uWQ', m.wq); f1('uWRate', m.wRate);
        if (u.uResolution != null) gl.uniform2f(u.uResolution, w, h);
        f1('uTime', f.clock); f1('uWallTime', (performance.now() - startWall) / 1000); f1('uDelta', f.dt);
        i1('uFrame', iFrame);
        f1('uFull', f.full ? 1 : 0);
        f1('uLive', m.live); f1('uPlaying', m.playing); f1('uTrackTime', m.trackTime); f1('uTrackLength', m.trackLength);
        f1('uProgress', m.progress); f1('uTrackAge', m.trackAge); f1('uSampleRate', m.sampleRate || 44100); f1('uAhead', m.ahead);
        f1('uBass', m.bass); f1('uMid', m.mid); f1('uTreble', m.treble); f1('uLoudness', m.loud); f1('uEnergy', m.energy);
        f1('uOnset', m.onset); f1('uBrightness', m.bright); f1('uWidth', m.width);
        f1('uBpm', m.bpm); f1('uBeatConfidence', m.beatConf); f1('uBeatRef', m.beatRef);
        if (u.uHit != null) gl.uniform3f(u.uHit, m.hit[0], m.hit[1], m.hit[2]);
        if (u.uPalette != null) gl.uniform3fv(u.uPalette, m.palette.flat());
        f1('uHasCover', m.hasCover); f1('uHasPrev', prev ? 1 : 0);
        i1('uObjects', TEX.OBJECTS); i1('uObjNow', TEX.OBJNOW); i1('uOBase', m.oBase || 0); f1('uOQ', m.oq || 0); f1('uOFps', m.ofps || 0);
        f1('uSpatial', m.spatial || 0); f1('uSpatialProgress', m.spatialProgress ?? -1);
        i1('uNoteRoll', TEX.NOTES); i1('uObjCount', m.objCount || 0);
        if (u.uObjColour != null) gl.uniform3fv(u.uObjColour, m.objColour || NO_COLOURS);
        if (u.iMouse != null) gl.uniform4f(u.iMouse, 0, 0, 0, 0);
        if (u.iDate != null) {
            const d = new Date();
            gl.uniform4f(u.iDate, d.getFullYear(), d.getMonth(), d.getDate(), d.getHours() * 3600 + d.getMinutes() * 60 + d.getSeconds() + d.getMilliseconds() / 1000);
        }
        setParams(u, scene.built.header, scene.values);
    }

    /// The card's time a frame → the resolution (see the top).
    function measure(budget) {
        const now = performance.now();
        let ms = null;
        if (timer.ext) {
            const drop = () => { for (const t of timer.q) t.qs.forEach(q => gl.deleteQuery(q)); timer.q = []; };
            if (gl.getParameter(timer.ext.GPU_DISJOINT_EXT)) drop();
            while (timer.q.length && timer.q[0].qs.every(q => gl.getQueryParameter(q, gl.QUERY_RESULT_AVAILABLE))) {
                const t = timer.q.shift();
                ms = 0;
                t.qs.forEach((q, k) => {
                    const v = gl.getQueryParameter(q, gl.QUERY_RESULT) / 1e6;
                    const was = timer.pass[t.names[k]];
                    timer.pass[t.names[k]] = was == null ? v : was + (v - was) * 0.1;
                    ms += v;
                    gl.deleteQuery(q);
                });
                take(ms, budget, now);
            }
            if (timer.q.length > 8) drop();
        } else if (timer.last) {
            // No timer: frames coming later than 60 a second, with some slack.
            const gap = now - timer.last;
            if (gap < 250) take(Math.max(0, gap - 17) + budget * 0.5, budget, now);
        }
        timer.last = now;
    }
    function take(ms, budget, now) {
        timer.avg = timer.n ? timer.avg + (ms - timer.avg) * 0.1 : ms;
        timer.n++;
        if (timer.n < 10 || now - lastAdjust < 1200) return;
        const max = scene?.quality || 1;
        if (timer.avg > budget * 1.3 && scale > 0.25) {
            scale = Math.max(0.25, scale * Math.sqrt(budget / timer.avg) * 0.95);
            lastAdjust = now; timer.n = 5;
        } else if (timer.avg > Math.max(40, budget * 6) && scale <= 0.25) {
            if (++timer.over > 30) {
                heavy = `too heavy for this video card: ${timer.avg.toFixed(0)} ms a frame at a quarter of the resolution`;
                opts.onHeavy?.({ scene: scene?.id, ms: timer.avg, why: heavy });
            }
        } else if (timer.avg < budget * 0.45 && scale < max) {
            if (++timer.under > 60) { scale = Math.min(max, scale * 1.2); lastAdjust = now; timer.under = 0; timer.n = 5; }
        } else {
            timer.over = 0;
        }
    }

    const api = {
        sink,
        get dead() { return dead; },
        get sceneId() { return scene?.id ?? null; },
        /// The scene shown draws the sound objects (// @spatial).
        get spatial() { return !!scene?.built.header.spatial; },
        get heavy() { return heavy; },
        setScene: compile,
        setValues(values) { if (scene) scene.values = values || {}; },
        clearScene() { if (scene) dropScene(scene); scene = null; },
        frame,
        info: () => ({ scale, gpuMs: timer.avg, passMs: { ...timer.pass }, timed: !!timer.ext, floatRT: R?.floatRT, w: R?.tw, h: R?.th, compileMs: scene?.compileMs,
            feedback: !!scene?.feedback, heavy }),
    };
    return api;
}
