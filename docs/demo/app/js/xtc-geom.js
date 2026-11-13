// ══════════════════════════════════════════════════════════════════════
// The listening triangle: what makes one valid, and what it implies.
//
// Pure arithmetic, no DOM, no storage, no Tauri. It is imported by two
// separate windows — the converter's rack badge and the geometry window —
// and the whole reason it is its own file is that those two must never
// disagree about what counts as a triangle or where the thresholds sit.
//
// Every number below is derived, not chosen. Where a threshold comes from a
// wavelength, the wavelength is written down next to it.
// ══════════════════════════════════════════════════════════════════════

export const C_AIR = 343.0;      // m/s, dry air at 20 C — matches the Rust side
export const F_TOP = 8000.0;     // top of the XTC band (F_HI in xtc.rs)

/// Sanity rails. Not taste — the range in which the triangle is a triangle and
/// the filter is a filter. Values outside are rejected rather than clamped,
/// because a clamped number would be reported back as if it had been measured.
export const LIMITS = {
    span:  { min: 200, max: 5000, def: 2000, label: 'Speaker span',  hint: 'centre to centre' },
    distL: { min: 300, max: 8000, def: 3000, label: 'Left → head',   hint: 'to the centre of your head' },
    distR: { min: 300, max: 8000, def: 3000, label: 'Right → head',  hint: 'to the centre of your head' },
    headW: { min: 120, max: 220,  def: 180,  label: 'Head width',    hint: 'ear to ear' },
    // The same seat said the other way. People describe a room as "so wide by so
    // deep" far more readily than as two diagonals, so this is the way in that
    // matches how the setup is actually thought about.
    depth: { min: 150, max: 8000, def: 750,  label: 'How far back you sit',
             hint: 'from the line joining the speakers' },
};

export const FIELDS = ['span', 'distL', 'distR', 'headW'];

/// The two ways of saying where you are, and the fields each one asks for.
/// Both end up as the same stored triangle; only the way in differs.
export const MODES = {
    depth:  { fields: ['span', 'depth', 'headW'],
              title: 'Width and depth',
              blurb: 'How far back you sit from the line joining the speakers. Assumes you are on the centre line.' },
    direct: { fields: ['span', 'distL', 'distR', 'headW'],
              title: 'Straight to each speaker',
              blurb: 'The straight line from each speaker to your head. The only way to describe a seat that is off centre.' },
};

/// Straight-line distance to one speaker, for someone sitting centred at `depth`.
/// Pythagoras on the half-span and the depth — and the whole reason the two
/// modes exist, because these two numbers are easy to mistake for each other.
export function directFromDepth(span, depth) {
    return Math.sqrt((span / 2) * (span / 2) + depth * depth);
}

/// The perpendicular depth implied by a pair of direct distances.
export function depthFromDirect(g) {
    const pos = headPosition(g);
    return pos ? pos.y : null;
}

/// Would these figures make sense if they were DEPTHS rather than diagonals?
///
/// This is the one mistake the form invites, and it fails in a recognisable way:
/// two distances that do not reach across the span are impossible as diagonals,
/// but perfectly ordinary as a depth. Rather than only refusing, say what the
/// figures would mean read the other way, so the person can see the difference
/// instead of being told they are wrong.
export function depthReading(g) {
    if (!(g.span > 0)) return null;
    const d = (Number.isFinite(g.distL) && Number.isFinite(g.distR))
        ? (g.distL + g.distR) / 2 : NaN;
    if (!Number.isFinite(d) || d <= 0) return null;
    if (g.distL + g.distR > g.span) return null;        // a valid triangle already
    const lim = LIMITS.depth;
    if (d < lim.min || d > lim.max) return null;
    return { depth: d, direct: directFromDepth(g.span, d) };
}

// A path difference of d millimetres is a timing error of d / 343 microseconds.
// The cancellation survives while that stays inside a quarter period of the
// highest frequency being cancelled:
//     quarter period at 8 kHz = 31.2 us -> 10.7 mm
//     quarter period at 2 kHz = 125  us -> 42.9 mm
export const ASYM_FULL = 10.7;
export const ASYM_SOFT = 42.9;

/// Full angle subtended by the two speakers at the head, in degrees.
/// Law of cosines, so an off-centre chair needs no special case.
export function spanAngleDeg(g) {
    const { span, distL, distR } = g;
    if (!(span > 0 && distL > 0 && distR > 0)) return null;
    const c = (distL * distL + distR * distR - span * span) / (2 * distL * distR);
    if (!(c >= -1 && c <= 1)) return null;          // not a triangle
    return Math.acos(c) * 180 / Math.PI;
}

/// What the span angle says about the setup.
///
/// It is DESCRIPTIVE, not a grade, and that is a correction. The first version
/// graded it — narrow good, wide bad — on the received wisdom that crosstalk
/// cancellation wants a 20–30° pair and does little at 60°. Measured against
/// this filter, that is not true: sweeping the span from 10° to 150° and reading
/// the off-diagonal of C·H gives a mean cancellation between −18 and −26 dB at
/// every angle, with no fall-off at the wide end. Head movement, not angle, is
/// what the wisdom is really about — see MOVEMENT_NOTE.
///
/// So this now only helps you check you measured the right room: a number far
/// from what you pictured means the tape went somewhere unintended.
export function angleVerdict(deg) {
    if (deg == null) return { key: 'bad', text: 'not a triangle' };
    if (deg < 20)   return { key: 'info', text: 'narrow — you are well back from the speakers' };
    if (deg <= 70)  return { key: 'info', text: 'a conventional listening triangle' };
    if (deg <= 120) return { key: 'info', text: 'wide — you sit close in, with the speakers well to your sides' };
    return { key: 'info', text: 'very wide — you are nearly level with the speakers' };
}

/// The constraint that measurement does support, stated once so both windows
/// quote the same thing. Numbers from perturbing the head laterally against a
/// filter designed for the centred position, averaged over 500 Hz – 6 kHz:
///
///     centred   −19 to −26 dB
///     +20 mm     −8 to −15 dB
///     +50 mm     −8 to −11 dB
///     +100 mm    −5 to −13 dB
///
/// Angle barely moves those rows; distance from the seat you measured moves
/// them a lot. That is the real sweet spot, and it is small.
export const MOVEMENT_NOTE =
    'Measured: two centimetres off the seat you measured costs about half the cancellation, '
  + 'and ten centimetres leaves almost none. Sit where the tape measure was.';

export function asymmetryMm(g) { return Math.abs(g.distL - g.distR); }

export function asymmetryVerdict(mm) {
    const us = (mm / C_AIR) * 1000;                  // mm / (m/s) -> microseconds
    if (mm <= ASYM_FULL) {
        return { key: 'good', text: `Symmetric to ${mm.toFixed(0)} mm (${us.toFixed(0)} µs) — the whole band is intact.` };
    }
    if (mm <= ASYM_SOFT) {
        return {
            key: 'marginal',
            text: `Off centre by ${mm.toFixed(0)} mm (${us.toFixed(0)} µs). The filter is symmetric, so it is built `
                + `for the mean of your two distances — cancellation above about 2 kHz will be partial.`,
        };
    }
    return {
        key: 'poor',
        text: `Off centre by ${mm.toFixed(0)} mm (${us.toFixed(0)} µs) — past the point where one symmetric filter `
            + `describes this room. Move the chair onto the centre line, or expect the top of the band to reinforce `
            + `rather than cancel.`,
    };
}

/// Hard validity, kept separate from advice: advice colours the readout, this
/// decides whether Save is allowed at all.
export function isGeometryValid(g) {
    for (const k of FIELDS) {
        const lim = LIMITS[k];
        const v = g[k];
        if (!Number.isFinite(v)) return { ok: false, why: `${lim.label} is empty` };
        if (v < lim.min || v > lim.max) {
            return { ok: false, why: `${lim.label} must be between ${lim.min} and ${lim.max} mm` };
        }
    }
    if (g.distL + g.distR <= g.span) {
        return {
            ok: false,
            why: `Those distances cannot meet: ${g.distL} + ${g.distR} mm has to exceed the ${g.span} mm span, `
               + `or the two paths never reach each other.`,
        };
    }
    if (Math.abs(g.distL - g.distR) >= g.span) {
        return { ok: false, why: 'One distance exceeds the other by more than the whole span — check the two figures.' };
    }
    if (spanAngleDeg(g) == null) return { ok: false, why: 'Those three distances do not form a triangle.' };
    // The model needs a perpendicular depth to the ear plane; a head level with
    // the speakers collapses its right triangle. Mirrors XtcGeometry::is_usable.
    const pos = headPosition(g);
    if (!pos || pos.y <= 50) {
        return {
            ok: false,
            why: 'That puts you level with the speakers — there is no depth for the filter to work from. '
               + 'Move back, or check the span.',
        };
    }
    return { ok: true, why: '' };
}

/// Turn a depth-mode entry into the stored triangle. Left and right are equal by
/// construction: this mode cannot express an off-centre seat, and says so.
export function triangleFromDepth(span, depth, headW) {
    const d = directFromDepth(span, depth);
    return { span, distL: d, distR: d, headW };
}

/// Where the head sits relative to the speaker baseline, by trilateration.
/// Returns null when the numbers do not close. Shared so the plan drawing and
/// any future measurement agree on where the listener actually is.
export function headPosition(g) {
    const xL = -g.span / 2;
    const x = (g.distL * g.distL - g.distR * g.distR) / (2 * g.span);
    const ySq = g.distL * g.distL - (x - xL) * (x - xL);
    if (!(ySq > 0)) return null;
    return { x, y: Math.sqrt(ySq) };
}
