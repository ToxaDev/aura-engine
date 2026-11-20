// The site's demo of the streamer's web remote: these pages are the remote's
// own (desktop-app/src and web/remote as the streamer serves them); this file
// takes the place of the bridge to the server (shim.js). What the streamer
// would answer comes from the landing page (demo/remote-host.js): a silent
// simulated player and a small invented library. Every frame is one device.
(function () {
  'use strict';
  var host = null, w = window;
  for (var i = 0; i < 4; i++) {
    try { if (w.__auraRemoteHost && w.__auraRemoteHost.sim) { host = w.__auraRemoteHost; break; } } catch (e) {}
    if (w === w.parent) break;
    w = w.parent;
  }
  // opened on its own (an address typed, a bookmark): the demo is on the landing page
  if (!host) { if (window.top === window) location.replace('../../#streamer'); return; }
  var sim = host.sim;

  // this device: the remote's page, or a window of it shown over the page (the
  // EQ, the analyzer), which shares the page's events
  var dev = null;
  try { dev = window.parent !== window && window.parent.__AURA_REMOTE__ ? window.parent.__AURA_REMOTE__ : null; } catch (e) { dev = null; }
  var TOP = !dev;
  if (!TOP) document.documentElement.classList.add('rm-frame');

  // what the pages keep: the streamer's shared store (host.store), as the
  // server mirrors it to every device
  try { Object.defineProperty(window, 'localStorage', { configurable: true, get: function () { return host.store; } }); } catch (e) {}

  // ── events: the engine's (the shared player), and this device's own ──
  var own = new Map();
  var OWN = /^remote:/;
  function deliver(event, payload) {
    var list = own.get(event);
    if (list) Array.from(list).forEach(function (f) { try { f({ event: event, id: 0, payload: payload }); } catch (e) { console.error(e); } });
  }
  var unlisteners = [];
  function listen(event, f) {
    if (!TOP) return dev.listen(event, f);
    if (!own.has(event)) own.set(event, new Set());
    own.get(event).add(f);
    var u = OWN.test(event) ? function () {} : sim.listen(event, f);
    var off = function () { own.get(event).delete(f); u(); };
    unlisteners.push(off);
    return Promise.resolve(off);
  }
  function emit(event, payload) {
    if (!TOP) return dev.emit(event, payload);
    if (OWN.test(event)) setTimeout(function () { deliver(event, payload); }, 0);
    else sim.emit(event, payload);
    return Promise.resolve();
  }
  addEventListener('pagehide', function () { unlisteners.forEach(function (u) { try { u(); } catch (e) {} }); });

  // ── the commands ──
  var overlay = function (page) { return function () { emit('remote:open', { page: page }); return null; }; };
  var FULL = 'This works on the streamer itself — the demo has no files or devices';
  var LOCAL = {
    eq_window_open: function (a) { emit('remote:open', { page: 'eq.html' + (a && a.tab ? '?tab=' + a.tab : '') }); return null; },
    xtc_geometry_open: overlay('xtc-geometry.html'),
    analyzer_open: function (a) { emit('remote:open', { page: (a && a.url) || 'analytics.html?mode=live', kind: 'analyzer' }); return null; },
    an_window_png: function () { return Promise.reject('Screenshots are not available on the streamer'); },
    window_keep_place: function () { return null; }, app_quit: function () { return null; },
    ui_cursor_client: function () { return null; }, blind_test_trials: function () { return null; },
    ui_pack_info: function () { return null; }, crash_report_state: function () { return 'none'; },
    crash_report_done: function () { return null; }, crash_report_retry: function () { return null; },
    radio_store_load: function () { return '{}'; }, vis_list: function () { return []; },
    clear_custom_filter: function () { return null; },
    // what needs the streamer's computer: said, not done
    cue_window_open: function () { host.say(FULL); return null; },
    perf_open: function () { host.say(FULL); return null; },
    update_install: function () { host.say(FULL); return null; },
    player_radio: function () { host.say(FULL); return null; },
    player_input_listen: function () { host.say(FULL); return null; },
  };
  var READS = /^(player_(status|queue|devices|list_devices|get_\w+|ui_log|.*_list)|.*_get\w*|lib_\w+)$/;
  function invoke(cmd, args) {
    if (Object.prototype.hasOwnProperty.call(LOCAL, cmd)) return Promise.resolve(LOCAL[cmd](args));
    var r = host.invoke(cmd, args || {});
    if (!READS.test(cmd)) r.then(statusSoon, statusSoon);
    return r;
  }

  // ── the aura protocol, answered here (the player's routes) ──
  var rawFetch = window.fetch.bind(window);
  var lastStatus = 0;
  function statusSoon() { lastStatus = 0; }
  window.fetch = function (input, init) {
    var url = typeof input === 'string' ? input : input && input.url;
    if (url && url.indexOf('https://aura.localhost/') === 0) {
      if (url.indexOf('https://aura.localhost/player/cover') === 0) {
        var c = host.coverOf(+new URL(url).searchParams.get('id'));
        return c ? rawFetch(c) : Promise.resolve(new Response('', { status: 404 }));
      }
      var r = sim.route(url);
      var status = url.indexOf('https://aura.localhost/player/status') === 0;
      // the answer is made again in this page's own realm
      return r.arrayBuffer().then(function (ab) {
        if (status && TOP) {
          // the remote reads the status as the bridge hands it on (remote.js, library.js)
          var t = performance.now();
          if (t - lastStatus > 900) {
            lastStatus = t;
            try { window.dispatchEvent(new CustomEvent('aura-remote-status', { detail: JSON.parse(new TextDecoder().decode(ab)) })); } catch (e) {}
          }
        }
        return new Response(new Uint8Array(ab), { status: r.status, headers: { 'Content-Type': r.headers.get('Content-Type') || '' } });
      });
    }
    return rawFetch(input, init);
  };

  // the demo's frame scrolled away or the tab hidden: the drawing loops wait
  var held = [], rawRaf = window.requestAnimationFrame.bind(window), rawCancel = window.cancelAnimationFrame.bind(window);
  window.requestAnimationFrame = function (cb) {
    if (sim.visible && host.visible !== false) return rawRaf(cb);
    held.push(cb); return -held.length;
  };
  window.cancelAnimationFrame = function (id) { if (id < 0) held[-id - 1] = null; else rawCancel(id); };
  sim.onVisible(function () {
    var q = held; held = [];
    q.forEach(function (cb) { if (cb) rawRaf(cb); });
  });

  var windowLike = function (label) {
    return new Proxy({ label: label }, {
      get: function (t, k) {
        if (k in t) return t[k];
        if (k === 'then') return undefined;
        if (k === 'close' || k === 'destroy') {
          return function () { if (!TOP) window.parent.postMessage({ remoteClose: true }, '*'); return Promise.resolve(); };
        }
        if (typeof k === 'string' && (k.indexOf('on') === 0 || k === 'listen')) return function () { return Promise.resolve(function () {}); };
        return function () { return Promise.resolve(null); };
      },
    });
  };
  var label = TOP ? 'main' : location.pathname.split('/').pop().replace(/\.html$/, '');
  if (!TOP && /analytics\.html$/.test(location.pathname)) {
    import('./remote/anpager.js').catch(function () {});
  }
  function Size(a, b) { this.width = a; this.height = b; }
  function Pos(a, b) { this.x = a; this.y = b; }
  var sizes = { LogicalSize: Size, PhysicalSize: Size, LogicalPosition: Pos, PhysicalPosition: Pos };
  var no = function () { return Promise.resolve(null); };
  window.__TAURI__ = {
    core: { invoke: invoke, convertFileSrc: function (p) { return p; } },
    event: { listen: listen, emit: emit, once: function (e, f) { return listen(e, f); } },
    window: Object.assign({ getCurrentWindow: function () { return windowLike(label); } }, sizes),
    webview: { getCurrentWebview: function () { return windowLike(label); } },
    webviewWindow: { getCurrentWebviewWindow: function () { return windowLike(label); } },
    dpi: sizes,
    app: { getVersion: function () { return Promise.resolve('2.0.1'); } },
    dialog: {
      ask: function () { return Promise.resolve(false); }, confirm: function () { return Promise.resolve(false); },
      message: function (t) { host.say(String(t)); return Promise.resolve(); }, open: no, save: no,
    },
    shell: { open: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    opener: { openUrl: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    fs: { writeFile: function () { return Promise.reject('Not available on the streamer'); }, writeTextFile: function () { return Promise.reject('Not available on the streamer'); } },
    clipboardManager: { writeText: function (t) { return navigator.clipboard ? navigator.clipboard.writeText(t) : Promise.reject('no clipboard'); } },
  };
  window.__AURA_REMOTE__ = { client: 'demo', invoke: invoke, emit: emit, listen: listen };
  // the landing page is told of use (its slides do not turn under a hand) and of Escape (full size closes)
  if (TOP) {
    ['pointerdown', 'pointermove', 'wheel', 'keydown', 'touchstart'].forEach(function (ev) {
      addEventListener(ev, function (e) {
        try { if (w.__heroTouched) w.__heroTouched(); } catch (x) {}
        if (ev === 'keydown' && e.key === 'Escape' && !document.body.classList.contains('rm-paged')) {
          try { w.dispatchEvent(new CustomEvent('aura-remote-escape')); } catch (x) {}
        }
      }, { passive: true, capture: true });
    });
  }
  // no confirm boxes over the site: the demo's answer is no
  window.confirm = function () { return false; };
  window.alert = function (t) { host.say(String(t)); };
})();
