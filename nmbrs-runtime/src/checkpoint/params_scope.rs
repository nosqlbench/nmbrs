// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-107 — the consumed-params derivation.
//!
//! Which workload params does a phase's scope actually consume?
//! The answer is DERIVED, never declared (a declaration surface
//! would drift from the programs and reintroduce stale-skip bugs
//! by hand):
//!
//! - **GK backward closure** — polydat owns every step of it
//!   ([`polydat::kernel::PolydatProgram::owned_extern_closure`]
//!   seeds from owned outputs; `resolve_externs_through` walks
//!   the scope chain; both are projections of the ONE
//!   construction-time node inventory). Names still unresolved
//!   past the workload root that match declared params are
//!   consumed from the params module. Aliasing through upstream
//!   rebindings (`alias := run_tag` at the root, phase reads
//!   `alias`) resolves correctly with no textual guessing.
//! - **Textual union** — `{name}` interpolation sites in the
//!   phase's canonical config (op statement text, governance
//!   fields like `cycles:`/`timeout:`) never enter compiled
//!   programs; a substring scan over the canonical config text
//!   covers them. False positives only over-invalidate, and only
//!   when that specific param changes — the safe direction.
//!
//! The result maps each consumed name to a digest of its CURRENT
//! raw value; equality of stored vs freshly computed digests is
//! the per-param leg of SRD-107's three-way skip-validity check.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use polydat::kernel::{PolydatKernel, PolydatProgram};

/// SHA-256 hex of a param's raw string value — the stored
/// representation is a digest, never the value itself, so rows
/// stay uniform and value contents (hosts, credentials-adjacent
/// strings) don't land in provenance stores.
pub(crate) fn value_digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"nmbrs-param-value-v1\n");
    h.update(value.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Derive the phase's consumed-params map: `name -> value_digest`
/// over exactly the params the phase's scope consumes.
///
/// `ancestors_below_session` is innermost-first (immediate parent
/// … workload root), the params module EXCLUDED — the same shape
/// [`crate::scope_tree::ScopeTree::ancestor_kernels_split`]
/// returns. `phase_config_text` is the canonical config
/// serialization ([`super::phase_config_canonical_text`]), shared
/// with the config digest so the two legs read one surface.
pub(crate) fn consumed_params(
    own_program: &PolydatProgram,
    op_template_programs: &[Arc<PolydatProgram>],
    ancestors_below_session: &[Arc<PolydatKernel>],
    phase_config_text: &str,
    params: &HashMap<String, String>,
) -> BTreeMap<String, String> {
    // ALL graph reasoning lives in polydat (SRD-107's one-walker
    // rule): the owned-output extern slices seed the walk, and
    // `resolve_externs_through` resolves them up the scope chain
    // to the terminal set the params module must satisfy. This
    // function only composes those projections with the textual
    // scan and the value digests.
    let mut seed: Vec<String> = own_program.owned_extern_closure();
    for prog in op_template_programs {
        seed.extend(prog.owned_extern_closure());
    }
    let ancestor_programs: Vec<std::sync::Arc<PolydatProgram>> = ancestors_below_session
        .iter()
        .map(|k| k.program().clone())
        .collect();
    let ancestor_refs: Vec<&PolydatProgram> =
        ancestor_programs.iter().map(|p| p.as_ref()).collect();
    let terminal: BTreeSet<String> = PolydatProgram::resolve_externs_through(seed, &ancestor_refs)
        .into_iter()
        .collect();

    // Terminal intersection + textual union.
    let mut out = BTreeMap::new();
    for (name, value) in params {
        let gk = terminal.contains(name);
        let textual = phase_config_text.contains(&format!("{{{name}}}"));
        if gk || textual {
            out.insert(name.clone(), value_digest(value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use polydat::dsl::compile::compile_polydat_interpreter;

    fn kernel(source: &str) -> Arc<PolydatKernel> {
        Arc::new(compile_polydat_interpreter(source).expect("compile test kernel"))
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn names(map: &BTreeMap<String, String>) -> Vec<&str> {
        map.keys().map(String::as_str).collect()
    }

    #[test]
    fn direct_extern_consumption() {
        let phase = kernel("extern p1: String\nout := p1\n");
        let got = consumed_params(
            &phase.program(),
            &[],
            &[],
            "",
            &params(&[("p1", "a"), ("p2", "b")]),
        );
        assert_eq!(names(&got), vec!["p1"]);
    }

    #[test]
    fn alias_rebinding_resolves_through_ancestor() {
        // Root rebinds `alias := run_tag`; the phase reads `alias`.
        // The closure must land on run_tag — the aliasing case that
        // defeats textual scanning.
        let root = kernel("extern run_tag: String\nalias := run_tag\n");
        let phase = kernel("extern alias: String\nout := alias\n");
        let got = consumed_params(
            &phase.program(),
            &[],
            &[root],
            "",
            &params(&[("run_tag", "a"), ("other", "b")]),
        );
        assert_eq!(names(&got), vec!["run_tag"]);
    }

    #[test]
    fn sibling_outputs_do_not_drag_their_params_in() {
        // THE precision test: the root produces `a` from p1 and
        // `b` from p2; a phase consuming only `a` must consume
        // only p1 — whole-program over-approximation would pull
        // p2 and reintroduce whole-module invalidation.
        let root = kernel("extern p1: String\nextern p2: String\na := p1\nb := p2\n");
        let phase = kernel("extern a: String\nout := a\n");
        let got = consumed_params(
            &phase.program(),
            &[],
            &[root],
            "",
            &params(&[("p1", "x"), ("p2", "y")]),
        );
        assert_eq!(names(&got), vec!["p1"]);
    }

    #[test]
    fn textual_interpolation_site_is_consumed() {
        let phase = kernel("out := 1\n");
        let got = consumed_params(
            &phase.program(),
            &[],
            &[],
            r#"{"ops":{"q":{"stmt":"SELECT * FROM {keyspace}.t"}}}"#,
            &params(&[("keyspace", "ks"), ("unrelated", "z")]),
        );
        assert_eq!(names(&got), vec!["keyspace"]);
    }

    #[test]
    fn op_template_externs_seed_the_walk() {
        let root = kernel("extern p1: String\nfield := p1\n");
        let phase = kernel("out := 1\n");
        let op_template = kernel("extern field: String\nrow := field\n");
        let got = consumed_params(
            &phase.program(),
            &[op_template.program().clone()],
            &[root],
            "",
            &params(&[("p1", "x"), ("p2", "y")]),
        );
        assert_eq!(names(&got), vec!["p1"]);
    }

    #[test]
    fn iteration_var_resolved_by_scope_is_not_a_param() {
        // A comprehension scope produces `section` from a literal;
        // the phase's `section` extern resolves there and never
        // reaches the params module.
        let comprehension = kernel("section := \"b\"\n");
        let phase = kernel("extern section: String\nout := section\n");
        let got = consumed_params(
            &phase.program(),
            &[],
            &[comprehension],
            "",
            &params(&[("run_tag", "a")]),
        );
        assert!(got.is_empty(), "got: {got:?}");
    }

    #[test]
    fn coordinates_are_excluded_and_empty_set_is_empty() {
        let phase = kernel("input cycle: u64\nout := cycle\n");
        let got = consumed_params(&phase.program(), &[], &[], "", &params(&[("p1", "a")]));
        assert!(got.is_empty(), "got: {got:?}");
    }

    /// The derivation counts a program's extern as consumed only
    /// when it feeds an output the program OWNS. A bare
    /// `compile_polydat_interpreter` re-exports every declared extern as an
    /// output WITHOUT the `inherited` passthrough marking (that
    /// marking is applied by the runtime's scope synthesis), so
    /// this fixture legitimately reads as consumption. The
    /// production shape — cascade-declared extern slots whose
    /// re-exports ARE marked inherited and therefore do NOT count
    /// — is pinned end-to-end by `refine_prereq_validity::
    /// unconsumed_param_flip_still_skips_the_prereq`.
    #[test]
    fn bare_compile_extern_reexport_reads_as_owned() {
        let phase = kernel("extern p1: String\nout := 1\n");
        let got = consumed_params(&phase.program(), &[], &[], "", &params(&[("p1", "a")]));
        assert_eq!(
            names(&got),
            vec!["p1"],
            "bare-compile re-exports carry no inherited marking"
        );
    }

    #[test]
    fn value_digest_tracks_the_value() {
        assert_eq!(value_digest("a"), value_digest("a"));
        assert_ne!(value_digest("a"), value_digest("b"));
    }
}
