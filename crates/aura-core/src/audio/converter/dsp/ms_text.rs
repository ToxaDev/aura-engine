//! How the app names the multistage upsampler wherever it says how a chain or a render was made: the PFR row of
//! the player and of the converter, the render's tags and the single-stage fallback's filename mark. Single-stage
//! keeps its own PFR texts.

use super::ms_h1::H1Stage;

/// The short chain in a few words: its stage ratios and their lengths, e.g. "×2 · ×4, 95 + 287 taps".
pub fn chain_desc(chain: &[H1Stage]) -> String {
    let ratios: Vec<String> = chain.iter().map(|s| format!("×{}", s.ratio)).collect();
    let taps: Vec<String> = chain.iter().map(|s| s.taps.len().to_string()).collect();
    format!("{}, {} taps", ratios.join(" · "), taps.join(" + "))
}

/// The PFR row of a multistage chain: `taps_label` the long filter's size ("30M"), `l` the whole ratio.
pub fn pfr_text(taps_label: &str, l: usize, chain: &[H1Stage]) -> String {
    format!(
        "{} taps at the source rate + short exact interpolator ×{} ({}): matches single-stage to −204…−213 dBFS",
        taps_label,
        l,
        chain_desc(chain)
    )
}

/// The render's `AURA_UPSAMPLER` tag value: "multistage x2(95)+x4(287)" — the mode and the chain it ran.
pub fn tag(chain: &[H1Stage]) -> String {
    let stages: Vec<String> = chain.iter().map(|s| format!("x{}({})", s.ratio, s.taps.len())).collect();
    format!("multistage {}", stages.join("+"))
}

/// The VORBIS key a render carries when its upsampler was a choice: the multistage chain it ran ([`tag`]), or
/// [`SINGLE_TAG`] for the single-stage fallback. A render that is single-stage either way has none.
pub const TAG_KEY: &str = "AURA_UPSAMPLER";

/// The `AURA_UPSAMPLER` value of a render the hidden single-stage fallback made where multistage would have run.
pub const SINGLE_TAG: &str = "single";

/// Added to the filter segment of such a fallback render's filename ("Kaiser 30M Single"), so it sits beside the
/// default (multistage) render of the same file instead of replacing it. The default carries no mark.
pub const SINGLE_NAME_MARK: &str = "Single";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::converter::dsp::ms_h1::h1_chain;

    #[test]
    fn texts_name_the_chain() {
        let chain = h1_chain(44_100, 8).expect("the shipped x8 chain");
        let (a, b) = (chain[0].taps.len(), chain[1].taps.len());
        assert_eq!(chain_desc(chain), format!("×2 · ×4, {a} + {b} taps"));
        assert_eq!(
            pfr_text("30M", 8, chain),
            format!(
                "30M taps at the source rate + short exact interpolator ×8 (×2 · ×4, {a} + {b} taps): \
                 matches single-stage to −204…−213 dBFS"
            )
        );
        assert_eq!(tag(chain), format!("multistage x2({a})+x4({b})"));
        let one = h1_chain(48_000, 2).expect("the shipped x2 chain");
        assert_eq!(tag(one), format!("multistage x2({})", one[0].taps.len()));
    }
}
