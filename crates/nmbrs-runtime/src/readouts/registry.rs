// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Static registry of built-in readouts.
//!
//! Returns [`ReadoutHandle`](super::ReadoutHandle) — an
//! `Arc<dyn Readout>` — so the rest of the engine can hold
//! readouts uniformly whether they're compile-time builtins
//! (this module) or runtime workload-registered customs (a
//! later push). The Arc allocation per lookup is cheap
//! (one per workload-load + per phase fire, not per render),
//! and the unit-struct readouts have zero-size payloads.

use std::sync::Arc;

use super::Readout;
use super::ReadoutHandle;
use super::builtins;

/// Looks up a built-in readout by name. Returns `None` when
/// the name isn't registered; callers turn that into a
/// load-time error per
/// `feedback_never_ignore_silently`.
pub struct Registry;

impl Registry {
    /// Resolve `name` to its built-in implementation.
    pub fn lookup(name: &str) -> Option<ReadoutHandle> {
        match name {
            "each_close" => Some(Arc::new(builtins::each_close::EachClose) as Arc<dyn Readout>),
            "error_readout" => {
                Some(Arc::new(builtins::error_readout::ErrorReadout) as Arc<dyn Readout>)
            }
            "metric" => Some(Arc::new(builtins::metric::Metric) as Arc<dyn Readout>),
            "phase_outcome" => {
                Some(Arc::new(builtins::phase_outcome::PhaseOutcomeReadout) as Arc<dyn Readout>)
            }
            "phase_starting" => {
                Some(Arc::new(builtins::phase_starting::PhaseStarting) as Arc<dyn Readout>)
            }
            "phase_status" => {
                Some(Arc::new(builtins::phase_status::PhaseStatus) as Arc<dyn Readout>)
            }
            "phase_summary" => {
                Some(Arc::new(builtins::phase_summary::PhaseSummary) as Arc<dyn Readout>)
            }
            "scope_close" => Some(Arc::new(builtins::scope_close::ScopeClose) as Arc<dyn Readout>),
            "scope_header" => {
                Some(Arc::new(builtins::scope_header::ScopeHeader) as Arc<dyn Readout>)
            }
            "scope_open" => Some(Arc::new(builtins::scope_open::ScopeOpen) as Arc<dyn Readout>),
            "session_banner" => {
                Some(Arc::new(builtins::session_banner::SessionBanner) as Arc<dyn Readout>)
            }
            "session_notice" => {
                Some(Arc::new(builtins::session_notice::SessionNotice) as Arc<dyn Readout>)
            }
            "session_summary" => {
                Some(Arc::new(builtins::session_summary::SessionSummary) as Arc<dyn Readout>)
            }
            "trace" => Some(Arc::new(builtins::trace::Trace) as Arc<dyn Readout>),
            "truncated_phases" => {
                Some(Arc::new(builtins::truncated_phases::TruncatedPhases) as Arc<dyn Readout>)
            }
            _ => None,
        }
    }

    /// All registered built-in names. Used by parser
    /// diagnostics ("did you mean …?") and by the future
    /// `nmbrs --list-readouts` introspection.
    pub fn all_names() -> &'static [&'static str] {
        &[
            "each_close",
            "error_readout",
            "metric",
            "phase_outcome",
            "phase_starting",
            "phase_status",
            "phase_summary",
            "scope_close",
            "scope_header",
            "scope_open",
            "session_banner",
            "session_notice",
            "session_summary",
            "trace",
            "truncated_phases",
        ]
    }
}
