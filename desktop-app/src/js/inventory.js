// What this installation can actually convert with.
//
// The filter blobs are a separate, very large download, so no two machines
// running this app necessarily have the same ones. A slider position with no
// blob behind it is a dead end the user only discovers after picking files —
// so the app asks the backend once at startup what is on disk, starts on a
// combination that works, and marks the ones that do not.

// The tap counts and FS multipliers the sliders offer, in slider order.
// One definition, shared by the sliders, the specs line and the availability
// check. These used to be three separate copies, so a change to the filter
// matrix had to be made in all of them or they would quietly disagree.
//
// The backend answers with the same grid — see `TAP_LADDER` and `TARGET_RATES`
// in `converter/dsp/filter.rs`. A tap count here that is not a rung there gets
// no inventory entry and would read as unavailable.
export const TAP_PRESETS = [1000000, 5000000, 10000000, 30000000];
export const FS_PRESETS = [2, 4, 8, 16];

// The two source families. Output rate = family base x FS multiplier.
const FAMILY_BASES = [44100, 48000];

// What a working install should land on: FS8 is the design point the filter
// matrix is built around and what the ready-made bundles ship.
const PREFERRED_FS = 8;
// When FS8 has no blobs, try these in order — nearest to FS8 first.
const FS_FALLBACK_ORDER = [8, 4, 16, 2];

export const inventory = {
    // False until the backend has answered. While false every combination
    // reads as available: see loadFilterInventory().
    known: false,
    // taps -> Map(outputRateHz -> { linear, minimum })
    cells: new Map(),
    // taps -> [{ name, url }]
    packs: new Map(),
    // Where a pack should be extracted, and everywhere that was searched.
    dest: '',
    searched: []
};

/// Ask the backend what is on disk. Safe to call more than once.
export async function loadFilterInventory() {
    try {
        // Resolved here rather than at module scope so the pure decision
        // functions below can be exercised outside a Tauri shell.
        const r = JSON.parse(await window.__TAURI__.tauri.invoke('filter_inventory'));
        inventory.cells.clear();
        for (const e of r.present || []) {
            if (!inventory.cells.has(e.taps)) inventory.cells.set(e.taps, new Map());
            inventory.cells.get(e.taps).set(e.rate, {
                linear: !!e.linear,
                minimum: !!e.minimum
            });
        }
        inventory.packs.clear();
        for (const [taps, list] of Object.entries(r.packs || {})) {
            inventory.packs.set(parseInt(taps, 10), list || []);
        }
        inventory.dest = r.dest || '';
        inventory.searched = r.searched || [];
        inventory.known = true;
    } catch (e) {
        // The probe failed. Leave `known` false, which makes every combination
        // read as available, and change nothing the user had selected.
        //
        // That direction is the safe one: a wrong "you have this" costs a
        // pre-flight error naming the exact file, which is the behaviour
        // without this module at all. A wrong "you don't" would hide filters
        // the user really has and silently move them off their own settings.
        inventory.known = false;
    }
    return inventory.known;
}

/// Does this installation have any filter at all?
export function anyFiltersInstalled() {
    return !inventory.known || inventory.cells.size > 0;
}

/// Can we convert at `taps` and this FS multiplier?
///
/// True when either source family is covered: a pack split by rate family (the
/// 30M ones) leaves the other family missing, and that is caught per-file by
/// the pre-flight check, which knows the actual source rates. Here we only
/// know the multiplier, so "some family works" is the strongest honest answer.
///
/// `needMinimum` demands the minimum-phase blob as well — Hybrid-Phase renders
/// both branches in full, so half a pair is not enough for it.
export function comboAvailable(taps, fs, needMinimum) {
    if (!inventory.known) return true;
    const rates = inventory.cells.get(taps);
    if (!rates) return false;
    return FAMILY_BASES.some(base => {
        const c = rates.get(base * fs);
        return !!c && c.linear && (!needMinimum || c.minimum);
    });
}

/// Is this tap count usable at any multiplier?
export function tapsAvailable(taps, needMinimum) {
    if (!inventory.known) return true;
    return FS_PRESETS.some(fs => comboAvailable(taps, fs, needMinimum));
}

/// The best working starting point, or null when nothing is installed.
///
/// FS8 wins across the whole ladder before any other multiplier is considered,
/// because it is the design point of the matrix and what the bundles ship;
/// only if no tap count has FS8 does a different multiplier come into play.
export function bestCombo(needMinimum) {
    const ladder = [...TAP_PRESETS].reverse();
    for (const taps of ladder) {
        if (comboAvailable(taps, PREFERRED_FS, needMinimum)) {
            return { taps, fs: PREFERRED_FS };
        }
    }
    for (const taps of ladder) {
        for (const fs of FS_FALLBACK_ORDER) {
            if (comboAvailable(taps, fs, needMinimum)) return { taps, fs };
        }
    }
    return null;
}

/// The closest working combination to one the user (or a previous install)
/// asked for. Returns null when nothing is installed.
export function nearestAvailable(taps, fs, needMinimum) {
    if (comboAvailable(taps, fs, needMinimum)) return { taps, fs };

    // Hold the tap count and move the multiplier first: filter size is the
    // deliberate quality choice, the multiplier just has to suit the DAC.
    const here = FS_PRESETS.indexOf(fs);
    const byDistance = [...FS_PRESETS].sort(
        (a, b) => Math.abs(FS_PRESETS.indexOf(a) - here)
                - Math.abs(FS_PRESETS.indexOf(b) - here)
    );
    for (const alt of byDistance) {
        if (comboAvailable(taps, alt, needMinimum)) return { taps, fs: alt };
    }

    // That tap count is not installed at all — keep the multiplier and take
    // the largest size that has it.
    for (const alt of [...TAP_PRESETS].reverse()) {
        if (comboAvailable(alt, fs, needMinimum)) return { taps: alt, fs };
    }

    return bestCombo(needMinimum);
}

/// The download(s) that would add a tap count. Two for 30M, whose blobs are
/// split by rate family; one for every other size.
export function packsFor(taps) {
    return inventory.packs.get(taps) || [];
}
