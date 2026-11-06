# Aura Engine visualization scenes — the instruments (Spec 2.0)

This part comes after the book (Spec 1) and adds to it: everything there holds here too. A scene written for it
says so in its header — `// @spec 2` (the older `// @spatial` means the same):

```glsl
// @name     Sand Stage
// @spec     2
// @about    …
```

With it the scene also gets **the song's instruments**. The first time a person switches a scene to "With
instruments" in the studio, the player downloads the instruments pack once (networks that take a song apart).
From then on every song is analysed once, in the background, when it is first played with such a scene on screen
(on a graphics card in about twenty seconds, on a processor in a minute or more), and the result — **the track's
map** — is kept on disk, so the song never waits again. Until the map is there, and on a computer without the
pack, the scene still gets objects taken from the mix alone (F5), so it is never empty.

---

# Part F — the instruments

## F1. The track's instruments

The map is the song's list of instruments, **the same for the whole song**: slot `i` is one instrument from the
first second to the last. There are `auraObjectCount()` of them in slots `0 … n−1` (at most 32), always in this
order:

1. **the kit, piece by piece**: `AURA_KICK`, `AURA_SNARE`, `AURA_TOMS`, `AURA_HATS`, `AURA_CYMBAL` (the ride),
   `AURA_CYMBAL` (the crash) — the pieces the song has;
2. `AURA_BASS`;
3. the voices, `AURA_VOICE` (the lead and, where one stands apart, a second voice);
4. `AURA_GUITAR`, then `AURA_PIANO`, then `AURA_OTHER` (synths, strings, pads, brass, organ …);
5. `AURA_AMBIENCE` last: not an instrument but the room — the reverb and the diffuse sound of the whole mix.

Within a family the instruments come left to right. A family may have several: two guitars, left and right, are
two slots; so are a lead synth and a pad.

**On a live stream** (the radio) the song is not there ahead of time: its instruments are found as it plays, on a
graphics card, a few seconds after a station starts (until then, and wherever they could not be found in time,
the objects come from the mix, F5). The map grows with the song: `auraObjectCount()` rises as instruments are
found. Once a scene has been given the map its slots only grow — an instrument found later (a second guitar coming
in) takes the next slot, at the end of the row after the room, and keeps it to the song's end; one that stops
keeps its slot, silent. An instrument's `pos.x` and `width` settle over its first ten seconds, then stay. A new
song — where the stream says one begins, else after two seconds of silence, and at the latest after ten minutes —
starts a new map with its slots afresh. The kit comes piece by piece, as on a file, where the graphics card has
room for the drum network beside the separation (the toms and the cymbals become instruments of their own once
they play); else as three pieces taken from the drums' sound (kick, snare, hi-hat). The bass and the voices have
their notes (F4) and flash on their strong notes, as on a file, but their notes are there only about two seconds
ahead of the sound, not the 3.5 s of a file. So do the guitars, the keys and the others: until their source has
played some forty strong notes in the song, its notes go to its places by pan, as the bass's do; then the source
is split into its instruments by their notes, as on a file, and each place is taken over by the instrument nearest
to it — the slot keeps its position, and its colour turns, once, to the instrument's (lighter for a higher
register). A place no instrument takes goes silent and keeps its slot; a later instrument of the same source
standing near it takes that slot, else the next one at the end of the row.

```glsl
struct AuraObject {
    int   kind;       // what it is: AURA_KICK … AURA_AMBIENCE (and AURA_MIX before the map, F5)
    bool  main;       // true for every instrument of the map
    float presence;   // 0…1 it is playing: fades in over 0.06 s, out over 0.3 s, and only after a second of silence
    float energy;     // 0…1 how loud now, over 45 dB up to the SONG's loud level: a quiet instrument stays low
    vec3  pos;        // x: −1 hard left … +1 hard right — FIXED for the song (nothing moves in a mix)
                      // y: 0 low … 1 high — its pitch now (40 Hz → 0, 12 kHz → 1): the note an instrument plays
                      //    (the last one held between notes); fixed for each kit piece (the kick low, the hi-hat high)
                      // z: 0 near … 1 far — dry and loud is near, reverberant and quiet is far; slow (≥ 1 s)
    float width;      // 0…1 how spread it is between the speakers — fixed for the song
    float onset;      // 0…1 jumps to 1 on each of its hits (the kit), its strong notes or, where it plays no note,
                      //    the attacks of its sound; dies away in ~0.12 s
    float coherence;  // 0…1 how alike its two channels are (1 a dry point, 0 a wide wash)
    float age;        // seconds since it last came in (its presence rose from 0): a first moment to mark
    vec3  origin;     // = pos for the map's instruments
};
```

- **Colour.** Each instrument has its own colour, `auraObjectColour(i)`, fixed for the song and made from its
  sound: the hue from its family (the kit steel blue to white — the kick darkest, the cymbals lightest; the bass
  violet; voices pink; guitars orange to yellow; keys blue to cyan; the others teal to green; the room grey-blue),
  lighter for a higher register, more vivid for a brighter sound. Two guitars get two neighbouring colours. The
  colours are true to the sound, not bright: a scene that glows lifts them itself (F6).
- **Notes.** Every instrument that plays notes (the bass, voices, guitars, keys, others) has them all, with their
  pitch, start, end and strength: F4.
- **The kit on a slower computer.** Where the drum network cannot run (no graphics card), the kit comes as three
  pieces taken from the drums' sound — kick, snare, hi-hat — in the same slots and order.

## F2. Functions

| Function | Returns | Cost |
|---|---|---|
| `int auraObjectCount()` | the song's instruments, slots `0 … n−1`; **0 until the map is there** | free |
| `vec3 auraObjectColour(int i)` | instrument `i`'s colour (RGB 0…1), the same all song | free |
| `float auraObjectPresence(int i)` | its presence now — skip silent or empty slots with it | 1 read |
| `AuraObject auraObjectNow(int i)` | instrument `i` as heard now | 4 reads |
| `AuraObject auraObject(int i, float t)` | at `t` seconds from now, −6 … +3.5, smoothly between frames | 10 reads |
| `float auraObjectEnergy(int i, float t)` | its `energy` at `t`, the nearest frame | 2 reads |
| `float auraObjectOnset(int i, float t)` | its `onset` at `t`, the nearest frame | 2 reads |
| `float auraObjectNotes(int i, float t)` | how many notes it plays at `t` (0 for the kit) | 2 reads |
| `float auraNote(int i, float key, float t)` | 0…1: how strongly it plays MIDI note `key` at `t`; 0 when silent | 2 reads |
| `float auraNoteOnset(int i, float key, float t)` | 0…1: a flash as that note starts, dying in ~0.12 s | 2 reads |
| `int auraFind(int kind)` | the slot of that kind playing loudest now, −1 if none | 32 reads |
| `float auraObjectsHave(float t)` | 1 where the objects are known at `t`, else 0 | 1 read |
| `uSpatial` | where the objects come from: 0 nothing yet, 1 the mix alone (F5), 2 the song's instruments (the map) | — |
| `uSpatialProgress` | the heard song's analysis, 0…1 (1: the map is there); −1: the pack is not installed | — |
| `AURA_OBJECTS` = 32, `AURA_KEY_LOW` = 21, `AURA_KEY_HIGH` = 108 | slots; the notes' keys (a piano's range) | — |

In a scene of several passes (Spec 1, C6) all of this works in every pass.

**Which to take.** `auraObject(i, t)` blends two frames of every value — use it for one instrument's trail or its
future, a few calls per instrument. When **every pixel looks at every instrument** (rings from each, a light for
each) use `auraObjectNow(i)` once for its place and kind, and `auraObjectEnergy` / `auraObjectOnset` at the times
you need: 13 instruments × `auraObject` would be 130 reads for one moment — far over the budget (D2) when a pixel needs
several moments; with the cheap ones it is ~26. Loop `for (int i = 0; i < AURA_OBJECTS; i++) { if (i >= n) break; … }` with
`int n = auraObjectCount();`.

## F3. Placing them

`pos.x` is where an instrument sounds between the speakers — but many mixes keep nearly everything in the middle:
the kit pieces all within ±0.1, often the bass, the voice and a pad too. Drawn by `x` alone they pile up in one
spot. Lay the stage out by **family** and use `x` as a nudge:

```glsl
// A stage seen from the front: the voice in front, the kit at the back as a drum set.
bool isKit(int k) { return k == AURA_KICK || k == AURA_SNARE || k == AURA_TOMS || k == AURA_HATS || k == AURA_CYMBAL; }
float rowOf(int k) {                         // depth, front (−) to back (+)
    if (k == AURA_VOICE) return -0.9;
    if (k == AURA_GUITAR || k == AURA_PIANO) return -0.25;
    if (k == AURA_OTHER) return 0.2;
    if (k == AURA_BASS) return 0.75;
    return 1.35;                             // the kit
}
vec2 kitSpot(int k, int nth) {               // the kick in the middle, snare and hi-hat left, toms and ride right
    if (k == AURA_KICK) return vec2(0.0, 0.0);
    if (k == AURA_SNARE) return vec2(-0.5, -0.2);
    if (k == AURA_HATS) return vec2(-0.95, -0.05);
    if (k == AURA_TOMS) return vec2(0.5, -0.15);
    return nth == 0 ? vec2(0.95, 0.15) : vec2(-0.7, 0.4);   // the ride, then the crash
}
// …in the loop: widen x (±0.3 should read as "left"), then set a family's members side by side
float x = sign(o.pos.x) * pow(abs(o.pos.x), 0.6) * 1.6 + (float(nth) - 0.5 * float(count - 1)) * 1.1;
vec2 at = isKit(o.kind) ? vec2(0.0, 1.35) + kitSpot(o.kind, nthCymbal) * 0.9 : vec2(x, rowOf(o.kind));
```

(`nth` and `count`: the instrument's number within its family and the family's size — count them in a first
loop over the slots; the families come in order, F1.) Depth `pos.z` and width `width` are what the mix
suggests, not a measurement: use them gently (a light a little further back, a wide sound a wider light).
`AURA_AMBIENCE` has `x = 0`, `width = 1`: draw it around everything — a haze, a dome, dust.

## F4. Notes

The notes are a roll per instrument: at any `t` (−6 … +3.5 s) and any key (MIDI 21 … 108, a piano's range: 60 is
middle C, 69 the A of 440 Hz), `auraNote(i, key, t)` is the note's strength (0 when it does not sound) and
`auraNoteOnset(i, key, t)` a flash as it starts. The strength is on the instrument's own scale: its loudest notes
are near 1 however quiet the instrument is in the mix.

```glsl
// A piano roll for instrument i: keys up, time across — the future on the right, now in the middle.
float key = mix(float(AURA_KEY_LOW), float(AURA_KEY_HIGH), uv.y);
float t = (uv.x - 0.5) * 6.0;
float v = auraNote(i, key, t);                 // rounded to the nearest key
float f = auraNoteOnset(i, key, t);
col += auraObjectColour(i) * v + vec3(1.0) * f;
```

- A key is a whole note: `auraNote` rounds `key`. For a note's height use the key itself
  (`(key − 21.0) / 87.0`, 0 … 1) — or `pos.y` of the instrument for the lead note now.
- Looking up every key of every instrument in every pixel is too much (88 × 13 × 2 reads). Draw notes where they
  are: loop over the keys near the pixel's own key (a roll, a stave, a star field by pitch), or over a few keys an
  instrument plays (`auraObjectNotes(i, t)` says how many; 0 means skip it).
- The future of the notes is there: a note can light up before it sounds (`auraNote(i, key, 0.5)`: the note half a
  second ahead), and a flash at `t = 0` lands exactly as it is heard.
- The kit has no notes: its pieces are hits (`onset`), with a fixed height each.

## F5. Before the map, and without the pack

Until the song's map is there (`auraObjectCount() == 0`, `uSpatial == 1`) — the first seconds of a new song, or
always on a computer without the pack — the objects come from the mix alone: **`AURA_BASS`**, **`AURA_HATS`**
(the highs), **`AURA_AMBIENCE`** and one or more **`AURA_MIX`** parts — places in the mix where something sounds,
of no known instrument; a main one lives all song, an extra one (`main` false) comes where a sound of its own
stands apart (a backing voice, a second guitar) and goes when it stops (`age`: seconds since it came; `origin`:
the main part's place when it came). They sit in any slots: loop over all `AURA_OBJECTS` and skip empty ones with
`auraObjectPresence(i)`. No colours, no notes.

A scene must look good then too. Two good ways: draw the Spec 1 picture from the music functions (the spectrum,
the hits — Part B) and let the instruments take over when `auraObjectCount()` turns positive; or draw these few
objects plainly — they are places in the mix, not instruments. When `uSpatial` is 0 or `uLive` is 0 show the
empty picture breathing gently (D1).

## F6. Drawing instruments well

- **Presence** fades an instrument in and out — never keep its light on when it is 0. **Energy** is its size or
  brightness, **onset** a flash, a ring, a spark; **notes** the finer detail (a star per note, a ripple at its
  pitch).
- **Lift the colours** for light: keep the hue, raise the brightest channel to 1
  (`c /= max(max(c.r, c.g), c.b)`), then mix toward grey by a Saturation setting. Offer "Per instrument | Album
  cover | App" as the colour choice (`auraPalette(float(i) / float(n))` spreads the cover's colours over them).
- **Many at once.** Thirteen instruments that flash together make a white blob. Keep each one's share modest, let
  hits of the hi-hat and ride (hundreds a song) be small and quick, the kick big and slow, and give the sum a soft
  ceiling (`1.0 - exp(-x)`) rather than adding without end.
- **Settings** (A3) for an instruments scene: a section for the instruments (what each one draws, how strongly;
  switches per family — voice, drums, bass, guitars, keys, others — when the picture is busy), one for their
  layout (width, depth), one for the notes if they are drawn, then Colour, Motion and the rest as in Spec 1.

## F7. A complete example

The song's instruments as lights on a dark stage, laid out as F3 says, each in its colour and as bright as it
plays; every hit and strong note sends a ring out from it (F2's cheap onset, looked up at the ring's own time);
before the map, one light breathes with the mix's bass.

```glsl
// @name     Stage Lights
// @about    The song's instruments as lights on a dark stage — the kit at the back, the voice in front — each in its colour, as bright as it plays; every hit and strong note sends a ring out from it.
// @spec     2
//
// @section  "Lights"   "One for each instrument"
// @param    float  size    1.0  0.4  2.0  0.01  x   "Size"    "How big the lights are"
// @param    float  rings   1.0  0.0  2.0  0.01  x   "Rings"   "How bright the rings its hits and notes send out"
// @param    float  reach   1.0  0.3  2.0  0.01  x   "Reach"   "How far a ring runs before it fades"
//
// @section  "Colour"
// @param    choice colours 0    "Per instrument|Album cover|App" "Colours" "A colour for each instrument, or the cover's or the app's"

bool isKit(int k) { return k == AURA_KICK || k == AURA_SNARE || k == AURA_TOMS || k == AURA_HATS || k == AURA_CYMBAL; }
float rowOf(int k) {                                   // depth on the screen: the kit at the back (up), the voice in front
    if (k == AURA_VOICE) return -0.55;
    if (k == AURA_GUITAR || k == AURA_PIANO) return -0.3;
    if (k == AURA_OTHER || k == AURA_MIX) return -0.08;
    if (k == AURA_BASS) return 0.15;
    return 0.4;
}
vec2 kitSpot(int k) {
    if (k == AURA_SNARE) return vec2(-0.3, -0.06);
    if (k == AURA_HATS) return vec2(-0.6, 0.0);
    if (k == AURA_TOMS) return vec2(0.3, -0.04);
    if (k == AURA_CYMBAL) return vec2(0.6, 0.1);
    return vec2(0.0);                                  // the kick in the middle
}
vec3 colourOf(int i, int n) {
    vec3 c = colours == 1 ? auraPalette(float(i) / float(n)) : colours == 2 ? auraAppPalette(float(i) / float(n))
                                                                            : auraObjectColour(i);
    return c / max(max(c.r, max(c.g, c.b)), 0.05);     // as bright as the hue allows: it is light
}

vec4 scene(vec2 uv, vec2 p) {
    int n = auraObjectCount();
    if (uLive < 0.5 || n == 0) {                        // before the map: the mix's bass as one breathing light
        float e = uLive > 0.5 ? uBass : 0.2 + 0.1 * sin(uTime);
        float d = length(p - vec2(0.0, uFull > 0.5 ? -0.1 : -0.5));
        vec3 c = vec3(0.55, 0.35, 1.0) * (0.1 + 0.9 * e) * 0.03 / (d * d + 0.03);
        return vec4(c, clamp(max(c.r, max(c.g, c.b)), 0.0, 1.0));
    }
    float lift = uFull > 0.5 ? 0.0 : -0.35;             // the player's band keeps its text up: the stage stands lower
    vec3 col = vec3(0.0);
    float ringSum = 0.0;
    int seen[6] = int[6](0, 0, 0, 0, 0, 0);             // how many of each row came before, to set them side by side
    for (int i = 0; i < AURA_OBJECTS; i++) {
        if (i >= n) break;
        AuraObject o = auraObjectNow(i);                // 4 reads: its kind, place, presence, energy
        if (o.kind == AURA_AMBIENCE) continue;
        int row = isKit(o.kind) ? 0 : o.kind == AURA_BASS ? 1 : o.kind == AURA_OTHER ? 2 : o.kind == AURA_VOICE ? 4 : 3;
        int nth = seen[row]++;
        vec2 at = isKit(o.kind) ? vec2(0.0, 0.4) + kitSpot(o.kind)
                : vec2(sign(o.pos.x) * pow(abs(o.pos.x), 0.6) * 1.5 + (float(nth) - 0.5) * 0.6, rowOf(o.kind));
        at.y = at.y * (uFull > 0.5 ? 1.0 : 0.6) + lift;
        vec3 c = colourOf(i, n);
        float d = length(p - at);
        float r = 0.03 * size * (1.0 + o.energy);
        col += c * o.presence * (0.1 + o.energy) * r * r / (d * d + r * r);        // the light and its glow
        // its rings: at distance d the ring shows its onset d / 0.8 seconds ago (2 reads)
        float on = auraObjectOnset(i, -d / 0.8);
        float ring = on * on * exp(-d / (0.35 * reach)) * smoothstep(0.0, 0.02, d);
        col += c * ring * rings * 0.5;
        ringSum += ring;
    }
    col = 1.0 - exp(-col * 1.5);                        // many lights at once: a soft ceiling, not white
    return vec4(col, clamp(max(col.r, max(col.g, col.b)), 0.0, 1.0));
}
```
