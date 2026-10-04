//! `GpuBankCache` — 3-entry LRU cache for `GpuFilterBank`.
//!
//! Keyed by `(file path, L, full filter length)`.  Holds a most-recently-used
//! list of up to 3 entries; the oldest is evicted when a fourth would be added.
//! The cache lives inside `GpuPolyCtx` behind a `Mutex` and is accessed only
//! from the player thread.

use std::collections::VecDeque;
use std::sync::Arc;

use super::filter_buf::GpuFilterBank;

const CACHE_SIZE: usize = 3;

#[derive(Clone, PartialEq, Eq)]
pub struct BankKey {
    pub path: String,
    pub l: usize,
    pub full_len: usize,
}

pub struct GpuBankCache {
    entries: VecDeque<(BankKey, Arc<GpuFilterBank>)>,
}

impl GpuBankCache {
    pub fn new() -> Self {
        GpuBankCache {
            entries: VecDeque::with_capacity(CACHE_SIZE),
        }
    }

    /// Look up a bank by key; returns `None` if not cached.
    /// Moves a hit to the front (most recently used).
    pub fn get(&mut self, key: &BankKey) -> Option<Arc<GpuFilterBank>> {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == key) {
            let entry = self.entries.remove(pos).unwrap();
            let val = Arc::clone(&entry.1);
            self.entries.push_front(entry);
            Some(val)
        } else {
            None
        }
    }

    /// Insert a new entry. Evicts the least recently used entry if the cache
    /// is full.
    pub fn insert(&mut self, key: BankKey, bank: Arc<GpuFilterBank>) {
        // Evict the oldest entry if at capacity.
        while self.entries.len() >= CACHE_SIZE {
            self.entries.pop_back();
        }
        self.entries.push_front((key, bank));
    }

    /// Drop all cached entries, freeing their GPU buffers.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Keep only the entries whose keys appear in `keep`; evict the rest.
    /// Selective eviction for when the next track uses different filter banks.
    #[allow(dead_code)]
    pub fn retain_keys(&mut self, keep: &[BankKey]) {
        self.entries.retain(|(k, _)| keep.contains(k));
    }
}
