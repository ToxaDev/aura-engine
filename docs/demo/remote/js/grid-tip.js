// The grid picture of Auto and Auto Shift: where the source samples sit
// among the output samples.
//
// One short stretch of the wave and its output samples (dots), the same in
// both modes, at an upsampling ratio of 4, and the source samples (rings):
//   Auto       on every 4th dot (the odd length);
//   Auto Shift half a dot earlier, between two dots: every output sample is
//              half an output sample later than in Auto (the even length, the
//              stored rungs' grid, as 1G).
// Only the rings move. No text inside the picture.
//
// Two sizes: the read-out of the Resolution slider (gridGraphSvg, no
// background of its own, its edges faded by CSS into the pane) and the
// tooltip of the Auto / Auto S marks (gridTipSvg, with one caption line).

const L = 4;
const OUT_STEP = 20;           // px between two output samples
const SRC_STEP = OUT_STEP * L; // px between two source samples

const f = (v) => (Math.round(v * 10) / 10).toString();

/// The caption under the tooltip picture, by mode ('auto' / 'shift').
export const GRID_CAPTION = {
    auto: 'Source samples on the output points',
    shift: 'Source samples between the output points',
};

/// The read-out picture's tooltip: what the two grids are.
export const GRID_EXPLAIN = {
    auto: 'Auto: each track gets its own filter, as long as that track needs (more taps would change no sample). ' +
        'The source samples (amber rings) stand on the output points: every 4th point is the original sample, ' +
        'the ones between are computed.',
    shift: 'Auto S (shifted grid): the same filter, the output points moved half a step, so the source samples (amber rings) ' +
        'fall between them, the grid the 1G filter uses. No point copies a source sample; all of them are computed.',
};

/// The output samples' x across a picture `w` wide: the same in both modes.
export function gridOutX(w) {
    const xs = [];
    for (let x = 0; x <= w + 1e-9; x += OUT_STEP) xs.push(x);
    return xs;
}

/// The source samples' x for a mode across a picture `w` wide: on a dot
/// (Auto) or half a dot earlier (Auto Shift).
export function gridSrcX(mode, w) {
    const xs = [];
    for (let x = OUT_STEP - (mode === 'shift' ? OUT_STEP / 2 : 0); x <= w + 1e-9; x += SRC_STEP) xs.push(x);
    return xs;
}

/// A smooth stretch of a band-limited wave, `h` tall.
const waveY = (x, h) => h / 2 - h * 0.28 * Math.sin((2 * Math.PI * (x + 30)) / 230) - h * 0.065 * Math.sin((2 * Math.PI * x) / 97);

function picture(mode, w, h, { grid, glow, ring }) {
    let d = '';
    for (let x = 0; x <= w; x += 2) d += `${x === 0 ? 'M' : 'L'}${f(x)} ${f(waveY(x, h))}`;
    const src = gridSrcX(mode, w);
    // The grid: a hairline at every output point, the source samples' lines
    // dashed and stronger, the wave's middle.
    const lines = grid
        ? gridOutX(w).map((x) => `<line x1="${f(x)}" y1="0" x2="${f(x)}" y2="${h}" stroke="#a78bfa" stroke-opacity="0.16" stroke-width="1"/>`).join('')
            + src.map((x) => `<line x1="${f(x)}" y1="0" x2="${f(x)}" y2="${h}" stroke="#fbbf24" stroke-opacity="0.32" stroke-width="1" stroke-dasharray="2 2"/>`).join('')
            + `<line x1="0" y1="${f(h / 2)}" x2="${w}" y2="${f(h / 2)}" stroke="#94a3b8" stroke-opacity="0.12" stroke-width="1"/>`
        : src.map((x) => `<line x1="${f(x)}" y1="${f(waveY(x, h) + ring + 1)}" x2="${f(x)}" y2="${h - 2}" stroke="#fbbf24" stroke-opacity="0.35" stroke-width="1"/>`).join('');
    const rings = src.map((x) =>
        `<circle cx="${f(x)}" cy="${f(waveY(x, h))}" r="${ring}" fill="none" stroke="#fbbf24" stroke-width="1.4"/>`).join('');
    const dots = gridOutX(w).map((x) => `<circle cx="${f(x)}" cy="${f(waveY(x, h))}" r="2.2" fill="#c4b5fd"/>`).join('');
    const body = `<path d="${d}" fill="none" stroke="#a78bfa" stroke-opacity="0.55" stroke-width="1.5"/>` + rings + dots;
    return lines + (glow
        ? `<defs><filter id="${glow}" x="-20%" y="-50%" width="140%" height="200%"><feGaussianBlur stdDeviation="1.6" result="b"/>`
            + `<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter></defs><g filter="url(#${glow})">${body}</g>`
        : body);
}

/// The read-out picture (200 x 46): grid, wave, dots and rings, glowing, no
/// background.
export function gridGraphSvg(mode) {
    const m = mode === 'shift' ? 'shift' : 'auto';
    return `<svg class="tap-graph-pic" width="200" height="46" viewBox="0 0 200 46" aria-hidden="true">`
        + picture(m, 200, 46, { grid: true, glow: `tapGraphGlow-${m}`, ring: 5 }) + '</svg>';
}

/// The tooltip picture (160 x 60).
export function gridTipSvg(mode) {
    const m = mode === 'shift' ? 'shift' : 'auto';
    return `<svg class="grid-tip-pic" width="160" height="60" viewBox="0 0 160 60" aria-hidden="true">`
        + picture(m, 160, 60, { grid: false, glow: null, ring: 5.5 }) + '</svg>';
}

/// The tooltip's HTML for a mode: the picture and its caption.
export function gridTipHtml(mode) {
    const m = mode === 'shift' ? 'shift' : 'auto';
    return `<div class="grid-tip">${gridTipSvg(m)}<div class="grid-tip-cap">${GRID_CAPTION[m]}</div></div>`;
}
