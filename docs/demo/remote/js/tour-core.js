// ══════════════════════════════════════════════════════════════════════
// The newcomer's tour — what it says, in what order, and where the tip
// goes.
//
// A tour is a list of steps. A step points at a part of the window (one
// element or a few), says in a sentence or two what it is for, and asks
// nothing of the listener: no file to drop, no button to press — Next,
// Back, Skip. Only the main tour's last step hands them one button, the
// big player's edge that folds it back (Done does the same). This file
// holds the steps and all their texts, when the tour offers itself, and
// the arithmetic that keeps the tip beside what it points at and inside
// the window. No DOM here: tests/tour.test.mjs runs it in node.
// tour-engine.js draws a tour; tour.js wires the main window's.
// ══════════════════════════════════════════════════════════════════════

/// Where the main window remembers the tour: absent on a fresh profile.
export const TOUR_KEY = 'auraTour';
/// The analyzer's own tour (its ? in the title bar); the windows share the
/// profile.
export const TOUR_KEY_ANALYZER = 'auraTourAnalyzer';
/// The EQ window's own tour. The main tour, started again, clears it: the
/// EQ's tour offers itself again the next time that window opens.
export const TOUR_KEY_EQ = 'auraTourEq';
/// The Cue Consistency window's own tour, cleared the same way.
export const TOUR_KEY_CUE = 'auraTourCue';

// UI texts, all in one place. A step's text is a string, or two — `empty`
// and `rows` — when it reads differently with nothing on the list. A button
// is named by its sign in braces, "the {?} button": the sign is drawn as a
// small key (textParts), never left alone in the line.
export const TOUR_TEXTS = {
    help: {
        aria: 'Tour',
        tip: 'The tour: where everything is, one step at a time',
    },
    offer: {
        title: 'A quick tour?',
        body: 'A short tour shows where everything is. No files needed, just Next.',
        start: 'Take the tour',
        skip: 'Skip',
    },
    again: {
        title: 'The tour',
        body: 'Where everything is, one step at a time. No files needed, just Next.',
        start: 'Start',
        cancel: 'Not now',
    },
    nav: {
        next: 'Next',
        back: 'Back',
        done: 'Done',
        skip: 'Skip tour',
        count: (i, n) => `${i} / ${n}`,
    },
    // In the order of MAIN_STEPS: the main window from the top down — the
    // settings, the rack, the list's head and the list — then the player
    // last, it grows to the whole window, and its edge folds it back.
    main: {
        settings: {
            title: 'The main settings',
            text: 'These decide how a file is rendered, by a conversion and by the player alike (BIT-PERFECT aside). A click on one opens its control; Next shows each of them.',
        },
        fs: {
            title: 'FS multiplier',
            text: 'The output rate: the source\'s base, 44.1k or 48k, times 2, 4, 8 or 16 (FS8: 352.8k or 384k). Choose a rate your DAC takes.',
        },
        taps: {
            title: 'Filter resolution',
            text: 'The filter\'s length, 5k to 30M taps: more taps make it steeper and slower to get ready. Its slider dims a length that isn\'t installed and names the download that has it.',
        },
        gpu: {
            title: 'GPU acceleration',
            text: 'The filter runs on the video card (Vulkan): faster, with the same result. The player takes it when the processor has too little margin; without it, a slow one plays fewer taps.',
        },
        apodizing: {
            title: 'Apodizing',
            text: 'Rolls a 44.1 or 48 kHz source off from 20, 19 or 18 kHz against its converter\'s pre-ringing. While Adaptive Apodizer (on as shipped) picks per file, this says AA.',
        },
        headroom: {
            title: 'Headroom',
            text: 'The output\'s true-peak ceiling: louder peaks come down to it, quieter files are not raised. Lower it for an EQ after the player; Adaptive Headroom skips it where it gains nothing.',
        },
        rack: {
            title: 'Advanced DSP',
            text: 'The stages a file goes through, zone by zone, in the order they run. Out of the box all are on but Hybrid-Phase, Continuous Alpha and Crosstalk Cancel.',
        },
        eq: {
            title: 'Equalizer',
            text: 'Its SET key opens the EQ window: your headphone or room correction, and the speakers\' balance and delay. The first time it opens, it offers a short tour of its own.',
        },
        view: {
            title: 'Files, radio, input',
            text: 'This button turns the list round your files, internet radio and the ASIO input; right-click goes back. Switching doesn\'t stop playback; a pip breathes where something plays.',
        },
        radio: {
            title: 'Radio',
            text: 'Pick from featured stations, favorites and recent ones, search the catalog, or paste a stream\'s address below. Streams play through the rack, just as files do.',
        },
        playlist: {
            title: 'Playlists',
            text: 'Keep several lists: this name switches between them, adds and renames them. Drag a row to change the order; the player follows it.',
        },
        convertAll: {
            title: 'Convert all',
            text: 'Converts every file not converted yet, with the rack as it is now or with its own if the track keeps one. Its arrow picks FLAC or DSF; hold it to cancel a run.',
        },
        drop: {
            title: 'Your list',
            empty: 'Drop audio files here, or click to browse: WAV, FLAC, MP3, OGG, AAC, M4A. Each file becomes a row.',
            rows: 'Drop more audio files anywhere on the list; each one becomes a row.',
        },
        row: {
            title: 'A row',
            empty: 'Every file on the list gets these buttons:',
            rows: 'Its buttons:',
            keys: [
                ['play', 'listen, through the rack as it is set'],
                ['convert', 'convert: write it to a file'],
                ['memory', 'track memory: this track keeps its own rack'],
                ['remove', 'take it off the list'],
            ],
        },
        player: {
            title: 'The player',
            text: 'Last, the player: it plays the list through the settings and the rack above it. Next shows its parts, the lowest row first.',
        },
        badges: {
            title: 'What plays',
            text: 'A badge for each stage of the rack that is on: lit while it changes the sound, dim while it does not. A bar running under them: a stage is switching in or out.',
        },
        output: {
            title: 'Output',
            text: 'Where the sound goes, and how loud. BIT-PERFECT sends the file to the device untouched: no rack, no volume.',
        },
        analyzer: {
            title: 'Analyzer',
            text: 'Measures what you hear, live: this button or Ctrl+Shift+A; for a file, right-click its row. The first time it opens, it offers a short tour of its own.',
        },
        transport: {
            title: 'Transport',
            text: 'Previous, play, stop, next; outside this tour, Space plays and pauses. The last one is repeat: the whole list, one track, or off.',
        },
        menu: {
            title: 'Right-click the player',
            text: 'Picture delay holds the bars and the picture back for a device that plays late: a DAC filter, active speakers.',
        },
        instant: {
            title: 'Instant start',
            text: 'On: the sound starts at once, and the stages switch in as they get ready. Off: every stage is prepared first, so the first sound already has them all.',
        },
        listening: {
            title: 'Listening mode',
            text: 'The settings fold away: the player and the list take the window, and what you hear does not change. The L key or the player\'s top edge switches between the two.',
        },
        picture: {
            title: 'The picture',
            text: 'Behind the title: the cover, or a scene that moves with the music. Its small buttons: full screen on the left, the look and the spectrum bars on the right.',
        },
        studio: {
            title: 'Visualization studio',
            text: 'Here the right-click menu has Visualization, which picks the scene. At the end of its list, Studio… is where you write and tune scenes of your own, or have an AI write them.',
        },
        fold: {
            title: 'Your turn',
            text: 'Press this edge, or L, to fold the player back. That ends the tour; the {?} button brings it back.',
        },
    },
    // The analyzer window's own tour: what each part of it shows.
    analyzer: {
        help: {
            aria: 'Analyzer tour',
            tip: 'What each part of the analyzer shows, one step at a time',
        },
        // The first time the window opens.
        offer: {
            title: 'A quick tour?',
            body: 'What each part of this window shows, one step at a time. Nothing to click, just Next.',
            start: 'Take the tour',
            skip: 'Skip',
        },
        again: {
            title: 'The analyzer',
            body: 'What each part of this window shows, one step at a time. Nothing to click, just Next.',
            start: 'Start',
            cancel: 'Not now',
        },
        chain: {
            title: 'The chain',
            text: 'The stages this track goes through, in order. A thin gold line under it: the whole track is still being measured.',
        },
        metrics: {
            title: 'The numbers',
            text: 'S is the file, B the sound after the source stages, O what comes out of the rack. Whole-track values appear once they are final; click one to copy it.',
        },
        graphs: {
            title: 'The graphs',
            text: 'One graph at a time: click a name to switch. Next shows each of them.',
        },
        spectrum: {
            title: 'Spectrum',
            text: 'Level by frequency for S, B and O: over the whole track, or live at the playhead.',
        },
        loudness: {
            title: 'Loudness',
            text: 'Loudness over time, short-term and momentary, with the integrated level and the range. Changes of the chain are marked on it.',
        },
        spectrogram: {
            title: 'Spectrogram',
            text: 'Frequency over time, the level in colour, over the whole track: S, B or O, or what the chain changed.',
        },
        waveform: {
            title: 'Waveform',
            text: 'The samples over time: the wheel zooms, down to single samples. The strip on top is the whole track.',
        },
        histogram: {
            title: 'Histogram',
            text: 'How much of the track sits at each loudness. A hard-limited master piles up in one narrow peak.',
        },
        stereo: {
            title: 'Stereo',
            text: 'The output\'s stereo picture: the vectorscope, the correlation and the width, live and over the whole track.',
        },
        tools: {
            title: 'Tools',
            text: 'Snapshot freezes the spectrum\'s O curve to compare against; B shows or hides B on the spectrum. Export: a text for a forum, a picture, CSV or TSV.',
        },
        reports: {
            title: 'Reports',
            text: 'The numbers to copy in the formats forums know: foo_truepeak, foo_dr and the two-line lab report. PNG saves this window as a picture.',
        },
        end: {
            title: 'That\'s the analyzer',
            text: 'This button brings the tour back any time.',
        },
    },
    // The EQ window's own tour: the curve and its tools, then the speakers.
    eq: {
        help: {
            aria: 'EQ tour',
            tip: 'How the EQ works, one step at a time',
        },
        offer: {
            title: 'A quick tour?',
            body: 'How the EQ works and how to set up your speakers, one step at a time. Nothing to click, just Next.',
            start: 'Take the tour',
            skip: 'Skip',
        },
        again: {
            title: 'The EQ',
            body: 'How the EQ works and how to set up your speakers, one step at a time. Nothing to click, just Next.',
            start: 'Start',
            cancel: 'Not now',
        },
        graph: {
            title: 'The curve',
            empty: 'Start here: open a headphone or room profile, paste one, or find your headphones in AutoEq. Or start from scratch and draw the curve yourself.',
            rows: 'Click to add a band, drag a point to move it; the wheel over a point sets its width, Ctrl+wheel its gain. Double-click a point to type its values.',
        },
        bands: {
            title: 'Bands',
            text: 'Each band as a chip: click one to pick it, Ctrl or Shift to pick several. Picked bands move together on the graph.',
        },
        tools: {
            title: 'Tools',
            text: 'Range sets the graph\'s scale, Spectrum draws the music under the curve. Beside them: Undo, Redo, and a new band.',
        },
        files: {
            title: 'Import and export',
            text: 'Import takes a profile from AutoEq, Equalizer APO, Peace, REW or squig.link, or drop the file on the window. Export writes the curve for Equalizer APO.',
        },
        presets: {
            title: 'Presets and A/B',
            text: 'Presets keep your curves by name; A and B hold two of them to compare. The switch turns the whole EQ on and off.',
        },
        output: {
            title: 'Level',
            text: 'Output is the level after the EQ. Auto keeps the loudness the same with the EQ on and off; Safe lowers it so it cannot clip.',
        },
        speakers: {
            title: 'Speakers',
            text: 'For speakers placed unevenly in a room: a balance and a delay. They stay when you switch or reset presets, and are not saved in them.',
        },
        balance: {
            title: 'Balance',
            text: 'Makes one side quieter: L/R for left and right, M/S for a wider or narrower sound. Drag it, or type R1.5 or L0.7 in dB.',
        },
        delay: {
            title: 'Delay',
            text: 'Holds the nearer speaker back, up to 30 ms, so both arrive together. Type L2.04 in ms, or a distance such as L70cm.',
        },
        distances: {
            title: 'Distances',
            text: 'Measure from your seat to each speaker: the delay is worked out for you, and the balance too if you ask.',
        },
        keep: {
            title: 'Keep with EQ off',
            text: 'Off, the EQ\'s switch turns the balance and delay off with the bands. On, they keep playing with the EQ off, and the EQ badge in the main window shows SPK.',
        },
        end: {
            title: 'That\'s the EQ',
            text: 'This button brings the tour back any time.',
        },
    },
    // The Cue Consistency window's own tour: the switches, the stage, then
    // the controls under it.
    cue: {
        help: {
            aria: 'Cue Consistency tour',
            tip: 'How the Cue window works, one step at a time',
        },
        offer: {
            title: 'A quick tour?',
            body: 'What the stage shows and what each control does, one step at a time. Nothing to click, just Next.',
            start: 'Take the tour',
            skip: 'Skip',
        },
        again: {
            title: 'Cue Consistency',
            body: 'What the stage shows and what each control does, one step at a time. Nothing to click, just Next.',
            start: 'Start',
            cancel: 'Not now',
        },
        ab: {
            title: 'A and B',
            text: 'A plays the stage, B the dry signal at the same level, bit for bit. The B key switches too.',
        },
        delivery: {
            title: 'Speakers or headphones',
            text: 'On speakers XTC follows the stage, so the cues reach each ear as meant. On headphones the stage plays alone.',
        },
        confidence: {
            title: 'Pan-pot confidence',
            text: 'How much of the track reads as panned sources, the material the stage works on. A live hall recording reads low and is left nearly as it is.',
        },
        stage: {
            title: 'The stage',
            text: 'Each place in the mix is a column of its own sound, as wide as it is loud, coloured by its pitch. The faint strips show where the speakers alone would put it.',
        },
        width: {
            title: 'Stage width',
            text: 'The angle a hard pan is given, from 30 to 90 degrees. Move it and the solid columns walk out from the strips: that is the widening you hear.',
        },
        cues: {
            title: 'The cues',
            text: 'Strength, the size of the head and how much level difference leaves the bass. Every control has an {i} that explains it, and a double-click resets it.',
        },
        shadow: {
            title: 'Head shadow',
            text: 'Where time hands over to level (f1), where the head\'s shadow begins (f2), and how much of that shadow is used.',
        },
        read: {
            title: 'Reading the mix',
            text: 'The pan law the mix is read with. Calibrate takes the places found so far as its sources, Clear forgets them.',
        },
        bands: {
            title: 'Bands',
            text: 'The numbers per band, folded until you open them: time and level differences, in and out.',
        },
        end: {
            title: 'That\'s the Cue window',
            text: 'This button brings the tour back any time.',
        },
    },
};

/// The EQ window's steps: the curve first, the tools round it, the level,
/// then the speakers' box under it, and the ? last. What each one points
/// at is tour-eq.js's.
export const EQ_STEPS = [
    { id: 'graph',     side: ['bottom', 'top'], pad: 2 },
    { id: 'bands',     side: ['top', 'bottom'], pad: 3 },
    { id: 'tools',     side: ['bottom', 'top'], pad: 3 },
    { id: 'files',     side: ['bottom', 'top'], pad: 3 },
    { id: 'presets',   side: ['bottom', 'top'], pad: 3 },
    { id: 'output',    side: ['top', 'bottom'], pad: 3 },
    { id: 'speakers',  side: ['top', 'bottom'], pad: 3 },
    { id: 'balance',   side: ['top', 'bottom'], pad: 3 },
    { id: 'delay',     side: ['top', 'bottom'], pad: 3 },
    { id: 'distances', side: ['top', 'bottom'], pad: 3 },
    { id: 'keep',      side: ['top', 'bottom'], pad: 3 },
    { id: 'end',       side: ['bottom', 'right'], pad: 4 },
];

/// The Cue window's steps: its two switches and the meter on top, the stage,
/// then the controls under it, and the ? last. What each one points at is
/// tour-cue.js's.
export const CUE_STEPS = [
    { id: 'ab',         side: ['bottom', 'top'], pad: 3 },
    { id: 'delivery',   side: ['bottom', 'top'], pad: 3 },
    { id: 'confidence', side: ['bottom', 'top'], pad: 3 },
    { id: 'stage',      side: ['bottom', 'top'], pad: 2 },
    { id: 'width',      side: ['top', 'bottom'], pad: 3 },
    { id: 'cues',       side: ['top', 'bottom'], pad: 3 },
    { id: 'shadow',     side: ['top', 'bottom'], pad: 3 },
    { id: 'read',       side: ['top', 'bottom'], pad: 3 },
    { id: 'bands',      side: ['top', 'bottom'], pad: 3 },
    { id: 'end',        side: ['bottom', 'right'], pad: 4 },
];

/// The analyzer's steps. `tab`: the graph a step shows (the tour picks its
/// tab, and puts back the one that was chosen).
export const ANALYZER_STEPS = [
    { id: 'chain',       side: ['bottom', 'top'], pad: 3 },
    { id: 'metrics',     side: ['right', 'bottom', 'top'], pad: 3 },
    { id: 'graphs',      side: ['bottom', 'top'], pad: 3 },
    { id: 'spectrum',    side: ['left', 'bottom', 'top'], pad: 2, tab: 'anSpectrumSection' },
    { id: 'loudness',    side: ['left', 'bottom', 'top'], pad: 2, tab: 'anLoudnessSection' },
    { id: 'spectrogram', side: ['left', 'bottom', 'top'], pad: 2, tab: 'anSpectrogramSection' },
    { id: 'waveform',    side: ['left', 'bottom', 'top'], pad: 2, tab: 'anWaveformSection' },
    { id: 'histogram',   side: ['left', 'bottom', 'top'], pad: 2, tab: 'anHistogramSection' },
    { id: 'stereo',      side: ['left', 'bottom', 'top'], pad: 2, tab: 'anStereoSection' },
    { id: 'tools',       side: ['bottom', 'top'], pad: 3 },
    { id: 'reports',     side: ['top', 'bottom'], pad: 3 },
    { id: 'end',         side: ['bottom', 'left'], pad: 4 },
];

/// The main window's steps, in order (Anton 1.10): the main window first,
/// zone by zone from the top down — the settings, the rack, the list's head
/// and the list — the eye never going back to a zone it has left; then the
/// player last, one short step up from the list, its lowest row first; then
/// it grows to the whole window (the listening mode) and what is in it is
/// shown; and last its edge, which the listener presses to fold it back.
/// `mode`: the layout the step is shown in (the tour switches to it; Skip
/// puts the listener's back);
/// `side`: where the tip would rather stand; `pad`: the room around what it
/// points at; `rings: 'first'`: only the first thing shown is lit (a menu
/// row, the menu seen round it); `live`: what it points at is the
/// listener's to press (tour-engine.js). What each one points at is
/// tour.js's (the DOM's); the texts are TOUR_TEXTS.main.
export const MAIN_STEPS = [
    // The settings at the top: what every file is rendered with.
    { id: 'settings',   mode: 'studio',    side: ['bottom', 'top'], pad: 2 },
    { id: 'fs',         mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'taps',       mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'gpu',        mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'apodizing',  mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'headroom',   mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    // The rack under them.
    { id: 'rack',       mode: 'studio',    side: ['top', 'bottom'], pad: 4 },
    // Its EQ badge: where the EQ window opens.
    { id: 'eq',         mode: 'studio',    side: ['top', 'bottom'], pad: 4 },
    // Past the player, the list's head and the list: F/R first, the radio it
    // shows in the list's place, then the files' own (`view`: the list's view
    // the step is shown in; the tour switches to it and puts the listener's
    // back).
    { id: 'view',       mode: 'studio',    side: ['bottom', 'top'], pad: 4 },
    { id: 'radio',      mode: 'studio',    side: ['top', 'bottom'], pad: 4, view: 'radio' },
    { id: 'playlist',   mode: 'studio',    side: ['bottom', 'top'], pad: 4, view: 'files' },
    { id: 'convertAll', mode: 'studio',    side: ['bottom', 'top'], pad: 4, view: 'files' },
    { id: 'drop',       mode: 'studio',    side: ['top', 'bottom'], pad: 4, view: 'files' },
    { id: 'row',        mode: 'studio',    side: ['top', 'bottom'], pad: 3, view: 'files' },
    // Last, the player over the list, from its lowest row up.
    { id: 'player',     mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'badges',     mode: 'studio',    side: ['bottom', 'top'], pad: 4 },
    { id: 'output',     mode: 'studio',    side: ['bottom', 'top'], pad: 3 },
    { id: 'analyzer',   mode: 'studio',    side: ['bottom', 'top'], pad: 4 },
    { id: 'transport',  mode: 'studio',    side: ['bottom', 'top'], pad: 4 },
    { id: 'menu',       mode: 'studio',    side: ['bottom', 'top'], pad: 4 },
    { id: 'instant',    mode: 'studio',    side: ['bottom', 'top'], pad: 2, rings: 'first' },
    // It grows to the whole window.
    { id: 'listening',  mode: 'listening', side: ['bottom', 'top'], pad: 3 },
    { id: 'picture',    mode: 'listening', side: ['bottom', 'top'], pad: 4 },
    { id: 'studio',     mode: 'listening', side: ['bottom', 'top'], pad: 2, rings: 'first' },
    { id: 'fold',       mode: 'listening', side: ['bottom', 'top'], pad: 3, live: true },
];

export const STEP_MODES = ['studio', 'listening'];

/// The text a step shows: the plain one, or the one for an empty or a
/// filled list.
export function stepText(t, rows) {
    if (!t) return '';
    if (typeof t.text === 'string') return t.text;
    return rows ? (t.rows ?? t.empty ?? '') : (t.empty ?? t.rows ?? '');
}

/// A text cut into its words and the buttons it names by their sign in
/// braces: "the {?} button" → [{ text: 'the ' }, { key: '?' }, { text: ' button' }].
/// tour-engine.js draws a key as a small square, like the button itself.
export function textParts(s) {
    const out = [];
    String(s ?? '').split(/\{([^{}\s]{1,3})\}/).forEach((p, k) => {
        if (k % 2) out.push({ key: p });
        else if (p) out.push({ text: p });
    });
    return out;
}

// ── when the tour offers itself ───────────────────────────────────────

/// What a start of the window does about its tour. `stored` is what the
/// profile holds under TOUR_KEY, or TOUR_KEY_ANALYZER for the analyzer's
/// window (null when nothing). Everyone the tour has
/// not been offered to gets the offer once: a newcomer, and someone coming
/// from an earlier version alike (Anton 1.10). A profile marked only
/// 'hinted' (an earlier build lit the ? for an upgrader instead) has not
/// been offered it either. Anyone the tour has met is left alone.
export function shouldOffer({ stored }) {
    if (stored == null) return 'offer';
    try { if (JSON.parse(stored)?.state === 'hinted') return 'offer'; } catch (_) { /* not ours */ }
    return null;
}

/// The record kept under TOUR_KEY.
export function tourRecord(state, at = new Date().toISOString()) {
    return JSON.stringify({ v: 1, state, at });
}

// ── the walk through the steps ────────────────────────────────────────

/// The next step to show from `i` in direction `dir` (+1 / -1), passing
/// over the ones `ok` turns down (their element is not in this window).
/// -1 or n: out of the list.
export function nextIndex(n, i, dir, ok = () => true) {
    let j = i + dir;
    while (j >= 0 && j < n && !ok(j)) j += dir;
    return j;
}

/// "3 / 12": the place among the steps that will be shown.
export function stepCount(n, i, ok = () => true) {
    let total = 0, at = 0;
    for (let k = 0; k < n; k++) {
        if (!ok(k)) continue;
        total++;
        if (k <= i) at = total;
    }
    return { at, total };
}

// ── rectangles ────────────────────────────────────────────────────────

export const rect = (x, y, w, h) => ({ x, y, w, h });

export function padRect(r, p) {
    return { x: r.x - p, y: r.y - p, w: r.w + 2 * p, h: r.h + 2 * p };
}

export function unionRect(rs) {
    const list = (rs || []).filter(r => r && r.w > 0 && r.h > 0);
    if (!list.length) return null;
    const x0 = Math.min(...list.map(r => r.x)), y0 = Math.min(...list.map(r => r.y));
    const x1 = Math.max(...list.map(r => r.x + r.w)), y1 = Math.max(...list.map(r => r.y + r.h));
    return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

/// The part of a rectangle inside the window (a hole that runs past the
/// edge is drawn to the edge; the tip is placed against what is seen).
export function clipRect(r, view) {
    const x0 = Math.max(0, r.x), y0 = Math.max(0, r.y);
    const x1 = Math.min(view.w, r.x + r.w), y1 = Math.min(view.h, r.y + r.h);
    return { x: x0, y: y0, w: Math.max(0, x1 - x0), h: Math.max(0, y1 - y0) };
}

export function lerpRect(a, b, t) {
    const l = (p, q) => p + (q - p) * t;
    return { x: l(a.x, b.x), y: l(a.y, b.y), w: l(a.w, b.w), h: l(a.h, b.h) };
}

export const easeOutCubic = t => 1 - Math.pow(1 - Math.min(1, Math.max(0, t)), 3);

// ── the tip beside what it points at ──────────────────────────────────

/// Where the tip goes. `view` {w, h}: the window; `target` {x, y, w, h}:
/// what the step points at (holes and their padding included), or null
/// for a tip in the middle; `tip` {w, h}: its measured size; `prefer`:
/// sides in order of preference. The first side with room wins; with room
/// nowhere the tip lies over the target's lower part, inside the window.
///
/// Returns { x, y, side, ax, ay }: the tip's top-left corner, the side of
/// the target it stands on ('bottom' = under it), and where its arrow
/// points out of it, in the tip's own coordinates (null: no arrow).
/// `opt.aim`: the part of the target the arrow points at (a step that
/// shows several things points at the first); the tip still keeps off the
/// whole target.
export function placeTip(view, target, tip, prefer = ['bottom', 'top'], opt = {}) {
    const m = opt.margin ?? 8;         // from the window's edge
    const gap = opt.gap ?? 12;         // between the target and the tip (the arrow lives here)
    const inset = opt.inset ?? 16;     // the arrow keeps this far from the tip's corners
    const clamp = (v, lo, hi) => (hi < lo ? lo : Math.max(lo, Math.min(hi, v)));
    const W = view.w, H = view.h;
    if (!target) {
        return {
            x: Math.round(clamp((W - tip.w) / 2, m, W - m - tip.w)),
            y: Math.round(clamp((H - tip.h) * 0.42, m, H - m - tip.h)),
            side: 'center', ax: null, ay: null,
        };
    }
    const t = clipRect(target, view);
    // Centred on what the arrow points at, so the arrow is right at it;
    // above, below or beside the whole target.
    const a = opt.aim ? clipRect(opt.aim, view) : t;
    const cx = a.x + a.w / 2, cy = a.y + a.h / 2;
    const hx = () => clamp(cx - tip.w / 2, m, W - m - tip.w);
    const vy = () => clamp(cy - tip.h / 2, m, H - m - tip.h);
    const sides = {
        bottom: () => ({ x: hx(), y: t.y + t.h + gap, fits: t.y + t.h + gap + tip.h <= H - m }),
        top: () => ({ x: hx(), y: t.y - gap - tip.h, fits: t.y - gap - tip.h >= m }),
        right: () => ({ x: t.x + t.w + gap, y: vy(), fits: t.x + t.w + gap + tip.w <= W - m }),
        left: () => ({ x: t.x - gap - tip.w, y: vy(), fits: t.x - gap - tip.w >= m }),
    };
    const order = [...prefer, 'bottom', 'top', 'right', 'left'].filter((s, k, a) => sides[s] && a.indexOf(s) === k);
    for (const side of order) {
        const p = sides[side]();
        if (!p.fits) continue;
        const x = Math.round(p.x), y = Math.round(p.y);
        if (side === 'bottom' || side === 'top') {
            return { x, y, side, ax: Math.round(clamp(cx - x, inset, tip.w - inset)), ay: side === 'bottom' ? 0 : tip.h };
        }
        return { x, y, side, ax: side === 'right' ? 0 : tip.w, ay: Math.round(clamp(cy - y, inset, tip.h - inset)) };
    }
    // No room beside it: over its lower part, in the window.
    return {
        x: Math.round(hx()),
        y: Math.round(clamp(t.y + t.h - tip.h - m, m, H - m - tip.h)),
        side: 'inside', ax: null, ay: null,
    };
}

// ── into the ? ────────────────────────────────────────────────────────

/// The move that takes a box (the tip) into another (the ? button): the
/// centre to the centre, one scale for both sides so it shrinks without
/// squashing. For a transform with its origin in the box's centre.
export function flyTransform(from, to) {
    const dx = (to.x + to.w / 2) - (from.x + from.w / 2);
    const dy = (to.y + to.h / 2) - (from.y + from.h / 2);
    const s = from.w > 0 ? Math.max(0.03, Math.min(1, to.w / from.w)) : 1;
    return { dx, dy, s };
}

/// How long the tip takes to fly into the ?: 400–700 ms, a little longer
/// the farther it goes.
export function flyDuration(from, to) {
    const f = flyTransform(from, to);
    const d = Math.hypot(f.dx, f.dy);
    return Math.round(Math.max(400, Math.min(700, 420 + d * 0.35)));
}
