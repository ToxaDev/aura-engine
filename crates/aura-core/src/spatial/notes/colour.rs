//! An instrument's colour (the Spec 2 contract, §7). The hue is its
//! family's, stepped by its place in the family so two of a kind differ; a
//! note instrument's register sets the lightness (low dark, high light) and
//! the brightness of its sound the saturation. Fixed for the track (medians
//! over its notes), and never from the room: measured 28.09 on the
//! research's synthetic sets, the same lead dry and wet differs in register
//! by d′ 0.04 and in brightness (the harmonic profile's slope) by 0.28,
//! while different instruments differ by 2.8–3.8 and 0.5–1.2; the cues the
//! room moves (coherence, width, attack, odd/even, decay: d′ 1.0–6.2) are
//! not used.

use super::objects::Timbre;
use crate::spatial as sp;

/// A family's hue (degrees), saturation, lightness, and the hue steps of
/// its second, third … member (each family spreads away from its
/// neighbours on the wheel: the voice from rose to pink, the guitar from
/// orange to gold and red-orange, "other" from teal to green).
fn family(kind: u8) -> (f32, f32, f32, &'static [f32]) {
    match kind {
        sp::K_VOICE => (345.0, 0.75, 0.62, &[0.0, -22.0, -44.0, 10.0]),
        sp::K_BASS => (265.0, 0.60, 0.42, &[0.0, 20.0, -18.0]),
        sp::K_KICK => (212.0, 0.35, 0.38, &[0.0]),
        sp::K_SNARE => (208.0, 0.30, 0.55, &[0.0]),
        sp::K_TOMS => (216.0, 0.32, 0.48, &[0.0, 8.0]),
        sp::K_HATS => (200.0, 0.25, 0.72, &[0.0]),
        sp::K_CYMBAL => (196.0, 0.20, 0.82, &[0.0, 6.0]),
        sp::K_DRUMS => (210.0, 0.30, 0.55, &[0.0]),
        sp::K_GUITAR => (30.0, 0.85, 0.55, &[0.0, 18.0, -14.0, 32.0]),
        sp::K_PIANO => (192.0, 0.70, 0.55, &[0.0, 18.0, -10.0]),
        sp::K_OTHER => (165.0, 0.60, 0.52, &[0.0, -30.0, -55.0, 8.0, -15.0]),
        sp::K_AMBIENCE => (220.0, 0.15, 0.40, &[0.0]),
        _ => (0.0, 0.0, 0.6, &[0.0]),
    }
}

fn hsl(h: f32, s: f32, l: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r + m, g + m, b + m]
}

/// The colour of an object of `kind`, the `index`-th of its family (RGB
/// 0…1). With a note instrument's timbre: lightness by its register (C2 →
/// 0.38 … C6 → 0.72), saturation by its brightness (a harmonic profile
/// falling 12 dB an octave three quarters of the family's, 3 dB all of it).
pub fn colour(kind: u8, timbre: Option<&Timbre>, index: usize) -> [f32; 3] {
    let (h, mut s, mut l, steps) = family(kind);
    if let Some(t) = timbre {
        if t.pitch.is_finite() {
            l = 0.38 + 0.34 * ((t.pitch - 36.0) / 48.0).clamp(0.0, 1.0);
        }
        if t.tilt.is_finite() {
            s *= 0.75 + 0.25 * ((t.tilt + 12.0) / 9.0).clamp(0.0, 1.0);
        }
    }
    // past the family's steps: the same steps again, a little lighter each round
    let (step, round) = (steps[index % steps.len()], (index / steps.len()) as f32);
    hsl(h + step, s, (l + 0.06 * round).clamp(0.2, 0.88))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_of_a_family_differ_and_the_register_lights() {
        let low = Timbre { pitch: 40.0, tilt: -6.0, ..Default::default() };
        let high = Timbre { pitch: 80.0, ..low.clone() };
        let (a, b) = (colour(sp::K_OTHER, Some(&low), 0), colour(sp::K_OTHER, Some(&low), 1));
        assert!(a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f32>() > 0.1);
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        assert!(lum(colour(sp::K_OTHER, Some(&high), 0)) > lum(colour(sp::K_OTHER, Some(&low), 0)) + 0.1);
        for k in 1..=13u8 {
            let c = colour(k, None, 0);
            assert!(c.iter().all(|v| (0.0..=1.0).contains(v)), "{k}: {c:?}");
        }
    }
}
