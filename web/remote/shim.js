// The bridge: the desktop app's pages run in a browser as they are. What they
// ask of Tauri goes to the streamer's server; what only makes sense for a window
// is answered here. Loaded before anything else on every page (assets.rs).
(function () {
    'use strict';

    // ── who this page is ────────────────────────────────────────────────
    const CLIENT = Math.random().toString(36).slice(2);
    const TOP = window.parent === window;
    function post(url, body) {
        return fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    }

    // ── what the pages keep: on the server, mirrored in localStorage (kv.rs) ──
    // Kept per device, not shared: which tab is open, and the like.
    const OWN = /^remote\./;
    const store = window.localStorage;
    const raw = { set: Storage.prototype.setItem, remove: Storage.prototype.removeItem, clear: Storage.prototype.clear };
    const pending = new Map();
    let flushTimer = 0;
    function flush() {
        clearTimeout(flushTimer); flushTimer = 0;
        for (const [key, value] of pending) post('/api/kv', { key, value, from: CLIENT }).catch(() => {});
        pending.clear();
    }
    // A slider writes on every step: what goes out is the last value.
    function queueWrite(key, value) {
        if (OWN.test(key)) return;
        pending.set(key, value);
        if (!flushTimer) flushTimer = setTimeout(flush, 150);
    }
    if (TOP) {
        // Synchronous on purpose: the page's modules read the store as they load.
        try {
            const x = new XMLHttpRequest();
            x.open('GET', '/api/kv', false);
            x.send();
            if (x.status === 200) {
                const all = JSON.parse(x.responseText);
                for (const k of Object.keys(store)) if (!OWN.test(k) && !(k in all)) raw.remove.call(store, k);
                for (const k of Object.keys(all)) raw.set.call(store, k, all[k]);
            }
        } catch (_) {}
    }
    Storage.prototype.setItem = function (k, v) {
        raw.set.call(this, k, v);
        if (this === store) queueWrite(String(k), String(v));
    };
    Storage.prototype.removeItem = function (k) {
        raw.remove.call(this, k);
        if (this === store) queueWrite(String(k), null);
    };
    window.addEventListener('pagehide', flush);

    // ── events: the server's, and between this browser's pages ──────────
    const listeners = new Map();
    // One channel per page and the frames over it (an overlay hears what the page
    // hears). Not one for the whole browser: two tabs each have the server's events
    // already, and a tab must never hear its own change come back through another.
    let root = CLIENT;
    if (!TOP) { try { root = window.parent.__AURA_REMOTE__.client; } catch (_) {} }
    const channel = 'BroadcastChannel' in window ? new BroadcastChannel('aura-events-' + root) : null;
    function deliver(event, payload) {
        const list = listeners.get(event);
        if (list) for (const f of [...list]) { try { f({ event, id: 0, payload }); } catch (e) { console.error(e); } }
    }
    if (channel) channel.onmessage = (m) => deliver(m.data.event, m.data.payload);
    function listen(event, f) {
        if (!listeners.has(event)) listeners.set(event, new Set());
        listeners.get(event).add(f);
        return Promise.resolve(() => listeners.get(event).delete(f));
    }
    function emit(event, payload) {
        deliver(event, payload);
        if (channel) channel.postMessage({ event, payload });
        return Promise.resolve();
    }
    if (TOP && 'EventSource' in window) {
        const es = new EventSource('/api/events');
        es.onmessage = (m) => {
            let e;
            try { e = JSON.parse(m.data); } catch (_) { return; }
            if (e.event === 'kv:changed') {
                // Another device changed it: the mirror follows, and the page is told.
                if (e.payload.from === CLIENT) return;
                if (e.payload.value == null) raw.remove.call(store, e.payload.key);
                else raw.set.call(store, e.payload.key, e.payload.value);
            }
            emit(e.event, e.payload);
        };
    }

    // ── meters over Wi-Fi ───────────────────────────────────────────────
    // The bars, the HP lamp and the background under HP's unit come in blocks
    // a few seconds long, timed to the DAC, and the page's per-frame requests
    // are answered here from them (meterfeed.js). Until that module is in,
    // and for the other meters (the EQ window's analyzer), a meter gets a
    // fresh answer at most ~15 times a second; in between, the last one again.
    const METER = /\/aura\/player\/(spectrum|eq_spectrum|phase_lamp|phase_view|levels|scope)\b/;
    const METER_MS = 66;
    const meters = new Map();
    const rawFetch = window.fetch.bind(window);
    let feed = null;
    if (TOP) {
        import('/remote/meterfeed.js')
            .then((m) => { feed = m.createFeed({ fetch: rawFetch }); window.__AURA_METERS__ = feed; })
            .catch((e) => console.warn('[remote] meterfeed.js:', e));
    }
    const STATUS = /\/aura\/player\/status\b/;
    window.fetch = function (input, init) {
        const url = typeof input === 'string' ? input : (input && input.url) || '';
        const get = !(init && init.method && init.method !== 'GET');
        if (get && STATUS.test(url)) {
            // The page asks for the status four times a second (its own loop,
            // made for the app's local link). Over the network the real
            // request goes once a second while it plays, every 3 s at rest,
            // at once after a command and while something is in motion (a
            // seek, a rebuild); in between the page gets the last answer
            // again, its position moved on by the time gone. One for all the
            // frames: theirs go through the top page's.
            let f = statusFetch;
            if (!TOP) { try { f = window.parent.__AURA_STATUS__ || f; } catch (_) {} }
            return f(input, init);
        }
        if (feed && get) {
            const r = feed.handle(url);
            if (r) return r;
        }
        const m = METER.exec(url);
        if (!m || !get) return rawFetch(input, init);
        const key = url.split('?')[0];
        const now = performance.now();
        let e = meters.get(key);
        if (!e) { e = { t: -1e9, last: null, flight: null }; meters.set(key, e); }
        const answer = (x) => new Response(x.body, { status: x.status, headers: { 'Content-Type': x.type } });
        if (e.flight) return e.flight.then(answer);
        if (e.last && now - e.t < METER_MS) return Promise.resolve(answer(e.last));
        e.t = now;
        e.flight = rawFetch(input, init)
            .then(async (r) => ({ body: await r.arrayBuffer(), status: r.status, type: r.headers.get('Content-Type') || '' }))
            .then((x) => { e.last = x; return x; })
            .finally(() => { e.flight = null; });
        return e.flight.then(answer);
    };
    const st = { body: null, json: null, at: -1e9, flight: null, stale: true };
    const STATUS_PLAY_MS = 1000, STATUS_REST_MS = 3000;
    function statusTtl(j) {
        if (!j) return 0;
        if (j.rebuilding || j.pending || (j.arming && (j.arming.busy || j.arming.step !== 'done')) || (j.jump && j.jump.pending)) return 0;
        return j.state === 'playing' ? STATUS_PLAY_MS : STATUS_REST_MS;
    }
    function statusAnswer() {
        let body = st.body;
        const j = st.json;
        if (j && j.state === 'playing' && typeof j.positionS === 'number') {
            const dt = (performance.now() - st.at) / 1000;
            const pos = j.durationS > 0 ? Math.min(j.durationS, j.positionS + dt) : j.positionS + dt;
            body = JSON.stringify(Object.assign({}, j, { positionS: pos }));
        }
        return new Response(body, { status: 200, headers: { 'Content-Type': 'application/json' } });
    }
    function statusFetch(input, init) {
        const t = performance.now();
        if (st.flight) return st.flight.then((ok) => (ok ? statusAnswer() : rawFetch(input, init)));
        if (st.json && !st.stale && t - st.at < statusTtl(st.json)) return Promise.resolve(statusAnswer());
        st.stale = false;
        const sent = t;
        let res0 = null;
        st.flight = rawFetch(input, init).then(async (res) => {
            res0 = res;
            const head = performance.now();
            feed && feed.noteRtt(head - sent);
            feed && feed.noteClock && feed.noteClock(sent, head, res.headers.get('x-aura-clock'));
            if (!res.ok) return false;
            const text = await res.clone().text();
            const j = JSON.parse(text);
            Object.assign(st, { body: text, json: j, at: performance.now() });
            feed && feed.onStatus(j);
            window.dispatchEvent(new CustomEvent('aura-remote-status', { detail: j }));
            return true;
        }).catch(() => false).finally(() => { st.flight = null; });
        return st.flight.then((ok) => (ok ? statusAnswer() : (res0 || rawFetch(input, init))));
    }
    /// A command was sent: the next status is the engine's own, not the last one.
    function statusStale() { st.stale = true; }
    window.__AURA_STATUS_STALE__ = statusStale;
    window.__AURA_STATUS__ = statusFetch;

    // What moves the sound: the feed takes a block at once (and again once
    // the engine has rebuilt its ring).
    // Commands that only read: they leave the held status as it is.
    const READS = /^(player_(status|queue|devices|list_devices|get_\w+|ui_log|.*_list)|.*_get\w*)$/;
    const MOVES = /^player_(seek|play|pause|stop|next|prev|set_settings|set_track_settings|set_eq|set_cue|set_device|radio|remove|clear)$/;

    // ── what only a window has ──────────────────────────────────────────
    // A window opened by a command is a full-page frame over this page
    // (remote.js puts it up); these answer with what the page expects.
    const overlay = (page) => () => { emit('remote:open', { page }); return null; };
    const LOCAL = {
        eq_window_open: (a) => { emit('remote:open', { page: 'eq.html' + (a && a.tab ? '?tab=' + a.tab : '') }); return null; },
        cue_window_open: overlay('cue.html'),
        xtc_geometry_open: overlay('xtc-geometry.html'),
        perf_open: overlay('perf.html'),
        // The analyzer: one frame at a time (live or a file), over the remote.
        analyzer_open: (a) => { emit('remote:open', { page: (a && a.url) || 'analytics.html?mode=live', kind: 'analyzer' }); return null; },
        an_window_png: () => Promise.reject('Screenshots are not available on the streamer'),
        window_keep_place: () => null,
        app_quit: () => null,
        ui_cursor_client: () => null,
        blind_test_trials: () => null,
        ui_pack_info: () => null,
        crash_report_state: () => 'none',
        crash_report_done: () => null,
        crash_report_retry: () => null,
        // Radio favourites stay in the page's store (shared through the server).
        radio_store_load: () => '{}',
        vis_list: () => [],
        clear_custom_filter: () => null,
    };

    // The commands the app keeps in the order the page sent them (Tauri runs
    // them on one thread): device, then play. Everything else overtakes freely,
    // as the app's `async` commands do. Over HTTP nothing keeps the order but this.
    const ANY_ORDER = /^(player_add|player_dsd_capabilities|player_paths_exist|player_render_checks|player_devices|player_status|player_queue|player_cue_view|check_filters|gpu_status|gpu_info|filter_inventory|tfs_unavailable|dsd_export_modulators|get_conversion_progress|get_queue_status|perf_history|spatial_status|spatial_track_status|lib_)/;
    let chain = Promise.resolve();
    function send(cmd, args) {
        return post('/api/invoke/' + cmd, args || {}).then(async (r) => {
            const v = await r.json().catch(() => null);
            if (!r.ok) throw v == null ? 'HTTP ' + r.status : v;
            return v;
        });
    }
    // A page that joins while music plays must not touch it. Booting, the app's
    // page sends its whole rack (once with a passing value: apodizing on, then
    // off), which in a window happens before anything plays; here another device
    // may be playing, and the engine would rebuild mid-song (a 0.27 s gap). The
    // engine already has the rack the shared store holds, so while a page boots
    // over a live engine its rack is not sent. A stopped engine (the server just
    // started) still takes it: that is how the rack gets there.
    const BOOT_QUIET = /^player_set_(settings|device|repeat)$/;
    let bootQuiet = false;
    if (TOP) {
        try {
            const x = new XMLHttpRequest();
            x.open('GET', '/aura/player/status', false);
            x.send();
            const st = x.status === 200 ? JSON.parse(x.responseText).state : null;
            bootQuiet = !!st && st !== 'stopped';
        } catch (_) {}
        if (bootQuiet) {
            const t = setInterval(() => {
                if (!window.__auraBooted) return;
                clearInterval(t);
                setTimeout(() => { bootQuiet = false; }, 2000);
            }, 50);
        }
    }

    function invoke(cmd, args) {
        if (Object.prototype.hasOwnProperty.call(LOCAL, cmd)) return Promise.resolve(LOCAL[cmd](args));
        if (bootQuiet && BOOT_QUIET.test(cmd)) return Promise.resolve(null);
        if (ANY_ORDER.test(cmd)) return send(cmd, args);
        const r = chain.then(() => send(cmd, args));
        chain = r.catch(() => {});
        if (!READS.test(cmd)) {
            const stale = () => { try { (TOP ? statusStale : window.parent.__AURA_STATUS_STALE__)?.(); } catch (_) {} };
            r.then(stale, stale);
        }
        if (MOVES.test(cmd)) {
            const kick = () => { try { (feed || (TOP ? null : window.parent.__AURA_METERS__))?.kick(); } catch (_) {} };
            r.then(kick, kick);
        }
        return r;
    }

    // Any method of a window or a webview: does nothing, says nothing is wrong.
    const windowLike = (label) => new Proxy({ label }, {
        get(t, k) {
            if (k in t) return t[k];
            if (k === 'then') return undefined;
            if (k === 'close' || k === 'destroy') {
                return () => { if (!TOP) window.parent.postMessage({ remoteClose: true }, '*'); return Promise.resolve(); };
            }
            // onResized, onDragDropEvent, listen, ...: hand back an unlisten.
            if (typeof k === 'string' && (k.startsWith('on') || k === 'listen')) return () => Promise.resolve(() => {});
            return () => Promise.resolve(null);
        },
    });
    const label = TOP ? 'main' : location.pathname.replace(/^\/|\.html$/g, '');
    // A window of the app shown as a page of the remote (remote.js): its own
    // title bar's buttons give way to the remote's Back (remote.css).
    if (!TOP) document.documentElement.classList.add('rm-frame');
    // The analyzer on a phone: its numbers and each graph a page of their own,
    // swiped or picked by the dots at the foot (anpager.js).
    if (!TOP && /analytics\.html$/.test(location.pathname)) {
        import('/remote/anpager.js').catch((e) => console.warn('[remote] anpager.js:', e));
    }
    const sizes = {
        LogicalSize: class { constructor(w, h) { this.width = w; this.height = h; } },
        PhysicalSize: class { constructor(w, h) { this.width = w; this.height = h; } },
        LogicalPosition: class { constructor(x, y) { this.x = x; this.y = y; } },
        PhysicalPosition: class { constructor(x, y) { this.x = x; this.y = y; } },
    };

    window.__TAURI__ = {
        core: { invoke },
        event: { listen, emit, once: (e, f) => listen(e, f) },
        window: { getCurrentWindow: () => windowLike(label), ...sizes },
        webview: { getCurrentWebview: () => windowLike(label) },
        dpi: { ...sizes },
        app: { getVersion: () => fetch('/api/whoami').then((r) => r.json()).then((w) => w.version) },
        dialog: {
            ask: (text) => Promise.resolve(window.confirm(text)),
            confirm: (text) => Promise.resolve(window.confirm(text)),
            message: (text) => { window.alert(text); return Promise.resolve(); },
            // No system dialogs on a phone: the library takes their place.
            open: () => Promise.resolve(null),
            save: () => Promise.resolve(null),
        },
        shell: { open: (url) => { window.open(url, '_blank', 'noopener'); return Promise.resolve(); } },
        fs: {
            writeFile: () => Promise.reject('Not available on the streamer'),
            writeTextFile: () => Promise.reject('Not available on the streamer'),
        },
        clipboardManager: { writeText: (t) => navigator.clipboard ? navigator.clipboard.writeText(t) : Promise.reject('no clipboard') },
    };
    window.__AURA_REMOTE__ = { client: CLIENT, invoke, emit, listen };
})();

// A page let go a little short of its end finishes the way by itself: under
// a finger, the last row half out of sight is not left there.
(() => {
    if (!matchMedia('(hover: none) and (pointer: coarse)').matches) return;
    let last = null, touching = false, t = 0;
    const settle = () => {
        const el = last;
        if (!el || touching) return;
        const cs = getComputedStyle(el);
        if (cs.scrollSnapType && cs.scrollSnapType !== 'none') return;   // a list with its own stops
        const max = el.scrollHeight - el.clientHeight, rest = max - el.scrollTop;
        if (rest > 2 && rest < Math.min(110, el.clientHeight * 0.2)) el.scrollTo({ top: max, behavior: 'smooth' });
    };
    const later = () => { clearTimeout(t); t = setTimeout(settle, 160); };
    document.addEventListener('scroll', (e) => {
        last = e.target === document ? document.scrollingElement : e.target;
        if (!touching) later();
    }, true);
    addEventListener('touchstart', () => { touching = true; clearTimeout(t); }, { passive: true, capture: true });
    addEventListener('touchend', () => { touching = false; later(); }, { passive: true, capture: true });
    addEventListener('touchcancel', () => { touching = false; later(); }, { passive: true, capture: true });
})();

// The remote is a web page, not an installed app: a service worker left by a
// test build (localhost or https) is taken off.
if (window.top === window && 'serviceWorker' in navigator && window.isSecureContext) {
    navigator.serviceWorker.getRegistrations()
        .then((all) => all.forEach((r) => r.unregister()))
        .catch(() => {});
}
