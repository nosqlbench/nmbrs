// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Scratch-directory naming shared by the integration tests.
//!
//! A test binary runs its tests on parallel threads, and a scratch name
//! built from the process id and the system clock alone can repeat:
//! on Windows the clock advances in 100 ns steps, so two tests starting
//! together read the same value, share a directory, and one finds the
//! other's session artifacts ("session directory already contains
//! artifacts"). A per-process sequence number keeps every name distinct
//! whatever the clock reads.

use std::sync::atomic::{AtomicU64, Ordering};

/// The next number in this test binary's scratch-name sequence: unique
/// among the names this process creates.
pub fn seq() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
