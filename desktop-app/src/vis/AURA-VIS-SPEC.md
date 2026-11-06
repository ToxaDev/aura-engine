# Aura Engine visualization scenes — the book (Spec 1.0)

You are writing a **scene**: a visualization for the Aura Engine music player — a high-end player that renders
its sound seconds before it is heard. A scene is one text file of **GLSL ES 3.00** (WebGL 2) that the player
compiles and runs on the video card for every pixel of every frame.

Two things make a scene here different from a visualizer anywhere else, and a good scene uses both:

1. **It sees the future.** The player hands the scene the music as functions of time, including **the next ~3.5
   seconds of sound** — already rendered, it will sound exactly so. A scene can show a beat on its way before it
   is heard: things appear far away (or at an edge) from the sound to come and arrive at the focus the moment
   they sound; a bar can glow just before its note starts.
2. **It is tuned by a person.** Every scene declares its settings, grouped in sections, and the player builds a
   settings panel from them. A scene is only as good as its settings: a person should be able to make it calm or
   wild, thin or bold, cool or warm, without touching the code — and never meet a setting they do not understand.

Read it all once. Part A is the file and the settings (how to design them well is the heart of it). Part B is
what the player gives. Part C is technique. Part D is the rules and the answer format. Part E is a complete
example.

This book is **Spec 1**: the sound of the whole mix, which every player has. A second part, **the instruments**
(Spec 2), adds the song's instruments as objects in space (the voice in the middle, a drum a little to the left,
the room's sound all around); when a scene is written for it, that part follows this book.

---

# Part A — the file and its settings

## A1. The file

```glsl
// @name     Bars
// @author   (who wrote it)
// @about    One sentence: what the picture is and how it follows the music.
// @feedback                                    (optional, see C4)
// @quality  0.75                               (optional, see D2)
// @spec     1                                  (optional: the book it is written for — 1 this one, 2 the instruments)
// @license  CC BY-NC-SA 3.0 — adapted from …   (optional: the licence, when the scene is someone else's work)
//
// @section  "Bars"   "The bars themselves"
// @param    int    count   48   8 128 1        "Count"   "How many bars across"
// @param    float  gap     0.25 0.0 0.8 0.01   "Gap"     "Space between bars, a share of a bar"
//
// @section  "Colour"
// @param    choice colours 0 "Album cover|App|Custom" "Colours" "Where the colours come from"
// @param    color  low     #22d3ee             "Custom bottom" "With Custom: the colour at a bar's foot"

vec4 scene(vec2 uv, vec2 p) { ... }
```

Header lines are GLSL comments, so the file is plain GLSL. Only `@name` is required; a scene worth sharing has
`@about` and sections of settings.

## A2. Settings: the syntax

`// @section "Name" "What this part of the picture is"` — starts a group of settings; every `@param` after it
belongs to it, up to the next `@section`. The panel shows sections in file order, each with a head that folds it
(all open at first) and a scrollable list under it.

`// @param <type> <name> <default> [<min> <max> [<step>] [<unit>]] ["Label"] ["What it does"]`

| type | uniform in the code | what follows the name |
|---|---|---|
| `float` | `uniform float name;` | default, min, max (required), step, unit — `2.5 0.5 3.5 0.1 s` |
| `int` | `uniform int name;` | default, min, max (required), step |
| `bool` | `uniform bool name;` | `0`/`1` |
| `color` | `uniform vec3 name;` (RGB 0…1) | `#rrggbb` |
| `choice` | `uniform int name;` (0, 1, 2 …) | default index, then the options in one quoted string: `0 "Album cover|App|Custom"` |

- The **unit** is a short word after the numbers shown next to the value: `s`, `px`, `%`, `°`, `x`, `bpm`.
- The **label** is 1–3 plain words, capitalised like a sentence ("Fall speed", not "fallSpeed", not "FALL SPEED").
- The **text** says what changes on screen, in one line, in words a listener uses ("How fast a bar drops after its
  sound"), not the formula.
- The app declares the uniforms — **use the name directly** (`count`, `gap`, `colours`), do not declare them.
  A name is a plain identifier, must not start with `u`, `i`, `music`, `aura`, and is not a GLSL keyword.
- A `choice` is an `int` in the code: compare it with its index (`if (colours == 2)`), and write a comment
  naming the options where you use it.

## A3. Designing the settings — the heart of a good scene

A person opens the panel to make the picture *theirs*. Give them the handles they would reach for, named so they
know what will happen, with ranges where every value still looks good. Design the settings while you design the
picture: for every element you draw, ask "what about it would someone want different?"

### Principles

1. **One section per element of the picture**, plus the shared ones. Elements are the things a viewer would name:
   "the bars", "the peaks", "the rings", "the particles", "the background", "the camera". Shared sections:
   **Colour** (where colours come from, custom colours, glow, brightness) and **Motion** (how the picture follows
   the music: which part of the sound drives it, how strongly, how fast it rises and falls, how far ahead it
   looks). A **Layout** section holds position, size and camera when the picture has them.
2. **Order sections by how often people reach for them**: the main element first (what the scene is about), then
   Colour, then Motion, then the other elements from near to far (foreground → background), Layout last.
3. **3–7 sections, 2–7 settings each, about 12–35 in all.** Enough to shape the look; never a wall of knobs.
4. **Every setting changes something visible** at every point of its range. No setting that only matters in a
   corner case; no two settings that do the same thing; no raw constants exposed because they happened to be in
   the code ("Noise frequency 2" means nothing — "Grain size" does).
5. **Ranges are safe**: from the calmest sensible value to the wildest that still looks deliberate. Nothing in a
   range may break the picture (a black screen, a solid colour, flicker at a single pixel).
6. **Defaults look great** on typical music (pop, rock, electronic, orchestral alike) in the player's band: the
   scene must be beautiful before anyone touches it.
7. **Choose the form of each setting well**: amounts and sizes are `float` sliders; counts are `int`; a switch
   between a few distinct behaviours is a `choice` (not a float that jumps); on/off parts are `bool`; colours are
   `color`, offered next to a `choice` that picks where colours come from.
8. **Make sizes resolution-proof**: widths and gaps in pixels (`px`, scaled by `uResolution.y / 1080.0` when you
   want them to grow with the screen) or as shares of the picture — never in raw `p` units the person cannot
   picture.
9. **Name the music's part in words**: a `choice` "Driven by" with `"Bass|Middle|Highs|Loudness|Beat"` is clearer
   than three weights.
10. **Time settings in seconds** with unit `s` (look-ahead, hold, trail), speeds in "per second" or "x".

### What each kind of element usually needs

Take what fits the picture; leave out what does not. These are the handles people miss when they are absent.

- **Bars / spectrum columns** — count; gap (share of a bar); height (share of the picture); curve/contrast
  (how much the quiet bands shrink); rounded ends; mirror (grow both ways); frequency range or layout
  (log/linear, bass in the middle or at the left); *peaks*: show, hold time, fall speed, thickness;
  *motion*: rise (attack) and release (fall) speeds, anticipation (glow before the sound).
- **Lines / waveforms / oscilloscopes** — thickness (px); number of lines; spacing; amplitude; smoothness; trail
  or persistence (s); which channel (choice: Mid|Side|Left|Right|Stereo X–Y); glow.
- **Rings / tunnels / radial pictures** — rings per second of sound; ring width (px); depth or look-ahead (s);
  focus size; twist/spin; how far the sound bends a ring; centre height.
- **Particles / sparks / stars** — amount; size (px); speed; life (s); spread; what emits them (choice: Kicks|
  Highs|Onsets|Beat); fade; colour over life.
- **Feedback clouds (MilkDrop-style)** — trail length (decay); push/zoom on the bass; swirl/rotation; turbulence;
  colour drift; how much a kick jolts it.
- **Terrain / 3-D surfaces** — rows or grid density; height; smoothness; horizon; camera height and tilt; fog;
  wireframe or filled (choice).
- **Background** — show (bool); style (choice: Plain|Gradient|Stars|Grain); its colours; vignette; how much it
  breathes with the music (keep it subtle — it sits behind the player's text).
- **Colour (shared)** — `choice` "Colours": "Album cover|App|Custom"; one to three `color`s for Custom; colour
  spread / hue shift; saturation; brightness; glow/bloom.
- **Motion (shared)** — driven by (choice); reaction amount; rise speed; fall speed; smoothing; look-ahead (s);
  beat sync (bool: pulse on the tempo's beats).
- **Layout / camera (shared)** — position (centre height), size/zoom, rotation, camera drift.

### Before you answer, check your settings

- Would a person who never read the code understand every label and its line of text?
- Does each section hold one element or one shared aspect, and are they in the order of A3.2?
- Does moving each slider from end to end change the picture visibly, and never break it?
- Are the defaults the best-looking version you can make?
- Is there a way to use the album's colours, the app's colours and one's own?

---

# Part B — what the player gives

## B1. The entry point and coordinates

```glsl
vec4 scene(vec2 uv, vec2 p)
```

- `uv` — 0…1 across the drawing, (0,0) at the **bottom left**.
- `p` — centred and square: (0,0) in the middle, **y from −1 (bottom) to +1 (top)**, x from −aspect to +aspect
  (`aspect = uResolution.x / uResolution.y`). Use `p` for anything round.
- Return the colour as **straight (not premultiplied) RGBA, 0…1**. Alpha 0 lets the player's dark background
  show through; alpha 1 covers it. Values outside 0…1 are clamped on screen.
- `vec4 scene(vec2 uv)` (one argument) is accepted too; so is Shadertoy's `mainImage` (C5).
- One pixel in `p` units: `2.0 / uResolution.y`. `fwidth()` works for anti-aliasing.

Do **not** write `#version`, `precision`, `out` variables, `void main()`, or `uniform` lines for anything this
book provides — the app adds them (a `#version` line and repeated declarations are removed).

## B2. Where a scene is seen

1. **The player's big view**, behind the track's title and time, in a band about 420 × 260 px (wider than tall).
   The app keeps its text readable by fading the scene: the **top half** (title, artist, the technical line) is
   shown at 10–75 %, full strength from 62 % to 86 % of the height (from the top), then it fades out to the
   **bottom edge** (the seek bar). The lower-middle is the stage — put the focus there: for round scenes a centre
   around `p.y = -0.2`; for things that stand on a floor, a floor near `uv.y = 0.16`.
2. **Full screen** (`uFull = 1`), any shape — 16:9, 21:9, a 3840 × 1600 monitor. No fading: use the middle
   (`p.y = 0`) and a floor near `uv.y = 0.06`.
3. **The studio's preview**, next to the code, in both shapes.

Design for all three: `uResolution` and `p`, never fixed pixel numbers; lines at least ~1.5 px at 420 px wide.

## B3. Time

| uniform | meaning |
|---|---|
| `float uTime` | the scene's clock, seconds. Runs with the music; **slows to a stop when the music is paused** (the picture holds still), runs at 0.45× while nothing plays. Use it for motion that is not the music's. |
| `float uWallTime` | real seconds since the scene started; never stops. |
| `float uDelta` | seconds of `uTime` since the last frame. |
| `int uFrame` | frames drawn since the scene started. |
| `vec2 uResolution` | the drawing's size in pixels (may be lower than the screen's — the app lowers it on slow cards). |
| `float uFull` | 1 on the whole screen, 0 in the player's band. |

The app draws up to **60 frames a second**.

## B4. The music

Every music function takes **`t` — seconds from the moment being heard now**:
`t = 0` is what sounds now; **`t > 0` is the future**, up to about **+3.5 s** (already rendered — it will sound
exactly so); `t < 0` is the past, down to about **−6 s**. `t` is continuous: between the 20 ms slices values are
interpolated. Where there is no data (before a track starts, just after a seek, beyond what is rendered) the
functions return **0**; `musicHas(t)` says whether there is data; `uAhead` says how far the future reaches right
now (seconds; shorter just after a start or a seek).

### What the player gives, and how often

| data | resolution | range of `t` |
|---|---|---|
| spectrum, levels, hits, stereo, loudness | a slice every **20 ms** (50 a second), each from a ~40 ms analysis window; **48 bands** 25 Hz – 20 kHz, log-spaced (≈ 1/5 octave) | −6 … +3.5 s |
| waveform | about **22 000 samples a second**, left and right | −0.25 … +0.5 s |
| tempo, beat phase | re-estimated every ~0.5 s from ~9 s of onsets (past and future) | any `t` |
| track, cover colours | when they change | — |

The app fetches the data about 7 times a second, ahead of time, and interpolates it to the exact moment heard for
every frame, accounting for the output device's delay. While a picture is shown the player keeps ~3.8 s rendered
ahead so the future is there.

### Functions

```glsl
float musicSpectrum(float f, float t);   // 0…1: level at log-frequency f (0 = 25 Hz … 1 = 20 kHz)
float musicSpectrumDb(float f, float t); // the same band's real level, dBFS (−96 … 0)
float musicFreqToF(float hz);            // Hz → f
float musicFToFreq(float f);             // f → Hz

float musicBass(float t);        // 0…1: level of 25–250 Hz
float musicMid(float t);         // 0…1: 250 Hz – 4 kHz
float musicTreble(float t);      // 0…1: 4–20 kHz
float musicHit(int part, float t);  // 0…1: a hit (a sudden rise) in part 0 bass, 1 mid, 2 treble —
                                    // jumps to ~1 and dies away over 0.3 / 0.22 / 0.15 s

float musicLoudness(float t);    // 0…1: overall level
float musicLoudnessDb(float t);  // the mix's RMS, dBFS (about −60 … 0)
float musicEnergy(float t);      // 0…1: slow loudness (~1.5 s, centred): build-ups, drops, quiet parts
float musicOnset(float t);       // 0…1: how much new sound starts (spectral flux) — drums, notes
float musicBrightness(float t);  // 0…1 on the f scale: the spectrum's centre of weight (dark … bright)
float musicPeakDb(float t);      // the samples' peak, dBFS

float musicWidth(float t);       // 0 mono … 1 very wide (side against mid)
float musicBalance(float t);     // −1 left … +1 right
float musicCorrelation(float t); // 1 mono, ~0 wide, < 0 out of phase

float musicWave(float t, int ch); // −1…1: the waveform; ch 0 left, 1 right, 2 mid (L+R)/2, 3 side (L−R)/2.
                                  // Only near now (t −0.25 … +0.5 s). An oscilloscope over 40 ms:
                                  //   float y = musicWave(uv.x * 0.04 - 0.02, 2);
                                  // A stereo goniometer: a point at (musicWave(t,3), musicWave(t,2)) for t in 0…0.02

float musicBeatPhase(float t);   // 0 on a beat, rising to 1 just before the next
float musicBeat(float t);        // 1 on each beat, falling to 0 within ~0.15 s (scaled by uBeatConfidence)
float musicHas(float t);         // 1 where there is data at t, else 0
```

`musicSpectrum`, the part levels and `musicLoudness` are **normalized**: each follows its own recent top with a
slow automatic gain (quiet music still moves, loud music does not pin at 1); the spectrum is tilted +4.5 dB/octave
above 500 Hz so the highs are visible. Use the `…Db` functions for real levels.

### The same for now — free, no lookups

```glsl
uniform float uBass, uMid, uTreble;     // musicBass(0.0) …
uniform vec3  uHit;                     // musicHit(0..2, 0.0)
uniform float uLoudness, uEnergy, uOnset, uBrightness, uWidth;
uniform float uBpm;                     // tempo, beats a minute (0 while not found)
uniform float uBeatConfidence;          // 0…1: how sure the tempo is
uniform float uAhead;                   // seconds of future there right now
```

### The player and the track

```glsl
uniform float uLive;        // 1 while a track is playing or paused, 0 when nothing plays: then draw a calm idle state
uniform float uPlaying;     // 1 playing … 0 paused or stopped (glides over ~0.3 s)
uniform float uTrackTime;   // seconds into the track of what is heard (the sound's own clock: uTrackTime + t)
uniform float uTrackLength; // the track's length, seconds (0 if unknown)
uniform float uProgress;    // uTrackTime / uTrackLength, 0…1
uniform float uTrackAge;    // seconds since this track started playing (intros, fades on a new track)
uniform float uSampleRate;  // the output's sample rate, Hz
```

### Colours and the cover

```glsl
uniform vec3  uPalette[4];  // 0…1 RGB: the album cover's colours, most vivid first; the app's without a cover
uniform float uHasCover;    // 1 when the track has a cover
vec3  auraPalette(float x);    // a smooth loop through the cover's colours (x wraps)
vec3  auraAppPalette(float x); // a smooth loop through the app's colours: sky, violet, pink, amber
vec4  auraCover(vec2 uv);      // the cover picture (uv 0…1 over the square image), vec4(0) without one
```

The usual colour setting: `// @param choice colours 0 "Album cover|App|Custom" "Colours" "Where the colours come from"`
with one or two `color` params for Custom, and a small function that picks (see Part E).

### Helpers

```glsl
float auraHash(vec2 p);   // 0…1 random per point
float auraNoise(vec2 p);  // 0…1 smooth value noise
float auraFbm(vec2 p);    // 0…1 fractal noise, 5 octaves
vec3  auraHsv(vec3 hsv);  // HSV (all 0…1) → RGB
mat2  auraRot(float a);   // 2-D rotation
const float PI = 3.14159265, TAU = 6.2831853;
```

Your own functions may use any other names (they must not repeat these).

---

# Part C — technique

## C1. Motion without memory: look back, look ahead

A scene has no memory between frames (unless it uses feedback, C4) — but it has the music's past and future, which
is better: every smoothing, release, hold and anticipation is a look along `t`.

```glsl
// Release: a level that rises at once and falls at `fall` per second — the loudest of the last moments,
// each faded by its age. 8–16 steps are enough; keep the step ≥ 0.02 s.
float released(float f, float fall) {
    float v = 0.0;
    for (int i = 0; i < 12; i++) {
        float t = -float(i) * 0.04;
        v = max(v, musicSpectrum(f, t) * exp(t * fall));
    }
    return v;
}

// Peak hold: the highest level of the last ~1.5 s, held `hold` seconds, then falling at `speed` a second.
float peak(float f, float hold, float speed) {
    float p = 0.0;
    for (int i = 0; i < 16; i++) {
        float t = -float(i) * 0.1;
        p = max(p, musicSpectrum(f, t) - speed * max(0.0, -t - hold));
    }
    return p;
}

// Smooth: the average around now (a gentle attack both ways).
float smooth3(float f) { return (musicSpectrum(f, -0.04) + musicSpectrum(f, 0.0) + musicSpectrum(f, 0.04)) / 3.0; }

// Anticipation: something lights up `lead` seconds before its sound.
float soon = max(0.0, musicSpectrum(f, lead) - musicSpectrum(f, 0.0));
```

Loops over time run for every pixel: keep them short (≤ 16 steps), and if a value is the same for a whole column
or ring, compute it once per pixel from the column's own `f` (not per step of another loop).

## C2. Showing the future — patterns

- **Depth = time.** Something at distance `z` (0 near … 1 far) shows the sound at `t = z * ahead`: it is born far
  away from the future sound and reaches the viewer at `t = 0` — exactly when it is heard.
- **Radius = time.** Rings travel inward or outward; radius `r` shows `t = r * k`; one circle is now.
  An exponential mapping (`t = log(r0 / r) / k`) gives every second the same room.
- **Position = time.** A scrolling landscape or waveform: from the right edge (future) to a "now" line.
- **Mark the arrival**: a flash, a bright line or a focus circle where `t` crosses 0.
- The past (`t < 0`) can leave a fading trail behind the focus.
- Ride with the sound: patterns that should stay with their moment of music use the sound's own clock,
  `uTrackTime + t`, not `uTime`.

## C3. Beat and tempo

`musicBeatPhase(t)` and `musicBeat(t)` place pulses on the tempo's grid, past and future (`uBpm`,
`uBeatConfidence`). Scale beat-driven motion by `uBeatConfidence`, so music without a clear beat does not pulse
at random. `musicHit(0, t)` (kicks) is the event itself; the beat grid is the regular pulse between them.

## C4. Feedback: the previous frame

Add `// @feedback` to the header and read what your scene returned last frame:

```glsl
vec4 auraPrev(vec2 uv);   // your last frame's colour at uv (a half-float buffer: small decays and values > 1 keep)
```

Draw the new things and add a faded, slightly moved/zoomed/rotated `auraPrev` for trails, smoke, echoes
(MilkDrop-style). Lessons: decay colour and alpha **together** (× 0.9 … 0.99); nothing may be lit every frame in
the same place (it piles up into a solid blob); drift hue by rotating about the grey axis, not by mixing channels
(that turns everything grey). Map `p` to the previous frame's `uv` with
`0.5 + p * uResolution.y / (2.0 * uResolution)`.

## C5. Shadertoy code

`void mainImage(out vec4 fragColor, in vec2 fragCoord)` works with `iResolution` (vec3), `iTime` (= `uTime`),
`iTimeDelta`, `iFrame`, `iMouse` (always 0), `iDate`, `iSampleRate`, `iChannelResolution[4]` and:

| channel | what |
|---|---|
| `iChannel0` | the music as Shadertoy gives it: 512 × 2 — row 0 (`y = 0.25`) the spectrum 0 – 11 kHz, linear, 0…1; row 1 (`y = 0.75`) the waveform, 0.5 = silence |
| `iChannel1` | your previous frame (feedback turns on by itself when `iChannel1` is used) |
| `iChannel2` | the album cover |
| `iChannel3` | noise, 256 × 256 RGBA, repeating |

The output of `mainImage` is colour on black: **black is transparent**, the brightest channel is the alpha. All the
`music…` functions work in `mainImage` too. Prefer `scene()` for new work.


## C6. Several passes

A scene can draw in several passes, like Shadertoy's buffers: up to four pictures of its own (**A, B, C, D**) that
the card keeps between frames, then the picture shown. Use them for what one pass cannot do: a simulation that
lives on (ink carried by a flow, sand that settles), a trail that is processed, and the camera's work done on the
whole picture (a blur by depth, a bloom from a small copy of the bright parts).

```glsl
// @name     Ink
// @about    Each kick drops ink at a place that moves round with the beats; a slow turn winds the drops into arcs.
//
// @common
const float TURN = 0.6;                                 // the swirl, radians a second

// @pass A  iChannel0=A:linear
void mainImage(out vec4 o, in vec2 fc) {
    vec2 uv = fc / iResolution.xy, p = (2.0 * fc - iResolution.xy) / iResolution.y;
    float dt = min(uDelta, 0.05);
    vec2 back = vec2(p.y, -p.x) * TURN * dt;            // where this ink was a frame ago
    vec3 last = texture(iChannel0, uv + back * iResolution.y / iResolution.xy * 0.5).rgb;
    float a = floor(uTrackTime * max(uBpm, 60.0) / 60.0) * 2.4;   // a new place on every beat
    vec2 at = 0.45 * vec2(cos(a), sin(a));
    float drop = musicHit(0, 0.0) * exp(-dot(p - at, p - at) * 60.0);
    o = vec4(last * exp(-dt / 5.0) + auraPalette(a * 0.1) * drop * 0.08, 1.0);
}

// @pass image  iChannel0=A:linear
vec4 scene(vec2 uv, vec2 p) {
    vec3 c = 1.0 - exp(-texture(iChannel0, uv).rgb * 2.0);
    return vec4(c, max(c.r, max(c.g, c.b)));
}
```

- `// @common` starts code every pass gets; `// @pass A` … `// @pass D` a buffer, `// @pass image` the picture shown,
  each up to the next such line. Settings, the music functions and the helpers work in every pass.
- On a pass line, what its `iChannel0` … `iChannel3` read: a buffer (`A` … `D`), `noise` (256 × 256, random; green
  and alpha are red and blue moved by 37, 17 texels, as Shadertoy's), `noise64`, `cover`, `music` (C5's sound
  picture) or `none`; then `:nearest`, `:linear` or `:mipmap` (smaller copies, for `textureLod` blurs) and `:clamp`
  or `:repeat`. A buffer read by itself or by an earlier pass gives what it held a frame ago; read by a later
  pass, what it holds now. The passes run A, B, C, D, then image.
- A buffer is written by `mainImage` and keeps its values as they are: half floats, above 1 and below 0 too
  (a depth, a speed, a blur size in alpha). It is the drawing's size — lower on a slow card — and starts empty
  (all 0) on the first frame and after a size change: start the simulation over there (`iFrame == 0`, or a texel
  that is `vec4(0.0)`).
- Steps of a simulation use `uDelta` (`iTimeDelta`) — clamp it (`min(uDelta, 0.05)`); it is 0 while paused.
- The image pass follows the one-pass rules (B1, C5): colour on black, black transparent.
- The frame's cost is the sum of the passes (D2). A pass that reads 64 texels of another at full size is dear;
  a blur reads a smaller copy (`:mipmap`, `textureLod(ch, uv, 3.0)`).
- Shadertoy's shaders with buffers come over this way: Common → `// @common`, Buffer A → `// @pass A` with its
  channels, Image → `// @pass image`; keep the author's credit and licence in the header (`// @license`).

## C7. What makes a scene beautiful

The scenes people keep watching share a handful of habits. None is expensive; together they are the difference
between a graph and a picture.

- **Light adds up, then a soft shoulder.** Sum light freely — glows over glows, above 1 — and bring it to the
  screen at the end with a curve that bends instead of cutting: `col = 1.0 - exp(-col * exposure)` (or a filmic
  curve). Clamping each light to 1 flattens every bright place into the same white.
- **Glow.** A bright thing lights the air around it: add a wide, weak halo to a sharp core
  (`core * exp(-d * d / (r * r)) + halo * r / (d + r)`). With passes: take the parts above a threshold, blur a
  small copy (`:mipmap`, `textureLod(…, 4.0)`), add it back.
- **Depth of field.** Things away from the focus spread into discs that grow with the distance from the focus
  (`size ∝ abs(1.0 - focus / depth)`). A disc keeps its energy — twice as wide, a quarter as bright — and a lens
  draws its rim a little brighter. Near grains become large soft discs, far ones a haze: depth without geometry.
- **Points smaller than a pixel keep their light.** Draw a grain at least a pixel wide and scale its brightness by
  its true area over the pixel's (`(r * r) / (w * w)`); where a pixel holds many grains, draw their average. A
  point that is simply too small flickers on and off as it moves.
- **Glints.** A tiny flat facet flashes only when its face is turned halfway between the light and the eye:
  `pow(max(dot(n, normalize(L + V)), 0.0), 40.0)`. Give every grain its own random facet and let it turn: the
  field twinkles. Let the highs sharpen or quicken it and it twinkles with the cymbals.
- **Air.** Fade with distance toward the horizon's colour (`mix(col, haze, 1.0 - exp(-dist * k))`) and put a
  soft glow band on the horizon. Far things paler and bluer read as far away at once.
- **Colour with a plan.** A dark, slightly cool ground; warm light; the cover's colours in the paint or the
  particles rather than in the light itself; one accent colour for the thing the eye should follow.
- **Motion that breathes.** Let the camera drift slowly (tens of seconds a cycle), never jerk with the beat; the
  music moves what is in the scene. Waves that arrive on the beat (C2, C3) read as music; noise that only shakes
  does not.
- **Make the music visible where the eye is.** A wave that only tilts randomly turned facets changes nothing you
  can see; carry it in light or colour — a band of light on the crest, a tint in the colour of what sent it.
  Many sources at once: each its own colour, and a soft ceiling on their sum (`1.0 - exp(-sum)`).
- **Finish.** A fine grain in the mid tones and half a step of dither (`± 0.5 / 255`) take the banding out of dark
  gradients; faint colour fringes towards the corners (red outward, blue inward) feel like a lens. Keep them
  subtle — they are seasoning.

---

# Part D — rules and the answer

## D1. Rules

- **Loops need a constant bound** (`for (int i = 0; i < 64; i++)`; a `#define` or `const int` is fine; a param's
  maximum is fine: `i < count` with `count` ≤ 128). `while` and `do` are refused. Keep a loop ≤ ~200 steps.
- Nothing outside the shader: no text, no files, no network, no JavaScript. Textures only from the app.
- **Idle**: when `uLive` is 0 there is no music — draw something calm from `uTime` (a slow breath), never black.
- **Paused**: the music functions still answer and `uTime` stops: the picture holds still.
- Keep the picture clean and legible: a dark or transparent ground, bright thin lines or soft glows. The player
  shows its text over the band.

## D2. Budget

- Aim for **≤ 3 ms a frame at 1920 × 1080** on an ordinary video card: per pixel at most a few dozen `music…`
  calls (each `musicSpectrum` reads 4 texels). Keep values in variables; prefer the `u…` "now" uniforms for
  values that are the same for every pixel.
- If frames take too long, the app lowers the resolution (down to a quarter), then stops the scene with a notice.
  `// @quality 0.5` starts a heavy scene at half resolution.
- A scene of several passes (C6) costs the sum of its passes; the heaviest is usually the one that walks through
  space per pixel — keep its loops short, and do everything that is the same over a region (a flow's path, a
  wave's height) once per pixel, not once per step.

## D3. The answer

Answer with **one** ```glsl code block that holds the whole file, starting with the `// @name` line: `@about`,
the sections and settings designed as in A3, then the code. Comments in the code are welcome; no text is needed
outside the block. If you were given a scene to change, keep its settings that still make sense (and their
names), and return the whole file.

---

# Part E — a complete example

```glsl
// @name     Bars
// @author   Aura Engine
// @about    The spectrum as bars with falling peak caps; a bar glows a moment before its sound arrives.
//
// @section  "Bars"     "The bars themselves"
// @param    int    count     48    8    128  1               "Count"         "How many bars across"
// @param    float  gap       0.3   0.0  0.8  0.01            "Gap"           "Space between bars, as a share of a bar"
// @param    float  height    0.6   0.2  1.0  0.01            "Height"        "The tallest a bar gets, as a share of the picture"
// @param    float  curve     1.4   0.5  3.0  0.05  x         "Contrast"      "Higher: only the loud bands stand tall"
// @param    float  rounding  1.0   0.0  1.0  0.05            "Rounded ends"  "0 square … 1 fully round"
// @param    bool   mirror    0                               "Mirror"        "Grow up and down from a middle line"
//
// @section  "Colour"
// @param    choice colours   0     "Album cover|App|Custom"  "Colours"       "Where the colours come from"
// @param    color  low       #22d3ee                         "Custom foot"   "With Custom: the colour at a bar's foot"
// @param    color  high      #f472b6                         "Custom top"    "With Custom: the colour at a bar's top"
// @param    float  glow      0.6   0.0  2.0  0.05            "Glow"          "Soft light around the bars"
//
// @section  "Motion"   "How the bars follow the music"
// @param    float  fall      5.0   1.0  20.0 0.5   x         "Release"       "How quickly a bar drops after its sound"
// @param    float  lead      0.15  0.0  0.5  0.01  s         "Anticipation"  "A bar starts to glow this long before its sound"
//
// @section  "Peaks"    "Caps that hold the loudest moment, then fall"
// @param    bool   peaks     1                               "Show peaks"    "Caps above the bars"
// @param    float  hold      0.3   0.0  1.0  0.05  s         "Hold"          "How long a cap waits before it falls"
// @param    float  drop      0.8   0.2  3.0  0.05  x         "Fall speed"    "Picture heights a second"
//
// @section  "Layout"
// @param    float  floorY    0.16  0.0  0.5  0.01            "Floor"         "Where the bars stand in the player (full screen: near the bottom)"

vec3 barColour(float y, float x) {
    if (colours == 2) return mix(low, high, y);                  // Custom
    if (colours == 1) return auraAppPalette(0.1 + x * 0.45 + y * 0.2);   // App
    return auraPalette(x * 0.45 + y * 0.2);                      // Album cover
}

float sdRoundBox(vec2 q, vec2 b, float r) {
    vec2 d = abs(q) - b + r;
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

vec4 scene(vec2 uv, vec2 p) {
    float n = float(count);
    float col = floor(uv.x * n);
    float f = (col + 0.5) / n;                                    // the band this bar shows
    float px = uResolution.x / n;                                 // a column's width in pixels
    float base = uFull > 0.5 ? 0.06 : floorY;
    float room = (1.0 - base) * uResolution.y * height;           // a full bar's height in pixels

    // The bar's level: rises at once, falls at `fall` a second (a look back); idle: a slow breath.
    float v, pk = 0.0, soon = 0.0;
    if (uLive > 0.5) {
        v = 0.0;
        for (int i = 0; i < 10; i++) {
            float t = -float(i) * 0.04;
            v = max(v, musicSpectrum(f, t) * exp(t * fall));
        }
        if (peaks) {
            for (int i = 0; i < 14; i++) {
                float t = -float(i) * 0.1;
                pk = max(pk, pow(musicSpectrum(f, t), curve) - drop * max(0.0, -t - hold));
            }
        }
        soon = max(0.0, musicSpectrum(f, lead) - musicSpectrum(f, 0.0));
    } else {
        v = 0.18 + 0.12 * sin(uTime * 0.8 + f * 7.0);
    }
    v = pow(clamp(v, 0.0, 1.0), curve);

    // Pixel space: x from the column's centre, y up from the floor (or from the middle line when mirrored).
    vec2 fc = uv * uResolution;
    float cx = (col + 0.5) * px;
    float y0 = mirror ? 0.5 * uResolution.y : base * uResolution.y;
    float y = mirror ? abs(fc.y - y0) : fc.y - y0;
    float h = max(2.0, v * room * (mirror ? 0.5 : 1.0));
    float hw = 0.5 * px * (1.0 - gap);
    float r = hw * rounding;
    float d = sdRoundBox(vec2(fc.x - cx, y - h * 0.5), vec2(hw, h * 0.5), min(r, h * 0.5));

    float body = clamp(0.5 - d, 0.0, 1.0);
    float halo = glow * 0.35 * exp(-max(d, 0.0) / (3.0 + 6.0 * glow)) * (0.4 + v);
    vec3 c = barColour(clamp(y / room, 0.0, 1.0), f);
    // Glow before the sound: the top of the bar brightens as its note approaches.
    c += vec3(soon * 2.0) * smoothstep(h - 12.0, h, y);

    float cap = 0.0;
    if (peaks && uLive > 0.5) {
        float ph = max(pk, v) * room * (mirror ? 0.5 : 1.0) + 4.0;
        cap = clamp(1.5 - abs(y - ph) , 0.0, 1.0) * step(abs(fc.x - cx), hw);
    }
    float a = clamp(body + halo + cap, 0.0, 1.0);
    vec3 rgb = c * (body + halo) + barColour(1.0, f) * cap * 1.2;
    return vec4(rgb / max(a, 1e-3), a);
}
```
