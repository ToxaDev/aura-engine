
export function truncateMiddle(str, maxLen = 58) {
    if (!str || str.length <= maxLen) return str;
    const keep = Math.floor((maxLen - 1) / 2);
    return str.slice(0, keep) + '\u2026' + str.slice(str.length - keep);
}

export function formatTaps(value) {
    const v = parseInt(value);
    if (v >= 1000000) return (v / 1000000).toFixed(1) + 'M';
    return (v / 1000).toString() + 'K';
}

export function formatRate(hz) {
    const v = parseInt(hz);
    if (v === 0) return 'Auto';
    if (v >= 1000) return (v / 1000) + 'kHz';
    return v + 'Hz';
}
