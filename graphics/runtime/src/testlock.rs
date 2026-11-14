//! One lock for every test that reads or writes the process-wide module state (exposure settings, fur / rim selectors, depth
//! consumer bits, datapath strengths). The tests run on parallel threads of one process; a per-module lock does not stop a
//! test of another module from changing the same globals.

use std::sync::{Mutex, MutexGuard};

static GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Holds the global test lock until dropped; a panic of an earlier test (poisoned lock) does not matter.
pub fn lock() -> MutexGuard<'static, ()> {
    GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
