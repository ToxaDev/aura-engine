
// ══════════════════════════════════════════════════════════════════════
// A visualization scene: its text → its settings and a GLSL program
// (Anton 27.09; the contract is src/vis/AURA-VIS-SPEC.md).
//
// A scene is GLSL written against our contract: header comments name it
// and declare its settings (`// @param …` → a slider and a uniform), and a
// `vec4 scene(vec2 uv, vec2 p)` (or Shadertoy's mainImage) gives a pixel's
// colour. Here the text is read and wrapped: before it, the version, the
// uniforms and the music functions (the prelude); after it, main(). The
// compiler's messages are brought back to the scene's own line numbers.
// ══════════════════════════════════════════════════════════════════════

// The music texture's rows (renderer.js / music.js write them).
export const ROW = {
    SPEC: 0, SPEC_DB: 48, BASS: 96, MID: 97, TREBLE: 98, HIT: 99, LOUD: 102, LOUD_DB: 103, ENERGY: 104,
    ONSET: 105, BRIGHT: 106, PEAK_DB: 107, WIDTH: 108, BALANCE: 109, CORR: 110, K: 127,
};
export const SLOTS = 512;          // slices kept on the card (10.24 s at 20 ms)
export const MROWS = 128;
export const WAVE_W = 256, WAVE_H = 128;   // waveform ring: 32768 samples (≈1.4 s at 24 kHz)
// Sound objects (spatial scenes, spatial/mod.rs): 32 slots × 4 RGBA texels a
// frame — (presence, energy, kind÷32, main) (x, y, z, width) (onset,
// coherence, age÷60, 0) (origin x y z, 0) — the frame's index in the last
// texel; frames by index modulo OBJ_ROWS (86 a second). A slot keeps its
// object while it lives.
export const OBJ_N = 32, OBJ_VALS = 16, OBJ_W = OBJ_N * 4 + 1, OBJ_ROWS = 1024;
// The notes of the track map (the instruments tier): a column per frame of the
// same ring as the objects (frame & 1023), a row per instrument and key —
// row = instrument · 88 + key − 21 — RG16F: the note's velocity while it
// sounds, its onset flash (1 at its start, dying in 0.12 s).
export const KEY_LOW = 21, KEYS = 88, NOTE_ROWS = OBJ_N * KEYS;
export const OBJ_KINDS = ['', 'VOICE', 'BASS', 'KICK', 'SNARE', 'HATS', 'DRUMS', 'GUITAR', 'PIANO', 'OTHER', 'AMBIENCE', 'MIX', 'TOMS', 'CYMBAL'];
// The books a scene can be written for (`// @spec`): 1 — the whole mix
// (src/vis/AURA-VIS-SPEC.md, no download); 2 — the song's instruments as well
// (+ src/vis/AURA-VIS-SPEC-INSTRUMENTS.md, the instruments pack). The newest
// this app knows; a minor version only adds, so an older scene keeps working.
export const SPEC_MAJOR = 2;

const RESERVED = new Set(`scene mainImage main PI TAU fragColor fragCoord`.split(' '));
// GLSL's own functions: a setting cannot take their names.
const BUILTINS = new Set(`radians degrees sin cos tan asin acos atan sinh cosh tanh asinh acosh atanh pow exp log exp2 log2
sqrt inversesqrt abs sign floor trunc round roundEven ceil fract mod modf min max clamp mix step smoothstep isnan isinf length
distance dot cross normalize faceforward reflect refract matrixCompMult outerProduct transpose determinant inverse lessThan
lessThanEqual greaterThan greaterThanEqual equal notEqual any all not texture textureSize texelFetch textureLod textureGrad
textureProj dFdx dFdy fwidth floatBitsToInt intBitsToFloat packHalf2x16 unpackHalf2x16`.split(/\s+/));
const KEYWORDS = new Set(`attribute const uniform varying layout centroid flat smooth break continue do for while switch case
default if else in out inout float int void bool true false invariant discard return mat2 mat3 mat4 mat2x2 mat2x3 mat2x4
mat3x2 mat3x3 mat3x4 mat4x2 mat4x3 mat4x4 vec2 vec3 vec4 ivec2 ivec3 ivec4 bvec2 bvec3 bvec4 uint uvec2 uvec3 uvec4 lowp
mediump highp precision sampler2D sampler3D samplerCube sampler2DShadow samplerCubeShadow sampler2DArray
sampler2DArrayShadow isampler2D isampler3D isamplerCube isampler2DArray usampler2D usampler3D usamplerCube
usampler2DArray struct`.split(/\s+/));

// ── the header ─────────────────────────────────────────────────────────

/// Quoted strings and bare words of a header line, in order.
function tokens(s) {
    const out = [];
    const re = /"([^"]*)"|(\S+)/g;
    let m;
    while ((m = re.exec(s))) out.push(m[1] != null ? { q: m[1] } : { w: m[2] });
    return out;
}

function hexColour(s) {
    const m = /^#?([0-9a-f]{6}|[0-9a-f]{3})$/i.exec(s || '');
    if (!m) return null;
    let h = m[1];
    if (h.length === 3) h = h.split('').map(c => c + c).join('');
    return [0, 2, 4].map(i => parseInt(h.slice(i, i + 2), 16) / 255);
}
export const colourHex = c => '#' + c.map(v => Math.round(Math.max(0, Math.min(1, v)) * 255).toString(16).padStart(2, '0')).join('');

/// The header of a scene: { name, author, about, license, feedback, spec, specMinor, specNewer, spatial, quality,
/// params, sections, errors }.
/// A param: { type, name, def, min, max, step, unit, options, label, why, section, line }.
/// A section: { name, why, params: [names] } — the settings panel's groups, in order
/// (params before any @section fall in a first, unnamed one).
/// `spec` (// @spec 1 | 2, 1 when absent): the book the scene is written for. 2 — it
/// draws the song's instruments too, and the player finds them for it (the
/// instruments pack); the older `// @spatial` means the same. `spatial` = spec ≥ 2.
/// `specNewer`: written for a book newer than this app knows (it may still run).
export function parseHeader(text) {
    const h = { name: '', author: '', about: '', license: '', feedback: false, spec: 1, specMinor: 0, specNewer: false, spatial: false,
        quality: 1, params: [], sections: [], errors: [] };
    let spatialTag = false;
    let section = null;
    const lines = String(text).split(/\r?\n/);
    lines.forEach((raw, i) => {
        const m = /^\s*\/\/\s*@(\w+)\s*(.*)$/.exec(raw);
        if (!m) return;
        const tag = m[1].toLowerCase(), rest = m[2].trim();
        const line = i + 1;
        const err = msg => h.errors.push({ line, msg });
        if (tag === 'name') h.name = rest.replace(/^"|"$/g, '');
        else if (tag === 'author') h.author = rest.replace(/^"|"$/g, '');
        else if (tag === 'about') h.about = rest.replace(/^"|"$/g, '');
        else if (tag === 'license' || tag === 'licence') h.license = rest.replace(/^"|"$/g, '');
        else if (tag === 'feedback') h.feedback = !/^(off|0|false|no)\b/i.test(rest);
        else if (tag === 'spatial') spatialTag = !/^(off|0|false|no)\b/i.test(rest);
        else if (tag === 'spec') {
            const v = /^(\d+)(?:\.(\d+))?\b/.exec(rest);
            if (!v || Number(v[1]) < 1) err('@spec wants the book\'s number: 1 (the mix) or 2 (the instruments)');
            else { h.spec = Number(v[1]); h.specMinor = Number(v[2] || 0); }
        } else if (tag === 'quality') {
            const q = Number(rest.split(/\s+/)[0]);
            if (Number.isFinite(q)) h.quality = Math.max(0.25, Math.min(1, q)); else err('@quality wants a number 0.25…1');
        } else if (tag === 'section') {
            const t = tokens(rest);
            const quotes = t.filter(x => x.q != null).map(x => x.q);
            const name = quotes[0] ?? t.filter(x => x.w).map(x => x.w).join(' ');
            if (!name) return err('@section wants a name: // @section "Rings" "What they do"');
            section = { name, why: quotes[1] || '', params: [] };
            h.sections.push(section);
        } else if (tag === 'param') {
            const t = tokens(rest);
            const words = [], quotes = [];
            for (const x of t) (x.q != null ? quotes : words).push(x.q ?? x.w);
            const [type, name, ...nums] = words;
            if (!['float', 'int', 'bool', 'color', 'colour', 'choice'].includes(type)) return err(`@param: the type is float, int, bool, color or choice, not "${type || ''}"`);
            if (!name || !/^[A-Za-z_]\w*$/.test(name)) return err('@param: a name (a plain identifier) comes after the type');
            if (KEYWORDS.has(name) || RESERVED.has(name) || /^(u[A-Z_]|i[A-Z]|music|aura|gl_)/.test(name))
                return err(`@param: "${name}" is taken (the app's names start with u, i, music, aura)`);
            if (BUILTINS.has(name)) return err(`@param: "${name}" is a GLSL function's name — call the setting something else (e.g. "${name}Amount")`);
            if (h.params.some(p => p.name === name)) return err(`@param: "${name}" twice`);
            const p = { type: type === 'colour' ? 'color' : type, name, line };
            // choice: the options are the first quoted string ("A|B|C"), then the label and the text.
            if (p.type === 'choice') {
                const opts = (quotes.shift() || '').split('|').map(s => s.trim()).filter(Boolean);
                if (opts.length < 2) return err(`@param choice ${name}: the options come first, quoted: "One|Two|Three"`);
                const d = Math.round(Number(nums[0] ?? 0));
                Object.assign(p, { options: opts, min: 0, max: opts.length - 1, step: 1, def: Number.isFinite(d) ? Math.min(opts.length - 1, Math.max(0, d)) : 0 });
            }
            p.label = quotes[0] || name;
            p.why = quotes[1] || '';
            if (!section) { section = { name: '', why: '', params: [] }; h.sections.push(section); }
            p.section = section.name;
            section.params.push(name);
            if (p.type === 'choice') { h.params.push(p); return; }
            if (p.type === 'color') {
                const c = hexColour(nums[0]);
                if (!c) return err(`@param color ${name}: the default is #rrggbb`);
                p.def = c;
            } else if (p.type === 'bool') {
                const v = String(nums[0] ?? '0').toLowerCase();
                p.def = v === '1' || v === 'true' || v === 'on' ? 1 : 0;
                Object.assign(p, { min: 0, max: 1, step: 1 });
            } else {
                // A word after the numbers is the unit shown with the value ("s", "px", "%", "°").
                const unitAt = nums.findIndex(w => !/^[-+]?(\d+\.?\d*|\.\d+)(e[-+]?\d+)?$/i.test(w));
                if (unitAt >= 0) { p.unit = nums[unitAt]; nums.splice(unitAt); }
                const [d, lo, hi, st] = nums.map(Number);
                if (![d, lo, hi].every(Number.isFinite)) return err(`@param ${type} ${name}: default, min and max are numbers`);
                if (!(hi > lo)) return err(`@param ${name}: max must be above min`);
                p.min = lo;
                p.max = hi;
                p.step = Number.isFinite(st) && st > 0 ? st : (p.type === 'int' ? 1 : (hi - lo) / 100);
                if (p.type === 'int') { p.step = Math.max(1, Math.round(p.step)); p.min = Math.round(lo); p.max = Math.round(hi); }
                p.def = Math.min(p.max, Math.max(p.min, p.type === 'int' ? Math.round(d) : d));
            }
            h.params.push(p);
        }
    });
    if (!h.name) h.name = 'Untitled';
    if (spatialTag && h.spec < 2) h.spec = 2;
    h.spatial = h.spec >= 2;
    h.specNewer = h.spec > SPEC_MAJOR;
    return h;
}

/// The same text with its @param defaults set to `values` (a scene shared
/// carries its tuning).
export function withDefaults(text, header, values) {
    const lines = String(text).split(/\r?\n/);
    const eol = /\r\n/.test(text) ? '\r\n' : '\n';
    for (const p of header.params) {
        if (!(p.name in values)) continue;
        const i = p.line - 1;
        const v = values[p.name];
        const shown = p.type === 'color' ? colourHex(v) : p.type === 'bool' ? (v ? '1' : '0') : fmtNum(v, p);
        // Replace the default: the third word after @param.
        lines[i] = lines[i].replace(/^(\s*\/\/\s*@param\s+\S+\s+\S+\s+)(\S+)/, (_, a) => a + shown);
    }
    return lines.join(eol);
}
export function fmtNum(v, p) {
    if (p.type === 'int') return String(Math.round(v));
    const d = Math.min(6, Math.max(0, (String(p.step).split('.')[1] || '').length));
    return Number(v).toFixed(d);
}

// ── the program ────────────────────────────────────────────────────────

const PRELUDE = `#version 300 es
precision highp float;
precision highp int;
precision highp sampler2D;
uniform sampler2D uMusic;      // ${SLOTS} slices × ${MROWS} rows (R32F)
uniform sampler2D uWaveTex;    // ${WAVE_W}×${WAVE_H} RGBA32F: L, R, lap, 1
uniform sampler2D uPrevTex, uCoverTex, uAudio, uNoise;
uniform int uKBase, uWBase;
uniform float uQ, uQps, uWQ, uWRate, uWLap;
uniform vec2 uResolution;
uniform float uTime, uWallTime, uDelta, uFull;
uniform int uFrame;
uniform float uLive, uPlaying, uTrackTime, uTrackLength, uProgress, uTrackAge, uSampleRate, uAhead;
uniform float uBass, uMid, uTreble, uLoudness, uEnergy, uOnset, uBrightness, uWidth, uBpm, uBeatConfidence, uBeatRef;
uniform vec3 uHit;
uniform vec3 uPalette[4];
uniform float uHasCover, uHasPrev;
uniform vec4 iMouse, iDate;
uniform vec3 iChannelResolution[4];
uniform sampler2D uObjects;    // ${OBJ_W}×${OBJ_ROWS} RGBA32F: the sound objects by frame
uniform sampler2D uObjNow;     // ${OBJ_N * 4}×1 RGBA32F: the sound objects as heard now
uniform int uOBase;
uniform float uOQ, uOFps, uSpatial, uSpatialProgress;
uniform int uObjCount;         // the track map's instruments (0 before the map)
uniform vec3 uObjColour[${OBJ_N}];
uniform sampler2D uNoteRoll;   // ${OBJ_ROWS}×${NOTE_ROWS} RG16F: the instruments' notes by frame
@@CONSTS@@

float _m_at(int k, int r) {
    ivec2 c = ivec2(k & ${SLOTS - 1}, r);
    return texelFetch(uMusic, ivec2(c.x, ${ROW.K}), 0).r == float(k) ? texelFetch(uMusic, c, 0).r : 0.0;
}
bool _m_there(int k) { return k >= 0 && texelFetch(uMusic, ivec2(k & ${SLOTS - 1}, ${ROW.K}), 0).r == float(k); }
// A row at t seconds from now, between the two slices around it.
float _m_row(int r, float t) {
    float q = uQ + t * uQps;
    float fq = floor(q);
    int k = uKBase + int(fq);
    return mix(_m_at(k, r), _m_at(k + 1, r), q - fq);
}
float _m_bands(int r0, float f, float t) {
    float b = clamp(f, 0.0, 1.0) * 48.0 - 0.5;
    float fb = floor(b);
    int b0 = int(clamp(fb, 0.0, 47.0)), b1 = int(clamp(fb + 1.0, 0.0, 47.0));
    float q = uQ + t * uQps;
    float fq = floor(q);
    int k = uKBase + int(fq);
    float fr = q - fq, fr2 = clamp(b - fb, 0.0, 1.0);
    bool ta = _m_there(k), tb = _m_there(k + 1);
    int ka = k & ${SLOTS - 1}, kb = (k + 1) & ${SLOTS - 1};
    float a0 = ta ? texelFetch(uMusic, ivec2(ka, r0 + b0), 0).r : 0.0;
    float a1 = ta ? texelFetch(uMusic, ivec2(ka, r0 + b1), 0).r : 0.0;
    float c0 = tb ? texelFetch(uMusic, ivec2(kb, r0 + b0), 0).r : 0.0;
    float c1 = tb ? texelFetch(uMusic, ivec2(kb, r0 + b1), 0).r : 0.0;
    return mix(mix(a0, a1, fr2), mix(c0, c1, fr2), fr);
}
float musicHas(float t) {
    float q = uQ + t * uQps;
    int k = uKBase + int(floor(q + 0.5));
    return _m_there(k) ? 1.0 : 0.0;
}
float musicSpectrum(float f, float t) { return _m_bands(${ROW.SPEC}, f, t); }
float musicSpectrumDb(float f, float t) { return musicHas(t) > 0.5 ? _m_bands(${ROW.SPEC_DB}, f, t) * 96.0 - 96.0 : -96.0; }
float musicFreqToF(float hz) { return log(max(hz, 1.0) / 25.0) / log(800.0); }
float musicFToFreq(float f) { return 25.0 * pow(800.0, f); }
float musicBass(float t) { return _m_row(${ROW.BASS}, t); }
float musicMid(float t) { return _m_row(${ROW.MID}, t); }
float musicTreble(float t) { return _m_row(${ROW.TREBLE}, t); }
float musicHit(int part, float t) { return _m_row(${ROW.HIT} + clamp(part, 0, 2), t); }
float musicLoudness(float t) { return _m_row(${ROW.LOUD}, t); }
float musicLoudnessDb(float t) { return musicHas(t) > 0.5 ? _m_row(${ROW.LOUD_DB}, t) * 96.0 - 96.0 : -96.0; }
float musicEnergy(float t) { return _m_row(${ROW.ENERGY}, t); }
float musicOnset(float t) { return _m_row(${ROW.ONSET}, t); }
float musicBrightness(float t) { return _m_row(${ROW.BRIGHT}, t); }
float musicPeakDb(float t) { return musicHas(t) > 0.5 ? _m_row(${ROW.PEAK_DB}, t) * 96.0 - 96.0 : -96.0; }
float musicWidth(float t) { return _m_row(${ROW.WIDTH}, t); }
float musicBalance(float t) { return _m_row(${ROW.BALANCE}, t); }
float musicCorrelation(float t) { return musicHas(t) > 0.5 ? _m_row(${ROW.CORR}, t) : 1.0; }
vec2 _m_w(int i) {
    int lap = i >> 15;
    vec4 s = texelFetch(uWaveTex, ivec2(i & ${WAVE_W - 1}, (i >> 8) & ${WAVE_H - 1}), 0);
    return s.b == float(lap) ? s.rg : vec2(0.0);
}
float musicWave(float t, int ch) {
    float q = uWQ + t * uWRate;
    float fq = floor(q);
    int i = uWBase + int(fq);
    if (i < 0) return 0.0;
    vec2 a = _m_w(i), b = _m_w(i + 1);
    vec2 s = mix(a, b, q - fq);
    return ch == 0 ? s.x : ch == 1 ? s.y : ch == 2 ? 0.5 * (s.x + s.y) : 0.5 * (s.x - s.y);
}
// The beat grid: beats at uBeatRef + n·60/uBpm seconds from now.
float musicBeatPhase(float t) { return uBpm > 0.0 ? fract((t - uBeatRef) * uBpm / 60.0) : 0.0; }
float musicBeat(float t) {
    if (uBpm <= 0.0) return 0.0;
    float ph = musicBeatPhase(t) * 60.0 / uBpm;     // seconds since the beat
    return uBeatConfidence * exp(-ph / 0.05);
}
// ── the sound objects (Spec 2: a scene with // @spec 2) ──
const int AURA_OBJECTS = ${OBJ_N};
${OBJ_KINDS.map((n, i) => n ? `const int AURA_${n} = ${i};` : '').filter(Boolean).join('\n')}
// kind: what it is; main: the source's leading part (the lead voice); presence 0…1 (fades in when it is
// born, out when it goes); pos: x −1 left … +1 right, y 0 low … 1 high (its pitch), z 0 near … 1 far;
// age: seconds since it was born; origin: where it came from (the main part's place when it was born).
struct AuraObject { int kind; bool main; float presence; float energy; vec3 pos; float width; float onset;
    float coherence; float age; vec3 origin; };
AuraObject _o_make(vec4 a, vec4 b, vec4 c, vec4 d) {
    return AuraObject(int(a.z * 32.0 + 0.5), a.w > 0.5, a.x, a.y, b.xyz, b.w, c.x, c.y, c.z * 60.0, d.xyz);
}
bool _o_there(int k) { return k >= 0 && texelFetch(uObjects, ivec2(${OBJ_W - 1}, k & ${OBJ_ROWS - 1}), 0).r == float(k); }
vec4 _o_tex(int k, int c) { return texelFetch(uObjects, ivec2(c, k & ${OBJ_ROWS - 1}), 0); }
AuraObject auraObjectNow(int i) {
    int s = clamp(i, 0, ${OBJ_N - 1}) * 4;
    return _o_make(texelFetch(uObjNow, ivec2(s, 0), 0), texelFetch(uObjNow, ivec2(s + 1, 0), 0),
                   texelFetch(uObjNow, ivec2(s + 2, 0), 0), texelFetch(uObjNow, ivec2(s + 3, 0), 0));
}
// Presence alone, now (one read): skip the empty slots first.
float auraObjectPresence(int i) { return texelFetch(uObjNow, ivec2(clamp(i, 0, ${OBJ_N - 1}) * 4, 0), 0).x; }
AuraObject auraObject(int i, float t) {
    int s = clamp(i, 0, ${OBJ_N - 1}) * 4;
    float q = uOQ + t * uOFps;
    float fq = floor(q);
    int k = uOBase + int(fq);
    vec4 z = vec4(0.0);
    vec4 a0 = z, a1 = z, a2 = z, a3 = z, b0 = z, b1 = z, b2 = z, b3 = z;
    if (_o_there(k)) { a0 = _o_tex(k, s); a1 = _o_tex(k, s + 1); a2 = _o_tex(k, s + 2); a3 = _o_tex(k, s + 3); }
    if (_o_there(k + 1)) { b0 = _o_tex(k + 1, s); b1 = _o_tex(k + 1, s + 1); b2 = _o_tex(k + 1, s + 2); b3 = _o_tex(k + 1, s + 3); }
    float f = q - fq;
    // a slot that is empty on one side keeps the other side's place (only its presence goes)
    if (a0.x <= 0.0) { a1 = b1; a2.yzw = b2.yzw; a3 = b3; a0.zw = b0.zw; }
    if (b0.x <= 0.0) { b1 = a1; b2.yzw = a2.yzw; b3 = a3; b0.zw = a0.zw; }
    vec4 m0 = mix(a0, b0, f);
    m0.zw = f < 0.5 ? a0.zw : b0.zw;
    return _o_make(m0, mix(a1, b1, f), mix(a2, b2, f), mix(a3, b3, f));
}
float auraObjectsHave(float t) { return _o_there(uOBase + int(floor(uOQ + t * uOFps + 0.5))) ? 1.0 : 0.0; }
// The slot of a kind's main (else loudest) object now; −1 when none plays.
int auraFind(int kind) {
    int best = -1;
    float be = -1.0;
    for (int i = 0; i < ${OBJ_N}; i++) {
        vec4 a = texelFetch(uObjNow, ivec2(i * 4, 0), 0);
        if (a.x <= 0.0 || int(a.z * 32.0 + 0.5) != kind) continue;
        float e = a.y + (a.w > 0.5 ? 10.0 : 0.0);
        if (e > be) { be = e; best = i; }
    }
    return best;
}
// ── the track map (the instruments tier): its instruments, their colours and notes ──
const int AURA_KEY_LOW = ${KEY_LOW}, AURA_KEY_HIGH = ${KEY_LOW + KEYS - 1};
int auraObjectCount() { return uObjCount; }
vec3 auraObjectColour(int i) { return uObjColour[clamp(i, 0, ${OBJ_N - 1})]; }
// instrument i's note \`key\` (rounded) at t: (its velocity while it sounds, its onset flash); the nearest frame
vec2 _n_at(int i, float key, float t) {
    int kk = int(floor(key + 0.5));
    if (i < 0 || i >= ${OBJ_N} || kk < AURA_KEY_LOW || kk > AURA_KEY_HIGH) return vec2(0.0);
    int k = uOBase + int(floor(uOQ + t * uOFps + 0.5));
    if (!_o_there(k)) return vec2(0.0);
    return texelFetch(uNoteRoll, ivec2(k & ${OBJ_ROWS - 1}, i * ${KEYS} + kk - AURA_KEY_LOW), 0).rg;
}
float auraNote(int i, float key, float t) { return _n_at(i, key, t).r; }
float auraNoteOnset(int i, float key, float t) { return _n_at(i, key, t).g; }
// how many notes instrument i plays at t (value 11 of its frame)
float auraObjectNotes(int i, float t) {
    int k = uOBase + int(floor(uOQ + t * uOFps + 0.5));
    return _o_there(k) ? _o_tex(k, clamp(i, 0, ${OBJ_N - 1}) * 4 + 2).w * 16.0 : 0.0;
}
// instrument i's loudness and its hit flash at t: the nearest frame alone (2 reads, not auraObject's 18) —
// for a loop over every instrument in every pixel
float auraObjectEnergy(int i, float t) {
    int k = uOBase + int(floor(uOQ + t * uOFps + 0.5));
    return _o_there(k) ? _o_tex(k, clamp(i, 0, ${OBJ_N - 1}) * 4).y : 0.0;
}
float auraObjectOnset(int i, float t) {
    int k = uOBase + int(floor(uOQ + t * uOFps + 0.5));
    return _o_there(k) ? _o_tex(k, clamp(i, 0, ${OBJ_N - 1}) * 4 + 2).x : 0.0;
}
vec3 auraPalette(float x) {
    x = fract(x) * 4.0;
    int i = int(floor(x));
    float f = smoothstep(0.0, 1.0, fract(x));
    vec3 a = uPalette[i & 3], b = uPalette[(i + 1) & 3];
    return mix(a, b, f);
}
// The app's own colours (sky, violet, pink, amber), whatever the cover.
vec3 auraAppPalette(float x) {
    const vec3 A0 = vec3(0.220, 0.741, 0.973), A1 = vec3(0.659, 0.333, 0.969), A2 = vec3(0.957, 0.447, 0.714), A3 = vec3(0.980, 0.800, 0.082);
    x = fract(x) * 4.0;
    float f = smoothstep(0.0, 1.0, fract(x));
    int i = int(floor(x));
    vec3 a = i == 0 ? A0 : i == 1 ? A1 : i == 2 ? A2 : A3;
    vec3 b = i == 0 ? A1 : i == 1 ? A2 : i == 2 ? A3 : A0;
    return mix(a, b, f);
}
vec4 auraCover(vec2 uv) {
    if (uHasCover < 0.5 || uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) return vec4(0.0);
    return texture(uCoverTex, uv);                         // (the picture is stored bottom row first)
}
vec4 auraPrev(vec2 uv) { return uHasPrev > 0.5 ? texture(uPrevTex, uv) : vec4(0.0); }
float auraHash(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}
float auraNoise(vec2 p) {
    vec2 i = floor(p), f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    return mix(mix(auraHash(i), auraHash(i + vec2(1, 0)), u.x), mix(auraHash(i + vec2(0, 1)), auraHash(i + vec2(1, 1)), u.x), u.y);
}
float auraFbm(vec2 p) {
    float s = 0.0, a = 0.5;
    for (int i = 0; i < 5; i++) { s += a * auraNoise(p); p = p * 2.03 + 17.1; a *= 0.5; }
    return s / 0.96875;
}
vec3 auraHsv(vec3 c) {
    vec3 p = abs(fract(c.xxx + vec3(1.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - 3.0);
    return c.z * mix(vec3(1.0), clamp(p - 1.0, 0.0, 1.0), c.y);
}
mat2 auraRot(float a) { float c = cos(a), s = sin(a); return mat2(c, s, -s, c); }
// Shadertoy's names.
#define iResolution vec3(uResolution, 1.0)
#define iTime uTime
#define iTimeDelta uDelta
#define iFrame uFrame
#define iSampleRate uSampleRate
`;
// A scene of one pass: Shadertoy's channels are the app's (C5). A scene of
// several passes (// @pass): each pass has its own four (the same number of
// lines, so the scene's lines keep their numbers either way).
const CHANNELS_ONE = `#define iChannel0 uAudio
#define iChannel1 uPrevTex
#define iChannel2 uCoverTex
#define iChannel3 uNoise
`;
const CHANNELS_OWN = `uniform sampler2D iChannel0;
uniform sampler2D iChannel1;
uniform sampler2D iChannel2;
uniform sampler2D iChannel3;
`;

// Names the app declares (a scene's own `uniform` lines for them are taken out).
const PROVIDED = new Set(`uMusic uWaveTex uPrevTex uCoverTex uAudio uNoise uKBase uWBase uQ uQps uWQ uWRate uWLap uResolution
uTime uWallTime uDelta uFull uFrame uLive uPlaying uTrackTime uTrackLength uProgress uTrackAge uSampleRate uAhead uBass uMid
uTreble uLoudness uEnergy uOnset uBrightness uWidth uBpm uBeatConfidence uBeatRef uHit uPalette uHasCover uHasPrev iMouse iDate
iChannelResolution iResolution iTime iTimeDelta iFrame iSampleRate iChannel0 iChannel1 iChannel2 iChannel3
uObjects uObjNow uOBase uOQ uOFps uSpatial uSpatialProgress uObjCount uObjColour uNoteRoll`.split(/\s+/));

/// The text written for book `major` — the studio's Standard / With instruments
/// switch: 2 sets a `// @spec 2` line (after the header's other `// @` lines),
/// 1 takes it out (the default). The older `// @spatial` line goes either way.
export function setSpec(text, major) {
    const eol = /\r\n/.test(text) ? '\r\n' : '\n';
    const lines = String(text).split(/\r?\n/);
    const tagged = l => /^\s*\/\/\s*@(spec|spatial)\b/i.test(l);
    let has = lines.findIndex(tagged);
    for (let i = lines.length - 1; i > has; i--) if (tagged(lines[i])) lines.splice(i, 1);
    if (major < 2) {
        if (has >= 0) lines.splice(has, 1);
        return lines.join(eol);
    }
    const line = '// @spec     ' + major;
    if (has >= 0) { lines[has] = line; return lines.join(eol); }
    // After @name / @author / @about / @feedback / @quality, before the settings.
    let at = 0;
    for (let i = 0; i < lines.length; i++) {
        const m = /^\s*\/\/\s*@(\w+)/.exec(lines[i]);
        if (m && /^(name|author|about|feedback|quality)$/i.test(m[1])) at = i + 1;
        else if (m) break;
        else if (lines[i].trim() && !/^\s*\/\//.test(lines[i])) break;
    }
    lines.splice(at, 0, line);
    return lines.join(eol);
}
export const PRELUDE_LINES = (PRELUDE + CHANNELS_ONE).split('\n').length - 1;
/// The prelude for a scene: PI and TAU are the app's unless the scene declares its own (Shadertoy's
/// code often does) — one line either way, so the lines keep their numbers.
function preludeFor(plain) {
    const own = name => new RegExp(`#\\s*define\\s+${name}\\b|\\bfloat\\b[^;{}()]*\\b${name}\\s*=(?!=)`).test(plain);
    const keep = ['PI = 3.14159265', 'TAU = 6.2831853'].filter(c => !own(c.split(' ')[0]));
    return PRELUDE.replace('@@CONSTS@@', keep.length ? `const float ${keep.join(', ')};` : '// (PI and TAU: the scene\'s own)');
}

/// Strip comments (keeping line breaks), for the checks below.
function uncomment(src) {
    return src.replace(/\/\*[\s\S]*?\*\//g, m => m.replace(/[^\n]/g, ' ')).replace(/\/\/[^\n]*/g, '');
}

// ── scenes of several passes (Shadertoy's buffers) ─────────────────────
// `// @common` starts code every pass gets; `// @pass A` … `// @pass D` a
// buffer (a picture of its own, kept between frames), `// @pass image` the
// picture shown — each up to the next such line. On a pass line, what its
// iChannel0…3 read: `iChannel0=A:mipmap:repeat` — a buffer (itself or a
// later one: as it was a frame ago; an earlier one: as it is now), `noise`
// (256×256, Shadertoy's), `noise64`, `cover`, `music` (Shadertoy's 512×2
// sound picture) or `none`; then how: nearest | linear | mipmap, clamp |
// repeat. The passes run A, B, C, D, then image.
export const PASS_NAMES = ['A', 'B', 'C', 'D', 'image'];
export const CHANNEL_SOURCES = ['A', 'B', 'C', 'D', 'noise', 'noise64', 'cover', 'music', 'none'];

/// A scene's passes: null for a scene of one pass (no `// @pass` line), else
/// { passes: [{ name, line, from, to, channels: [{ src, filter, wrap } × 4] }] (in running order),
///   common: [[from, to] …], errors } — lines 0-based, `to` exclusive.
export function parsePasses(text) {
    const lines = String(text).split(/\r?\n/);
    const tags = [];
    lines.forEach((l, i) => {
        const m = /^\s*\/\/\s*@(pass|common)\b\s*(.*)$/i.exec(l);
        if (m) tags.push({ i, kind: m[1].toLowerCase(), rest: m[2].trim() });
    });
    if (!tags.some(t => t.kind === 'pass')) return null;
    const errors = [], passes = [], common = [];
    tags.forEach((t, k) => {
        const from = t.i + 1, to = k + 1 < tags.length ? tags[k + 1].i : lines.length;
        if (t.kind === 'common') { common.push([from, to]); return; }
        const words = t.rest.split(/\s+/).filter(Boolean);
        const raw = words.shift() || '';
        const name = /^image$/i.test(raw) ? 'image' : /^[a-d]$/i.test(raw) ? raw.toUpperCase() : null;
        const line = t.i + 1;
        if (!name) { errors.push({ line, msg: `@pass: the pass is A, B, C, D or image, not "${raw}"` }); return; }
        if (passes.some(p => p.name === name)) { errors.push({ line, msg: `@pass ${name} twice` }); return; }
        const channels = [0, 1, 2, 3].map(() => ({ src: 'none', filter: 'linear', wrap: 'clamp' }));
        for (const w of words) {
            const c = /^iChannel([0-3])=([\w:]+)$/i.exec(w);
            if (!c) { errors.push({ line, msg: `@pass ${name}: "${w}" — a channel is written iChannel0=A (or noise, cover, music, none)` }); continue; }
            const [src, ...how] = c[2].split(':');
            const s = CHANNEL_SOURCES.find(x => x.toLowerCase() === src.toLowerCase());
            if (!s) { errors.push({ line, msg: `@pass ${name}: iChannel${c[1]} reads A, B, C, D, noise, noise64, cover, music or none, not "${src}"` }); continue; }
            const ch = { src: s, filter: 'linear', wrap: /^noise/.test(s) ? 'repeat' : 'clamp' };
            for (const h of how) {
                const hl = h.toLowerCase();
                if (['nearest', 'linear', 'mipmap'].includes(hl)) ch.filter = hl;
                else if (['clamp', 'repeat'].includes(hl)) ch.wrap = hl;
                else errors.push({ line, msg: `@pass ${name}: iChannel${c[1]}: "${h}" — nearest, linear or mipmap; clamp or repeat` });
            }
            channels[Number(c[1])] = ch;
        }
        passes.push({ name, line, from, to, channels });
    });
    if (!passes.some(p => p.name === 'image') && !errors.length) errors.push({ line: 1, msg: 'a scene of several passes needs `// @pass image` — the picture shown' });
    for (const p of passes) for (const ch of p.channels) {
        if (PASS_NAMES.includes(ch.src) && ch.src !== 'image' && !passes.some(q => q.name === ch.src))
            errors.push({ line: p.line, msg: `@pass ${p.name}: it reads buffer ${ch.src}, which the scene does not have` });
    }
    passes.sort((a, b) => PASS_NAMES.indexOf(a.name) - PASS_NAMES.indexOf(b.name));
    return { passes, common, errors };
}

/// A scene's text → { header, fs, entry, feedback, offset, userLines, errors: [{line, msg}] }, and for a scene
/// of several passes `passes: [{ name, fs, entry, channels }]` in running order (fs is then the image pass's).
/// Lines of the scene keep their numbers in every fs after PRELUDE_LINES (and the params'
/// declarations, counted in `offset`): a pass is the whole text with the other passes' lines emptied.
export function buildScene(text) {
    const header = parseHeader(text);
    const errors = [...header.errors];
    const src = String(text).replace(/\r\n?/g, '\n');
    const plain = uncomment(src);
    // Lines taken out (kept empty, so the numbers hold).
    const lines = src.split('\n');
    const plainLines = plain.split('\n');
    const own = new Set(header.params.map(p => p.name));
    plainLines.forEach((l, i) => {
        if (/^\s*#\s*version\b/.test(l)) lines[i] = '';
        else if (/^\s*precision\s+\w+\s+\w+\s*;\s*$/.test(l)) lines[i] = '';
        else {
            const m = /^\s*uniform\s+[\w\s]*?\b(\w+)\s*(\[[^\]]*\])?\s*;\s*$/.exec(l);
            if (m && (PROVIDED.has(m[1]) || own.has(m[1]))) lines[i] = '';
            else if (/^\s*out\s+vec4\s+\w+\s*;\s*$/.test(l)) lines[i] = '';
        }
    });
    plainLines.forEach((l, i) => {
        if (/\b(while|do)\b/.test(l)) errors.push({ line: i + 1, msg: '`while` and `do` loops are not allowed: use a `for` loop with a constant bound' });
        if (/\bvoid\s+main\s*\(/.test(l)) errors.push({ line: i + 1, msg: 'no main() — write `vec4 scene(vec2 uv, vec2 p)` (or mainImage); the app adds main()' });
    });
    const entryOf = code => /\bvec4\s+scene\s*\(\s*(in\s+)?vec2\s+\w+\s*,\s*(in\s+)?vec2\s+\w+\s*\)/.test(code) ? 'scene2'
        : /\bvec4\s+scene\s*\(\s*(in\s+)?vec2\s+\w+\s*\)/.test(code) ? 'scene1'
        : /\bvoid\s+mainImage\s*\(/.test(code) ? 'mainImage' : null;

    const decl = header.params.map(p =>
        `uniform ${p.type === 'color' ? 'vec3' : p.type === 'choice' ? 'int' : p.type} ${p.name};`).join('\n');
    const offset = PRELUDE_LINES + (decl ? decl.split('\n').length : 0);
    const shown = entry => entry === 'scene2' ? 'c = scene(uv, p);'
        : entry === 'scene1' ? 'c = scene(uv);'
        : 'vec4 m = vec4(0.0, 0.0, 0.0, 1.0);\n    mainImage(m, fc);\n    m.rgb = clamp(m.rgb, 0.0, 16.0);\n    c = vec4(m.rgb, clamp(max(max(m.r, m.g), m.b), 0.0, 1.0));\n    c.rgb = c.a > 0.0 ? c.rgb / max(c.a, 1e-4) : vec3(0.0);';
    const mainShown = entry => `
out vec4 auraOut;
void main() {
    vec2 fc = gl_FragCoord.xy;
    vec2 uv = fc / uResolution;
    vec2 p = (2.0 * fc - uResolution) / uResolution.y;
    vec4 c = vec4(0.0);
    ${shown(entry)}
    if (any(isnan(c)) || any(isinf(c))) c = vec4(0.0);
    auraOut = vec4(clamp(c.rgb, 0.0, 16.0), clamp(c.a, 0.0, 1.0));
}
`;
    // A buffer keeps what its mainImage wrote, as it is (only a NaN or an infinity is dropped).
    const mainBuffer = `
out vec4 auraOut;
void main() {
    vec4 m = vec4(0.0);
    mainImage(m, gl_FragCoord.xy);
    auraOut = any(isnan(m)) || any(isinf(m)) ? vec4(0.0) : m;
}
`;
    const head = decl ? decl + '\n' : '';

    const mp = parsePasses(src);
    if (!mp) {
        const entry = entryOf(plain);
        if (!entry) errors.push({ line: 1, msg: 'the scene needs `vec4 scene(vec2 uv, vec2 p) { … }` (or Shadertoy\'s `void mainImage(out vec4 fragColor, in vec2 fragCoord)`)' });
        // Feedback: asked for, or Shadertoy's iChannel1 used.
        const feedback = header.feedback || /\biChannel1\b/.test(plain);
        const fs = preludeFor(plain) + CHANNELS_ONE + head + lines.join('\n') + '\n' + mainShown(entry);
        return { header, fs, entry, feedback, offset, userLines: lines.length, errors };
    }
    errors.push(...mp.errors);
    // Before the first @pass / @common: the header, and code every pass gets.
    const first = Math.min(...mp.passes.map(p => p.from - 1), ...mp.common.map(c => c[0] - 1));
    const shared = i => i < first || mp.common.some(([a, b]) => i >= a && i < b);
    const passes = mp.passes.map(P => {
        const mine = i => shared(i) || (i >= P.from && i < P.to);
        const body = lines.map((l, i) => mine(i) ? l : '').join('\n');
        const entry = entryOf(plainLines.slice(P.from, P.to).join('\n'));
        if (P.name === 'image' ? !entry : entry !== 'mainImage')
            errors.push({ line: P.line, msg: P.name === 'image' ? '@pass image needs `vec4 scene(vec2 uv, vec2 p)` or `void mainImage(out vec4 fragColor, in vec2 fragCoord)`'
                : `@pass ${P.name} needs \`void mainImage(out vec4 fragColor, in vec2 fragCoord)\`: a buffer keeps what it writes` });
        const fs = preludeFor(uncomment(body)) + CHANNELS_OWN + head + body + '\n' + (P.name === 'image' ? mainShown(entry) : mainBuffer);
        return { name: P.name, fs, entry, channels: P.channels };
    });
    const image = passes.find(p => p.name === 'image');
    return { header, fs: image?.fs || '', entry: image?.entry || null, feedback: false, offset, userLines: lines.length, errors, passes };
}

/// The compiler's log → [{ line, msg }] on the scene's own lines (line 0:
/// in the app's part, after the scene).
export function mapErrors(log, offset, userLines) {
    const out = [];
    for (const raw of String(log || '').split('\n')) {
        const m = /^\s*(?:ERROR|WARNING):\s*\d+:(\d+):\s*(.*)$/.exec(raw);
        if (!m) { if (raw.trim()) out.push({ line: 0, msg: raw.trim() }); continue; }
        let line = Number(m[1]) - offset;
        const msg = m[2].replace(/^'([^']*)'\s*:\s*/, (_, w) => w ? `'${w}': ` : '');
        if (line < 1 || line > userLines) line = 0;
        out.push({ line, msg });
    }
    return out;
}

/// A scene's values: the defaults, overlaid with what was kept (in range).
export function valuesOf(header, kept) {
    const v = {};
    for (const p of header.params) {
        const k = kept?.[p.name];
        if (p.type === 'color') v[p.name] = Array.isArray(k) && k.length === 3 && k.every(Number.isFinite) ? k.slice() : p.def.slice();
        else if (Number.isFinite(Number(k)) && k !== null && k !== '') v[p.name] = Math.min(p.max, Math.max(p.min, Number(k)));
        else v[p.name] = p.def;
    }
    return v;
}

/// Take the scene out of an AI's answer: the code block (```glsl … ```),
/// else the text itself.
export function extractCode(answer) {
    const s = String(answer || '');
    const blocks = [...s.matchAll(/```[ \t]*([\w-]*)[^\n]*\n([\s\S]*?)```/g)];
    if (blocks.length) {
        const best = blocks.find(b => /@name|scene\s*\(|mainImage/.test(b[2])) || blocks.sort((a, b) => b[2].length - a[2].length)[0];
        return best[2].replace(/\s+$/, '') + '\n';
    }
    return s.trim() + '\n';
}

// ── a shader from Shadertoy, as its site gives it (the JSON of a shader) ──
// Its buffers' own ids, and the few pictures of its library the app has.
const ST_BUFFERS = { '4dXGR8': 'A', 'XsXGR8': 'B', '4sXGR8': 'C', 'XdfGR8': 'D' };
const ST_MEDIA = {
    '08b42b43ae9d3c0605da11d0eac86618ea888e62cdd9518ee8b9097488b31560': { src: 'none', note: 'Shadertoy\'s font picture (Font 1) is not in this app: its letters will not show' },
};

/// A Shadertoy shader's JSON (the site's, as text or parsed: [{ info, renderpass }] or { Shader: … })
/// → the text of a scene of several passes (or of one, when it has no buffers), with the author, the
/// link and the licence in its header; null when it is not such JSON.
export function fromShadertoy(input) {
    let d = input;
    if (typeof d === 'string') {
        const t = d.trim();
        if (!/^[[{]/.test(t) || !/renderpass/.test(t)) return null;
        try { d = JSON.parse(t); } catch (_) { return null; }
    }
    if (Array.isArray(d)) d = d[0];
    if (d && d.Shader) d = d.Shader;
    if (!d || !Array.isArray(d.renderpass)) return null;
    const info = d.info || {};
    const name = String(info.name || 'From Shadertoy').trim();
    const user = String(info.username || '').trim();
    const link = info.id ? `https://www.shadertoy.com/view/${info.id}` : 'https://www.shadertoy.com';
    const about = String(info.description || '').replace(/\s+/g, ' ').trim();
    const notes = [];
    const passes = [];
    let common = null;
    for (const rp of d.renderpass) {
        const type = String(rp.type || '').toLowerCase();
        const code = String(rp.code || '').replace(/\r\n?/g, '\n').replace(/\s+$/, '') + '\n';
        if (type === 'common') { common = code; continue; }
        if (type === 'sound') { notes.push('its Sound tab (sound the shader makes) is not used: the scene follows the music the player plays'); continue; }
        let pass = null;
        if (type === 'image') pass = 'image';
        else if (type === 'buffer') pass = ST_BUFFERS[(rp.outputs || [])[0]?.id] || { 'buffer a': 'A', 'buffer b': 'B', 'buffer c': 'C', 'buffer d': 'D' }[String(rp.name || '').toLowerCase()];
        if (!pass) { notes.push(`its pass "${rp.name || type}" is not supported here and is left out`); continue; }
        const channels = [];
        for (const inp of rp.inputs || []) {
            const ch = Number(inp.channel);
            if (!(ch >= 0 && ch <= 3)) continue;
            const it = String(inp.type || inp.ctype || '').toLowerCase();
            const s = inp.sampler || {};
            let src = 'none';
            if (it === 'buffer') src = ST_BUFFERS[inp.id] || 'none';
            else if (it === 'music' || it === 'musicstream' || it === 'mic') src = 'music';
            else if (it === 'texture') {
                const hash = (/([0-9a-f]{64})/.exec(String(inp.filepath || inp.src || '')) || [])[1];
                const known = ST_MEDIA[hash];
                if (known) { src = known.src; if (known.note) notes.push(`iChannel${ch} of ${pass === 'image' ? 'the picture' : 'pass ' + pass}: ${known.note}`); }
                else { src = 'noise'; notes.push(`iChannel${ch} of ${pass === 'image' ? 'the picture' : 'pass ' + pass}: a picture of Shadertoy's library (${inp.filepath || inp.id}) — noise instead`); }
            } else notes.push(`iChannel${ch} of ${pass === 'image' ? 'the picture' : 'pass ' + pass}: its ${it || 'input'} is not available here`);
            const filter = ['nearest', 'linear', 'mipmap'].includes(s.filter) ? s.filter : 'linear';
            const wrap = s.wrap === 'repeat' ? 'repeat' : 'clamp';
            channels[ch] = `iChannel${ch}=${src}${src === 'none' ? '' : `:${filter}:${wrap}`}`;
        }
        passes.push({ pass, channels: channels.filter(Boolean), code });
    }
    if (!passes.some(p => p.pass === 'image')) return null;
    passes.sort((a, b) => PASS_NAMES.indexOf(a.pass) - PASS_NAMES.indexOf(b.pass));
    const out = [
        `// @name     ${name}`,
        `// @author   ${user || 'unknown'} — ${link}`,
        ...(about ? [`// @about    ${about}`] : []),
        `// @license  CC BY-NC-SA 3.0 (Shadertoy's terms, unless its code says otherwise) — adapted from "${name}" by ${user || 'unknown'}, ${link}`,
        '//',
        '// Brought from Shadertoy by the studio. ' + (notes.length ? 'Notes:' : 'It runs as it was written; the music reaches it through iChannel "music" or the music functions.'),
        ...notes.map(n => `//   - ${n}`),
    ];
    // (one pass that reads nothing: a plain scene; its channels otherwise, as Shadertoy had them)
    const one = passes.length === 1 && !common && !passes[0].channels.length;
    if (one) return out.join('\n') + '\n\n' + passes[0].code;
    if (common) out.push('', '// @common', common);
    for (const p of passes) out.push('', `// @pass ${p.pass}${p.channels.length ? '  ' + p.channels.join('  ') : ''}`, p.code);
    return out.join('\n').replace(/\n{3,}/g, '\n\n') + (out[out.length - 1].endsWith('\n') ? '' : '\n');
}
