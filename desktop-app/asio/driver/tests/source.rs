// SPDX-License-Identifier: GPL-3.0-only
//! The DLL carries its sources' id behind the marker, where Aura Player
//! reads it from a registered file.

#![cfg(windows)]

#[test]
fn the_dll_carries_its_source_id_behind_the_marker() {
    let s = &aura_player_asio::AURA_PLAYER_ASIO_SOURCE;
    let marker = b"AuraPlayerASIO-source:";
    assert!(s.starts_with(marker));
    let id = &s[marker.len()..];
    assert_eq!(id.len(), 64);
    assert!(id.iter().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
}
