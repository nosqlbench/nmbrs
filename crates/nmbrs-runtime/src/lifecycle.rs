// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Lifecycle event vocabulary — the kind-tag [`EventType`]
//! plus its [`SubjectKind`].
//!
//! [`EventType`] names the lifecycle slot that triggered a
//! render (`on_phase_end`, `on_update`, …); [`SubjectKind`]
//! names what kind of subject the firing context represents
//! (session / phase / iteration / scope). Together they are
//! the shared lifecycle vocabulary used by the readout binder
//! (to validate and dispatch readouts) and by the checkpoint
//! log (to tie a durable data record to its lifecycle kind-tag).
//!
//! See SRD-63 §4.1. Two kinds of events exist:
//!
//! - **Lifecycle events** — fire exactly once per
//!   `(slot, subject)`. `_start` and `_end` are
//!   delaminated; nothing fires twice for the same
//!   subject under one slot.
//! - **Refresh events** — fire repeatedly while the
//!   subject is in flight. Currently only [`EventType::Update`].
//!
//! Push 2 wires `Update` and `PhaseEnd` through the activity
//! pipeline; the remaining variants are reachable from Push 3
//! onward as the workload-side `readouts:` parser maps slot
//! names to events and Push 4 wires wildcard binding to
//! cover scope and session lifecycles.

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum EventType {
    /// `on_session_start` — the run is opening.
    SessionStart,
    /// `on_session_end` — the run is closing. Final render
    /// for any session-scoped readout.
    SessionEnd,

    /// `on_phase_start` — a phase is opening. Equivalent to
    /// today's deleted phase-starting log row when the
    /// `phase_starting` readout is bound.
    PhaseStart,
    /// `on_phase_end` — a phase is closing. The ✓ DONE line
    /// (`phase_outcome` readout) is bound here today.
    PhaseEnd,

    /// `on_each_start` — a `for_each` / `for_combinations`
    /// iteration is opening. The current scope-ancestor
    /// header (`· for_each profile=…`) becomes the
    /// `scope_header` readout bound here in Push 3.
    EachStart,
    /// `on_each_end` — an iteration is closing.
    EachEnd,

    /// `on_scope_start` — a non-iteration scope group is
    /// opening.
    ScopeStart,
    /// `on_scope_end` — a non-iteration scope group is
    /// closing.
    ScopeEnd,

    /// `on_update` — periodic refresh tick. Today's inline
    /// progress thread fires this at 0.5 s; the TUI fires
    /// it per-frame. Drives the live status content.
    Update,
}

impl EventType {
    /// Lower-snake-case name matching the `readouts:` slot
    /// keyword (`on_update`, `on_phase_end`, …). Used by
    /// the `trace` diagnostic readout and by Push 3's
    /// workload-block parser.
    pub fn slot_name(self) -> &'static str {
        match self {
            EventType::SessionStart => "on_session_start",
            EventType::SessionEnd => "on_session_end",
            EventType::PhaseStart => "on_phase_start",
            EventType::PhaseEnd => "on_phase_end",
            EventType::EachStart => "on_each_start",
            EventType::EachEnd => "on_each_end",
            EventType::ScopeStart => "on_scope_start",
            EventType::ScopeEnd => "on_scope_end",
            EventType::Update => "on_update",
        }
    }

    /// What kind of subject the context that fires this
    /// event represents. Used by the binder to validate at
    /// bind-time that bound readouts accept the slot's
    /// subject kind. `Update` rides on the surrounding
    /// phase, so it reports `Phase`.
    pub fn subject_kind(self) -> SubjectKind {
        match self {
            EventType::SessionStart | EventType::SessionEnd => SubjectKind::Session,
            EventType::PhaseStart | EventType::PhaseEnd | EventType::Update => SubjectKind::Phase,
            EventType::EachStart | EventType::EachEnd => SubjectKind::Iteration,
            EventType::ScopeStart | EventType::ScopeEnd => SubjectKind::Scope,
        }
    }
}

/// What kind of subject a context (and a render) is
/// scoped to. Determined by the firing event and the
/// surface that built the context. Builtins declare
/// which kinds they accept; the binder validates at
/// bake-time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    /// The whole run. Used for `on_session_start` /
    /// `on_session_end`.
    Session,
    /// A single phase activity. Used for `on_phase_start` /
    /// `on_phase_end` / `on_update`.
    Phase,
    /// One iteration of a `for_each` / `for_combinations`
    /// scope. Used for `on_each_start` / `on_each_end`.
    Iteration,
    /// A non-iteration scope group (`do_while` /
    /// `do_until`). Used for `on_scope_start` /
    /// `on_scope_end`.
    Scope,
}

impl SubjectKind {
    /// Lower-snake-case name for the storage / replay
    /// surface. Stored in the `readout_snapshots.subject_kind`
    /// column so `nmbrs replay` can group rows by subject.
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Session => "session",
            SubjectKind::Phase => "phase",
            SubjectKind::Iteration => "iteration",
            SubjectKind::Scope => "scope",
        }
    }
}
