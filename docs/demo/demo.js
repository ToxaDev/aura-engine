// The live demo in the hero: the app's own pages (main window, Aura EQ, XTC
// geometry, the analyzer) run in iframes over a simulated player
// (demo/sim.js) — silent, driven by level and spectrum envelopes measured
// from the real tracks. This file only builds the little desktop: the main
// window in front on the right, always sharp; the other three stacked
// behind it in depth — the further back, the smaller and softer — and
// shuffled forward on a click; the glow when a window is "opened"; on a
// phone the main window and, under it, one of the others chosen by tabs;
// lazy start. Windows move by transform and filter only.
import { createSim } from './sim.js?v=201-3';
import { auraShow } from './aura-reveal.js?v=198-2';

const V = '?v=201-3';
const MAIN = { id: 'main', title: 'Player', src: 'demo/app/index.html' + V, w: 440, h: 932,
  poster: 'media/shot-main-195.webp?v=197-1' };
const SIDE = [
  { id: 'eq',  title: 'Aura EQ',  src: 'demo/app/eq.html' + V, w: 820, h: 700, poster: 'media/shot-eq.webp' },
  { id: 'an',  title: 'Analyzer', src: 'demo/app/analyzer.html?mode=live&v=196-2', w: 1180, h: 720, poster: 'media/shot-analyzer.webp' },
  { id: 'xtc', title: 'XTC',      src: 'demo/app/xtc-geometry.html' + V, w: 730, h: 820, poster: 'media/shot-xtc.webp' },
];
// The virtual desk, in the windows' own pixels: the main window at its
// left edge, whole; to its right a column the other three share, never
// over the main one. The column is as wide as the stage leaves it (COL_MIN
// to COL_MAX). In it three depth slots, front to back: the box the window
// is fitted into (keeping its proportions) as shares of the column's width
// and the desk's height, its top, how soft it is (px on screen) and how
// much light it gets.
const DESK_H = 960, GAP = 30, COL_MIN = 420, COL_MAX = 820;
const MAIN_X = 0, MAIN_Y = (DESK_H - MAIN.h) / 2, COL_X = MAIN.w + GAP;
const SLOTS = [
  { bw: 1,   bh: .72, top: .25, blur: 0,   dim: 1 },
  { bw: .88, bh: .5,  top: .125, blur: 1.2, dim: .85 },
  { bw: .76, bh: .4,  top: 0,   blur: 2.4, dim: .68 },
];
// (a staircase: each window further back sits higher and keeps to the
// column's right edge, so its title bar stays in sight above the one in
// front)
const PHONE_BELOW = 640;          // container width under which the windows go one under another
const TABS_H = 46;                // the phone's tab bar and its gaps, px

// A window waits, unseen, until the working app in it has drawn itself, and
// comes in only then. The app not ready after this long: its picture instead.
const LIVE_GIVEUP = 9000;
const INTRO_MIN = 1500;            // the page's intro (index.html) stays this long, ms from the page's start
const STAGGER = 280;               // between two windows coming in, ms
// how far each window behind the front one keeps off the column's right
// edge, as shares of the column: their right edges never stand on one line
const OFFSET = [0, .09, .18];
const wait = ms => new Promise(r => setTimeout(r, ms));
const frames = n => new Promise(r => { const f = () => (--n <= 0 ? r() : requestAnimationFrame(f)); requestAnimationFrame(f); });

export function mountDemo(root) {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const sim = createSim({ reducedMotion: reduced });
  window.__auraSim = sim;

  // the main window's poster has no version baked in: it gets the page's one (index.html .boot-ver)
  const ver = root.querySelector('.boot-ver')?.textContent || '';
  root.classList.remove('demo-boot');
  root.classList.add('demo');
  root.innerHTML = '';
  const desk = el('div', 'demo-desk');
  const plane = el('div', 'demo-plane');
  const toast = el('div', 'demo-toast');
  const tabs = el('div', 'demo-tabs'); tabs.setAttribute('role', 'tablist'); tabs.setAttribute('aria-label', 'More windows');
  desk.append(plane, tabs, toast);
  root.append(desk);

  const byId = {};
  for (const w of [MAIN, ...SIDE]) {
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
    const glass = el('div', 'glass');
    if (w.id !== 'main') glass.title = 'Bring ' + w.title + ' forward';
    glass.addEventListener('pointerdown', e => { e.preventDefault(); forward(w.id); });
    box.append(frame, img);
    if (w.id === 'main' && ver) { const v = el('span', 'poster-ver'); v.textContent = ver; box.append(v); }
    box.append(glass);
    plane.append(box);
    byId[w.id] = { ...w, box, frame, ready, setReady, loadPoster };
    if (w.id !== 'main') {
      const tab = el('button'); tab.type = 'button'; tab.textContent = w.title; tab.setAttribute('role', 'tab');
      tab.addEventListener('click', () => forward(w.id));
      tabs.append(tab);
      byId[w.id].tab = tab;
    }
  }
  byId.main.box.classList.add('main', 'top');
  byId.main.box.style.zIndex = '40';

  const stack = SIDE.map(w => w.id);       // front to back
  let phone = false, s = 1, pm = null;     // pm: the phone's placing
  let col = COL_MAX;                       // the helpers' column width, desk px

  function place() {
    stack.forEach((id, i) => {
      const w = byId[id];
      w.box.style.zIndex = String(30 - i);
      w.box.classList.toggle('top', i === 0);
      w.box.dataset.depth = String(i);
      if (w.tab) w.tab.setAttribute('aria-selected', String(i === 0));
      if (phone) {
        const k = Math.min(1, pm.W / w.w);
        w.box.style.transform = `translate(${((pm.W - w.w * k) / 2).toFixed(1)}px, ${pm.y.toFixed(1)}px) scale(${k.toFixed(4)})`;
        w.box.style.filter = 'none';
        return;
      }
      const sl = SLOTS[i], k = Math.min(1, sl.bw * col / w.w, sl.bh * DESK_H / w.h);
      // the front one in the middle of the column's lower part; each one
      // behind it a step higher, at the column's right edge
      const f = byId[stack[0]], kf = Math.min(1, SLOTS[0].bw * col / f.w, SLOTS[0].bh * DESK_H / f.h);
      const y0 = SLOTS[0].top * DESK_H + (DESK_H * (1 - SLOTS[0].top) - f.h * kf) / 2;
      const x = i === 0 ? COL_X : Math.max(COL_X, COL_X + col * (1 - OFFSET[i]) - w.w * k);
      const y = y0 - (SLOTS[0].top - sl.top) * DESK_H;
      w.box.style.transform = `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px) scale(${k.toFixed(4)})`;
      // blur() works in the box's own pixels, before the scales: divide
      // by them so the softness on screen is the slot's
      w.box.style.filter = sl.blur
        ? `blur(${(sl.blur / Math.max(.2, k * s)).toFixed(2)}px) brightness(${sl.dim})` : 'none';
    });
    if (phone) {
      const m = byId.main, k = Math.min(1, pm.W / m.w);
      m.box.style.transform = `translate(${((pm.W - m.w * k) / 2).toFixed(1)}px, 0) scale(${k.toFixed(4)})`;
      const f = byId[stack[0]];
      desk.style.height = Math.round(pm.y + f.h * Math.min(1, pm.W / f.w)) + 'px';
    } else {
      byId.main.box.style.transform = `translate(${MAIN_X}px, ${MAIN_Y}px)`;
    }
  }
  function forward(id) {
    const i = stack.indexOf(id);
    if (i <= 0) return;
    stack.splice(i, 1); stack.unshift(id);
    place();
  }
  function glow(id) {
    const w = byId[id]; if (!w) return;
    forward(id);
    w.box.classList.remove('glow'); void w.box.offsetWidth; w.box.classList.add('glow');
    clearTimeout(w.glowT); w.glowT = setTimeout(() => w.box.classList.remove('glow'), 1700);
    if (phone) w.box.scrollIntoView?.({ block: 'nearest', behavior: reduced ? 'auto' : 'smooth' });
  }
  let toastT = 0;
  // a short note at the foot of the window it came from (the main one when
  // the page does not say)
  const WIN_OF = { main: 'main', eq: 'eq', xtc: 'xtc', 'analyzer-live': 'an' };
  function say(text, from) {
    const w = byId[WIN_OF[from] || from] || byId.main;
    const r = w.box.getBoundingClientRect(), d = desk.getBoundingClientRect();
    toast.style.left = (r.left - d.left + r.width / 2).toFixed(0) + 'px';
    toast.style.top = 'auto';
    toast.style.bottom = Math.max(8, d.bottom - r.bottom + 14).toFixed(0) + 'px';
    toast.textContent = text; toast.classList.add('on');
    clearTimeout(toastT); toastT = setTimeout(() => toast.classList.remove('on'), 2200);
  }
  // a script or style of a window that did not arrive (a dropped connection)
  // leaves it blank (the analyzer stood empty behind the EQ): load that
  // window again, twice at most
  function reload(w) {
    if (w.reloadT || (w.reloads || 0) >= 2) return;
    w.reloads = (w.reloads || 0) + 1;
    w.reloadT = setTimeout(() => { w.reloadT = 0; w.frame.src = w.src; }, 300);
  }
  const broken = from => { const w = byId[WIN_OF[from] || from]; if (w && started) reload(w); };
  sim.host = { glow, say, front: forward, broken };

  function layout() {
    const W = root.clientWidth;
    phone = W < PHONE_BELOW;
    root.classList.toggle('phone', phone);
    if (phone) {
      // one plane, unscaled: each window carries its own scale
      const mh = MAIN.h * Math.min(1, W / MAIN.w);
      pm = { W, y: mh + TABS_H + 8 };
      plane.style.width = W + 'px'; plane.style.height = '0';
      plane.style.transform = 'none';
      desk.style.width = W + 'px';
      tabs.style.top = Math.round(mh + 12) + 'px';
      s = 1;
    } else {
      // the main window as tall as the screen allows; the column takes the
      // width left, and only when even its narrowest does not fit does the
      // whole desk get smaller
      const maxH = Math.max(440, innerHeight - 110);
      s = Math.min(1, maxH / DESK_H, W / (COL_X + COL_MIN));
      col = Math.max(COL_MIN, Math.min(COL_MAX, W / s - COL_X));
      const deskW = COL_X + col;
      plane.style.width = deskW + 'px'; plane.style.height = DESK_H + 'px';
      plane.style.transform = `scale(${s.toFixed(4)})`;
      desk.style.width = Math.round(deskW * s) + 'px';
      desk.style.height = Math.round(DESK_H * s) + 'px';
    }
    place();
  }
  new ResizeObserver(layout).observe(root);
  layout();
  // the first placing is not animated
  requestAnimationFrame(() => requestAnimationFrame(() => root.classList.add('ready')));
  setTimeout(() => root.classList.add('ready'), 500);

  // The windows come in one after another, the main one first, then the
  // others from the back to the front. With the page's intro (index.html)
  // they wait for all of them and the page, and come in as the intro goes.
  async function revealAll() {
    const all = Object.values(byId);
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
    for (const id of ['main', ...stack.slice().reverse()]) {
      const w = byId[id];
      await w.ready;
      const d = next - performance.now();
      if (d > 0) await wait(d);
      if (reduced) w.box.classList.remove('hold'); else auraShow(w.box, w.w, w.h);
      next = performance.now() + (reduced ? 0 : STAGGER);
    }
  }
  revealAll();

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
  }

  // Start lazily: once the demo is on screen.
  let started = false, onScreen = false;
  const start = () => {
    if (started) return; started = true;
    setTimeout(() => {
      for (const w of Object.values(byId)) if (!w.box.classList.contains('live')) w.loadPoster().then(w.setReady);
    }, LIVE_GIVEUP);
    for (const w of Object.values(byId)) {
      w.frame.addEventListener('load', () => settle(w));
      w.frame.src = w.src;
      // the main window's own start-up notice says a script did not come
      w.frame.addEventListener('load', () => {
        let bad = false;
        try { bad = /Failed to load/.test(w.frame.contentDocument.getElementById('bootError')?.textContent || ''); } catch (_) { /* not ours */ }
        if (bad) reload(w);
      });
    }
    // the first track starts by itself once the main window is up (silent,
    // as every sound of the demo); not where motion is to be kept down
    if (!reduced) byId.main.frame.addEventListener('load', () => setTimeout(() => {
      sim.invoke('player_status').then(st => { if (!st.trackId) sim.invoke('player_play', { id: 1 }); });
    }, 1200), { once: true });
  };
  const io = new IntersectionObserver(es => {
    onScreen = es.some(e => e.isIntersecting);
    sim.setVisible(onScreen && !document.hidden);
    if (onScreen) start();
  }, { rootMargin: '200px' });
  io.observe(root);
  // a page that paints no frames yet (opened in a background tab) gets no
  // observer callback: check the box itself once the page has loaded
  const inView = () => { const r = root.getBoundingClientRect(); return r.bottom > -200 && r.top < innerHeight + 200; };
  const late = () => setTimeout(() => { if (!started && inView()) { onScreen = true; sim.setVisible(!document.hidden); start(); } }, 300);
  document.readyState === 'complete' ? late() : addEventListener('load', late, { once: true });
  // (a window of no size at load, then opened up, the same way)
  addEventListener('resize', () => { if (!started && document.readyState === 'complete') late(); });
  document.addEventListener('visibilitychange', () => sim.setVisible(onScreen && !document.hidden));
  return sim;
}

function el(tag, cls) { const e = document.createElement(tag); if (cls) e.className = cls; return e; }
