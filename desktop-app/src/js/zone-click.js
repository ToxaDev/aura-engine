// ══════════════════════════════════════════════════════════════════════
// A click on the drop zone opens the file picker — but not a click that is
// the list's rows', the cancel button's, the playlists' or the radio's
// (Anton 4.10: any station in Featured opened "Select Audio Files").
//
// What the click went through is read from its own path, fixed when it was
// sent, not from the element it was sent to: a handler on its way may take
// that element out of the page first — the radio redraws its list from a
// station's click (and from More), before the click reaches the zone — and
// from an element out of the page closest() finds no ancestor at all.
// ══════════════════════════════════════════════════════════════════════

/// What in the zone has clicks of its own: no file picker there.
export const ZONE_OWN = '.file-item, .dz-cancel-btn, #plPlaylistCtrl, #rdView';

/// Whether the event went through an element matching `sel` (a selector
/// list too), by the event's path; without one (an engine that has none),
/// by the element it was sent to and its ancestors.
export function cameThrough(e, sel) {
    const path = typeof e?.composedPath === 'function' ? e.composedPath() : [];
    if (path.length) return path.some(n => typeof n?.matches === 'function' && n.matches(sel));
    return !!e?.target?.closest?.(sel);
}

/// Whether a click on the zone is one to open the file picker with
/// (`suppressed`: a drop or a drag has just ended there).
export function zoneClickBrowses(e, suppressed = false) {
    return !suppressed && !cameThrough(e, ZONE_OWN);
}
