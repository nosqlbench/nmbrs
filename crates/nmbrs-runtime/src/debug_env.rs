// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Developer debug switches read from `NMBRS_DEBUG_*`-style env
//! vars.
//!
//! Each switch is a boolean env flag ([`nmbrs_workload::boolean`]):
//! unset or `false` is off, `true` is on. A value that isn't a
//! boolean leaves the switch off and logs a warning naming the
//! canonical spelling, so `NMBRS_DEBUG_WIRES=1` never looks like it
//! worked.

/// Read the debug switch `name` once. Callers on hot paths cache the
/// result in a `OnceLock`.
pub fn debug_flag(name: &str) -> bool {
    nmbrs_workload::boolean::env_flag(name).unwrap_or_else(|e| {
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "{e}; debug switch {name} stays off"
        );
        false
    })
}
