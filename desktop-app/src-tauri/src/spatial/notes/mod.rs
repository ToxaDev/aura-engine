//! Notes (Anton 28.09, the "with instruments" tier): every note a separated
//! source plays — when it starts and stops, its pitch, how strong it is —
//! and from the notes the source's instruments and a colour for each one's
//! timbre.
//!
//! `bp`: Basic Pitch, the note network, and the decoding of its output into
//! notes. `stem`: a source's notes flagged, joined and measured (each
//! note's portrait). `cluster`: the notes → the source's instruments.

pub mod bp;
pub mod cluster;
pub mod colour;
pub mod objects;
pub mod stem;

#[cfg(test)]
use objects::analyse;

#[cfg(test)]
pub(crate) mod eval;
