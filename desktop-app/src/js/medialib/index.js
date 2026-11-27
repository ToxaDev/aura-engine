// ══════════════════════════════════════════════════════════════════════
// The media library: the albums of the listener's music folders, with
// their tags and covers (the same library the streamer serves). Shown as
// the list's fourth view, M, in the compact window (listview.js mounts it
// the first time it is shown), and as the landscape window's centre and
// left column (landscape.js).
//
//   mountMediaLibrary(host, { layout })  the browser: 'compact' or 'wide'
//   mountLibrarySide(host)               the sections and the music folders
//   onMediaLibrary(fn)                   the news, so side and centre agree
// ══════════════════════════════════════════════════════════════════════

import { mountMediaLibrary as mountView } from './view.js';
import { onMediaLibrary } from './api.js';
import { setListView } from '../listview.js';

export { mountLibrarySide } from './side.js';
export { onMediaLibrary };

/// Mounts a library browser into `host`. layout 'compact' (the M view in
/// the 480 px window) or 'wide' (the landscape centre). Returns
/// { setSection(name), refresh(), destroy() }; sections: 'albums',
/// 'artists', 'genres', 'folders', 'playlists'; `tools` adds pages of
/// model.js TOOLS as tabs to the compact one (the streamer's remote).
/// Once after the start, when the app has settled: the versions of
/// originals that are gone are deleted (orphans.js). The note is
/// shown where the library can show one (landscape), else kept for the
/// Conversions page.
let swept = false;
function sweepAtStart() {
    if (swept || !window.__TAURI__?.core) return;
    swept = true;
    setTimeout(async () => {
        try {
            const [{ sweepOrphans }, { fileOnAir }, { emit }] = await Promise.all([
                import('./orphans.js'), import('../player.js'), import('./api.js'),
            ]);
            const wide = () => document.body.classList.contains('win-landscape');
            await sweepOrphans({
                invoke: (c, a) => window.__TAURI__.core.invoke(c, a),
                playing: () => fileOnAir(),
                tell: wide() ? text => emit({ type: 'toast', text }) : null,
            });
        } catch (_) {}
    }, 20000);
}

export function mountMediaLibrary(host, { layout = 'compact', tools = [] } = {}) {
    sweepAtStart();
    return mountView(host, { layout: layout === 'wide' ? 'wide' : 'compact', tools });
}

let compact = null;

/// The M view in the list's place: the browser after the list's zone (the
/// zone is hidden while M is shown, medialib.css), its search and grid /
/// list switch in the list's head. Once; later calls return the same.
export function mountCompactLibrary() {
    if (compact) return compact;
    sweepAtStart();
    const zone = document.getElementById('dropZone');
    const head = document.getElementById('plListHead');
    if (!zone || !head) return null;
    const host = document.createElement('div');
    host.id = 'mlView';
    host.className = 'ml-view';
    zone.after(host);
    compact = mountView(host, { layout: 'compact', headHost: head });
    // "Show in Files" from the M view's playlists shows F.
    onMediaLibrary(ev => {
        if (ev.type === 'files' && ev.layout === 'compact') setListView('files');
    });
    return compact;
}
