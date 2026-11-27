// The live demo in the hero: the app's own pages run in iframes over a
// simulated player (demo/sim.js) — silent, driven by level and spectrum
// envelopes measured from real tracks, over a small invented library
// (demo/remote/remote/demo-lib.json and its generated covers). Two windows
// of the app share that player, side by side on the page: the compact window
// on the left, the Library window (the app's landscape look) on the right;
// a phone-wide page stacks them, the compact one first. The Library window's
// centre shows, in turn, the library and the tools it opens there: the
// Analyzer, the Visualizer, Cue Consistency, XTC and the EQ. A row of tabs
// under it picks one; they also turn by themselves, gently, until a hand is
// put on the window or a tab. Full size opens the app in the page's whole
// window, one look at a time: the app's own COMPACT / LIBRARY button, or the
// switch over it, turns one into the other there. Lazy start; windows move
// by transform and filter only.
import { createSim, createLibrary } from './sim.js?v=211-1';
import { auraShow } from './aura-reveal.js?v=198-2';

const V = 'v=211-1';
const LIB_DATA = 'demo/remote/remote/';           // the invented library (shared with the streamer's slide)
const MAIN = { id: 'main', title: 'Player', src: 'demo/app/index.html?' + V, w: 480, h: 942,   // the app's own window size
  poster: 'media/shot-main-195.webp?v=211-1' };
const LIB = { id: 'lib', title: 'Library', src: 'demo/app/index.html?lib&' + V, w: 1120, h: 800,
  poster: 'media/shot-library-1120.webp?v=211-1' };
// the Library window's centre: its own pages, in the order they turn
const TOOLS = [
  { id: 'albums', name: 'Library' },
  { id: 'analyzer', name: 'Analyzer' },
  { id: 'visualizer', name: 'Visualizer' },
  { id: 'cue', name: 'Cue Consistency' },
  { id: 'xtc', name: 'XTC' },
  { id: 'eq', name: 'EQ' },
];
// The virtual desk, in the windows' own pixels: the compact window at its
// left edge; the Library window to its right, a little apart, its middle a
// little over the desk's; the tabs in the room under it. Over the desk, the
// row with the Full size button.
const DESK_H = MAIN.h, OVER = -20;
const LIB_X = MAIN.w - OVER, LIB_Y = Math.round((DESK_H - LIB.h) / 2) - 30;
const DESK_W = LIB_X + LIB.w;
const TOP_H = 44;                  // the Full size row, px
const STACK_GAP = 18;              // a phone-wide page: between the compact window and the Library one, px
const PHONE_BELOW = 640;           // container width under which the windows stack (and full size opens compact)
const TURN_MS = 9000;              // a tool's time before the next comes, while nobody touches
const FIRST_TURN_MS = 6500;        // the library itself first, a little shorter

// A window waits, unseen, until the working app in it has drawn itself, and
// comes in only then. The app not ready after this long: its picture instead.
const LIVE_GIVEUP = 9000;
const INTRO_MIN = 1500;            // the page's intro (index.html) stays this long, ms from the page's start
const STAGGER = 280;               // between two windows coming in, ms
const wait = ms => new Promise(r => setTimeout(r, ms));
const frames = n => new Promise(r => { const f = () => (--n <= 0 ? r() : requestAnimationFrame(f)); requestAnimationFrame(f); });

export function mountDemo(root) {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  // the player is made once the invented library has come (start())
  let sim = null, visible = false;
  const setVisible = v => { visible = v; sim?.setVisible(v || !!big.el); };
  // full size: the same app, the page's whole window (made on its button's click)
  const big = { el: null, sw: null, view: 'lib', frames: {} };

  // the main window's poster has no version baked in: it gets the page's one (index.html .boot-ver)
  const ver = root.querySelector('.boot-ver')?.textContent || '';
  root.classList.remove('demo-boot');
  root.classList.add('demo');
  root.innerHTML = '';
  const desk = el('div', 'demo-desk');
  const plane = el('div', 'demo-plane');
  const toast = el('div', 'demo-toast');
  // the tabs wait, unseen, for the Library window and come in with it
  const tabs = el('div', 'demo-tools wait'); tabs.setAttribute('role', 'tablist'); tabs.setAttribute('aria-label', 'Library window: what its centre shows');
  // the row over the desk: Full size (the two looks of the window turn there)
  const sw = el('div', 'demo-view demo-top wait');
  const fullBtn = el('button', 'rm-big'); fullBtn.type = 'button';
  fullBtn.innerHTML = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9"/></svg>Full size';
  fullBtn.addEventListener('click', () => openBig());
  sw.append(fullBtn);
  desk.append(sw, plane, tabs, toast);
  root.append(desk);

  const byId = {};
  for (const w of [MAIN, LIB]) {
    const box = el('div', 'dwin'); box.dataset.win = w.id;
    Object.assign(box.style, { width: w.w + 'px', height: w.h + 'px' });
    const img = new Image(); img.className = 'poster'; img.alt = ''; img.decoding = 'async';
    img.onerror = () => img.remove();
    // (the picture is fetched only if the app does not come)
    const loadPoster = () => { if (!img.src) img.src = w.poster; return (img.decode ? img.decode() : Promise.resolve()).catch(() => {}); };
    let setReady;
    const ready = new Promise(r => { setReady = r; });
    box.classList.add('hold');
    const frame = document.createElement('iframe');
    frame.title = 'AuraEngine — ' + w.title + ' window (live demo)';
    box.append(frame, img);
    if (w.id === 'main' && ver) { const v = el('span', 'poster-ver'); v.textContent = ver; box.append(v); }
    plane.append(box);
    byId[w.id] = { ...w, box, frame, ready, setReady, loadPoster };
  }
  byId.main.box.classList.add('main', 'top');
  byId.lib.box.classList.add('top');

  // ── the tools' tabs ──
  const tabOf = {};
  for (const t of TOOLS) {
    const b = el('button'); b.type = 'button'; b.textContent = t.name; b.setAttribute('role', 'tab');
    b.setAttribute('aria-selected', 'false');
    b.addEventListener('click', () => { stopTurning(); tool(t.id); });
    tabs.append(b); tabOf[t.id] = b;
  }
  let cur = null, turning = !reduced, turnT = 0, libLive = false;
  function mark(id) {
    cur = id;
    for (const t of TOOLS) tabOf[t.id].setAttribute('aria-selected', String(t.id === id));
  }
  // shows `id` in the Library window's centre (its window glows when the
  // ask came from the compact one)
  function tool(id, from) {
    if (!TOOLS.some(t => t.id === id)) id = 'analyzer';
    mark(id);
    try { byId.lib.frame.contentWindow.__demoTool?.(id); } catch (_) { /* not up yet */ }
    if (from === 'main') setTimeout(() => glow('lib'), 350);
    if (turning) schedule();
  }
  function schedule(ms = TURN_MS) {
    clearTimeout(turnT);
    if (!turning) return;
    turnT = setTimeout(() => {
      if (!visible || !libLive || big.el) { schedule(1500); return; }
      const k = TOOLS.findIndex(t => t.id === cur);
      tool(TOOLS[(k + 1) % TOOLS.length].id);
    }, ms);
  }
  function stopTurning() { turning = false; clearTimeout(turnT); }

  let phone = false, s = 1, started = false, onScreen = false;
  // Lays the desk out for its width. Side by side: both windows at one scale
  // s, the desk as tall as the screen allows and never wider than the stage.
  // Stacked (a phone-wide page): the compact window at its own kc across the
  // page, the Library window under it at the page's width, its tabs under it.
  // The windows sit on the plane (unscaled, px), each scaled by itself.
  function place() {
    const W = root.clientWidth, th = tabs.offsetHeight || 34;
    const at = (w, x, y, k) => { w.box.style.transform = `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px) scale(${k.toFixed(4)})`; };
    let dw, H, tabsTop;
    if (phone) {
      const kc = Math.min(1, W / MAIN.w), ly = Math.round(MAIN.h * kc) + STACK_GAP;
      s = W / LIB.w;
      dw = W;
      at(byId.main, (W - MAIN.w * kc) / 2, 0, kc);
      at(byId.lib, 0, ly, s);
      Object.assign(tabs.style, { left: '0px', width: W + 'px' });
      tabsTop = ly + Math.round(LIB.h * s) + 12;
      H = tabsTop + th;
    } else {
      const maxH = Math.max(440, innerHeight - 110 - TOP_H);
      s = Math.min(1, maxH / DESK_H, W / DESK_W);
      dw = Math.round(DESK_W * s);
      at(byId.main, 0, 0, s);
      at(byId.lib, LIB_X * s, LIB_Y * s, s);
      Object.assign(tabs.style, { left: Math.round(LIB_X * s) + 'px', width: Math.round(LIB.w * s) + 'px' });
      tabsTop = Math.round((LIB_Y + LIB.h) * s + Math.max(8, 14 * s));
      H = Math.max(Math.round(DESK_H * s), tabsTop + th);
    }
    desk.style.width = dw + 'px';
    desk.style.height = H + TOP_H + 'px';
    plane.style.top = TOP_H + 'px';
    tabs.style.top = TOP_H + tabsTop + 'px';
    sw.style.width = dw + 'px';
  }
  function glow(id) {
    const w = byId[id]; if (!w) return;
    w.box.classList.remove('glow'); void w.box.offsetWidth; w.box.classList.add('glow');
    clearTimeout(w.glowT); w.glowT = setTimeout(() => w.box.classList.remove('glow'), 1700);
  }
  let toastT = 0;
  // a short note at the foot of the window it came from (the compact one
  // when the page does not say)
  function say(text, from) {
    const w = byId[from] || byId.main;
    const r = w.box.getBoundingClientRect(), d = desk.getBoundingClientRect();
    toast.style.left = (r.left - d.left + r.width / 2).toFixed(0) + 'px';
    toast.style.top = 'auto';
    toast.style.bottom = Math.max(8, d.bottom - r.bottom + 14).toFixed(0) + 'px';
    toast.textContent = text; toast.classList.add('on');
    clearTimeout(toastT); toastT = setTimeout(() => toast.classList.remove('on'), 2200);
  }
  // a script or style of a window that did not arrive (a dropped connection)
  // leaves it blank: load that window again, twice at most
  function reload(w) {
    if (w.reloadT || (w.reloads || 0) >= 2) return;
    w.reloads = (w.reloads || 0) + 1;
    w.reloadT = setTimeout(() => { w.reloadT = 0; w.frame.src = w.src; }, 300);
  }
  const broken = from => { const w = byId[from]; if (w && started) reload(w); };
  // a tool asked for by a page: in the Library window's centre (in full
  // size the compact window turns into the Library one for it)
  const openTool = id => {
    stopTurning();
    if (big.el) {
      id = id === 'analyzer-live' ? 'analyzer' : id;
      bigView('lib');
      const go = () => { try { big.frames.lib.contentWindow.__demoTool?.(id); } catch (_) { /* not up yet */ } };
      go(); setTimeout(go, 400);
      return;
    }
    tool(id === 'analyzer-live' ? 'analyzer' : id, 'main');
  };
  const host = { glow, say, front() {}, broken, tool: openTool, touched: stopTurning,
    // the app's COMPACT / LIBRARY button: in full size the window turns; on
    // the page both looks are there already, the asked one lights up
    mode: (v, win) => {
      if (big.el && win && Object.values(big.frames).some(f => f.contentWindow === win)) bigView(v);
      else if (v === 'lib' || v === 'main') { stopTurning(); glow(v); }
    } };

  // ── full size: the same two looks of the window, in the page's whole window ──
  function openBig() {
    if (big.el || !sim) return;
    stopTurning();
    const o = el('div', 'rmm demo-big'); o.setAttribute('role', 'dialog'); o.setAttribute('aria-label', 'AuraEngine (live demo, full size)');
    const x = el('button', 'rmm-x'); x.type = 'button'; x.setAttribute('aria-label', 'Close');
    x.innerHTML = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M3.5 3.5l9 9M12.5 3.5l-9 9"/></svg>';
    const bsw = el('div', 'demo-view'); bsw.setAttribute('role', 'group'); bsw.setAttribute('aria-label', 'The app window: Library or compact');
    for (const [v, name] of [['lib', 'Library'], ['main', 'Compact']]) {
      const b = el('button'); b.type = 'button'; b.textContent = name; b.dataset.v = v;
      b.addEventListener('click', () => bigView(v));
      bsw.append(b);
    }
    const stage = el('div', 'demo-big-stage');
    for (const w of [LIB, MAIN]) {
      const f = document.createElement('iframe');
      f.title = 'AuraEngine — ' + w.title + ' window (live demo, full size)';
      f.src = w.src; stage.append(f); big.frames[w.id] = f;
    }
    o.append(bsw, stage, x);
    document.body.append(o);
    Object.assign(big, { el: o, sw: bsw, stage, view: innerWidth < PHONE_BELOW ? 'main' : 'lib' });
    x.addEventListener('click', closeBig);
    o.addEventListener('click', e => { if (e.target === o || e.target === stage) closeBig(); });
    document.documentElement.style.overflow = 'hidden';
    sim.setVisible(true);
    fitBig();
    setTimeout(() => { try { x.focus(); } catch (_) {} }, 300);
  }
  function closeBig() {
    if (!big.el) return;
    big.el.remove(); big.el = null; big.frames = {};
    document.documentElement.style.overflow = '';
    setVisible(visible);
    fullBtn.focus();
  }
  function bigView(v) {
    if (!big.el || (v !== 'lib' && v !== 'main')) return;
    big.view = v; fitBig();
  }
  // each look at its own size, the room allowing: the Library window up to
  // 1440 x 940 (never under 1120 x 800, scaled down then), the compact one
  // at its own 480 x 942; the one not shown waits unseen
  function fitBig() {
    if (!big.el) return;
    const r = big.stage.getBoundingClientRect(), aw = r.width, ah = r.height;
    for (const id in big.frames) {
      const f = big.frames[id], lib = id === 'lib';
      const w0 = lib ? Math.min(1440, Math.max(1120, aw)) : MAIN.w, h0 = lib ? Math.min(940, Math.max(800, ah)) : MAIN.h;
      const k = Math.min(1, aw / w0, ah / h0);
      Object.assign(f.style, { width: w0 + 'px', height: h0 + 'px',
        left: ((aw - w0 * k) / 2).toFixed(1) + 'px', top: ((ah - h0 * k) / 2).toFixed(1) + 'px',
        transform: `scale(${k.toFixed(4)})`, visibility: id === big.view ? 'visible' : 'hidden' });
    }
    for (const b of big.sw.querySelectorAll('button')) b.setAttribute('aria-pressed', String(b.dataset.v === big.view));
  }
  addEventListener('resize', fitBig);
  addEventListener('keydown', e => { if (e.key === 'Escape') closeBig(); });
  function makeSim(tracks) {
    let lib = null, entries = null;
    if (tracks) {
      lib = createLibrary(tracks, new URL(LIB_DATA + 'covers/', location.href).href);
      // the list it starts with: the newest album
      const first = lib.invoke('lib_albums', {})[0];
      entries = lib.invoke('lib_album', { id: first.id }).list.map(t => ({ path: t.path, name: t.path.split('\\').pop(), convs: [] }));
    }
    sim = createSim({ reducedMotion: reduced, tracks, entries, lib });
    sim.host = host;
    window.__auraSim = sim;
    sim.setVisible(visible);
  }

  function layout() {
    phone = root.clientWidth < PHONE_BELOW;
    root.classList.toggle('phone', phone);
    place();
  }
  new ResizeObserver(layout).observe(root);
  layout();
  // the first placing is not animated
  requestAnimationFrame(() => requestAnimationFrame(() => root.classList.add('ready')));
  setTimeout(() => root.classList.add('ready'), 500);

  // The windows come in one after another, the compact one first. With the
  // page's intro (index.html) they wait for all of them and the page, and
  // come in as the intro goes.
  const shown = () => [byId.main, byId.lib];
  async function revealAll() {
    const all = shown();
    const intro = window.__dzIntro;
    if (intro) {
      let n = 0;
      all.forEach(w => w.ready.then(() => intro.progress(++n, all.length)));
      await Promise.all(all.map(w => w.ready));
      if (document.readyState !== 'complete') await new Promise(r => addEventListener('load', r, { once: true }));
      const left = INTRO_MIN - performance.now();
      if (left > 0) await wait(left);
      await intro.out();
    }
    let next = 0;
    for (const w of all) {
      await w.ready;
      const d = next - performance.now();
      if (d > 0) await wait(d);
      show(w);
      next = performance.now() + (reduced ? 0 : STAGGER);
    }
  }
  function show(w) {
    if (w.shown) return; w.shown = true;
    if (reduced) w.box.classList.remove('hold'); else auraShow(w.box, w.w, w.h);
    if (w.id === 'lib') tabs.classList.remove('wait');
    sw.classList.remove('wait');
  }

  // A window's app counts as ready once it has loaded, its fonts are in and
  // its page has been quiet for a moment (1.5 s at most).
  async function settle(w) {
    let doc = null;
    try { doc = w.frame.contentDocument; } catch (_) { /* not ours */ }
    try { await doc?.fonts?.ready; } catch (_) { /* none */ }
    if (doc?.body) await new Promise(r => {
      let t = setTimeout(done, 250);
      const cap = setTimeout(done, 1500);
      const mo = new MutationObserver(() => { clearTimeout(t); t = setTimeout(done, 250); });
      mo.observe(doc.body, { childList: true, subtree: true, attributes: true, characterData: true });
      function done() { clearTimeout(t); clearTimeout(cap); mo.disconnect(); r(); }
    });
    await frames(2);
    w.box.classList.add('live');
    w.setReady();
    if (w.id === 'lib') {
      libLive = true;
      // it opens on the library itself; the tools follow
      mark('albums');
      if (turning) schedule(FIRST_TURN_MS);
    }
  }
  function load(w) {
    if (w.loading) return; w.loading = true;
    w.frame.addEventListener('load', () => settle(w));
    // the app's own start-up notice says a script did not come
    w.frame.addEventListener('load', () => {
      let bad = false;
      try { bad = /Failed to load/.test(w.frame.contentDocument.getElementById('bootError')?.textContent || ''); } catch (_) { /* not ours */ }
      if (bad) reload(w);
    });
    w.frame.src = w.src;
  }
  // the Library window after the compact one (the two read the same
  // settings, the first one seeds them)
  function startOther() {
    byId.main.ready.then(() => load(byId.lib));
  }

  // Start lazily: once the demo is on screen. The library comes first.
  const libData = fetch(LIB_DATA + 'demo-lib.json').then(r => (r.ok ? r.json() : Promise.reject(new Error(r.status))));
  const start = () => {
    if (started) return; started = true;
    setTimeout(() => {
      for (const w of shown()) if (!w.box.classList.contains('live')) w.loadPoster().then(w.setReady);
    }, LIVE_GIVEUP);
    libData.then(makeSim, () => makeSim(null)).then(() => {
      const first = byId.main;
      load(first);
      startOther();
      // the first track starts by itself once the first window is up
      // (silent, as every sound of the demo); not where motion is to be kept down
      if (!reduced) first.frame.addEventListener('load', () => setTimeout(() => {
        sim.invoke('player_status').then(st => { if (!st.trackId) sim.invoke('player_play', {}); });
      }, 1200), { once: true });
    });
    revealAll();
  };
  const io = new IntersectionObserver(es => {
    onScreen = es.some(e => e.isIntersecting);
    setVisible(onScreen && !document.hidden);
    if (onScreen) start();
  }, { rootMargin: '200px' });
  io.observe(root);
  // a page that paints no frames yet (opened in a background tab) gets no
  // observer callback: check the box itself once the page has loaded
  const inView = () => { const r = root.getBoundingClientRect(); return r.bottom > -200 && r.top < innerHeight + 200; };
  const late = () => setTimeout(() => { if (!started && inView()) { onScreen = true; setVisible(!document.hidden); start(); } }, 300);
  document.readyState === 'complete' ? late() : addEventListener('load', late, { once: true });
  // (a window of no size at load, then opened up, the same way)
  addEventListener('resize', () => { if (!started && document.readyState === 'complete') late(); });
  document.addEventListener('visibilitychange', () => setVisible(onScreen && !document.hidden));
}

function el(tag, cls) { const e = document.createElement(tag); if (cls) e.className = cls; return e; }
