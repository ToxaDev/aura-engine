// Tooltip for the player's taps chip: the filter size and how the player
// upsamples with it — one polyphase convolution, or (status `upsampler`, the
// short chain) the filter at the source rate and a short exact interpolator.

/// The taps chip's tooltip for the audible chain `aud` (status.audible).
export function buildTapsTip(aud) {
    const parts = [];
    if (aud.taps) parts.push(`Filter size: ${aud.taps} taps.`);
    if (aud.upsampler) {
        parts.push(`Multistage: the filter at the source rate + short exact interpolator (${aud.upsampler}); matches single-stage to −204…−213 dBFS.`);
    } else {
        parts.push('Ratio: \xd78 (the player always renders through the polyphase path).');
    }
    return parts.join(' ');
}
