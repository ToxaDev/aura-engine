// The load badge beside the version: this app's share of the processor and of
// the video card over the last second, and the memory it holds now, as perf.rs
// measures them each second.
// Faint like the version beside it; a click opens the load window (perf.html).

import { PERF_TIPS, loadText, ramText } from './perf-core.js';

const T = () => window.__TAURI__;

export function initPerfBadge() {
    const host = document.getElementById('appVersion');
    if (!host || document.getElementById('perfBadge')) return;
    const b = document.createElement('button');
    b.type = 'button';
    b.id = 'perfBadge';
    b.className = 'perf-badge';
    b.setAttribute('aria-label', "This app's load: open the load graphs");
    b.innerHTML =
        `<span class="pb-part" data-tip="${PERF_TIPS.cpu}"><span class="pb-k">CPU</span><span class="pb-v" id="perfCpu">–</span></span>` +
        '<span class="pb-sep" aria-hidden="true"></span>' +
        `<span class="pb-part" data-tip="${PERF_TIPS.gpu}"><span class="pb-k">GPU</span><span class="pb-v" id="perfGpu">–</span></span>` +
        '<span class="pb-sep" aria-hidden="true"></span>' +
        `<span class="pb-part" data-tip="${PERF_TIPS.ram}"><span class="pb-k">RAM</span><span class="pb-v" id="perfRam">–</span></span>`;
    // Right after the number, before the update badge (update.js appends it).
    const text = host.firstChild && host.firstChild.nodeType === 3 ? host.firstChild : null;
    host.insertBefore(b, text ? text.nextSibling : host.firstChild);

    b.addEventListener('click', e => {
        if (e.detail > 0) b.blur();   // Space stays the player's
        T()?.core?.invoke('perf_open').catch(() => {});
    });

    const cpu = b.querySelector('#perfCpu');
    const gpu = b.querySelector('#perfGpu');
    const ram = b.querySelector('#perfRam');
    const show = s => {
        if (!s) return;
        cpu.textContent = loadText(s.cpu);
        gpu.textContent = loadText(s.gpu);
        ram.textContent = ramText(s.ram);
    };
    T()?.core?.invoke('perf_history').then(h => show(h?.samples?.[h.samples.length - 1])).catch(() => {});
    T()?.event?.listen('perf:tick', e => show(e.payload)).catch?.(() => {});
}
