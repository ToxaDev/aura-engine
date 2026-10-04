// ══════════════════════════════════════════════════════════════════════
// The Convert all button's three faces (dropzone.js updateListHead): while
// a batch runs, its progress, held to cancel; after it, the batch's result;
// otherwise how many files the rack has not converted yet.
//
// The result takes no click (dropzone.js), but it is not a disabled button:
// a disabled one gets no pointer, so its tip — the batch's figures, file by
// file — never showed (Anton 3.10). It says aria-disabled instead.
// ══════════════════════════════════════════════════════════════════════

/// What the button shows: `converting` and `pct` (0–100) of a running
/// batch, `result` ({ text, title }) of a finished one, `left` files not
/// converted with the current rack.
export function convertAllView({ converting, pct = 0, result = null, left = 0 }) {
    if (converting) {
        return { disabled: false, ariaDisabled: false, busy: true, done: false, fill: pct,
            title: 'Hold to cancel the batch', text: pct.toFixed(0) + '%' };
    }
    if (result) {
        return { disabled: false, ariaDisabled: true, busy: false, done: true, fill: 0,
            title: result.title, text: result.text };
    }
    return { disabled: left === 0, ariaDisabled: false, busy: false, done: false, fill: 0,
        title: left > 0
            ? `Convert ${left} file${left === 1 ? '' : 's'} not yet converted with the current rack`
            : 'All files are already converted with the current rack',
        text: left > 0 ? `Convert all · ${left}` : 'Convert all' };
}
