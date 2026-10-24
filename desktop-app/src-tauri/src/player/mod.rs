//! Player mode: the converter's chain, live.
//!
//! Everything playback adds on top of the engine — the streaming convolver
//! (bit for bit with the converter's CPU path), the output-rate chain, the
//! rewritable output timeline, the WASAPI device and the transport. The
//! source stages are the engine's own `prepare_audio_phase`, run whole-file
//! in the background, so a track sounds the way the converter would write it.

pub mod album_probe;
pub mod analysis_cache;
pub mod arming;
pub mod blend;
pub mod calibration;
pub mod chain;
pub mod controller;
pub mod convolver;
pub mod dsd_cpu;
pub mod dsd_direct;
pub mod dsd_worker;
pub mod foreground;
pub mod gpu;
pub mod grow;
pub mod input;
pub mod output;
pub mod output_tap;
pub mod policy;
pub mod probe;
pub mod radio;
pub mod render;
pub mod selftest;
pub mod settings;
pub mod slow_gain;
pub mod source_stages;
pub mod stages;
pub mod timeline;
pub mod analytics;

#[cfg(test)]
mod equivalence;

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod file_vs_live;

#[cfg(test)]
mod ms_player;
#[cfg(test)]
mod ms_hybrid;

#[cfg(test)]
mod ms_quality;
#[cfg(test)]
mod first_block_bench;
