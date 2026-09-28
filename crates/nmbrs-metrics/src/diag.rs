// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Diagnostic logging sink for nmbrs-metrics.
//!
//! Reporters and instruments occasionally need to emit operational
//! warnings (sqlite write failures, histogram record failures, etc.).
//! To avoid corrupting TUI output with raw `eprintln!`, all such
//! warnings go through [`warn`] which routes to a pluggable callback.
//!
//! Consumers (like `nmbrs-runtime`) install a callback at startup that
//! forwards into their observer/log infrastructure. If no callback is
//! set, the default falls back to `eprintln!` so standalone/test use
//! still surfaces messages.

use std::sync::OnceLock;

/// Type of the installed warn callback.
pub type WarnFn = fn(&str);

/// Type of the installed info callback. INFO-level messages are routed
/// through the same TUI-safe sink pattern as warnings, so noteworthy
/// startup state (e.g. auto-inserted cadence layers — SRD-42 §"Tree
/// Construction → Logging") doesn't bypass the observer infrastructure.
pub type InfoFn = fn(&str);

static WARN_FN: OnceLock<WarnFn> = OnceLock::new();
static INFO_FN: OnceLock<InfoFn> = OnceLock::new();

/// Install the warn callback. Called once at startup. If called more
/// than once, the first install wins (consistent with the rest of the
/// nmbrs per-process singleton pattern).
pub fn set_warn_fn(f: WarnFn) {
    let _ = WARN_FN.set(f);
}

/// Install the info callback. Same first-install-wins semantics as
/// [`set_warn_fn`].
pub fn set_info_fn(f: InfoFn) {
    let _ = INFO_FN.set(f);
}

/// Emit a warning through the configured sink, or stderr if none.
pub fn warn(msg: &str) {
    if let Some(f) = WARN_FN.get() {
        f(msg);
    } else {
        eprintln!("{msg}");
    }
}

/// Emit an INFO message through the configured sink, or stderr if none.
/// Use sparingly — reserved for noteworthy startup/lifecycle state
/// that should be grep-able from a run log without enabling debug.
pub fn info(msg: &str) {
    if let Some(f) = INFO_FN.get() {
        f(msg);
    } else {
        eprintln!("{msg}");
    }
}
