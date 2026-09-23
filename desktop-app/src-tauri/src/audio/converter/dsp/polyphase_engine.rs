//! A polyphase pass an embedder can supply instead of the built-in one.
//!
//! The convolution is the one stage of the conversion that is both the most
//! expensive and the most self-contained: given the source-rate input, the
//! filter and the ratio, its output is a pure function of the three. Nothing
//! around it — the trims, the hybrid blend, the dither, the encode — needs to
//! know who computed it.
//!
//! So a host that has hardware the engine cannot reach on its own (a card
//! behind an API the engine has no binding for, a compute service, a cluster)
//! can install its own pass here and the rest of the pipeline is unchanged.
//! Nothing in the engine installs one: with no engine registered,
//! `run_polyphase_pass` runs exactly the code it has always run, so a build
//! that does not use this costs nothing and behaves identically.
//!
//! The contract is deliberately narrow, and it is the *whole* contract:
//!
//!   out[frame * L + phase] = scale * y_phase[frame]
//!
//! where `y_phase` is the input convolved with polyphase sub-filter `phase`
//! of the filter at `filter_path`, delayed by `ola_latency` samples:
//!
//!   y_phase[f] = 0                                   for f < ola_latency
//!   y_phase[f] = (x * h_phase)[f - ola_latency]       otherwise
//!
//! with `x` the source-rate input followed by `flush_input_samples` zeros.
//! That leading delay is not an accident to be optimized away — it is the
//! engine's own convolver latency, and the trim that follows the pass
//! subtracts exactly it. An implementation that omits it shifts the whole
//! track.
//!
//! An engine that cannot or will not handle a particular pass returns
//! `Ok(false)` and the built-in one runs instead, which is what makes this
//! safe to install unconditionally: declining is always allowed, and it is
//! the right answer whenever the alternative would be to fail a conversion.

// Nothing in this build installs one, so from the compiler's point of view
// the whole module is unreachable. That is the point of it: it is an
// interface offered outward, and the application is simply not the caller.
#![allow(dead_code)]

use std::sync::OnceLock;

/// Everything a polyphase pass needs to know, and nothing else.
pub struct PolyphaseRequest<'a> {
    /// The `.npy` the sub-filters are decomposed from. The engine has
    /// already decomposed it for its own pass; an alternate engine is
    /// expected to read this file rather than be handed a second copy of
    /// up to a quarter of a gigabyte of coefficients.
    pub filter_path: &'a str,
    /// Number of phases, which is also the upsampling ratio.
    pub l: usize,
    /// Length of the longest sub-filter. Phases differ by at most one tap;
    /// padding the short ones with a zero changes nothing.
    pub sub_taps: usize,
    pub audio_l: &'a [f64],
    pub audio_r: &'a [f64],
    pub total_input_samples: usize,
    /// Zeros fed after the real input, to drain the filter.
    pub flush_input_samples: usize,
    /// Pre-roll the engine's own convolver emits; see the module note.
    pub ola_latency: usize,
    /// Applied to every produced sample, as the built-in pass applies it.
    pub scale: f64,
    /// Fraction done, 0.0 to 1.0. Called from whatever thread the pass runs
    /// on; the engine forwards it to the row the interface is drawing.
    pub progress: &'a (dyn Fn(f64) + Sync),
}

/// An alternate polyphase pass.
pub trait PolyphaseEngine: Send + Sync {
    /// Fill `out_l` and `out_r` — each exactly
    /// `(total_input_samples + flush_input_samples) * l` long — and return
    /// `Ok(true)`.
    ///
    /// Return `Ok(false)` to decline, and the built-in pass runs instead.
    /// Declining is the right answer for anything recoverable, including a
    /// failure partway through: the buffers are the engine's and it will
    /// overwrite them completely. `Err` stops the conversion, so it is for
    /// cancellation and for damage that has already been done.
    fn run(
        &self,
        request: &PolyphaseRequest,
        out_l: &mut [f64],
        out_r: &mut [f64],
    ) -> Result<bool, String>;
}

static ENGINE: OnceLock<Box<dyn PolyphaseEngine>> = OnceLock::new();

/// Install the pass for the life of the process. The first call wins; a
/// later one is ignored rather than allowed to swap the engine out from
/// under a conversion already running.
pub fn install(engine: Box<dyn PolyphaseEngine>) -> bool {
    ENGINE.set(engine).is_ok()
}

pub(crate) fn installed() -> Option<&'static dyn PolyphaseEngine> {
    ENGINE.get().map(|boxed| &**boxed)
}
