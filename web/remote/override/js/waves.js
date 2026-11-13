// The streamer has no visualization scenes and no studio: the big player keeps
// the spectrum bars only. Stands in for the app's waves.js (same exports): the
// look switch does nothing, the right-click menu has no Visualization row.

export const wavesMenu = () => null;

export function initWaves({ wrap }) {
    if (wrap) wrap.dataset.look = 'none';
}
