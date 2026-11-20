//! The engine's side of the crash reports: what it remembers for a report and
//! what a fault leaves behind. Reading, sealing and sending them is the app's.

pub mod known;
pub mod pending;
