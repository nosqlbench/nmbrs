// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Flat name projections of the workload-construction registry.
//!
//! The single declaration point for every construction is
//! [`crate::construction`] — enumerable node types carrying each
//! element's name, valid forms, and documentation, discoverable
//! against partial ASTs. This module projects those tables down
//! to plain name lists for consumers that only need the
//! vocabulary surface (the synthesizer superfuzz's coverage
//! universe, docs tooling). The parser consumes the construction
//! tables directly for its rejection checks.
//!
//! Known hygiene gap (open): PHASE-level unknown keys are not
//! yet rejected at parse — see
//! [`crate::construction::PhaseNode`]'s `open_surface`.

use crate::construction as c;

fn names(table: &'static [c::ElementSpec]) -> Vec<&'static str> {
    table.iter().map(|e| e.name).collect()
}

/// Phase-level configuration keys (pinned to the
/// `WorkloadPhase` struct by the serde field probe in
/// `construction::tests`).
pub fn phase_fields() -> Vec<&'static str> {
    names(c::PHASE_ELEMENTS)
}

/// Op-template keys owned by the workload model (never forwarded
/// to the adapter's op-field surface).
pub fn op_model_fields() -> Vec<&'static str> {
    names(c::OP_MODEL_ELEMENTS)
}

/// Op keys that carry the statement payload.
pub fn op_stmt_fields() -> Vec<&'static str> {
    names(c::OP_STMT_ELEMENTS)
}

/// Keys of a phase-level `poll:` block (SRD-75).
pub fn phase_poll_fields() -> Vec<&'static str> {
    names(c::POLL_ELEMENTS)
}

/// Sub-keys of an op's `abstract:` interface (SRD-108 Part B).
pub fn abstract_interface_keys() -> Vec<&'static str> {
    names(c::ABSTRACT_ELEMENTS)
}

/// Evaluation kinds accepted inside `evaluations:`.
pub fn evaluation_kinds() -> Vec<&'static str> {
    names(c::EVALUATIONS_ELEMENTS)
}

/// Structural scenario-node keys.
pub fn scenario_node_keys() -> Vec<&'static str> {
    names(c::SCENARIO_NODE_ELEMENTS)
}

/// GK binding declaration classes used from workload `bindings:`
/// blocks.
pub fn binding_classes() -> Vec<&'static str> {
    names(c::BINDING_CLASS_ELEMENTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The projections stay live views of the construction
    /// tables — spot-check sentinel entries from each.
    #[test]
    fn projections_reflect_the_construction_tables() {
        assert!(phase_fields().contains(&"poll"));
        assert!(op_model_fields().contains(&"abstract"));
        assert!(op_stmt_fields().contains(&"stmt"));
        assert!(phase_poll_fields().contains(&"until"));
        assert!(abstract_interface_keys().contains(&"results"));
        assert!(evaluation_kinds().contains(&"relevancy"));
        assert!(scenario_node_keys().contains(&"for_combinations"));
        assert!(binding_classes().contains(&"cursor"));
    }
}
