// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-108 Part B — the typed implementation binder.
//!
//! Binds an implementation workload's op bodies into a
//! blueprint's ABSTRACT slots at load time, before any synthesis.
//! The blueprint owns every piece of scaffolding (extents,
//! concurrency, stop conditions, metrics, provenance classes);
//! the implementation contributes exactly the protocol matter:
//! op fields, adapter selection, captures, and op-level bindings.
//!
//! Every rule here is a LOAD error with a slot-coordinate name —
//! nothing defers to a runtime critical section. The interface's
//! type proof runs later, still at init: pre-map synthesis
//! verifies `yields` against the compiled op-template program
//! (see `nmbrs-runtime`'s SRD-108 verification hook).

use std::collections::BTreeSet;

use crate::model::{BindingsDef, ParsedOp, Workload, WorkloadPhase};

/// Bind `implementation` into `blueprint` in place.
///
/// Coverage is total in both directions: every abstract slot in
/// `blueprint` must be bound exactly once, and every op the
/// implementation provides must land in an abstract slot.
pub fn bind_implementation(
    blueprint: &mut Workload,
    implementation: Workload,
) -> Result<(), String> {
    // The set of wires the blueprint scaffold provides — the
    // `needs` availability check reads this. Params + workload
    // bindings + (per-phase) phase bindings.
    let provided_global: BTreeSet<String> = blueprint
        .declared_params
        .iter()
        .cloned()
        .chain(bindings_wire_names(&blueprint.bindings))
        .collect();

    let mut unbound: BTreeSet<String> = BTreeSet::new();
    for (phase_name, phase) in &blueprint.phases {
        for op in &phase.ops {
            if op.abstract_interface.is_some() {
                unbound.insert(format!("{phase_name}.{}", op.name));
            }
        }
    }

    for (phase_name, impl_phase) in implementation.phases {
        let offending = scaffolding_fields_set(&impl_phase);
        if !offending.is_empty() {
            return Err(format!(
                "implementation phase '{phase_name}' declares scaffolding \
                 fields [{}] — scaffolding belongs to the blueprint; an \
                 implementation phase carries only `ops:`",
                offending.join(", ")
            ));
        }
        let Some(blueprint_phase) = blueprint.phases.get_mut(&phase_name) else {
            return Err(format!(
                "implementation provides phase '{phase_name}', but the \
                 blueprint declares no such phase"
            ));
        };
        for impl_op in impl_phase.ops {
            let slot_key = format!("{phase_name}.{}", impl_op.name);
            let Some(slot) = blueprint_phase
                .ops
                .iter_mut()
                .find(|op| op.name == impl_op.name)
            else {
                return Err(format!(
                    "implementation op '{slot_key}' names no op in the \
                     blueprint phase"
                ));
            };
            if slot.abstract_interface.is_none() {
                return Err(format!(
                    "implementation op '{slot_key}' targets a CONCRETE \
                     blueprint op — only abstract slots accept bindings"
                ));
            }
            if !unbound.remove(&slot_key) {
                return Err(format!("implementation binds slot '{slot_key}' twice"));
            }
            bind_slot(slot, impl_op, &slot_key)?;

            // `needs` availability: every guaranteed wire must be
            // provided by the blueprint scaffold — params, workload
            // bindings, or this phase's bindings. (The full type
            // proof is pre-map synthesis; this is the early named
            // error.)
            let mut provided = provided_global.clone();
            provided.extend(bindings_wire_names(&blueprint_phase.bindings));
            if let Some(iface) = slot.abstract_interface.as_ref() {
                for need in iface.needs.keys() {
                    if !provided.contains(need) {
                        return Err(format!(
                            "slot '{slot_key}' declares need '{need}', but \
                             the blueprint provides no param or binding \
                             by that name"
                        ));
                    }
                }
            }
        }
    }

    if !unbound.is_empty() {
        return Err(format!(
            "abstract slot(s) [{}] remain unbound — the implementation \
             must cover every abstract op",
            unbound.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    // Implementation params layer over the blueprint's: a NEW key
    // adds a protocol knob; a key the blueprint already declares
    // RE-DEFAULTS its value — the same specialization the CLI
    // performs per invocation, made durable in a document (e.g. a
    // turnkey variant pinning dataset and tier governance).
    // Declaration ownership stays with the blueprint: the key is
    // not re-declared, so consumed-param provenance and the CLI
    // override order are unchanged. Only the YAML-DECLARED subset
    // participates: `Workload.params` also carries every ad-hoc
    // CLI arg (tui=, workload=, …) overlaid at parse, and those
    // are invocation matter, not document matter.
    let blueprint_declared: BTreeSet<String> = blueprint.declared_params.iter().cloned().collect();
    let impl_declared: Vec<String> = implementation.declared_params;
    for k in impl_declared {
        if let Some(v) = implementation.params.get(&k) {
            blueprint.params.insert(k.clone(), v.clone());
        }
        if !blueprint_declared.contains(&k) {
            blueprint.declared_params.push(k);
        }
    }

    // Workload-level bindings concatenate blueprint-first.
    concat_bindings(&mut blueprint.bindings, implementation.bindings)
        .map_err(|e| format!("workload-level bindings: {e}"))?;

    Ok(())
}

/// Bind one implementation op into one abstract slot. The slot
/// keeps everything the blueprint declared; the implementation
/// fills the protocol surfaces. Any overlap is a collision error
/// — never a silent override.
fn bind_slot(slot: &mut ParsedOp, impl_op: ParsedOp, slot_key: &str) -> Result<(), String> {
    // Op-owned semantics the implementation may NOT set: these
    // are measurement/scaffolding surfaces. (`result:` is NOT in
    // this list — it is the delivery surface for the `results:`
    // interface leg, SRD-109 Part 3.)
    if impl_op.condition.is_some()
        || impl_op.delay.is_some()
        || !impl_op.metrics.is_empty()
        || impl_op.traverse.is_some()
        || impl_op.wrappers.is_some()
        || impl_op.while_cond.is_some()
        || impl_op.rate.is_some()
        || !matches!(impl_op.daemon, crate::model::DaemonSpec::Disabled)
    {
        return Err(format!(
            "implementation op '{slot_key}' sets op semantics \
             (if/delay/metrics/traverse/wrappers/while/rate/\
             daemon) — those belong to the blueprint slot"
        ));
    }
    if impl_op.abstract_interface.is_some() {
        return Err(format!(
            "implementation op '{slot_key}' declares `abstract:` — an \
             implementation provides bodies, not interfaces"
        ));
    }

    for (k, v) in impl_op.op {
        if slot.op.contains_key(&k) {
            return Err(format!(
                "slot '{slot_key}': op field '{k}' declared by BOTH the \
                 blueprint slot and the implementation — the blueprint \
                 is authoritative; remove one"
            ));
        }
        slot.op.insert(k, v);
    }
    // Op params follow the param layering rule, not the op-field
    // collision rule: params are the system's override surface
    // (doc → block → op cascade at parse, CLI last), and both
    // docs materialize their workload-level params into op params
    // — so an overlapping key here is normally a workload-level
    // re-default seen through the cascade. The implementation
    // (the more concrete layer) wins, mirroring the workload-level
    // merge above; op FIELDS keep the hard collision error.
    for (k, v) in impl_op.params {
        slot.params.insert(k, v);
    }
    for (k, v) in impl_op.tags {
        match slot.tags.get(&k) {
            Some(existing) if existing != &v => {
                return Err(format!(
                    "slot '{slot_key}': tag '{k}' has conflicting values \
                     ('{existing}' vs '{v}')"
                ));
            }
            _ => {
                slot.tags.insert(k, v);
            }
        }
    }
    if !impl_op.captures.is_empty() {
        let existing: BTreeSet<&str> = slot.captures.iter().map(|c| c.as_name.as_str()).collect();
        for cap in &impl_op.captures {
            if existing.contains(cap.as_name.as_str()) {
                return Err(format!(
                    "slot '{slot_key}': capture '{}' declared by both \
                     sides",
                    cap.as_name
                ));
            }
        }
        slot.captures.extend(impl_op.captures);
    }
    concat_bindings(&mut slot.bindings, impl_op.bindings)
        .map_err(|e| format!("slot '{slot_key}' bindings: {e}"))?;

    // SRD-109 Part 3 — `result:` bindings deliver the `results:`
    // interface leg. Blueprint-side entries for interface names
    // are collisions (paths are protocol matter); impl entries
    // colliding with blueprint entries are collisions like any
    // other surface; the merged spec concatenates blueprint-first.
    if let Some(iface) = slot.abstract_interface.as_ref() {
        let blueprint_declared = result_binding_names(slot.result.as_ref());
        for results_name in iface.results.keys() {
            if blueprint_declared.contains(results_name.as_str()) {
                return Err(format!(
                    "slot '{slot_key}': `results:` wire '{results_name}' \
                     also has a blueprint-side `result:` binding — the \
                     projection path is protocol matter; the \
                     implementation declares it"
                ));
            }
        }
    }
    if let Some(impl_result) = impl_op.result {
        let blueprint_declared = result_binding_names(slot.result.as_ref());
        for name in result_binding_names(Some(&impl_result)) {
            if blueprint_declared.contains(name.as_str()) {
                return Err(format!(
                    "slot '{slot_key}': result binding '{name}' declared \
                     by BOTH the blueprint slot and the implementation — \
                     remove one"
                ));
            }
        }
        slot.result = Some(match slot.result.take() {
            None => impl_result,
            Some(existing) => crate::model::ResultSpec::List(vec![existing, impl_result]),
        });
    }

    // `yields` / `results` presence: every promised wire must be
    // delivered by the bound op — a capture `as`-name for yields,
    // a `result:` binding LHS for results. (Type equality is
    // proven at pre-map synthesis against the compiled program.)
    if let Some(iface) = slot.abstract_interface.as_ref() {
        let delivered: BTreeSet<&str> = slot.captures.iter().map(|c| c.as_name.as_str()).collect();
        for yield_name in iface.yields.keys() {
            if !delivered.contains(yield_name.as_str()) {
                return Err(format!(
                    "slot '{slot_key}' promises yield '{yield_name}', \
                     but the bound implementation captures [{}] — add a \
                     capture (`[{yield_name}]` or `... as {yield_name}`)",
                    delivered.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        let projected = result_binding_names(slot.result.as_ref());
        for results_name in iface.results.keys() {
            if !projected.contains(results_name.as_str()) {
                return Err(format!(
                    "slot '{slot_key}' promises results wire \
                     '{results_name}', but the bound implementation \
                     declares result bindings [{}] — add a `result:` \
                     entry (`{results_name}: <path-expr>`, e.g. \
                     `{results_name}: rows[*].column`)",
                    projected.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    slot.interface_bound = true;
    Ok(())
}

/// Wire names a `result:` spec declares: map-shape keys plus the
/// `:=` LHS of string-shape source lines.
fn result_binding_names(spec: Option<&crate::model::ResultSpec>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(spec) = spec {
        spec.walk_fragments(|frag| match frag {
            crate::model::ResultFragment::Named { name, .. } => {
                out.insert(name.to_string());
            }
            crate::model::ResultFragment::Source(source) => {
                for line in source.lines() {
                    if let Some((lhs, _)) = line.trim().split_once(":=") {
                        out.insert(lhs.trim().to_string());
                    }
                }
            }
        });
    }
    out
}

/// Names of scaffolding fields an implementation phase illegally
/// sets. Only `ops:` is legal on an implementation phase.
fn scaffolding_fields_set(phase: &WorkloadPhase) -> Vec<&'static str> {
    let mut out = Vec::new();
    if phase.cycles.is_some() {
        out.push("cycles");
    }
    if phase.concurrency.is_some() {
        out.push("concurrency");
    }
    if phase.rate.is_some() {
        out.push("rate");
    }
    if phase.daemon {
        out.push("daemon");
    }
    if phase.adapter.is_some() {
        out.push("adapter");
    }
    if phase.errors.is_some() {
        out.push("errors");
    }
    if phase.tries.is_some() {
        out.push("tries");
    }
    if phase.tries_backoff.is_some() {
        out.push("tries_backoff");
    }
    if phase.interval.is_some() {
        out.push("interval");
    }
    if phase.repeat.is_some() {
        out.push("repeat");
    }
    if phase.error_rate_max.is_some() {
        out.push("error_rate_max");
    }
    if phase.timeout.is_some() {
        out.push("timeout");
    }
    if !phase.stop_when.is_empty() {
        out.push("stop_when");
    }
    if phase.tags.is_some() {
        out.push("tags");
    }
    if phase.for_each.is_some() {
        out.push("for_each");
    }
    if phase.continue_if.is_some() {
        out.push("continue_if");
    }
    if phase.loop_scope.is_some() {
        out.push("loop_scope");
    }
    if phase.iter_scope.is_some() {
        out.push("iter_scope");
    }
    if phase.checkpoint.is_some() {
        out.push("checkpoint");
    }
    if !phase.status_metrics.is_empty() {
        out.push("status_metrics");
    }
    if !phase.bindings.is_empty() {
        out.push("bindings");
    }
    if !phase.metrics.is_empty() {
        out.push("metrics");
    }
    if !phase.dimensions.is_empty() {
        out.push("dimensions");
    }
    out
}

/// Wire names a `BindingsDef` declares.
fn bindings_wire_names(bindings: &BindingsDef) -> Vec<String> {
    match bindings {
        BindingsDef::PolydatSource(s) => crate::inline::binding_wire_names(s),
        BindingsDef::Map(m) => m.keys().cloned().collect(),
    }
}

/// Concatenate `extra` onto `base`, blueprint-first. Mixed
/// map/source forms are rejected (no sensible concatenation) —
/// never silently dropped.
fn concat_bindings(base: &mut BindingsDef, extra: BindingsDef) -> Result<(), String> {
    if extra.is_empty() {
        return Ok(());
    }
    if base.is_empty() {
        *base = extra;
        return Ok(());
    }
    match (&mut *base, extra) {
        (BindingsDef::PolydatSource(b), BindingsDef::PolydatSource(e)) => {
            if !b.ends_with('\n') {
                b.push('\n');
            }
            b.push_str(&e);
            Ok(())
        }
        (BindingsDef::Map(b), BindingsDef::Map(e)) => {
            for (k, v) in e {
                if b.contains_key(&k) {
                    return Err(format!("binding '{k}' declared by both sides"));
                }
                b.insert(k, v);
            }
            Ok(())
        }
        _ => Err("mixed bindings forms (map vs polydat source) cannot \
                  be concatenated — use one form on both sides"
            .into()),
    }
}

/// Names of every unbound abstract slot in a workload — non-empty
/// means the workload cannot run (load error at initiation).
pub fn unbound_abstract_slots(workload: &Workload) -> Vec<String> {
    let mut out = Vec::new();
    for (phase_name, phase) in &workload.phases {
        for op in &phase.ops {
            if op.abstract_interface.is_some() && !op.interface_bound {
                out.push(format!("{phase_name}.{}", op.name));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(yaml: &str) -> Workload {
        crate::parse::parse_workload(yaml, &HashMap::new()).expect("parse test workload")
    }

    const BLUEPRINT: &str = r#"
params:
  suite_k: "10"
bindings: |
  query_vector := "0.1,0.2"
phases:
  probe:
    cycles: 4
    concurrency: 2
    ops:
      search:
        abstract:
          needs:
            query_vector: String
            suite_k: u64
          yields:
            key: String
"#;

    const IMPL_OK: &str = r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH {query_vector} LIMIT {suite_k}"
        captures: "[key]"
"#;

    #[test]
    fn binds_and_marks_the_slot() {
        let mut blueprint = parse(BLUEPRINT);
        let implementation = parse(IMPL_OK);
        bind_implementation(&mut blueprint, implementation).unwrap();
        let op = &blueprint.phases["probe"].ops[0];
        assert!(op.interface_bound);
        assert!(op.op.contains_key("stmt"));
        assert_eq!(op.captures.len(), 1);
        assert!(unbound_abstract_slots(&blueprint).is_empty());
    }

    /// Implementation params layer over the blueprint's: a new
    /// key adds a protocol knob (and is declared), while a key
    /// the blueprint already declares re-defaults its value with
    /// declaration ownership unchanged — no duplicate entry in
    /// `declared_params`. This is what lets a turnkey variant
    /// (extends-child of an impl) pin dataset/tier params for
    /// single `workload=` invocation.
    #[test]
    fn impl_params_add_knobs_and_redefault_blueprint_params() {
        let mut blueprint = parse(BLUEPRINT);
        let implementation = parse(
            r#"
implements: blueprint
params:
  suite_k: "25"
  knob: "x"
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH {query_vector} LIMIT {suite_k}"
        captures: "[key]"
"#,
        );
        bind_implementation(&mut blueprint, implementation).unwrap();
        assert_eq!(blueprint.params["suite_k"], "25");
        assert_eq!(blueprint.params["knob"], "x");
        assert_eq!(
            blueprint
                .declared_params
                .iter()
                .filter(|k| k.as_str() == "suite_k")
                .count(),
            1,
            "re-defaulting must not re-declare"
        );
        assert!(blueprint.declared_params.iter().any(|k| k == "knob"));
    }

    #[test]
    fn missing_yield_capture_is_named() {
        let mut blueprint = parse(BLUEPRINT);
        let implementation = parse(
            r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH"
"#,
        );
        let err = bind_implementation(&mut blueprint, implementation).unwrap_err();
        assert!(err.contains("yield 'key'"), "err: {err}");
    }

    #[test]
    fn unknown_slot_and_uncovered_slot_are_errors() {
        let mut blueprint = parse(BLUEPRINT);
        let implementation = parse(
            r#"
implements: blueprint
phases:
  probe:
    ops:
      wrong_name:
        stmt: "X"
"#,
        );
        let err = bind_implementation(&mut blueprint, implementation).unwrap_err();
        assert!(err.contains("wrong_name"), "err: {err}");

        let mut blueprint = parse(BLUEPRINT);
        let err =
            bind_implementation(&mut blueprint, parse("implements: blueprint\n")).unwrap_err();
        assert!(err.contains("remain unbound"), "err: {err}");
    }

    #[test]
    fn scaffolding_on_implementation_phase_is_rejected() {
        let mut blueprint = parse(BLUEPRINT);
        let implementation = parse(
            r#"
implements: blueprint
phases:
  probe:
    cycles: 99
    ops:
      search:
        stmt: "X"
        captures: "[key]"
"#,
        );
        let err = bind_implementation(&mut blueprint, implementation).unwrap_err();
        assert!(err.contains("scaffolding"), "err: {err}");
    }

    #[test]
    fn undeclared_need_is_named() {
        let mut blueprint = parse(
            r#"
phases:
  probe:
    cycles: 1
    ops:
      search:
        abstract:
          needs:
            not_provided: u64
"#,
        );
        let err = bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "X"
"#,
            ),
        )
        .unwrap_err();
        assert!(err.contains("need 'not_provided'"), "err: {err}");
    }

    #[test]
    fn op_field_collision_is_an_error() {
        let mut blueprint = parse(
            r#"
phases:
  probe:
    cycles: 1
    ops:
      search:
        stmt: "BLUEPRINT SIDE"
        abstract:
          yields:
            key: String
"#,
        );
        let err = bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "IMPL SIDE"
        captures: "[key]"
"#,
            ),
        )
        .unwrap_err();
        assert!(err.contains("'stmt'") && err.contains("BOTH"), "err: {err}");
    }

    #[test]
    fn unbound_slots_are_reported() {
        let blueprint = parse(BLUEPRINT);
        assert_eq!(unbound_abstract_slots(&blueprint), vec!["probe.search"]);
    }

    // ── SRD-109 Part 3: the `results:` interface leg ──

    const RESULTS_BLUEPRINT: &str = r#"
phases:
  probe:
    cycles: 1
    ops:
      search:
        abstract:
          results:
            keys: vec_i64
"#;

    #[test]
    fn results_wire_delivered_by_impl_result_binding() {
        let mut blueprint = parse(RESULTS_BLUEPRINT);
        bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH"
        result:
          keys: "rows[*].key"
"#,
            ),
        )
        .unwrap();
        let op = &blueprint.phases["probe"].ops[0];
        assert!(op.interface_bound);
        assert!(result_binding_names(op.result.as_ref()).contains("keys"));
    }

    #[test]
    fn missing_results_binding_is_named_with_remedy() {
        let mut blueprint = parse(RESULTS_BLUEPRINT);
        let err = bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH"
"#,
            ),
        )
        .unwrap_err();
        assert!(
            err.contains("results wire") && err.contains("keys") && err.contains("path-expr"),
            "err: {err}"
        );
    }

    #[test]
    fn blueprint_side_path_for_results_wire_is_a_collision() {
        let mut blueprint = parse(
            r#"
phases:
  probe:
    cycles: 1
    ops:
      search:
        result:
          keys: "rows[*].key"
        abstract:
          results:
            keys: vec_i64
"#,
        );
        let err = bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH"
        result:
          keys: "other[*].id"
"#,
            ),
        )
        .unwrap_err();
        assert!(err.contains("protocol matter"), "err: {err}");
    }

    #[test]
    fn result_binding_name_collision_across_sides_is_an_error() {
        // A NON-interface result wire declared by both sides is an
        // ordinary collision.
        let mut blueprint = parse(
            r#"
phases:
  probe:
    cycles: 1
    ops:
      search:
        result:
          row_count: count
        abstract:
          results:
            keys: vec_i64
"#,
        );
        let err = bind_implementation(
            &mut blueprint,
            parse(
                r#"
implements: blueprint
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH"
        result:
          row_count: count
          keys: "rows[*].key"
"#,
            ),
        )
        .unwrap_err();
        assert!(
            err.contains("row_count") && err.contains("BOTH"),
            "err: {err}"
        );
    }
}
