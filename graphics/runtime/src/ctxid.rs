//! Bounded verdict cache for the `this` pointers the hooks are called with (pure logic, no D3D).
//!
//! The hooks are inline detours of the native function bodies, so they are called for every object of
//! the class. Nothing is decided by pointer or COM identity any more: the only per-object question is
//! "is this an immediate context?" (`ID3D11DeviceContext::GetType() == 0`), asked once per distinct
//! `this` and cached here. The COM call lives in `d3d.rs`.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// `D3D11_DEVICE_CONTEXT_IMMEDIATE`.
pub const CONTEXT_IMMEDIATE: u32 = 0;

const SLOTS: usize = 32;

/// Bounded, lock-free cache of verdicts (positive and negative) per pointer. One word per entry:
/// the pointer with its lowest bit as the verdict (COM objects are at least 2-byte aligned; an odd
/// pointer is simply not cached). The oldest entry is overwritten when the table is full.
pub struct VerdictTable {
    slots: [AtomicUsize; SLOTS],
    next: AtomicUsize,
}

impl VerdictTable {
    pub const fn new() -> Self {
        VerdictTable { slots: [const { AtomicUsize::new(0) }; SLOTS], next: AtomicUsize::new(0) }
    }

    /// The cached verdict for `key`, if any.
    pub fn lookup(&self, key: usize) -> Option<bool> {
        if key == 0 || key & 1 != 0 {
            return None;
        }
        self.slots.iter().map(|s| s.load(Relaxed)).find(|&e| e & !1 == key).map(|e| e & 1 == 1)
    }

    /// Remembers the verdict (replacing an earlier one for the same key).
    pub fn store(&self, key: usize, verdict: bool) {
        if key == 0 || key & 1 != 0 {
            return;
        }
        let entry = key | verdict as usize;
        if let Some(s) = self.slots.iter().find(|s| s.load(Relaxed) & !1 == key) {
            s.store(entry, Relaxed);
            return;
        }
        let i = self.next.fetch_add(1, Relaxed) % SLOTS;
        self.slots[i].store(entry, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_remembers_positive_and_negative_verdicts() {
        let t = VerdictTable::new();
        assert_eq!(t.lookup(0x100), None);
        t.store(0x100, true);
        t.store(0x200, false);
        assert_eq!(t.lookup(0x100), Some(true));
        assert_eq!(t.lookup(0x200), Some(false));
        assert_eq!(t.lookup(0x300), None);
    }

    #[test]
    fn table_updates_in_place_and_ignores_unusable_keys() {
        let t = VerdictTable::new();
        t.store(0x100, false);
        t.store(0x100, true);
        assert_eq!(t.lookup(0x100), Some(true));
        t.store(0, true);
        t.store(0x101, true); // odd: not cacheable
        assert_eq!(t.lookup(0), None);
        assert_eq!(t.lookup(0x101), None);
    }

    #[test]
    fn table_is_bounded_and_overwrites_the_oldest() {
        let t = VerdictTable::new();
        for i in 0..(SLOTS + 4) {
            t.store(0x1000 + i * 0x10, true);
        }
        assert_eq!((0..SLOTS + 4).filter(|i| t.lookup(0x1000 + i * 0x10).is_some()).count(), SLOTS);
        assert_eq!(t.lookup(0x1000), None); // the first four were overwritten
        assert_eq!(t.lookup(0x1000 + (SLOTS + 3) * 0x10), Some(true));
    }
}
