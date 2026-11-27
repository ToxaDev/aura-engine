// ══════════════════════════════════════════════════════════════════════
// The landscape library's Analyzer page: the live analyzer (what is heard
// now — the source, the rack, the output) in the page itself. It is the
// analyzer's own page in a frame (analytics.html?mode=live&embedded): no
// window's head or buttons there (js/vis/embed-shim.js marks it), and it is
// told to end its measuring before the frame goes (`__auraAnLeave`). A
// file's own analysis still opens in a window, from the queue's row menu.
// ══════════════════════════════════════════════════════════════════════

export function analyzerPage() {
    const node = document.createElement('div');
    node.className = 'ml-an';
    const frame = document.createElement('iframe');
    frame.className = 'ml-an-frame';
    frame.title = 'Analyzer';
    frame.src = 'analytics.html?mode=live&embedded';
    node.append(frame);
    node.__leave = () => {
        try { frame.contentWindow?.__auraAnLeave?.(); } catch (_) {}
        try { frame.contentWindow?.__auraOff?.(); } catch (_) {}
        frame.remove();
    };
    return node;
}
