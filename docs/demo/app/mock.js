// The live demo on the site: this page runs in a frame of the landing page,
// and what the desktop app's engine would answer comes from the simulated
// player there (demo/sim.js). A classic script, first in <head>, so the
// pages' modules find window.__TAURI__ already in place.
//
// Two windows of the app share the player: the compact window
// (index.html) and the Library window (index.html?lib, opened in its
// landscape mode). A tool the Library window shows in its centre (the
// analyzer, Cue Consistency, XTC, the EQ) is a frame inside that frame; it
// finds the player two levels up.
(function () {
  var sim = null, w = window, depth = 0;
  for (var i = 0; i < 4 && w.parent !== w; i++) {
    w = w.parent; depth++;
    try { if (w.__auraSim) { sim = w.__auraSim; break; } } catch (e) { break; }
  }
  // A page opened on its own (its address in a tab of its own, a bookmark)
  // has no player behind it and would stand empty: it goes to the live demo
  // on the landing page instead.
  if (!sim) { if (window.top === window) location.replace('../../#demo'); return; }
  var nested = depth > 1;               // a page in the Library window's centre
  var file = location.pathname.split('/').pop() || 'index.html';
  var isLib = !nested && /(^|[?&])lib\b/.test(location.search);
  var label = nested ? 'embedded' : isLib ? 'lib' : 'main';
  var unlisteners = [];
  var noop = function () { return Promise.resolve(); };
  var host = function () { return sim.host || {}; };
  function LogicalSize(w, h) { this.width = w; this.height = h; this.type = 'Logical'; }
  function LogicalPosition(x, y) { this.x = x; this.y = y; this.type = 'Logical'; }
  // The frame is the whole screen this window has: setSize changes nothing
  // here, so the main window's fit (main.js) has to choose its list rows for
  // the frame's own height at once.
  if (!nested) try {
    Object.defineProperty(screen, 'availHeight', { configurable: true, get: function () { return innerHeight + 60; } });
  } catch (e) {}
  // The window's own 1 px edge is drawn by the site around the frame
  // (demo.css, .dwin::after), once.
  if (!nested) {
    var edge = document.createElement('style');
    edge.textContent = '#converterPanel, #anRoot, .win { border-color: transparent !important; }';
    document.head.appendChild(edge);
  }
  var win = {
    label: label,
    close: noop, minimize: noop, startDragging: noop, startResizeDragging: noop, toggleMaximize: noop,
    maximize: noop, unmaximize: noop, setFocus: noop, show: noop, hide: noop, setResizable: noop,
    setMinSize: noop, setFullscreen: noop, setSize: noop, setPosition: noop,
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
  var from = nested || isLib ? 'lib' : 'main';
  var say = function (t) { host().say && host().say(t, from); };
  var flip = function () { var h = host(); if (h.mode) h.mode(isLib ? 'main' : 'lib', window); else if (h.glow) h.glow(isLib ? 'main' : 'lib'); };
  var desktopOnly = function () { say(FULL_APP); return Promise.resolve(null); };
  // What the demo cannot do (empty the list, remove a file, install the
  // driver, add a music folder): the click does nothing but say so.
  var DEAD = '#plClear, .file-item-close, [data-act="remove"], .ml-sadd, .ml-smanage';
  var DEAD_TEXT = /^(Install|Update) driver$/;
  document.addEventListener('click', function (e) {
    var t = e.target && e.target.closest ? e.target.closest('button, [data-act]') : null;
    if (!t) return;
    // the window's LIBRARY / COMPACT switch: the demo turns to its other look
    // (the other window of the demo, kept loaded behind this one)
    if (!nested && t.id === 'winBtnOrient') {
      e.preventDefault(); e.stopImmediatePropagation();
      // (a press on the half already lit: this window is that look already)
      var seg = e.target.closest('.wo-seg');
      if (seg && seg.classList.contains(isLib ? 'wo-l' : 'wo-c')) return;
      flip();
      return;
    }
    if (!(t.matches(DEAD) || DEAD_TEXT.test((t.textContent || '').trim()))) return;
    e.preventDefault(); e.stopImmediatePropagation();
    say(FULL_APP);
  }, true);
  // Ctrl+L (the same switch) and F11 do nothing here
  if (!nested) addEventListener('keydown', function (e) {
    if (((e.ctrlKey || e.metaKey) && (e.key === 'l' || e.key === 'L')) || e.key === 'F11') {
      e.preventDefault(); e.stopImmediatePropagation();
      if (e.key !== 'F11') flip();
    }
  }, true);
  // a hand on the Library window: the demo stops turning its tools by itself
  if (isLib || nested) ['pointerdown', 'wheel', 'keydown'].forEach(function (ev) {
    addEventListener(ev, function (e) { if (e.isTrusted && host().touched) host().touched(); }, { capture: true, passive: true });
  });
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
    window: { getCurrentWindow: function () { return win; }, LogicalSize: LogicalSize, LogicalPosition: LogicalPosition },
    webviewWindow: { getCurrentWebviewWindow: function () { return win; } },
    webview: { getCurrentWebview: function () { return { onDragDropEvent: function () { return Promise.resolve(function () {}); } }; } },
    dpi: { LogicalSize: LogicalSize, PhysicalSize: LogicalSize, LogicalPosition: LogicalPosition, PhysicalPosition: LogicalPosition },
    app: { getVersion: function () { return Promise.resolve('2.1.1'); } },
    dialog: { open: desktopOnly, save: desktopOnly, ask: function () { return Promise.resolve(false); },
      confirm: function () { return Promise.resolve(false); }, message: noop },
    shell: { open: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    opener: { openUrl: function (u) { try { window.open(u, '_blank', 'noopener'); } catch (e) {} return Promise.resolve(); } },
    fs: { writeTextFile: desktopOnly, writeFile: desktopOnly, readTextFile: desktopOnly },
    path: { join: function () { return Promise.resolve([].join.call(arguments, '/')); } },
    clipboardManager: { writeText: function (t) { return navigator.clipboard ? navigator.clipboard.writeText(t) : Promise.reject('no clipboard'); } },
  };
  // the demo never offers a tour: the windows read it as taken already,
  // whatever the browser's storage holds (blocked, cleared, not yet seeded);
  // the Library window opens in its landscape mode, the compact one compact
  var TOUR_DONE = JSON.stringify({ v: 1, state: 'done', at: '2026-11-01T00:00:00Z' });
  var TOURS = { auraTour: 1, auraTourAnalyzer: 1, auraTourEq: 1, auraTourCue: 1, auraTourSwitch: 1, auraLibraryIntro: 1 };
  var MODE_KEY = 'auraWindowMode';
  var mode = JSON.stringify({ mode: isLib ? 'landscape' : 'compact', size: null, pos: null, max: false, tab: 'queue', compactPos: null });
  var rawGet = Storage.prototype.getItem, rawSet = Storage.prototype.setItem;
  Storage.prototype.getItem = function (k) {
    if (TOURS[k]) return TOUR_DONE;
    if (k === MODE_KEY) return mode;
    return rawGet.call(this, k);
  };
  Storage.prototype.setItem = function (k, v) {
    if (k === MODE_KEY) { mode = String(v); return; }
    return rawSet.call(this, k, v);
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
  // covers over the aura protocol: a track's is its invented album's
  // (sim.coverOf), anything else fails at once (no request)
  var srcProp = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
  Object.defineProperty(HTMLImageElement.prototype, 'src', {
    configurable: true, enumerable: srcProp.enumerable, get: srcProp.get,
    set: function (v) {
      v = String(v);
      if (v.indexOf('https://aura.localhost/') === 0) {
        var m = /\/player\/cover\?id=(\d+)/.exec(v), c = m && sim.coverOf ? sim.coverOf(+m[1]) : null;
        v = c || 'data:,';
      }
      srcProp.set.call(this, v);
    },
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
  var broken = function () { host().broken && host().broken(from); };
  if (!nested) {
    addEventListener('error', function (e) {
      var t = e.target;
      if (t && (t.tagName === 'SCRIPT' || (t.tagName === 'LINK' && t.rel === 'stylesheet'))) broken();
    }, true);
    addEventListener('unhandledrejection', function (e) {
      if (/dynamically imported module|Importing a module script failed/i.test(String(e.reason && e.reason.message || e.reason))) broken();
    });
  }
  // the EQ opens with its live spectrum on (the app's default is Off)
  if (file.indexOf('eq') === 0) addEventListener('load', function () {
    var on = document.querySelector('#spec button[data-v="on"]');
    if (on && !on.classList.contains('on')) on.click();
    unselectBand();
  });
  // The demo's EQ opens with no band chosen (Anton 22.11): the whole curve, no
  // band's panel over it. A click on the graph's empty floor, as a hand would
  // do, whenever a band comes up chosen (the app opens on band 1, and again as
  // the state arrives), until a real hand touches the page. Only while a band
  // is chosen: on an empty choice the same click would add a band.
  function unselectBand() {
    var hand = false, t0 = Date.now();
    ['pointerdown', 'keydown', 'wheel'].forEach(function (ev) {
      addEventListener(ev, function (e) { if (e.isTrusted) hand = true; }, { capture: true, passive: true });
    });
    (function look() {
      if (hand || Date.now() - t0 > 8000) return;
      var pop = document.getElementById('bandPop'), cv = document.getElementById('eqCanvas');
      var r = cv && cv.getBoundingClientRect();
      if (pop && !pop.hidden && r && r.width > 50) {
        var o = { bubbles: true, cancelable: true, clientX: r.left + r.width * 0.62, clientY: r.top + r.height * 0.82,
          button: 0, buttons: 1, pointerId: 1, pointerType: 'mouse', isPrimary: true };
        cv.setPointerCapture = function () {};
        cv.dispatchEvent(new PointerEvent('pointerdown', o));
        o.buttons = 0;
        cv.dispatchEvent(new PointerEvent('pointerup', o));
        cv.dispatchEvent(new PointerEvent('pointerleave', o));
        delete cv.setPointerCapture;
      }
      setTimeout(look, 120);
    })();
  }
  // The Library window's tools, for the landing page's switcher: the
  // library's own Analyzer and Visualizer pages (its side's sections) and
  // the windows it opens in its centre (centre-page.js). Imported here, by
  // the page's own address, they are the page's own modules.
  if (isLib) {
    var PAGES = { cue: 'cue_window_open', xtc: 'xtc_geometry_open', eq: 'eq_window_open' };
    window.__demoTool = function (id) {
      if (PAGES[id]) return import('./js/centre-page.js').then(function (m) { return m.openAppWindow(PAGES[id]); });
      var b = document.querySelector('#lsSide [data-sec="' + id + '"]');
      if (b) b.click();
      return Promise.resolve(!!b);
    };
  }
  // A tip written as `el.title = …` goes straight to data-tip, where the
  // app's tooltip.js moves it a moment later anyway, and only when it changes
  // (in a browser the moment between was enough for its own tooltip).
  var titleProp = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'title');
  if (titleProp && titleProp.set) Object.defineProperty(HTMLElement.prototype, 'title', {
    configurable: true, enumerable: titleProp.enumerable, get: titleProp.get,
    set: function (v) {
      v = String(v);
      if (this.hasAttribute('title')) this.removeAttribute('title');
      if (v) { if (this.dataset.tip !== v) this.dataset.tip = v; } else if ('tip' in this.dataset) delete this.dataset.tip;
    },
  });
  // no confirm boxes over the site: the demo's answer is no
  window.confirm = function () { return false; };
  window.alert = function (t) { say(String(t)); };
  // The compact window's height is fixed here (setSize changes nothing), but
  // the app's own fit makes the window as tall as its content. Fonts that are
  // a few px taller (Windows' Segoe UI) leave the panel 2-6 px too tall: it
  // would scroll by those pixels. The gaps above the list give them back
  // (only a few px; a larger lack is the rows' business, main.js).
  if (!nested && !isLib) (function () {
    var busy = false;
    function fitGap() {
      if (busy) return; busy = true;
      var p = document.getElementById('converterPanel');
      var gaps = [document.getElementById('plListHead'), document.getElementById('plBar')].filter(Boolean);
      if (p) {
        gaps.forEach(function (e) { e.style.marginTop = ''; });
        var over = p.scrollHeight - p.clientHeight;
        if (over > 0 && over <= 14) {
          gaps.forEach(function (e) {
            if (over <= 0) return;
            var m = parseFloat(getComputedStyle(e).marginTop) || 0, take = Math.min(over, Math.max(0, m - 3));
            if (take > 0) { e.style.marginTop = (m - take) + 'px'; over -= take; }
          });
        }
        if (p.scrollTop) p.scrollTop = 0;
      }
      busy = false;
    }
    function watch() {
      fitGap();
      if (window.ResizeObserver) {
        var ro = new ResizeObserver(function () { requestAnimationFrame(fitGap); });
        ['convSpecsLine', 'rkStrip', 'labField', 'plBar', 'dropZone'].forEach(function (id) { var e = document.getElementById(id); if (e) ro.observe(e, { box: 'border-box' }); });
      }
      if (document.fonts && document.fonts.ready) document.fonts.ready.then(fitGap);
      addEventListener('resize', fitGap);
    }
    if (document.readyState === 'complete') setTimeout(watch, 300); else addEventListener('load', function () { setTimeout(watch, 300); });
  })();
  // the page goes: its listeners on the shared bus go with it
  addEventListener('pagehide', function () { unlisteners.forEach(function (u) { try { u(); } catch (e) {} }); });
})();
