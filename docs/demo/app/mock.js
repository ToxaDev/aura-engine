// The live demo on the site: this page runs in a frame of the landing page,
// and what the desktop app's engine would answer comes from the simulated
// player there (demo/sim.js). A classic script, first in <head>, so the
// pages' modules find window.__TAURI__ already in place.
(function () {
  var sim = null;
  try { sim = parent !== window && parent.__auraSim; } catch (e) { sim = null; }
  // A page opened on its own (its address in a tab of its own, a bookmark)
  // has no player behind it and would stand empty: it goes to the live demo
  // on the landing page instead.
  if (!sim) { if (parent === window) location.replace('../../#demo'); return; }
  var file = location.pathname.split('/').pop() || 'index.html';
  var label = file.indexOf('eq') === 0 ? 'eq' : file.indexOf('xtc') === 0 ? 'xtc'
    : file.indexOf('analyzer') === 0 ? 'analyzer-live' : 'main';
  var unlisteners = [];
  var noop = function () { return Promise.resolve(); };
  function LogicalSize(w, h) { this.width = w; this.height = h; this.type = 'Logical'; }
  // The frame is the whole screen this window has: setSize changes nothing
  // here, so the main window's fit (main.js) has to choose its list rows for
  // the frame's own height at once. Measured against the real screen it chose
  // ten, waited for a resize that never came and cut them back, and each cut
  // started the next fit: the list jumped and often stayed at ten rows.
  try {
    Object.defineProperty(screen, 'availHeight', { configurable: true, get: function () { return innerHeight + 60; } });
  } catch (e) {}
  // The window's own 1 px edge is drawn by the site around the frame
  // (demo.css, .dwin::after), once. Drawn here as well, the two curves are
  // rasterised apart and at a scaled or fractional size they part in the
  // corners: the arc reads thicker and brighter than the straight sides.
  var edge = document.createElement('style');
  edge.textContent = '#converterPanel, #anRoot, .win { border-color: transparent !important; }';
  document.head.appendChild(edge);
  var win = {
    label: label,
    close: function () { if (label !== 'main') sim.host && sim.host.front('main'); return Promise.resolve(); },
    minimize: noop, startDragging: noop, toggleMaximize: noop, setFocus: noop, show: noop, hide: noop,
    setSize: function () { return Promise.resolve(); }, setPosition: noop,
    innerSize: function () { return Promise.resolve({ width: innerWidth, height: innerHeight }); },
    outerSize: function () { return Promise.resolve({ width: innerWidth, height: innerHeight }); },
    scaleFactor: function () { return Promise.resolve(1); },
    isMaximized: function () { return Promise.resolve(false); },
    isFullscreen: function () { return Promise.resolve(false); },
    onResized: function () { return Promise.resolve(function () {}); },
    onMoved: function () { return Promise.resolve(function () {}); },
    onFocusChanged: function () { return Promise.resolve(function () {}); },
    onCloseRequested: function () { return Promise.resolve(function () {}); },
    listen: function () { return Promise.resolve(function () {}); },
  };
  var FULL_APP = 'This function works in the full app';
  var desktopOnly = function () { sim.host && sim.host.say(FULL_APP, label); return Promise.resolve(null); };
  // What the demo cannot do (empty the list, remove a file, install the
  // driver): the click does nothing but say so.
  var DEAD = '#plClear, .file-item-close, [data-act="remove"]';
  var DEAD_TEXT = /^(Install|Update) driver$/;
  document.addEventListener('click', function (e) {
    var t = e.target && e.target.closest ? e.target.closest('button, [data-act]') : null;
    if (!t) return;
    if (!(t.matches(DEAD) || DEAD_TEXT.test((t.textContent || '').trim()))) return;
    e.preventDefault(); e.stopImmediatePropagation();
    sim.host && sim.host.say(FULL_APP, label);
  }, true);
  window.__TAURI__ = {
    core: {
      invoke: function (cmd, args) { return sim.invoke(cmd, args || {}); },
      convertFileSrc: function (p) { return p; },
    },
    event: {
      listen: function (name, cb) { var u = sim.listen(name, cb); unlisteners.push(u); return Promise.resolve(u); },
      once: function (name, cb) { var u = sim.listen(name, function (e) { u(); cb(e); }); unlisteners.push(u); return Promise.resolve(u); },
      emit: function (name, payload) { sim.emit(name, payload); return Promise.resolve(); },
      emitTo: function (_t, name, payload) { sim.emit(name, payload); return Promise.resolve(); },
    },
    window: { getCurrentWindow: function () { return win; }, LogicalSize: LogicalSize },
    webviewWindow: { getCurrentWebviewWindow: function () { return win; } },
    webview: { getCurrentWebview: function () { return { onDragDropEvent: function () { return Promise.resolve(function () {}); } }; } },
    dpi: { LogicalSize: LogicalSize, PhysicalSize: LogicalSize },
    app: { getVersion: function () { return Promise.resolve('2.0.1'); } },
    dialog: { open: desktopOnly, save: desktopOnly, ask: function () { return Promise.resolve(false); }, message: noop },
    shell: { open: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    opener: { openUrl: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    fs: { writeTextFile: desktopOnly, writeFile: desktopOnly, readTextFile: desktopOnly },
    path: { join: function () { return Promise.resolve([].join.call(arguments, '/')); } },
  };
  // the demo never offers the tour: the windows read it as taken already,
  // whatever the browser's storage holds (blocked, cleared, not yet seeded)
  var TOUR_DONE = JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' });
  var rawGet = Storage.prototype.getItem;
  Storage.prototype.getItem = function (k) {
    if (k === 'auraTour' || k === 'auraTourAnalyzer' || k === 'auraTourEq') return TOUR_DONE;
    return rawGet.call(this, k);
  };
  var realFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    var url = typeof input === 'string' ? input : input && input.url;
    // the answer is made again in this page's own realm: a buffer from the
    // landing page's realm fails this page's `instanceof ArrayBuffer`
    if (url && url.indexOf('https://aura.localhost/') === 0) {
      var r = sim.route(url);
      return r.arrayBuffer().then(function (ab) {
        return new Response(new Uint8Array(ab), { status: r.status, headers: { 'Content-Type': r.headers.get('Content-Type') || '' } });
      });
    }
    return realFetch(input, init);
  };
  // covers come over the aura protocol as images: the demo has none, so the
  // picture fails at once (the page then shows the title alone), no request
  var srcProp = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
  Object.defineProperty(HTMLImageElement.prototype, 'src', {
    configurable: true, enumerable: srcProp.enumerable, get: srcProp.get,
    set: function (v) { srcProp.set.call(this, String(v).indexOf('https://aura.localhost/') === 0 ? 'data:,' : v); },
  });
  // the demo scrolled away or the tab hidden: the page's drawing loops wait
  // (their frames are held and run when the demo is seen again)
  var held = [], rawRaf = window.requestAnimationFrame.bind(window), rawCancel = window.cancelAnimationFrame.bind(window);
  window.requestAnimationFrame = function (cb) {
    if (sim.visible) return rawRaf(cb);
    held.push(cb); return -held.length;
  };
  window.cancelAnimationFrame = function (id) { if (id < 0) held[-id - 1] = null; else rawCancel(id); };
  sim.onVisible(function () {
    var q = held; held = [];
    q.forEach(function (cb) { if (cb) rawRaf(cb); });
  });
  // a script or style of this window that did not arrive (the server dropped
  // the request) leaves it blank: the landing page loads it once more
  var broken = function () { sim.host && sim.host.broken && sim.host.broken(label); };
  addEventListener('error', function (e) {
    var t = e.target;
    if (t && (t.tagName === 'SCRIPT' || (t.tagName === 'LINK' && t.rel === 'stylesheet'))) broken();
  }, true);
  addEventListener('unhandledrejection', function (e) {
    if (/dynamically imported module|Importing a module script failed/i.test(String(e.reason && e.reason.message || e.reason))) broken();
  });
  // the EQ window opens with its live spectrum on (the app's default is Off)
  if (label === 'eq') addEventListener('load', function () {
    var on = document.querySelector('#spec button[data-v="on"]');
    if (on && !on.classList.contains('on')) on.click();
  });
  // A tip written as `el.title = …` goes straight to data-tip, where the
  // app's tooltip.js moves it a moment later anyway, and only when it changes.
  // In a browser the moment between is enough for its own tooltip: the
  // Convert all button rewrites its title on every progress poll, and under
  // the mouse that tooltip flashed over the button, wider than the window.
  var titleProp = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'title');
  if (titleProp && titleProp.set) Object.defineProperty(HTMLElement.prototype, 'title', {
    configurable: true, enumerable: titleProp.enumerable, get: titleProp.get,
    set: function (v) {
      v = String(v);
      if (this.hasAttribute('title')) this.removeAttribute('title');
      if (v) { if (this.dataset.tip !== v) this.dataset.tip = v; } else if ('tip' in this.dataset) delete this.dataset.tip;
    },
  });
  // the page goes: its listeners on the shared bus go with it
  addEventListener('pagehide', function () { unlisteners.forEach(function (u) { try { u(); } catch (e) {} }); });
})();
