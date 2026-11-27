// A window comes in through an alpha mask drawn by moving strands, like the
// aura strands of the page's background. Three masks are painted on a small
// canvas each frame and stretched over the window:
//   the window itself (where the strands have passed),
//   a soft lit edge along the front (the glow layer),
//   a band behind the front (the fog layer: a local tone change through
//   backdrop-filter that clears again once the front has moved on).

const DUR = 400, GLOW = .3, FOG = .25;   // ms; the layers' strength
const W = .34, AMP = .12, LAG = .55;     // the front's softness and waviness (shares of the window), the fog's lag (of DUR)
const WAVES = [[.9, .3, 1], [2.3, -.45, 2.6], [4.7, .7, .3]];   // frequency, speed, phase
const GLOW_BG = 'linear-gradient(135deg, #a78bfa, #f0abfc 35%, #67e8f9)';
const FOG_BG = 'rgba(120, 90, 220, .10)', FOG_BF = 'saturate(.35) contrast(.85) brightness(1.12)';

const sm = t => t * t * (3 - 2 * t);
const ease = t => (t < .5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
const clamp01 = v => (v < 0 ? 0 : v > 1 ? 1 : v);

function layer(cls) { const d = document.createElement('div'); d.className = cls; return d; }
function setMask(el, url) {
  el.style.webkitMaskImage = el.style.maskImage = url ? `url(${url})` : '';
}

// Runs the reveal on a window box (w × h px); resolves when it is whole.
export function auraShow(box, w, h) {
  const N = 112, cw = w >= h ? N : Math.max(24, Math.round(N * w / h)), ch = h >= w ? N : Math.max(24, Math.round(N * h / w));
  const mk = () => { const cv = document.createElement('canvas'); cv.width = cw; cv.height = ch; const g = cv.getContext('2d'); return { cv, g, im: g.createImageData(cw, ch) }; };
  const M = mk(), E = mk(), F = mk();
  for (const L of [M, E, F]) for (let i = 0; i < L.im.data.length; i += 4) L.im.data[i] = L.im.data[i + 1] = L.im.data[i + 2] = 255;
  const fog = layer('dz-fog'), glow = layer('dz-glow');
  box.append(fog, glow);
  glow.style.background = GLOW_BG; glow.style.opacity = String(GLOW);
  fog.style.background = FOG_BG; fog.style.backdropFilter = fog.style.webkitBackdropFilter = FOG_BF; fog.style.opacity = String(FOG);
  const seed = Math.random() * 10;
  const span = 1 + W + 2 * AMP;
  const front = q => q * span - AMP;

  function paint(el, time) {
    const md = M.im.data, ed = E.im.data, fd = F.im.data;
    const fr = front(ease(clamp01(el))), ff = front(ease(clamp01(el - LAG)));
    for (let y = 0, i = 3; y < ch; y++) {
      const v = (y + .5) / ch;
      for (let x = 0; x < cw; x++, i += 4) {
        const u = (x + .5) / cw;
        const s = (u + 1 - v) / 2, t = (u + v) / 2;
        let wob = 0;
        for (const [f, sp, ph] of WAVES) wob += Math.sin(6.2832 * (f * t + sp * time) + ph + seed);
        wob *= AMP / WAVES.length;
        const a = sm(clamp01((fr + wob - s) / W));
        const a2 = sm(clamp01((ff + wob - s) / W));
        md[i] = a * 255;
        ed[i] = Math.pow(4 * a * (1 - a), 1.6) * 255;
        fd[i] = a * (1 - a2) * 255;
      }
    }
    M.g.putImageData(M.im, 0, 0); E.g.putImageData(E.im, 0, 0); F.g.putImageData(F.im, 0, 0);
    setMask(box, M.cv.toDataURL()); setMask(glow, E.cv.toDataURL()); setMask(fog, F.cv.toDataURL());
  }

  box.classList.add('aura');
  paint(0, 0);
  box.classList.remove('hold');
  return new Promise(res => {
    const t0 = performance.now();
    const step = now => {
      const el = (now - t0) / DUR;
      paint(Math.min(el, 1 + LAG), (now - t0) / 1000);
      if (el < 1 + LAG) { requestAnimationFrame(step); return; }
      setMask(box, '');
      fog.remove(); glow.remove();
      box.classList.remove('aura');
      res();
    };
    requestAnimationFrame(step);
  });
}
