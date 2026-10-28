
// ══════════════════════════════════════════════════════════════════════
// The note under a scene with instruments while they are being found
// (Anton 3.10): until the song's instruments are there for what is heard,
// such a scene draws the stand-in taken from the mix (AURA-VIS-SPEC-
// INSTRUMENTS.md F5), and a small turning ring with a line or two says so,
// low in the middle of the picture (waves.js places it; listening.css).
// It goes, fading, as the instruments come — for the place being heard, not
// at the end of the work: they come before it ends, and a place the work
// has not reached yet has none.
// ══════════════════════════════════════════════════════════════════════

export const NOTE_TEXT = 'Separating instruments…';
export const NOTE_SUB = 'Showing the mix for now';

/// What the note says now, or null for none.
///   scene    — the scene drawn takes the instruments (`// @spatial`, `// @spec 2`);
///   state    — the player's ('playing', 'paused', …);
///   progress — the track's analysis 0…1 (music.js spatialProgress; −1: no pack);
///   busy     — the instruments are on their way (span's head: the work that
///              brings them is under way or about to start);
///   source   — where the heard objects come from (0 nothing yet, 1 the mix, 2 the instruments);
///   count    — the instruments the scene has (auraObjectCount: 0 before the map);
///   stream   — a live stream: no share of a whole to tell.
export function instrumentsNote({ scene, state, progress, busy, source, count, stream }) {
    if (!scene || (state !== 'playing' && state !== 'paused')) return null;
    if (!(progress >= 0) || !busy) return null;
    if (source === 2 && count > 0) return null;
    const pct = stream ? '' : ` ${Math.min(99, Math.max(0, Math.floor(progress * 100)))}%`;
    return { text: NOTE_TEXT + pct, sub: source === 1 ? NOTE_SUB : '' };
}

/// Where the note stands over what lies under it (px, screen coordinates
/// growing downwards): `floor` — the top of what it must stay clear of under
/// it (the seek bar; the transport on the whole screen), `ceiling` — the foot
/// of the text over it (null: none), `twoH` / `oneH` — its height as two lines
/// and as one. Its foot is `lift` over the floor — level with the row of small
/// switches that hangs 6 px over the seek bar at the player's two ends — on
/// two lines where there is room, else on one (a low big player with a title
/// on two lines and a long device name leaves 24 px between them).
export function notePlace({ floor, ceiling, twoH, oneH, lift = 6 }) {
    const room = ceiling == null ? Infinity : floor - ceiling;
    if (room >= twoH + lift + 3) return { lift, one: false };
    if (room >= oneH + lift + 2) return { lift, one: true };
    return { lift: Math.max(1, Math.floor(room - oneH - 1)), one: true };
}
