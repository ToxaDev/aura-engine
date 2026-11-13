// ── the custom filter button while Hybrid-Phase is on ─────────────────
// Hybrid-Phase blends two filters of its own, so a loaded .npy has nothing
// to replace: the button holds while it is on. It is not disabled — a
// disabled button takes no pointer, shows no tooltip and so could not say
// why (Anton 23.10: Stop let the player's hold go and the button stayed
// dead without a word). It stays dimmed with the not-allowed cursor, its
// tooltip says why, and a press shows that tooltip at once (converter.js).
// While the player holds the button too, its tooltip is the player's and
// the one put back on Stop is data-tip-idle (player.js): that is the one
// swapped here.

// UI texts.
export const HP_FILTER_TIP = 'Hybrid-Phase is on: it blends two built-in filters, so a custom filter can\'t be loaded. Switch Hybrid-Phase off to load one.';

/// Hold (`on`) or let go the button for Hybrid-Phase.
export function setFilterBtnHpLock(btn, on) {
    if (!btn) return;
    btn.disabled = false;
    btn.style.opacity = on ? '0.35' : '1';
    btn.style.cursor = on ? 'not-allowed' : '';
    if (filterBtnHpLocked(btn) === on) return;
    // A title not yet moved to data-tip by tooltip.js is the tip to keep.
    const title = btn.getAttribute('title');
    if (title !== null) {
        btn.removeAttribute('title');
        if (title) btn.dataset.tip = title;
    }
    const key = btn.dataset.tipIdle !== undefined ? 'tipIdle' : 'tip';
    if (on) {
        btn.dataset.hpLocked = '1';
        btn.dataset.tipHp = btn.dataset[key] ?? '';
        btn.dataset[key] = HP_FILTER_TIP;
    } else {
        const was = btn.dataset.tipHp;
        delete btn.dataset.hpLocked;
        delete btn.dataset.tipHp;
        if (was) btn.dataset[key] = was;
        else delete btn.dataset[key];
    }
}

export function filterBtnHpLocked(btn) {
    return btn?.dataset.hpLocked === '1';
}
