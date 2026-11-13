//! The engine: the converter, the player and the spatial scenes, with what
//! they need around them. Nothing here knows about windows: the desktop app
//! (`desktop-app/src-tauri`) and the streamer (`apps/streamer`) each put their
//! own front on it.

// The status JSON in player/controller.rs has a large number of fields;
// serde_json's json! macro uses 4 recursion levels per field.
#![recursion_limit = "512"]

pub mod app_dir;
pub mod audio;
pub mod checksum;
pub mod filters_dl;
pub mod update_sig;
pub mod commands;
pub mod crash_report;
pub mod events;
pub mod gpu_sched;
pub mod perf;
pub mod player;
pub mod protocol;
#[cfg(test)]
#[allow(dead_code)] // `line` is build.rs's
mod shader_stamp;
pub mod spatial;
pub mod watchdog;
