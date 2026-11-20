// The analyzer as pages on a phone (Anton 9.11): the window's numbers and
// each of its graphs take the whole screen, one at a time. A swipe left or
// right moves between them; the dots at the foot show how many there are and
// which one is up, and take a tap. Wider than a phone, the window's own
// layout (numbers beside the graphs, the graphs as tabs) stays as it is.

const NARROW = matchMedia('(max-width: 759px)');
const root = document.documentElement;

function whenReady(fn) {
    const go = () => (document.getElementById('anTabs') ? fn() : setTimeout(go, 150));
    go();
}

whenReady(() => {
    const body = document.getElementById('anBody');
    const tabsBar = document.getElementById('anTabs');
    const anRoot = document.getElementById('anRoot') || document.body;
    if (!body || !tabsBar) return;

    const foot = document.createElement('div');
    foot.id = 'rmAnFoot';
    foot.innerHTML = '<button type="button" class="rm-an-step" data-d="-1" aria-label="Previous">‹</button><div class="rm-an-mid"><div class="rm-an-name"></div><div class="rm-an-dots" role="tablist"></div></div><button type="button" class="rm-an-step" data-d="1" aria-label="Next">›</button>';
    anRoot.append(foot);
    const dots = foot.querySelector('.rm-an-dots');
    const name = foot.querySelector('.rm-an-name');

    // The pages: the numbers, then every graph tab that is shown now.
    const pages = () => [{ id: 'metrics', title: 'Metrics' },
        ...[...tabsBar.querySelectorAll('.an-tab')].filter((t) => !t.hidden).map((t) => ({ id: t, title: t.textContent.trim() }))];
    let at = 0;

    function paint() {
        const ps = pages();
        at = Math.max(0, Math.min(ps.length - 1, at));
        dots.replaceChildren(...ps.map((p, i) => {
            const d = document.createElement('button');
            d.type = 'button';
            d.className = 'rm-an-dot' + (i === at ? ' on' : '');
            d.setAttribute('aria-label', p.title);
            d.addEventListener('click', () => go(i));
            return d;
        }));
        name.textContent = ps[at].title;
        foot.querySelector('[data-d="-1"]').disabled = at === 0;
        foot.querySelector('[data-d="1"]').disabled = at === ps.length - 1;
    }
    function go(i, dir) {
        const ps = pages();
        if (i < 0 || i >= ps.length) return;
        const from = at;
        at = i;
        const p = ps[at];
        if (p.id === 'metrics') root.dataset.anPage = 'metrics';
        else { root.dataset.anPage = 'graph'; p.id.click(); }
        body.classList.remove('rm-an-in-l', 'rm-an-in-r');
        void body.offsetWidth;
        body.classList.add((dir ?? Math.sign(at - from)) < 0 ? 'rm-an-in-l' : 'rm-an-in-r');
        paint();
        // The graphs size themselves on a resize: let them see their new room.
        window.dispatchEvent(new Event('resize'));
    }
    foot.querySelectorAll('.rm-an-step').forEach((b) => b.addEventListener('click', () => go(at + +b.dataset.d)));

    // A swipe: mostly sideways, long enough, quick enough. Not on the graphs
    // that are dragged sideways themselves (a selection on the waveform and
    // the spectrogram): there the dots and the arrows move.
    let t0 = null;
    body.addEventListener('touchstart', (e) => {
        if (!NARROW.matches || e.touches.length !== 1) { t0 = null; return; }
        const own = e.target.closest('#anWaveformSection canvas, #anSpectrogramSection canvas');
        t0 = own ? null : { x: e.touches[0].clientX, y: e.touches[0].clientY, t: performance.now() };
    }, { passive: true });
    body.addEventListener('touchend', (e) => {
        if (!t0) return;
        const c = e.changedTouches[0];
        const dx = c.clientX - t0.x, dy = c.clientY - t0.y;
        if (Math.abs(dx) > 60 && Math.abs(dx) > 1.6 * Math.abs(dy) && performance.now() - t0.t < 700) go(at + (dx < 0 ? 1 : -1));
        t0 = null;
    }, { passive: true });

    // A tab hidden or shown (the songs of a stream): the dots follow.
    new MutationObserver(paint).observe(tabsBar, { subtree: true, attributes: true, attributeFilter: ['hidden', 'class'] });
    const mode = () => {
        root.classList.toggle('rm-pager', NARROW.matches);
        if (NARROW.matches && !root.dataset.anPage) root.dataset.anPage = 'metrics';
        paint();
    };
    NARROW.addEventListener('change', mode);
    mode();
});
