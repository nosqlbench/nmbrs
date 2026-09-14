// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! YAML workload parser and normalizer.
//!
//! Parses a YAML workload definition and normalizes all shorthand
//! forms into the canonical `ParsedOp` model.

use crate::model::{
    BindingsDef, ContinueIfSpec, MetricSpec, ParsedOp, ScenarioNode, ScopeLevel, StopConditionSpec,
    Workload, WorkloadPhase,
};
use crate::template::expand_templates;
use polydat::iteration::comprehension::spec::{
    ComprehensionSpec, ForSpec, parse_clause, parse_clause_list,
};
use serde_json::Value as JVal;
use std::collections::HashMap;

/// Parse a YAML workload string into a normalized Workload.
///
/// In-memory entry point: callers that have already resolved the
/// source text. **Rejects `extends:`** because there is no
/// resolution context for the relative path (no including-file
/// directory). Callers that need `extends:` support must use
/// [`parse_workload_from_path`].
pub fn parse_workload(
    yaml_source: &str,
    params: &HashMap<String, String>,
) -> Result<Workload, String> {
    // Stage 1: TEMPLATE expansion
    let expanded = expand_templates(yaml_source, params);

    // Stage 2: Parse YAML into generic Value
    let doc: JVal =
        serde_yaml::from_str(&expanded).map_err(|e| format!("YAML parse error: {e}"))?;

    let obj = doc.as_object().ok_or("workload must be a YAML mapping")?;

    // SRD-72: `extends:` requires a resolution context. The
    // text-only entry point has no including-file directory, so
    // a top-level `extends:` here is unresolvable. Direct the
    // caller to `parse_workload_from_path` instead.
    if obj.contains_key("extends") {
        return Err(
            "workload declares `extends:` but parse_workload was called \
             without a file path; use parse_workload_from_path instead"
                .to_string(),
        );
    }

    // Stage 3: Extract top-level fields
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut scenario_parse_errors: Vec<String> = Vec::new();
    let mut scenarios = parse_scenarios(obj.get("scenarios"), &mut scenario_parse_errors);
    // Resolve `scenario: <name>` includes after every scenario
    // has been parsed so forward references work and cycles are
    // detected with the full graph available.
    resolve_scenario_includes(&mut scenarios)?;

    let doc_bindings = extract_bindings(obj.get("bindings"));
    let doc_params = extract_value_map(obj.get("params"));
    let doc_tags = extract_string_map(obj.get("tags"));

    // Stage 4: Parse ops from blocks or top-level
    let mut all_ops = Vec::new();

    // SRD-13f Push D: workload-level `bindings:` live ONLY on
    // `Workload.bindings` and compile directly to the
    // workload-root Polydat Kernel. They no longer fold into ops at
    // parse time — descendant ops resolve workload-level wires
    // through the Polydat Kernel chain (workload-root → ... → op
    // kernel) via the SRD-13f cell-on-outputs cascade. So we
    // pass an empty `BindingsDef` to every op-producing path
    // here: block-level YAML bindings (parse_blocks) are the
    // only remaining parser-time "sugar" that expands into ops.
    if let Some(blocks_val) = obj.get("blocks") {
        parse_blocks(blocks_val, &doc_params, &doc_tags, &mut all_ops)?;
    }

    // Top-level ops (no blocks): no block sugar to inline.
    for key in ["ops", "op", "operations", "statements", "statement"] {
        if let Some(ops_val) = obj.get(key)
            && obj.get("blocks").is_none()
        {
            parse_ops_field(
                ops_val,
                "block0",
                &BindingsDef::default(),
                &doc_params,
                &doc_tags,
                &mut all_ops,
            )?;
        }
    }

    // Stage 5: Parse phases
    let (mut phases, phase_order) = parse_phases(obj.get("phases"), &doc_params, &doc_tags)?;

    // Stage 6: Auto-tag all ops (top-level and phase inline ops)
    for op in &mut all_ops {
        if !op.tags.contains_key("name") {
            op.tags.insert("name".to_string(), op.name.clone());
        }
        if !op.tags.contains_key("op") {
            op.tags.insert("op".to_string(), op.name.clone());
        }
    }

    // Stage 6.5 (SRD-108 Part A, completing SRD-20): a phase with
    // no inline ops and a `tags:` selector resolves its ops HERE,
    // from the merged document's top-level/block op pool. Parse
    // time is deliberate: everything downstream — synthesis,
    // validation, the SRD-107 config digest — sees a phase with
    // ordinary resolved ops, exactly as if they were inline. This
    // is the ad-hoc composition seam: an implementation workload
    // `extends:` a blueprint and contributes tagged block ops
    // that these selectors pick up.
    for (phase_name, phase) in phases.iter_mut() {
        let Some(selector) = phase.tags.clone() else {
            continue;
        };
        if !phase.ops.is_empty() {
            return Err(format!(
                "phase '{phase_name}' declares both inline ops and a \
                 `tags:` selector — a phase has exactly one source of \
                 ops. Drop the selector or move the ops into a tagged \
                 block."
            ));
        }
        let mut selected = crate::tags::TagFilter::filter_ops(&all_ops, &selector)
            .map_err(|e| format!("phase '{phase_name}' `tags:` selector: {e}"))?;
        if selected.is_empty() {
            return Err(format!(
                "phase '{phase_name}' `tags:` selector '{selector}' \
                 matched no ops ({} in the workload's op pool). A \
                 selector-only phase must bind at least one op — \
                 check the tag vocabulary against the blocks this \
                 workload (or its extends chain) declares.",
                all_ops.len()
            ));
        }
        // Mirror the inline-op auto-tagging: selected clones gain
        // the `phase` tag their inline siblings get at parse.
        for op in &mut selected {
            op.tags.insert("phase".to_string(), phase_name.clone());
        }
        phase.ops = selected;
    }

    // Stage 7: Resolve workload parameters
    // Priority: CLI params > workload defaults > env vars
    let yaml_params = extract_string_map(obj.get("params"));
    let mut resolved_params = HashMap::new();
    for (key, default_value) in &yaml_params {
        let resolved = if let Some(cli_value) = params.get(key) {
            // CLI override — coerce to the declared default's type. The
            // workload default is the source of truth for type, so a
            // numeric default makes a suffixed override (`10m`, `4Ki`)
            // resolve numerically rather than landing as a string; a
            // non-numeric default leaves the override untouched.
            crate::magnitude::coerce_param_override(default_value, cli_value)
        } else if let Some(env_name) = default_value.strip_prefix("env:") {
            // Environment variable lookup
            std::env::var(env_name).unwrap_or_else(|_| default_value.clone())
        } else {
            default_value.clone()
        };
        resolved_params.insert(key.clone(), resolved);
    }
    // Also include CLI params that aren't in the workload defaults
    // (ad-hoc parameters passed on the command line)
    for (key, value) in params {
        if !resolved_params.contains_key(key) {
            resolved_params.insert(key.clone(), value.clone());
        }
    }

    let declared_params: Vec<String> = yaml_params.keys().cloned().collect();

    // Legacy `summary:` and `plot:` keys: removed, no shim.
    // Operators must migrate to the unified `report:` block
    // (SRD-46). The error message names both new homes.
    if obj.contains_key("summary") || obj.contains_key("summaries") {
        return Err("`summary:` / `summaries:` removed; use `report:` with \
             `table <name> ...` directives instead (SRD-46)"
            .to_string());
    }
    if obj.contains_key("plot") || obj.contains_key("plots") {
        return Err("`plot:` / `plots:` removed; use `report:` with \
             `plot <name> ...` directives instead (SRD-46)"
            .to_string());
    }

    // ── Construction-model enforcement gate ──
    // The document must validate against the enumerable
    // construction grammar (docs/guide/construction_model.md):
    // unknown elements on closed node kinds, value-form
    // violations, and missing required elements are load errors,
    // reported together with their document paths. Runs on the
    // extends-merged document, after the targeted legacy-key
    // rejections above (their migration messages are better than
    // a generic unknown-element finding).
    {
        let doc = serde_json::Value::Object(obj.clone());
        let violations =
            crate::construction::validate_workload(&doc, crate::construction::Mode::Complete);
        if !violations.is_empty() {
            let mut lines: Vec<String> = violations
                .iter()
                .map(|v| format!("  {}: {}", v.path, v.message))
                .collect();
            lines.sort();
            return Err(format!(
                "workload construction: {} violation(s) \
                 (docs/guide/construction_model.md):\n{}",
                violations.len(),
                lines.join("\n")
            ));
        }
    }

    // Unified `report:` block (SRD-46) — plots, tables, defaults,
    // groups. Parser is in `crate::report::parse_report`. The
    // returned warnings are stashed on the Workload for
    // strict-mode promotion downstream (SRD-15).
    let (report, report_warnings) = if let Some(val) = obj.get("report") {
        let parsed = crate::report::parse_report(val).map_err(|e| format!("report: {e}"))?;
        (parsed.report, parsed.warnings)
    } else {
        (crate::report::Report::default(), Vec::new())
    };

    // SRD 21 §"Parameter Resolution": CLI overrides are the
    // outermost layer. Each op has already absorbed the
    // doc → block → op closest-wins merge for YAML-declared
    // params; now overlay the CLI map so `nmbrs run ...
    // concurrency=200` replaces any inherited block-level
    // value. Workload-level `resolved_params` was already
    // CLI-resolved above (line 66–87); this pass extends the
    // same rule down to per-op params.
    if !params.is_empty() {
        for op in &mut all_ops {
            for (key, value) in params {
                // Same SRD-32a exclusion as the inherited-params merge: a CLI
                // `rate=…` / `cycles=…` is a phase/activity override, not an op
                // field — don't leak it into op params (would trip rate).
                if ACTIVITY_PARAM_KEYS.contains(&key.as_str()) {
                    continue;
                }
                op.params
                    .insert(key.clone(), serde_json::Value::String(value.clone()));
            }
        }
    }

    // SRD-44 §"Resume protocol": a phase declared
    // `checkpoint: idempotent` that lives inside a do_while /
    // do_until loop is rejected at workload load time. The
    // do-loop iterates the same phase many times under one
    // checkpoint identity; checkpointing presumes each phase
    // execution is a discrete unit, which the loop directly
    // contradicts. Operators who really want a do-loop'd phase
    // to skip on resume must wrap the loop with explicit
    // identity, not lean on the phase's `checkpoint:` flag.
    for (scenario_name, nodes) in &scenarios {
        let mut bad: Vec<String> = Vec::new();
        collect_idempotent_under_do_loop(nodes, false, &phases, &mut bad);
        if !bad.is_empty() {
            return Err(format!(
                "scenario '{scenario_name}': phase{plural} {names} \
                 declared `checkpoint: idempotent` while nested inside \
                 a do_while / do_until loop. The loop iterates the \
                 same phase identity multiple times, which contradicts \
                 the per-execution unit checkpointing assumes. Either \
                 remove the `checkpoint:` declaration or restructure \
                 the loop. (SRD-44 §\"Resume protocol\".)",
                plural = if bad.len() == 1 { "" } else { "s" },
                names = bad.join(", "),
            ));
        }
    }

    // Doc-root `status_metrics:` — workload-wide default that
    // any phase without its own `status_metrics:` inherits.
    // Same accept-list shapes as the per-phase parser: list of
    // strings, single string, or comma-separated string.
    let doc_status_metrics: Vec<String> = match obj.get("status_metrics") {
        None => Vec::new(),
        Some(JVal::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(JVal::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(other) => {
            return Err(format!(
                "status_metrics: must be a list of names/patterns, a \
             comma-separated string, or omitted; got {other:?}"
            ));
        }
    };
    if !doc_status_metrics.is_empty() {
        for phase in phases.values_mut() {
            if phase.status_metrics.is_empty() {
                phase.status_metrics = doc_status_metrics.clone();
            }
        }
    }

    // Doc-root `readouts:` block (SRD-63 §5.0). Three
    // accepted shapes:
    //   A. Single scalar string  → bound at on_update.
    //   B. Mapping of slot → name | body string.
    //   C. Mapping of slot → list of (name | body) strings.
    // The slot keys must match the lower-cased
    // Event::slot_name values (`on_update`, `on_phase_end`, …).
    // Inline body strings keep their full text — the
    // body-grammar parser in nmbrs-runtime::readouts::parse
    // bakes them at activity-init time.
    let readouts = parse_readouts_block(obj.get("readouts"))?;

    // SRD-83 — top-level `stop_when:` (workload-shell conditions), same
    // shape as the per-phase block.
    let stop_when: Vec<crate::model::StopConditionSpec> = match obj.get("stop_when") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| format!("invalid top-level `stop_when` block: {e}"))?,
        None => Vec::new(),
    };
    // SRD-83 Part 5 — the effect verb is a closed vocabulary; an unknown
    // verb would silently resolve to the shell default at the trip site.
    for sc in &stop_when {
        sc.validate()
            .map_err(|e| format!("top-level `stop_when`: {e}"))?;
    }

    // SRD-108 Part B — `implements:` names the blueprint this
    // document provides op bodies for. A non-string value is
    // rejected rather than ignored.
    let implements: Option<String> = match obj.get("implements") {
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| {
                    format!("`implements:` must be a workload reference string, got {v}")
                })?
                .to_string(),
        ),
        None => None,
    };

    // SRD-106 Part 3 — `stick_session:` top-level bool. A non-bool
    // value is rejected rather than ignored (never-ignore-silently).
    let stick_session: Option<bool> = match obj.get("stick_session") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| format!("`stick_session:` must be a boolean, got {v}"))?,
        ),
        None => None,
    };

    Ok(Workload {
        description,
        scenarios,
        ops: all_ops,
        bindings: doc_bindings,
        params: resolved_params,
        phases,
        phase_order,
        declared_params,
        stop_when,
        report,
        report_warnings,
        scenario_parse_errors,
        resolution_warnings: Vec::new(),
        status_metrics: doc_status_metrics,
        readouts,
        wrappers: None,
        implements,
        stick_session,
    })
}

/// Path-based entry point: load a workload YAML from disk,
/// follow its `extends:` chain (SRD-72), and parse the merged
/// result into a [`Workload`].
///
/// `path` MUST be an existing file. Relative paths are resolved
/// against the cwd before being passed to the loader.
pub fn parse_workload_from_path(
    path: &std::path::Path,
    params: &HashMap<String, String>,
) -> Result<Workload, String> {
    let (merged_yaml, resolution_warnings) = crate::extends::load_and_merge(path)?;
    let mut workload = parse_workload(&merged_yaml, params)?;
    // Reference-resolution warnings ride the same surfacing
    // channel as report warnings: stashed here, logged (or
    // strict-promoted) by the runner.
    workload.resolution_warnings.extend(resolution_warnings);
    Ok(workload)
}

/// Parse the workload's `readouts:` block per SRD-63 §5.0.
/// Returns the populated bindings struct or a load-time
/// error.
fn parse_readouts_block(value: Option<&JVal>) -> Result<crate::model::ReadoutsBindings, String> {
    use crate::model::ReadoutsBindings;
    let mut out = ReadoutsBindings::default();
    let Some(value) = value else {
        return Ok(out);
    };

    // Form A — scalar shorthand for `on_update`.
    if let JVal::String(s) = value {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(out);
        }
        out.on_update.push(trimmed.to_string());
        return Ok(out);
    }

    // Forms B / C — mapping with slot keys.
    let JVal::Object(map) = value else {
        return Err(format!(
            "readouts: must be a scalar (sugar for on_update) or a mapping \
             of slot name → readout body; got {value:?}"
        ));
    };
    for (key, val) in map {
        let bodies: Vec<String> = match val {
            JVal::String(s) => vec![s.trim().to_string()],
            JVal::Array(items) => items
                .iter()
                .map(|item| match item {
                    JVal::String(s) => Ok(s.trim().to_string()),
                    other => Err(format!(
                        "readouts.{key}: list entries must be strings; got {other:?}"
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?,
            JVal::Null => continue,
            other => {
                return Err(format!(
                    "readouts.{key}: must be a string or list of strings; got {other:?}"
                ));
            }
        };
        let bodies: Vec<String> = bodies.into_iter().filter(|s| !s.is_empty()).collect();
        // Wildcard expansion (SRD-63 §4.1.1). The yaml key
        // can match a family rather than a single slot:
        //   `each_*`    → on_each_start + on_each_end
        //   `phase_*`   → on_phase_start + on_phase_end
        //   `scope_*`   → on_scope_start + on_scope_end
        //   `session_*` → on_session_start + on_session_end
        //   `*`         → every slot
        // Wildcard bindings are duplicated into each
        // matching slot so the binder's resolution doesn't
        // need a separate wildcard list. Render order:
        // explicit bindings first in declaration order
        // (handled by parser top-down), then any wildcard
        // expansions appended.
        let target_slots: Vec<&str> = match key.as_str() {
            "on_session_start" => vec!["on_session_start"],
            "on_session_end" => vec!["on_session_end"],
            "on_phase_start" => vec!["on_phase_start"],
            "on_phase_end" => vec!["on_phase_end"],
            "on_each_start" => vec!["on_each_start"],
            "on_each_end" => vec!["on_each_end"],
            "on_scope_start" => vec!["on_scope_start"],
            "on_scope_end" => vec!["on_scope_end"],
            "on_update" => vec!["on_update"],
            "session_*" => vec!["on_session_start", "on_session_end"],
            "phase_*" => vec!["on_phase_start", "on_phase_end"],
            "each_*" => vec!["on_each_start", "on_each_end"],
            "scope_*" => vec!["on_scope_start", "on_scope_end"],
            "*" => vec![
                "on_session_start",
                "on_session_end",
                "on_phase_start",
                "on_phase_end",
                "on_each_start",
                "on_each_end",
                "on_scope_start",
                "on_scope_end",
                "on_update",
            ],
            other => {
                return Err(format!(
                    "readouts: unknown slot '{other}'. Known: \
                 on_session_start/end, on_phase_start/end, \
                 on_each_start/end, on_scope_start/end, on_update; \
                 wildcards: each_*, phase_*, scope_*, session_*, *"
                ));
            }
        };
        for slot in target_slots {
            let target: &mut Vec<String> = match slot {
                "on_session_start" => &mut out.on_session_start,
                "on_session_end" => &mut out.on_session_end,
                "on_phase_start" => &mut out.on_phase_start,
                "on_phase_end" => &mut out.on_phase_end,
                "on_each_start" => &mut out.on_each_start,
                "on_each_end" => &mut out.on_each_end,
                "on_scope_start" => &mut out.on_scope_start,
                "on_scope_end" => &mut out.on_scope_end,
                "on_update" => &mut out.on_update,
                _ => unreachable!(),
            };
            target.extend(bodies.iter().cloned());
        }
    }
    Ok(out)
}

/// Walk a scenario tree and collect names of phases declared
/// `checkpoint: idempotent` that live under a `do_while` /
/// `do_until` ancestor. Used by the workload-load validation
/// step above (SRD-44).
fn collect_idempotent_under_do_loop(
    nodes: &[crate::model::ScenarioNode],
    in_do_loop: bool,
    phases: &HashMap<String, crate::model::WorkloadPhase>,
    out: &mut Vec<String>,
) {
    use crate::model::ScenarioNode;
    for node in nodes {
        match node {
            ScenarioNode::Phase(name) => {
                if in_do_loop {
                    let idempotent = phases
                        .get(name)
                        .and_then(|p| p.checkpoint.as_ref())
                        .map(|c| c.idempotent)
                        .unwrap_or(false);
                    if idempotent {
                        out.push(format!("'{name}'"));
                    }
                }
            }
            ScenarioNode::DoWhile { children, .. } | ScenarioNode::DoUntil { children, .. } => {
                collect_idempotent_under_do_loop(children, true, phases, out);
            }
            ScenarioNode::Comprehension { children, .. } => {
                collect_idempotent_under_do_loop(children, in_do_loop, phases, out);
            }
            ScenarioNode::IncludedScenario { children, .. } => {
                collect_idempotent_under_do_loop(children, in_do_loop, phases, out);
            }
            ScenarioNode::Bindings { children, .. } => {
                collect_idempotent_under_do_loop(children, in_do_loop, phases, out);
            }
        }
    }
}

/// Parse a YAML source into just the list of normalized ParsedOps.
/// Normalise an op-level `if:` clause so callers can write
/// expressions naturally. The downstream pipeline expects
/// the condition to be either a binding-name reference
/// (`{name}`) or an inline expression (`{{expr}}`). When
/// the operator writes a bare expression like
/// `cql_dialect == 'cass'`, that doesn't match either
/// form and the conditional dispenser fails at init when
/// it tries to look up a Polydat binding literally named
/// `cql_dialect == 'cass'`.
///
/// Heuristic: if the trimmed value already starts with `{`
/// (any braced form), or is a plain identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`), pass through unchanged.
/// Otherwise treat as an inline expression and wrap with
/// `{{...}}` so the existing inline-expression machinery
/// in `nmbrs-runtime::scope::build_scope` synthesises a
/// hidden binding (`__expr_N := <expr>`) and rewrites the
/// condition to reference it.
pub fn normalize_condition_clause(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    // Already in any braced form — pass through. The
    // downstream extractor handles `{{expr}}`, `{:=expr:=}`,
    // `{name}`, and `{expr-with-operators}` itself.
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    // Plain identifier — leave as-is so the legacy "if:
    // points at a single binding name" form keeps working.
    let is_plain_ident = trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if is_plain_ident {
        return trimmed.to_string();
    }
    // Anything else (operators, quotes, parens, …) → wrap
    // as an inline expression.
    format!("{{{{{trimmed}}}}}")
}

pub fn parse_ops(yaml_source: &str) -> Result<Vec<ParsedOp>, String> {
    let workload = parse_workload(yaml_source, &HashMap::new())?;
    Ok(workload.ops)
}

// -----------------------------------------------------------------
// Scenarios
// -----------------------------------------------------------------

fn parse_scenarios(
    val: Option<&JVal>,
    errors: &mut Vec<String>,
) -> HashMap<String, Vec<ScenarioNode>> {
    let mut scenarios = HashMap::new();
    let Some(val) = val else {
        return scenarios;
    };
    let Some(obj) = val.as_object() else {
        return scenarios;
    };

    for (scenario_name, steps_val) in obj {
        let nodes = parse_scenario_nodes_with_errors(steps_val, scenario_name, errors);
        scenarios.insert(scenario_name.clone(), nodes);
    }
    scenarios
}

/// Wrapper around the legacy `parse_scenario_nodes` that
/// accumulates `unknown-key` errors against a per-call sink.
/// Used by `parse_scenarios` so the top-level builder can
/// inspect the result and refuse to dispatch a malformed
/// workload (per "Never Ignore Silently"). The legacy
/// non-erroring shape is still exposed for the in-file
/// recursive callers that pre-date the error contract;
/// those paths quietly drop the unknown-key information,
/// but the top-level entry now sees it.
fn parse_scenario_nodes_with_errors(
    val: &JVal,
    scenario_name: &str,
    errors: &mut Vec<String>,
) -> Vec<ScenarioNode> {
    match val {
        JVal::Array(arr) => arr
            .iter()
            .flat_map(|item| parse_scenario_nodes_with_errors(item, scenario_name, errors))
            .collect(),
        JVal::Object(obj) => {
            if has_recognised_scenario_key(obj) {
                return parse_scenario_nodes(val);
            }
            // No recognized scenario-node key. Two legitimate
            // shapes can land here:
            //
            // 1. **Legacy command-string form** — `{ name: "run
            //    ..." }`. Each entry maps a step name to a CLI
            //    command string; the catch-all in
            //    `parse_scenario_nodes` produces one
            //    `ScenarioNode::Phase(<name>)` per key. The
            //    values are always strings (CLI command lines).
            //    Accepted.
            //
            // 2. **Malformed unknown-key node** — `{ iterate: {
            //    phases: [...] } }`, `{ for_with_typo: ... }`,
            //    etc. The value is a map or array, which means
            //    the author was trying to express structure the
            //    parser doesn't understand. Reject loudly per
            //    the project's "Never Ignore Silently" rule —
            //    silently dropping these used to cause
            //    confusing downstream errors (`phase 'iterate'
            //    not found` masking a typoed `for_each` key).
            let bad: Vec<&String> = obj
                .iter()
                .filter(|(_, v)| !matches!(v, JVal::String(_) | JVal::Null))
                .map(|(k, _)| k)
                .collect();
            if !bad.is_empty() {
                let bad_names: Vec<&str> = bad.iter().map(|s| s.as_str()).collect();
                errors.push(format!(
                    "scenario '{scenario_name}': unrecognised scenario-node key(s) \
                     {bad_names:?} carry non-string values (a map or array). \
                     Expected one of: `for_each` / `for`, `scenarios`, \
                     `for_combinations`, `do_while`, `do_until`, `bindings`, \
                     `set`, `scenario`. (The legacy `name: \"run ...\"` \
                     command-string form is still accepted when the value is \
                     a plain string.)"
                ));
                return Vec::new();
            }
            // All values are strings — legacy command-string
            // form. Route through the legacy catch-all.
            parse_scenario_nodes(val)
        }
        _ => parse_scenario_nodes(val),
    }
}

/// True iff this scenario-node object has at least one of the
/// recognized keys handled by `parse_scenario_nodes`. Used by
/// the error-aware wrapper to distinguish "malformed node"
/// (no recognized key) from "legitimate node" before the
/// silent-catchall in the legacy path can fire.
fn has_recognised_scenario_key(obj: &serde_json::Map<String, JVal>) -> bool {
    crate::vocab::scenario_node_keys()
        .iter()
        .any(|k| obj.contains_key(*k))
}

/// Emit the polydat RHS for a scenario `set:` value. A **bare identifier is a
/// wire REFERENCE**, consistent with comprehension r-values (SRD-18f): so
/// `set: { x: mnc }` binds `mnc`'s value, and an unresolved bare name is a
/// hard error at scope synthesis. A number/bool is that literal; a YAML
/// sequence is a list (of references / literals). A STRING literal must be
/// written explicitly as a polydat-quoted scalar — `'"verbose"'` in YAML —
/// because the pipeline's serde round-trips strip ordinary YAML quotes,
/// leaving a bare word indistinguishable from a reference.
fn emit_set_value_literal(value: &JVal) -> String {
    match value {
        JVal::Number(n) => n.to_string(),
        JVal::Bool(b) => b.to_string(),
        JVal::Null => "\"\"".to_string(),
        // A YAML sequence is a list literal; its elements are references
        // (bare) or literals.
        JVal::Array(elems) => {
            let parts: Vec<String> = elems.iter().map(emit_set_array_element).collect();
            format!("[{}]", parts.join(", "))
        }
        JVal::String(s) => {
            let t = s.trim();
            if is_polydat_quoted_string(t) {
                // Explicit polydat string literal (`'"verbose"'` in YAML).
                t.to_string()
            } else if crate::bindpoints::is_bare_identifier(t) {
                // Bare identifier ⇒ a wire reference (unquoted; polydat
                // resolves it against the in-scope chain).
                t.to_string()
            } else {
                // Free text / `{name}` templates ⇒ a string literal.
                let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            }
        }
        other => {
            let escaped = other.to_string().replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

/// One element of a `set:` sequence value: a bare-identifier element is a
/// reference (unquoted), a number/bool is its literal, any other string is a
/// quoted string element.
fn emit_set_array_element(e: &JVal) -> String {
    match e {
        JVal::String(s) if crate::bindpoints::is_bare_identifier(s.trim()) => s.trim().to_string(),
        JVal::Number(n) => n.to_string(),
        JVal::Bool(b) => b.to_string(),
        JVal::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
        other => other.to_string(),
    }
}

/// Recursively parse scenario nodes from YAML.
///
/// Handles:
/// - String: phase name
/// - Object with `for_each` + `phases`: for_each loop (phases parsed recursively)
/// - Array: list of nodes
///
/// Map a `ScopeLevel` keyword (the `each:` vocabulary) from YAML text.
fn parse_scope_level(s: &str) -> Option<ScopeLevel> {
    match s {
        "self" => Some(ScopeLevel::SelfScope),
        "op" => Some(ScopeLevel::Op),
        "phase" => Some(ScopeLevel::Phase),
        "scenario" => Some(ScopeLevel::Scenario),
        "workload" => Some(ScopeLevel::Workload),
        _ => None,
    }
}

/// SRD-101 — parse a `continue_if:` value into a [`ContinueIfSpec`]. Accepts
/// the short string form (`continue_if: "end_of(p) <= max"` → `each: scenario`)
/// and the long map form (`{ when, each }`, where `each` is a single level or a
/// list). `each` defaults to `scenario` (the enclosing sweep).
/// Render a JSON scalar (string or number) as a duration string for the
/// `tries: {backoff: {min, max}}` map. A string passes through (`"100ms"`);
/// a bare number becomes its decimal text (`100` → `"100"`, taken as ms by
/// the runtime's duration parser). Non-scalars yield `None`.
fn json_scalar_to_dur_string(v: &JVal) -> Option<String> {
    match v {
        JVal::String(s) => Some(s.clone()),
        JVal::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_continue_if(val: Option<&JVal>) -> Option<ContinueIfSpec> {
    match val? {
        JVal::String(when) => Some(ContinueIfSpec {
            when: when.clone(),
            each: vec![ScopeLevel::Scenario],
        }),
        JVal::Object(map) => {
            let when = map.get("when").and_then(|v| v.as_str())?.to_string();
            let each: Vec<ScopeLevel> = match map.get("each") {
                Some(JVal::String(s)) => parse_scope_level(s).into_iter().collect(),
                Some(JVal::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().and_then(parse_scope_level))
                    .collect(),
                _ => Vec::new(),
            };
            let each = if each.is_empty() {
                vec![ScopeLevel::Scenario]
            } else {
                each
            };
            Some(ContinueIfSpec { when, each })
        }
        _ => None,
    }
}

fn parse_scenario_nodes(val: &JVal) -> Vec<ScenarioNode> {
    match val {
        JVal::String(s) => vec![ScenarioNode::Phase(s.clone())],
        JVal::Array(arr) => arr.iter().flat_map(parse_scenario_nodes).collect(),
        JVal::Object(obj) => {
            let children = obj
                .get("phases")
                .map(parse_scenario_nodes)
                .unwrap_or_default();
            let counter = obj
                .get("counter")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // `for_each` is the canonical key; `for` is accepted as
            // a shorter synonym ("for k in 10,100" reads more
            // naturally and matches the Polydat comprehension text
            // grammar). Both keys are interchangeable; if both
            // appear, `for_each` wins so misconfigured workloads
            // don't silently change shape.
            if let Some(for_each_val) = obj.get("for_each").or_else(|| obj.get("for")) {
                // for_each supports three YAML shapes (string,
                // array, object) that collapse into one of three
                // semantic variants (Cartesian / Union / single-
                // clause). The detection rule lives in
                // `ComprehensionSpec::into_legacy` (and ultimately
                // in `comprehension_from_subspaces` underneath) —
                // single source of truth.
                //
                // YAML-shape → ForSpec mapping:
                //   "x in 1, y in 2"               → Inline (string)
                //   "x in 1, x in 2"               → Inline (string,
                //                                    name repeat ⇒ Union)
                //   ["x in 1", "y in 2"]           → UnionOfClauseLists
                //                                    (one entry per
                //                                    sub-space; name
                //                                    repeat across
                //                                    entries ⇒ Union;
                //                                    no repeat ⇒
                //                                    Cartesian after
                //                                    flattening)
                //   { x: "1", y: "2" }             → Inline (string,
                //                                    assembled from
                //                                    key=value pairs)
                let for_spec: Option<ForSpec> = match for_each_val {
                    JVal::String(spec) => Some(ForSpec::Inline(spec.clone())),
                    JVal::Array(arr) => {
                        // One sub-space per array entry. Each
                        // entry can hold multiple comma-separated
                        // clauses (cartesian within); cross-entry
                        // is unioned. Wrap each entry in a
                        // singleton inner-list so the
                        // UnionOfClauseLists shape carries the
                        // "one entry = one sub-space" semantic.
                        let groups: Vec<Vec<String>> = arr
                            .iter()
                            .filter_map(|item| item.as_str().map(|s| vec![s.to_string()]))
                            .collect();
                        if groups.is_empty() {
                            None
                        } else {
                            Some(ForSpec::UnionOfClauseLists(groups))
                        }
                    }
                    JVal::Object(map) => {
                        // Map form is always a single sub-space
                        // (keys are unique). Assemble into an
                        // inline string and route through
                        // ForSpec::Inline.
                        if map.is_empty() {
                            None
                        } else {
                            let inline = map
                                .iter()
                                .map(|(k, v)| format!("{k} in {}", v.as_str().unwrap_or("")))
                                .collect::<Vec<_>>()
                                .join(", ");
                            Some(ForSpec::Inline(inline))
                        }
                    }
                    _ => None,
                };

                match for_spec {
                    None => vec![],
                    Some(r#for) => {
                        let spec = ComprehensionSpec {
                            r#for,
                            // Optional `where:` key carries a
                            // filter predicate evaluated per
                            // emitted tuple.
                            r#where: obj.get("where").and_then(|v| v.as_str()).map(String::from),
                            // Optional `order:` key carries a
                            // traversal order spec (GK text form,
                            // e.g. "extrema/1").
                            order: obj.get("order").and_then(|v| v.as_str()).map(String::from),
                        };
                        match spec.into_algebra() {
                            Ok(comprehension) => vec![ScenarioNode::Comprehension {
                                comprehension,
                                children,
                                continue_if: parse_continue_if(obj.get("continue_if")),
                                anchor: obj
                                    .get("anchor")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            }],
                            Err(e) => {
                                eprintln!("warning: comprehension: {e}");
                                vec![]
                            }
                        }
                    }
                }
            } else if let Some(scenario_val) = obj.get("scenario").and_then(|v| v.as_str()) {
                // `scenario: <name>` — logical inclusion of
                // another scenario at this point in the tree.
                // Children remain empty here; resolution happens
                // post-parse via `resolve_scenario_includes`,
                // once every scenario in the workload is known.
                vec![ScenarioNode::IncludedScenario {
                    name: scenario_val.to_string(),
                    children: Vec::new(),
                }]
            } else if let Some(scenarios_val) = obj.get("scenarios") {
                // `scenarios: [name, name, ...]` — plural form
                // for composing several named scenarios at one
                // node in the tree. Each list entry expands to
                // its own `IncludedScenario`; resolution happens
                // post-parse via `resolve_scenario_includes`.
                // Reads more naturally than repeating
                // `- scenario: foo` for each entry; both forms
                // are interchangeable.
                //
                // Map / object entries (`{ scenario: foo }`) are
                // also accepted so a list can mix bare-string
                // includes with other scenario-node shapes
                // already supported by `parse_scenario_nodes`.
                match scenarios_val {
                    JVal::Array(arr) => arr
                        .iter()
                        .flat_map(|item| {
                            match item {
                                JVal::String(s) => vec![ScenarioNode::IncludedScenario {
                                    name: s.clone(),
                                    children: Vec::new(),
                                }],
                                // Anything else (object with
                                // `scenario:`, `for_each:`, etc.)
                                // routes through the standard parse
                                // path so list entries can be
                                // heterogeneous.
                                _ => parse_scenario_nodes(item),
                            }
                        })
                        .collect(),
                    JVal::String(s) => vec![ScenarioNode::IncludedScenario {
                        name: s.clone(),
                        children: Vec::new(),
                    }],
                    _ => Vec::new(),
                }
            } else if let Some(combo_val) = obj.get("for_combinations") {
                // Explicit for_combinations keyword (alias for
                // multi-clause for_each). Route through
                // ComprehensionSpec so the for_combinations branch
                // shares the for_each branch's single chokepoint.
                // Distinct-var input (the typical case) parses as
                // Cartesian via the name-repetition detection rule
                // — same semantics as the legacy direct
                // `Comprehension::cartesian(...)` path.
                let specs = parse_combination_specs(combo_val);
                let inline = specs
                    .iter()
                    .map(|(v, e)| format!("{v} in {e}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let spec = ComprehensionSpec {
                    r#for: ForSpec::Inline(inline),
                    r#where: obj.get("where").and_then(|v| v.as_str()).map(String::from),
                    order: obj.get("order").and_then(|v| v.as_str()).map(String::from),
                };
                match spec.into_algebra() {
                    Ok(comprehension) => vec![ScenarioNode::Comprehension {
                        comprehension,
                        children,
                        continue_if: parse_continue_if(obj.get("continue_if")),
                        anchor: obj.get("anchor").and_then(|v| v.as_str()).map(String::from),
                    }],
                    Err(e) => {
                        eprintln!("warning: for_combinations: {e}");
                        vec![]
                    }
                }
            } else if let Some(cond) = obj.get("do_while").and_then(|v| v.as_str()) {
                vec![ScenarioNode::DoWhile {
                    condition: cond.to_string(),
                    counter,
                    children,
                }]
            } else if let Some(cond) = obj.get("do_until").and_then(|v| v.as_str()) {
                vec![ScenarioNode::DoUntil {
                    condition: cond.to_string(),
                    counter,
                    children,
                }]
            } else if let Some(bindings_val) = obj.get("bindings") {
                // Scenario-tree-level `bindings:` — arbitrary GK
                // matter that installs as a scope-tree layer over
                // the parent. The source text is whatever the
                // author wrote (interpretation happens at kernel
                // build time via the canonical scope synthesizer).
                let source = match bindings_val {
                    JVal::String(s) => s.clone(),
                    JVal::Object(map) => {
                        // Map form: each `name: expr` produces
                        // one `name := <expr>` line. The GK
                        // compiler classifies the modifier from
                        // any leading `final`/`init`/`shared`
                        // keyword in `name`; the bare-name case
                        // becomes a cycle binding.
                        let mut out = String::new();
                        for (name, value) in map {
                            let v_text = match value {
                                JVal::String(s) => s.clone(),
                                JVal::Bool(b) => b.to_string(),
                                JVal::Number(n) => n.to_string(),
                                JVal::Null => String::new(),
                                other => other.to_string(),
                            };
                            out.push_str(&format!("{name} := {v_text}\n"));
                        }
                        out
                    }
                    _ => String::new(),
                };
                if source.trim().is_empty() {
                    Vec::new()
                } else {
                    if children.is_empty() {
                        eprintln!(
                            "warning: scenario-tree `bindings:` block has no \
                             `phases:` body — this is a no-op (the scope is \
                             entered and immediately exited with no descendants \
                             reading any of its declared names). If you meant \
                             to publish these bindings to a subtree, add a \
                             `phases:` block; if you meant to declare workload-\
                             level bindings, move them to the top-level \
                             `bindings:` field of the workload."
                        );
                    }
                    vec![ScenarioNode::Bindings { source, children }]
                }
            } else if let Some(set_val) = obj.get("set") {
                // `set: { name: value, ... }` — convenience sugar
                // that desugars to a `Bindings` node carrying GK
                // matter of the shape `const NAME := <polydat-literal>`.
                // The Polydat compiler handles workload-param
                // interpolation, string-literal interpolation,
                // and full const-expression evaluation at kernel
                // build time, so this form composes with every
                // other Polydat feature (no separate two-pass
                // evaluator). The `Bindings` node carries the
                // synthesized source; downstream code never sees
                // a SetParam-specific shape.
                //
                // String shorthand `set: name=value` is also
                // accepted for the single-override one-liner case.
                //
                // Map iteration order is preserved by serde_yaml
                // (insertion order); declaration order wins on
                // collision via the standard Polydat shadow semantics
                // for the same source.
                let pairs: Vec<(String, JVal)> = match set_val {
                    JVal::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    JVal::String(s) => match s.split_once('=') {
                        Some((k, v)) => {
                            vec![(k.trim().to_string(), JVal::String(v.trim().to_string()))]
                        }
                        None => {
                            eprintln!(
                                "warning: scenario `set:` string form must be \
                                     `name=value`, got `{s}` — ignoring"
                            );
                            Vec::new()
                        }
                    },
                    _ => Vec::new(),
                };
                if pairs.is_empty() {
                    Vec::new()
                } else if children.is_empty() {
                    // Same no-op condition as the explicit
                    // `bindings:` form: a `set:` with no
                    // `phases:` body publishes overrides to
                    // nothing. Almost certainly an author error.
                    let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
                    eprintln!(
                        "warning: scenario-tree `set:` block (overriding {:?}) \
                         has no `phases:` body — this is a no-op. Add a `phases:` \
                         block listing what the override applies to.",
                        names
                    );
                    Vec::new()
                } else {
                    // Synthesize the Polydat source body. Each pair
                    // becomes `const NAME := <polydat-literal>` — the
                    // single canonical effectively-const binding
                    // shape. The compiler tries const-fold at
                    // compile time (pure literals fold; values
                    // containing `{name}` interpolation depend on
                    // extern slots that aren't populated until
                    // materialize-wiring) and falls back to
                    // scope-init pull when fold isn't possible.
                    // Either way the value is materialized once
                    // and then immutable for the scope's
                    // lifetime — author doesn't need to think
                    // about which path the runtime takes.
                    //
                    // Each value lowers to `const NAME := <rhs>` via
                    // `emit_set_value_literal`. A BARE identifier is a wire
                    // reference (consistent with comprehension r-values,
                    // SRD-18f) — `set: { x: mnc }` binds mnc's value, and an
                    // unresolved bare name is a hard error at scope synthesis.
                    // Numbers/bools are literals; a YAML sequence is a list. A
                    // string literal is the explicit polydat-quoted form
                    // `'"verbose"'` (ordinary YAML quotes don't survive the
                    // pipeline's serde round-trips).
                    let mut source = String::new();
                    for (name, value) in &pairs {
                        source.push_str(&format!(
                            "const {name} := {}\n",
                            emit_set_value_literal(value),
                        ));
                    }
                    vec![ScenarioNode::Bindings { source, children }]
                }
            } else {
                obj.iter()
                    .map(|(name, _cmd)| ScenarioNode::Phase(name.clone()))
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

/// Resolve every `IncludedScenario { name, children: [] }` node
/// produced by [`parse_scenario_nodes`] into one whose `children`
/// hold a clone of the referenced scenario's resolved nodes.
///
/// Two failure modes are surfaced as parse errors:
///
/// 1. **Unknown scenario name** — `scenario: foo` where no
///    `scenarios.foo` exists.
/// 2. **Cycle** — `A` includes `B` which (transitively) includes
///    `A`. The error names the full cycle path so the operator
///    can fix the offending edge.
///
/// Resolution is depth-first with memoization so each scenario
/// is resolved at most once regardless of how many places
/// reference it. After this pass the workload model carries no
/// unresolved `IncludedScenario` nodes; downstream consumers
/// (scope tree, executor, runner) can treat the variant as a
/// fully-formed wrapper scope.
pub fn resolve_scenario_includes(
    scenarios: &mut HashMap<String, Vec<ScenarioNode>>,
) -> Result<(), String> {
    use std::collections::HashSet;

    // Snapshot the input so resolution reads from a stable map
    // while we mutate the output. Each resolved scenario is
    // recorded back into `out`.
    let input: HashMap<String, Vec<ScenarioNode>> = scenarios.clone();
    let mut out: HashMap<String, Vec<ScenarioNode>> = HashMap::new();

    fn resolve_nodes(
        nodes: &[ScenarioNode],
        input: &HashMap<String, Vec<ScenarioNode>>,
        out: &mut HashMap<String, Vec<ScenarioNode>>,
        stack: &mut Vec<String>,
    ) -> Result<Vec<ScenarioNode>, String> {
        let mut resolved = Vec::with_capacity(nodes.len());
        for n in nodes {
            resolved.push(resolve_one(n, input, out, stack)?);
        }
        Ok(resolved)
    }

    fn resolve_one(
        node: &ScenarioNode,
        input: &HashMap<String, Vec<ScenarioNode>>,
        out: &mut HashMap<String, Vec<ScenarioNode>>,
        stack: &mut Vec<String>,
    ) -> Result<ScenarioNode, String> {
        match node {
            ScenarioNode::Phase(name) => Ok(ScenarioNode::Phase(name.clone())),
            ScenarioNode::IncludedScenario { name, .. } => {
                if stack.iter().any(|s| s == name) {
                    let mut path = stack.clone();
                    path.push(name.clone());
                    return Err(format!(
                        "scenario include cycle detected: {}",
                        path.join(" -> "),
                    ));
                }
                let target = input.get(name).ok_or_else(|| {
                    format!(
                        "scenario include 'scenario: {name}' references an unknown \
                     scenario. Known scenarios: {}",
                        {
                            let mut names: Vec<&str> = input.keys().map(|s| s.as_str()).collect();
                            names.sort();
                            names.join(", ")
                        },
                    )
                })?;
                stack.push(name.clone());
                let children = resolve_nodes(target, input, out, stack)?;
                stack.pop();
                // Memoize the resolved scenario for any later
                // include reference. Idempotent: equivalent
                // resolved children produced regardless of
                // entry point.
                out.entry(name.clone()).or_insert_with(|| children.clone());
                Ok(ScenarioNode::IncludedScenario {
                    name: name.clone(),
                    children,
                })
            }
            ScenarioNode::Comprehension {
                comprehension,
                children,
                continue_if,
                anchor,
            } => Ok(ScenarioNode::Comprehension {
                comprehension: comprehension.clone(),
                children: resolve_nodes(children, input, out, stack)?,
                continue_if: continue_if.clone(),
                anchor: anchor.clone(),
            }),
            ScenarioNode::DoWhile {
                condition,
                counter,
                children,
            } => Ok(ScenarioNode::DoWhile {
                condition: condition.clone(),
                counter: counter.clone(),
                children: resolve_nodes(children, input, out, stack)?,
            }),
            ScenarioNode::DoUntil {
                condition,
                counter,
                children,
            } => Ok(ScenarioNode::DoUntil {
                condition: condition.clone(),
                counter: counter.clone(),
                children: resolve_nodes(children, input, out, stack)?,
            }),
            ScenarioNode::Bindings { source, children } => Ok(ScenarioNode::Bindings {
                source: source.clone(),
                children: resolve_nodes(children, input, out, stack)?,
            }),
        }
    }

    let mut visited: HashSet<String> = HashSet::new();
    let names: Vec<String> = scenarios.keys().cloned().collect();
    for name in names {
        if visited.contains(&name) {
            continue;
        }
        let mut stack = vec![name.clone()];
        let resolved = resolve_nodes(&input[&name], &input, &mut out, &mut stack)?;
        out.insert(name.clone(), resolved);
        visited.insert(name);
    }
    *scenarios = out;
    Ok(())
}

/// Parse combination specs from any of three YAML forms:
///
/// **Map form** (keys = variables, values = expressions):
/// ```yaml
/// for_combinations:
///   profile: "matching_profiles('{dataset}', '{prefix}')"
///   k: "{k_values}"
/// ```
///
/// **List form** (reuses for_each "var in expr" syntax):
/// ```yaml
/// for_combinations:
///   - "profile in matching_profiles('{dataset}', '{prefix}')"
///   - "k in {k_values}"
/// ```
///
/// **Inline form** (compact comma-separated):
/// ```yaml
/// for_combinations: "profile in profiles, k in {k_values}"
/// ```
fn parse_combination_specs(val: &JVal) -> Vec<(String, String)> {
    match val {
        // Map form: { "profile": "expr", "k": "expr" }
        JVal::Object(map) => map
            .iter()
            .map(|(key, val)| {
                let expr = val.as_str().unwrap_or("").to_string();
                (key.clone(), expr)
            })
            .collect(),
        // List form: ["profile in expr", "k in expr"]
        JVal::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                let s = item.as_str()?;
                match parse_clause(s) {
                    Ok(c) => Some((c.var().to_string(), c.expr().to_string())),
                    Err(e) => {
                        eprintln!("warning: for_combinations: {e}");
                        None
                    }
                }
            })
            .collect(),
        // Inline form: "profile in expr, k in expr"
        // Split on commas that are NOT inside parentheses (respects
        // function calls like `matching_profiles('{dataset}', '{prefix}')`).
        JVal::String(s) => match parse_clause_list(s) {
            Ok(clauses) => clauses
                .into_iter()
                .map(|c| (c.var().to_string(), c.expr().to_string()))
                .collect(),
            Err(e) => {
                eprintln!("warning: for_combinations: {e}");
                Vec::new()
            }
        },
        _ => {
            eprintln!("warning: for_combinations value must be a map, list, or string");
            Vec::new()
        }
    }
}

// -----------------------------------------------------------------
// Phases
// -----------------------------------------------------------------

/// Parse the `phases:` section of a workload YAML.
///
/// Each phase is a named map with optional `cycles`, `concurrency`,
/// `rate`, `adapter`, `errors`, `tags`, and `ops` fields.
/// Returns the phase map and a Vec preserving YAML definition order.
fn parse_phases(
    val: Option<&JVal>,
    doc_params: &HashMap<String, JVal>,
    doc_tags: &HashMap<String, String>,
) -> Result<(HashMap<String, WorkloadPhase>, Vec<String>), String> {
    let mut phases = HashMap::new();
    let mut phase_order = Vec::new();
    let Some(val) = val else {
        return Ok((phases, phase_order));
    };
    let Some(obj) = val.as_object() else {
        return Ok((phases, phase_order));
    };

    for (phase_name, phase_val) in obj {
        let Some(phase_obj) = phase_val.as_object() else {
            continue;
        };

        let cycles = phase_obj.get("cycles").map(|v| match v {
            JVal::Number(n) => n.to_string(),
            JVal::String(s) => s.clone(),
            other => other.to_string(),
        });

        let concurrency = phase_obj.get("concurrency").map(|v| match v {
            JVal::Number(n) => n.to_string(),
            JVal::String(s) => s.clone(),
            other => other.to_string(),
        });

        // A number or a `{param}` / iter-var reference (resolved at
        // the phase gather, the `timeout:` discipline). Anything
        // else is a load error — a malformed rate must never
        // silently become "unrated".
        let rate = match phase_obj.get("rate") {
            None => None,
            Some(JVal::Number(n)) => Some(n.to_string()),
            Some(JVal::String(s)) => Some(s.clone()),
            Some(other) => {
                return Err(format!(
                    "phase '{phase_name}': `rate` must be a number (ops/sec) \
                 or a {{param}} reference; got: {other}"
                ));
            }
        };

        // SRD-82 Part 6 — daemon phase (runs concurrently with foreground
        // siblings, stopped when they complete). Accept bool / 0|1 / on|off.
        let daemon = phase_obj
            .get("daemon")
            .map(|v| match v {
                JVal::Bool(b) => *b,
                JVal::Number(n) => n.as_u64().map(|u| u != 0).unwrap_or(false),
                JVal::String(s) => matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "on" | "yes" | "1"
                ),
                _ => false,
            })
            .unwrap_or(false);

        let adapter = phase_obj
            .get("adapter")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let errors = phase_obj
            .get("errors")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let error_rate_max = phase_obj.get("error_rate_max").and_then(|v| v.as_f64());

        // SRD-83 governance `timeout:` (GAP-12). A duration string, a
        // bare number (fractional seconds), or a `{param}` reference.
        // Param-bearing values resolve at the phase gather; literal
        // values must at least LOOK like a time value (non-empty, and
        // either numeric-leading or brace-opening) so a structural typo
        // fails at load, not at first dispatch. Full duration parsing
        // stays runtime-side (single parser: `timeval::parse_time_ms`).
        let timeout = match phase_obj.get("timeout") {
            None => None,
            Some(v) => {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    n if n.is_u64() || n.is_f64() => n.to_string(),
                    other => {
                        return Err(format!(
                            "phase '{phase_name}': `timeout` must be a duration \
                         string (e.g. \"2.5h\"), a number of seconds, or a \
                         {{param}} reference; got: {other}"
                        ));
                    }
                };
                let t = s.trim();
                if t.is_empty()
                    || !(t.starts_with('{') || t.starts_with(|c: char| c.is_ascii_digit()))
                {
                    return Err(format!(
                        "phase '{phase_name}': `timeout: \"{s}\"` is not a \
                         duration ('{{param}}', \"2.5h\", \"150ms\", or \
                         seconds)"
                    ));
                }
                Some(s)
            }
        };

        // Per-phase total-attempts budget — the phase-level surface of the
        // `tries` sigil (SRD-82 Part 3b). Inherits down to the phase's ops;
        // absent everywhere → no tries wrapper (single attempt). Two forms:
        // the sugared number (`tries: 20`) and the map form carrying retry
        // backoff (`tries: {count: 20, backoff: {ratio, min, max}}`).
        let (tries, tries_backoff) = match phase_obj.get("tries") {
            None => (None, None),
            Some(v) if v.is_u64() || v.is_i64() => (v.as_u64().map(|n| n as u32), None),
            Some(serde_json::Value::Object(m)) => {
                let count = m.get("count").and_then(|c| c.as_u64()).map(|n| n as u32);
                let backoff = m.get("backoff").and_then(|b| b.as_object()).map(|bo| {
                    crate::model::BackoffSpec {
                        ratio: bo.get("ratio").and_then(|r| r.as_f64()),
                        min: bo.get("min").and_then(json_scalar_to_dur_string),
                        max: bo.get("max").and_then(json_scalar_to_dur_string),
                    }
                });
                (count, backoff)
            }
            Some(other) => {
                return Err(format!(
                    "phase '{phase_name}': `tries` must be a number or a map \
                 {{count, backoff: {{ratio, min, max}}}}, got {other}"
                ));
            }
        };

        // SRD-83 — `stop_when:` is a list of {when, trigger?, effect?}.
        // StopConditionSpec derives Deserialize, so deserialize the
        // sub-tree directly; surface a malformed block as a parse error.
        // SRD-83 §throttle — adaptive backpressure governor: bool
        // sugar or the full spec map (serde, deny_unknown_fields).
        let throttle: Option<crate::model::ThrottleField> = match phase_obj.get("throttle") {
            Some(v) => Some(
                serde_json::from_value(v.clone())
                    .map_err(|e| format!("phase '{phase_name}' invalid `throttle` block: {e}"))?,
            ),
            None => None,
        };
        let stop_when: Vec<StopConditionSpec> = match phase_obj.get("stop_when") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| format!("invalid `stop_when` block: {e}"))?,
            None => Vec::new(),
        };
        // SRD-83 Part 5 — reject unknown effect verbs at load (a typo'd
        // verb would otherwise silently become the shell default).
        for sc in &stop_when {
            sc.validate()
                .map_err(|e| format!("phase `stop_when`: {e}"))?;
        }

        let tags = phase_obj
            .get("tags")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // SRD-13f Push D: phase-level AND workload-level
        // `bindings:` are captured on their own scope's AST
        // only — they do NOT fold into per-op bindings.
        // Workload bindings live on `Workload.bindings` and
        // compile to the workload-root kernel; phase bindings
        // live on `WorkloadPhase.bindings` and compile to the
        // phase kernel. Both reach ops through the Polydat Kernel
        // chain (cell-on-outputs cascade, SRD-13f Push B.2),
        // not parser-time concat.
        let phase_bindings_only = extract_bindings(phase_obj.get("bindings"));

        // Parse inline ops if present
        let mut inline_ops = Vec::new();
        for key in ["ops", "op", "operations", "statements", "statement"] {
            if let Some(ops_val) = phase_obj.get(key) {
                let phase_tags = {
                    let mut t = doc_tags.clone();
                    t.insert("phase".to_string(), phase_name.clone());
                    t
                };
                // Phase inline ops carry zero "outer YAML
                // bindings sugar" — they're directly under the
                // phase, no block wrapper. Workload + phase
                // bindings reach them via the Polydat Kernel chain.
                parse_ops_field(
                    ops_val,
                    phase_name,
                    &BindingsDef::default(),
                    doc_params,
                    &phase_tags,
                    &mut inline_ops,
                )?;
                break;
            }
        }

        // Auto-tag inline ops
        for op in &mut inline_ops {
            if !op.tags.contains_key("name") {
                op.tags.insert("name".to_string(), op.name.clone());
            }
            if !op.tags.contains_key("op") {
                op.tags.insert("op".to_string(), op.name.clone());
            }
        }

        let for_each = phase_obj
            .get("for_each")
            .or_else(|| phase_obj.get("for"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // SRD-101 — phase-level `continue_if` gate (bounds a `for_each` sweep).
        let continue_if = parse_continue_if(phase_obj.get("continue_if"));

        let loop_scope = phase_obj
            .get("loop_scope")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let iter_scope = phase_obj
            .get("iter_scope")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Phase-level `summary:` is gone (SRD-46 made `report:`
        // the canonical surface). Reject explicitly so silent
        // drops don't mislead operators migrating workloads.
        if phase_obj.contains_key("summary") {
            return Err(format!(
                "phase '{phase_name}': `summary:` is removed at phase level; \
                 use a `report:` block with `table <name> ...` items instead \
                 (SRD-46)"
            ));
        }
        // Per-phase `checkpoint:` declaration. Three forms
        // (short string, bool/none, full mapping) handled by
        // [`Checkpoint`]'s custom deserialize. Absent → None →
        // phase always re-runs on resume (per SRD-44 §"No
        // workload-level default").
        let checkpoint = phase_obj
            .get("checkpoint")
            .map(|v| serde_json::from_value::<crate::model::Checkpoint>(v.clone()))
            .transpose()
            .map_err(|e| format!("phase '{phase_name}' checkpoint: {e}"))?;

        // Phase-level `poll:` block (SRD-75). When present,
        // the phase's cycle execution runs in a wall-clock
        // loop until a Polydat predicate over captures returns
        // `true` or `timeout_ms` elapses. Distinct from the
        // OP-level `poll:` field (which lives on a single op
        // and wraps a `PollingDispenser` — SRD-32). The two
        // forms coexist; phase-poll is the synchronizer
        // pattern (SRD-75 driver use case), per-op poll is
        // the await-emptiness pattern.
        let phase_poll = match phase_obj.get("poll") {
            None => None,
            Some(v) => {
                let map = v.as_object().ok_or_else(|| {
                    format!(
                        "phase '{phase_name}': phase-level `poll:` must be a \
                     mapping with at least `until: <expr>`. Got a non-object \
                     value; if you intended an OP-level `poll:` flag, attach \
                     it to a specific op under `ops:` instead. SRD-75."
                    )
                })?;
                let until = map
                    .get("until")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!(
                            "phase '{phase_name}': phase-level `poll:` requires \
                         `until: <polydat-boolean-expression>`. SRD-75."
                        )
                    })?
                    .to_string();
                // Numbers or `{param}` / iter-var references
                // (resolved at the phase gather); anything else is
                // a load error — a malformed bound must never
                // silently fall back to the default.
                let numeric_or_ref = |field: &str| -> Result<Option<String>, String> {
                    match map.get(field) {
                        None => Ok(None),
                        Some(JVal::Number(n)) => Ok(Some(n.to_string())),
                        Some(JVal::String(s)) => Ok(Some(s.clone())),
                        Some(other) => Err(format!(
                            "phase '{phase_name}' poll: `{field}` must be a \
                             number or a {{param}} reference; got: {other}. \
                             SRD-75."
                        )),
                    }
                };
                let interval_ms = numeric_or_ref("interval_ms")?;
                let timeout_ms = numeric_or_ref("timeout_ms")?;
                let max_error_retries = numeric_or_ref("max_error_retries")?;
                let metric_name = map
                    .get("metric_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // SRD-75 §"Open questions" → §"on_timeout" — what
                // to do when the deadline fires. Closed vocabulary:
                // `error` (default; phase fails, scenario continues
                // by default error-routing policy) or `abort`
                // (request session stop; the whole run terminates).
                let on_timeout = match map.get("on_timeout") {
                    None => None,
                    Some(v) => {
                        let s = v.as_str().ok_or_else(|| {
                            format!(
                                "phase '{phase_name}' poll: `on_timeout` must be \
                             a string (`error` or `abort`). SRD-75."
                            )
                        })?;
                        let normalized = s.trim().to_ascii_lowercase();
                        if !matches!(normalized.as_str(), "error" | "abort") {
                            return Err(format!(
                                "phase '{phase_name}' poll: `on_timeout` must \
                                 be `error` or `abort`, got '{s}'. SRD-75."
                            ));
                        }
                        Some(normalized)
                    }
                };
                // SRD-75 (C5) — `require:` strict-gate selectors: a
                // single selector string or a list of them; each must
                // be a non-empty string.
                let require: Vec<String> = match map.get("require") {
                    None => Vec::new(),
                    Some(serde_json::Value::String(s)) => vec![s.clone()],
                    Some(serde_json::Value::Array(items)) => items
                        .iter()
                        .map(|v| {
                            v.as_str().map(str::to_string).ok_or_else(|| {
                                format!(
                                    "phase '{phase_name}' poll: `require` entries must \
                             be metric-selector strings. SRD-75."
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(_) => {
                        return Err(format!(
                            "phase '{phase_name}' poll: `require` must be a \
                         selector string or a list of them. SRD-75."
                        ));
                    }
                };
                if let Some(bad) = require.iter().find(|s| s.trim().is_empty()) {
                    let _ = bad;
                    return Err(format!(
                        "phase '{phase_name}' poll: `require` entries must be \
                         non-empty metric selectors. SRD-75."
                    ));
                }
                // Reject keys outside the documented surface — a
                // typo like `tinerval_ms:` should fail loudly, not
                // silently default. SRD-30 unknown-field hygiene.
                let allowed = crate::vocab::phase_poll_fields();
                for k in map.keys() {
                    if !allowed.contains(&k.as_str()) {
                        return Err(format!(
                            "phase '{phase_name}' poll: unknown key '{k}'. \
                             Allowed: [{}]. SRD-75.",
                            allowed.join(", "),
                        ));
                    }
                }
                Some(crate::model::PhasePollSpec {
                    until,
                    interval_ms,
                    timeout_ms,
                    max_error_retries,
                    metric_name,
                    on_timeout,
                    require,
                })
            }
        };
        // SRD-75 §"Concurrency": phase-poll is sequential
        // within one phase activation — the predicate's
        // evaluation depends on a serial sequence of capture
        // writes. `concurrency > 1` against this shape doesn't
        // have a meaningful semantic; reject at parse time
        // rather than producing surprising runtime behavior.
        if phase_poll.is_some() {
            if let Some(ref c) = concurrency {
                let trimmed = c.trim();
                if trimmed != "1" && !trimmed.is_empty() {
                    return Err(format!(
                        "phase '{phase_name}': `poll:` (SRD-75) is \
                         incompatible with `concurrency: {c}` — phase-poll \
                         is sequential within one activation (the predicate \
                         depends on a serial sequence of capture writes). \
                         Drop `concurrency` or set it to 1."
                    ));
                }
            }
            if inline_ops.is_empty() {
                return Err(format!(
                    "phase '{phase_name}': `poll:` requires at least one op \
                     under `ops:` — captures are written by op execution; \
                     a phase with `poll:` and no ops has nothing to do. \
                     SRD-75."
                ));
            }
        }

        // `status_metrics:` — names of relevancy aggregates to
        // surface on the inline progress line and the per-phase
        // ✓ DONE summary. Accepts a YAML list (`[name, name]`),
        // a single string, or a comma-separated string. Empty /
        // absent → no metrics tail (nothing presumed present).
        let status_metrics: Vec<String> = match phase_obj.get("status_metrics") {
            None => Vec::new(),
            Some(JVal::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect(),
            Some(JVal::String(s)) => s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            Some(other) => {
                return Err(format!(
                    "phase '{phase_name}' status_metrics: must be a list of metric \
                 names, a comma-separated string, or omitted; got {other:?}"
                ));
            }
        };

        // Phase-level `metrics:` — same schema as op `metrics:`,
        // but evaluated once at phase completion. Raw `value:`
        // expressions are preserved (no auto-inject into bindings):
        // the phase synthesiser emits `volatile __metric_<name> :=
        // <value>` directly, so a nondeterministic value such as
        // `phase_elapsed(phase_start)` is volatility-acknowledged
        // for strict mode in one place.
        let metrics = parse_phase_metrics_field(phase_obj.get("metrics"), phase_name)
            .map_err(|e| format!("phase '{phase_name}' metrics: {e}"))?;

        // Phase-level `dimensions:` — label names this phase introduces,
        // whose VALUES arrive from data through a metric's `cell:`. The
        // name is the structural declaration; declaring it here is what
        // lets a `cell:` reference be checked against the program before
        // any cycle runs, instead of surfacing as an attach-time panic on
        // the component tree.
        let dimensions = parse_dimensions_field(phase_obj.get("dimensions"), phase_name)?;

        // SRD-86 — phase `optimize:` block (workload-local config). A bare
        // string is sugar for `{ objective: <string> }` (see
        // `OptimizeBlock::from_yaml_value`).
        let optimize = match phase_obj.get("optimize") {
            Some(v) => Some(
                crate::model::OptimizeBlock::from_yaml_value(v)
                    .map_err(|e| format!("phase '{phase_name}' invalid `optimize` block: {e}"))?,
            ),
            None => None,
        };
        // A `cell:` must name a DECLARED dimension. This is the payoff for
        // reifying dimensions: the reference is checked against the program
        // at load, so a typo'd or undeclared dimension is a workload error
        // here rather than a component-tree surprise at attach time (or, in
        // the label-ownership case, a runtime panic).
        validate_cell_dimensions(phase_name, &dimensions, &inline_ops, &metrics)?;

        // SRD-109 — key-metric designations. Aggregate qualification is
        // MANDATORY: an unqualified family is a parse error carrying the
        // vocabulary, because there are no implied aggregates anywhere in
        // the reporting pipeline.
        let key_metrics: Vec<crate::model::KeyMetric> = match phase_obj.get("key_metrics") {
            None => Vec::new(),
            Some(v) => {
                let map = v.as_object().ok_or_else(|| {
                    format!(
                        "phase '{phase_name}': key_metrics must be a mapping of \
                     column: \"agg(family)\""
                    )
                })?;
                let mut out = Vec::new();
                for (col, spec) in map {
                    let spec = spec.as_str().ok_or_else(|| {
                        format!(
                            "phase '{phase_name}' key_metrics.{col}: expected a \
                         string \"agg(family)\""
                        )
                    })?;
                    let (agg_name, family) = spec
                        .trim()
                        .strip_suffix(')')
                        .and_then(|s| s.split_once('('))
                        .ok_or_else(|| {
                            format!(
                                "phase '{phase_name}' key_metrics.{col}: '{spec}' — \
                             aggregate qualification required; write agg(family). \
                             Aggregates: {}",
                                crate::model::KeyAgg::VOCAB
                            )
                        })?;
                    let agg = crate::model::KeyAgg::parse(agg_name.trim()).ok_or_else(|| {
                        format!(
                            "phase '{phase_name}' key_metrics.{col}: unknown \
                             aggregate '{}'. Aggregates: {}",
                            agg_name.trim(),
                            crate::model::KeyAgg::VOCAB
                        )
                    })?;
                    let family = family.trim().to_string();
                    match agg {
                        crate::model::KeyAgg::Span if !family.is_empty() => {
                            return Err(format!(
                                "phase '{phase_name}' key_metrics.{col}: span() is \
                                 family-less — it measures the activation wall clock"
                            ));
                        }
                        crate::model::KeyAgg::Span => {}
                        _ if family.is_empty() => {
                            return Err(format!(
                                "phase '{phase_name}' key_metrics.{col}: {}() needs \
                                 a family name",
                                agg_name.trim()
                            ));
                        }
                        _ => {}
                    }
                    out.push(crate::model::KeyMetric {
                        column: col.clone(),
                        agg,
                        family,
                    });
                }
                out
            }
        };

        phases.insert(
            phase_name.clone(),
            WorkloadPhase {
                dimensions,
                cycles,
                concurrency,
                rate,
                daemon,
                adapter,
                errors,
                tries,
                tries_backoff,
                interval: phase_obj
                    .get("interval")
                    .and_then(|v| v.as_str().map(str::to_string)),
                repeat: phase_obj.get("repeat").and_then(|v| v.as_u64()),
                error_rate_max,
                timeout,
                stop_when,
                throttle,
                tags,
                ops: inline_ops,
                for_each,
                continue_if,
                loop_scope,
                iter_scope,
                checkpoint,
                status_metrics,
                bindings: phase_bindings_only,
                metrics,
                poll: phase_poll,
                optimize,
                key_metrics,
            },
        );
        phase_order.push(phase_name.clone());
    }

    Ok((phases, phase_order))
}

// -----------------------------------------------------------------
// Blocks
// -----------------------------------------------------------------

fn parse_blocks(
    blocks_val: &JVal,
    doc_params: &HashMap<String, JVal>,
    doc_tags: &HashMap<String, String>,
    all_ops: &mut Vec<ParsedOp>,
) -> Result<(), String> {
    match blocks_val {
        JVal::Object(map) => {
            for (block_name, block_val) in map {
                parse_single_block(block_name, block_val, doc_params, doc_tags, all_ops)?;
            }
        }
        JVal::Array(arr) => {
            for (i, block_val) in arr.iter().enumerate() {
                let name = block_val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("block{}", i + 1));
                parse_single_block(&name, block_val, doc_params, doc_tags, all_ops)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_single_block(
    block_name: &str,
    block_val: &JVal,
    doc_params: &HashMap<String, JVal>,
    doc_tags: &HashMap<String, String>,
    all_ops: &mut Vec<ParsedOp>,
) -> Result<(), String> {
    let obj = match block_val.as_object() {
        Some(o) => o,
        None => return Ok(()),
    };

    // SRD-13f Push D: workload-level `bindings:` no longer
    // merge into block-level. Blocks are YAML organizational
    // sugar (not a Polydat scope), so a block's own `bindings:` is
    // *syntactic sugar* expanded into each enclosed op's
    // `op.bindings` at parse time — it does not cross any
    // kernel boundary, and the only "merge" left is this
    // block-sugar → op inlining inside `normalize_op_object`.
    let block_bindings = extract_bindings(obj.get("bindings"));
    let block_params = merge_value_maps(doc_params, &extract_value_map(obj.get("params")));
    let mut block_tags = merge_string_maps(doc_tags, &extract_string_map(obj.get("tags")));
    block_tags.insert("block".to_string(), block_name.to_string());

    // Find ops field
    for key in ["ops", "op", "operations", "statements", "statement"] {
        if let Some(ops_val) = obj.get(key) {
            parse_ops_field(
                ops_val,
                block_name,
                &block_bindings,
                &block_params,
                &block_tags,
                all_ops,
            )?;
            return Ok(());
        }
    }

    // If no ops field, check if the block value itself is a string (single op)
    if let Some(s) = block_val.as_str() {
        let mut op = ParsedOp::simple("stmt1", s);
        op.bindings = block_bindings;
        op.params = block_params;
        op.tags = block_tags;
        all_ops.push(op);
    }

    Ok(())
}

// -----------------------------------------------------------------
// Ops
// -----------------------------------------------------------------

fn parse_ops_field(
    ops_val: &JVal,
    block_name: &str,
    bindings: &BindingsDef,
    params: &HashMap<String, JVal>,
    tags: &HashMap<String, String>,
    all_ops: &mut Vec<ParsedOp>,
) -> Result<(), String> {
    // SRD-32a: activity/phase-scope param keys (cycles / concurrency /
    // rate / errors / error_rate_max) are consumed at phase/activity scope,
    // never as op fields. Strip them from the inherited params before they
    // reach any op, so an inherited `rate` doesn't collide with the `rate`
    // wrapper's field-ownership guard. A genuine op-level `rate:` reaches the
    // op via the typed `ParsedOp.rate` field, which is untouched.
    let op_scope_params = exclude_activity_keys(params);
    let params = &op_scope_params;
    let mut op_counter = 0;

    match ops_val {
        // Single string: op: "SELECT ..."
        JVal::String(s) => {
            op_counter += 1;
            let name = format!("stmt{op_counter}");
            let mut op = ParsedOp::simple(&name, s);
            op.bindings = bindings.clone();
            op.params = params.clone();
            op.tags = tags.clone();
            op.tags.insert("block".to_string(), block_name.to_string());
            all_ops.push(op);
        }

        // List of ops
        JVal::Array(arr) => {
            for item in arr {
                op_counter += 1;
                let auto_name = format!("stmt{op_counter}");
                let op = normalize_op_item(item, &auto_name, block_name, bindings, params, tags)?;
                all_ops.push(op);
            }
        }

        // Map of named ops
        JVal::Object(map) => {
            for (key, val) in map {
                let op = normalize_op_entry(key, val, block_name, bindings, params, tags)?;
                all_ops.push(op);
            }
        }

        _ => {}
    }

    Ok(())
}

/// Normalize a single op from a list item.
fn normalize_op_item(
    item: &JVal,
    auto_name: &str,
    block_name: &str,
    bindings: &BindingsDef,
    params: &HashMap<String, JVal>,
    tags: &HashMap<String, String>,
) -> Result<ParsedOp, String> {
    match item {
        JVal::String(s) => {
            let mut op = ParsedOp::simple(auto_name, s);
            op.bindings = bindings.clone();
            op.params = params.clone();
            op.tags = tags.clone();
            op.tags.insert("block".to_string(), block_name.to_string());
            Ok(op)
        }
        JVal::Object(map) => {
            // Check if first entry is name:stmt pattern
            if let Some((first_key, first_val)) = map.iter().next()
                && map.len() == 1
                && first_val.is_string()
            {
                let mut op = ParsedOp::simple(first_key, first_val.as_str().unwrap());
                op.bindings = bindings.clone();
                op.params = params.clone();
                op.tags = tags.clone();
                op.tags.insert("block".to_string(), block_name.to_string());
                return Ok(op);
            }
            // Full op object
            normalize_op_object(map, auto_name, block_name, bindings, params, tags)
        }
        _ => Ok(ParsedOp::simple(auto_name, "")),
    }
}

/// Normalize a named op from a map entry.
fn normalize_op_entry(
    key: &str,
    val: &JVal,
    block_name: &str,
    bindings: &BindingsDef,
    params: &HashMap<String, JVal>,
    tags: &HashMap<String, String>,
) -> Result<ParsedOp, String> {
    match val {
        JVal::String(s) => {
            let mut op = ParsedOp::simple(key, s);
            op.bindings = bindings.clone();
            op.params = params.clone();
            op.tags = tags.clone();
            op.tags.insert("block".to_string(), block_name.to_string());
            Ok(op)
        }
        JVal::Object(map) => normalize_op_object(map, key, block_name, bindings, params, tags),
        JVal::Array(arr) => {
            // Array at op level → moved to op.stmt
            let mut op_fields = HashMap::new();
            op_fields.insert("stmt".to_string(), JVal::Array(arr.clone()));
            let mut op = ParsedOp {
                traverse: None,
                name: key.to_string(),
                description: None,
                op: op_fields,
                bindings: bindings.clone(),
                params: params.clone(),
                tags: tags.clone(),
                condition: None,
                delay: None,
                metrics: HashMap::new(),
                result: None,
                wrappers: None,
                captures: Vec::new(),
                abstract_interface: None,
                interface_bound: false,
                daemon: crate::model::DaemonSpec::Disabled,
                daemon_cancel_grace_ms: None,
                while_cond: None,
                rate: None,
            };
            op.tags.insert("block".to_string(), block_name.to_string());
            Ok(op)
        }
        _ => Ok(ParsedOp::simple(key, "")),
    }
}

/// Human-readable name for a JSON value kind, for parse-time
/// error messages. ("string", "number", "array", etc.)
fn eval_value_kind(v: &JVal) -> &'static str {
    match v {
        JVal::Null => "null",
        JVal::Bool(_) => "bool",
        JVal::Number(_) => "number",
        JVal::String(_) => "string",
        JVal::Array(_) => "array",
        JVal::Object(_) => "mapping",
    }
}

/// Sub-keys allowed inside an op-template's `evaluations:`
/// block. The block is a reserved closed-vocab wrapper for
/// post-execution validation / scoring config — distinct from
/// per-adapter op fields. Anything else inside it is rejected at
/// parse time so silent-ignore traps (a misspelled `relevency:`,
/// a misplaced wrapper) cannot hide a misconfigured op. New
/// evaluation kinds are added to the construction registry.
fn evaluations_vocab() -> Vec<&'static str> {
    crate::vocab::evaluation_kinds()
}

/// Normalize a full op object (map of fields).
fn normalize_op_object(
    map: &serde_json::Map<String, JVal>,
    default_name: &str,
    block_name: &str,
    parent_bindings: &BindingsDef,
    parent_params: &HashMap<String, JVal>,
    parent_tags: &HashMap<String, String>,
) -> Result<ParsedOp, String> {
    let name = map
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(default_name)
        .to_string();

    let description = map
        .get("description")
        .or_else(|| map.get("desc"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // SRD-13f Push D: workload-level and phase-level
    // `bindings:` no longer touch ops at parse time — they
    // compile to their own kernels and reach ops via the GK
    // kernel chain. `parent_bindings` here carries ONLY
    // block-level YAML sugar (blocks are not a Polydat scope; their
    // `bindings:` field is a copy-paste reducer over each
    // enclosed op's bindings). The expansion below is the only
    // remaining parser-time inlining.
    let mut op_bindings =
        inline_block_sugar_into_op(parent_bindings, &extract_bindings(map.get("bindings")));
    let op_params = merge_value_maps(parent_params, &extract_value_map(map.get("params")));
    let mut op_tags = merge_string_maps(parent_tags, &extract_string_map(map.get("tags")));
    op_tags.insert("block".to_string(), block_name.to_string());

    // Determine op payload
    //
    // `reserved` lists keys handled by the workload model itself
    // (name, bindings, etc.) — they never reach the adapter.
    // `evaluations` is in this list because it's a closed-vocab
    // wrapper for validation/scoring config (relevancy, verify)
    // — its sub-keys are extracted and hoisted into `op_params`
    // below so downstream consumers
    // (`crate::validation::parse_relevancy` etc.) find them at
    // the same address whether the workload writes the
    // canonical wrapped form or the legacy top-level shorthand.
    // `metrics` and `result` are CORE op-template fields (SRD-40b
    // and SRD-66 respectively) extracted into ParsedOp.metrics /
    // ParsedOp.result; they must be kept out of `op_fields` so
    // adapters with a closed-vocabulary `known_op_fields()` (HTTP,
    // testkit) don't reject them as unknown. The CQL adapter
    // returns `None` from `known_op_fields()` (open vocabulary)
    // which masked this for the existing workloads.
    let reserved = crate::vocab::op_model_fields();
    let op_field_names = crate::vocab::op_stmt_fields();
    // Activity-level params excised from op fields before the
    // adapter sees them. `relevancy` / `verify` stay listed here
    // for the legacy top-level shorthand
    // (`relevancy: { ... }` directly under the op); the canonical
    // form puts them inside `evaluations:` and is handled
    // separately below.
    let activity_params = [
        "ratio",
        "adapter",
        "driver",
        "space",
        "instrument",
        "start-timers",
        "stop-timers",
        "verify",
        "relevancy",
        "strict",
        "poll",
        "poll_interval_ms",
        "timeout_ms",
        "poll_metric_name",
        "emit",
        "batch",
        "max_batch_size",
        "batchtype",
        "memo",
        "gutter",
        // SRD-63 op-level status visibility. `readout: visible` opts the
        // op into its own timed status leaf; excised into params so the
        // `readout` wrapper's trigger (which reads params) sees it rather
        // than the field falling through to the adapter op payload.
        "readout",
        // SRD-82 op shell — a per-op error-routing override (`errors:
        // "<pattern>:<actions>"`), resolved into a child of the phase policy
        // and pinned to this op's dispenser, and the op-level `tries:`
        // total-attempts sigil for the conditional tries wrapper. Both are
        // excised from op fields so they never reach the adapter as
        // op-payload keys.
        "errors",
        "tries",
        // Tries-wrapper companion knobs (SRD-82 Part 3b): retry
        // pacing and retry-error exemplar sampling (`exec_events`).
        // Consumed at wrapper build from op params; excised here so
        // the documented op-level standalone form actually lands in
        // params instead of leaking to the adapter as payload keys.
        "retry_backoff",
        "retry_backoff_max",
        "retry_backoff_ratio",
        "retry_exemplar_rate",
        "retry_exemplar_max_hz",
        "retry_advisory",
    ];

    let mut op_fields = if let Some(explicit_op) = op_field_names.iter().find_map(|k| map.get(*k)) {
        let mut m: HashMap<String, JVal> = match explicit_op {
            JVal::String(s) => {
                let mut m = HashMap::new();
                m.insert("stmt".to_string(), JVal::String(s.clone()));
                m
            }
            JVal::Object(o) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            other => {
                let mut m = HashMap::new();
                m.insert("stmt".to_string(), other.clone());
                m
            }
        };
        // Preserve sibling op-level fields so adapter-specific
        // extras (e.g. testkit's `result-latency`, `result-capacity`)
        // aren't silently dropped when the user writes shorthand:
        //
        //     insert:
        //       stmt: "INSERT ..."
        //       result-latency: "5ms"
        //
        // Without this loop the whole object would collapse to just
        // `stmt` and the sibling fields would never reach the adapter.
        // Keys already present in the explicit op payload win, so an
        // `op:` sub-object still has final say over its own shape.
        for (k, v) in map.iter() {
            if reserved.contains(&k.as_str())
                || op_field_names.contains(&k.as_str())
                || activity_params.contains(&k.as_str())
            {
                continue;
            }
            m.entry(k.clone()).or_insert_with(|| v.clone());
        }
        m
    } else {
        // All non-reserved, non-activity-param fields become op fields
        map.iter()
            .filter(|(k, _)| {
                !reserved.contains(&k.as_str())
                    && !op_field_names.contains(&k.as_str())
                    && !activity_params.contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    // Excise activity-level params from op fields into params
    let mut op_params = op_params;
    for ap in &activity_params {
        if let Some(val) = map.get(*ap) {
            // Activity params excised from op fields into params map
            op_params.insert(ap.to_string(), val.clone());
        }
    }

    // Canonical `evaluations:` wrapper — closed-vocab
    // validation/scoring config. Sub-keys are extracted and
    // hoisted into `op_params` so downstream consumers (e.g.
    // `crate::validation::parse_relevancy`,
    // `crate::validation::parse_assertions`) find them at the
    // same address whether the workload uses this canonical
    // form or the legacy top-level shorthand. Anything inside
    // `evaluations:` that isn't in `EVALUATIONS_VOCAB` is
    // rejected up front — the whole point of the wrapper is to
    // catch misspellings (`relevency:`) and misplaced wrappers
    // that the silent-routing path would otherwise drop on the
    // floor.
    if let Some(eval_val) = map.get("evaluations") {
        let eval_obj = eval_val.as_object().ok_or_else(|| {
            format!(
                "op '{name}' (block '{block_name}'): `evaluations:` must be a \
             mapping, got {kind}. Expected shape: \
             `evaluations: {{ relevancy: {{...}}, verify: [...] }}`.",
                kind = eval_value_kind(eval_val),
            )
        })?;
        for (k, v) in eval_obj.iter() {
            if !evaluations_vocab().contains(&k.as_str()) {
                return Err(format!(
                    "op '{name}' (block '{block_name}'): unknown key \
                     '{k}' under `evaluations:`. Allowed keys: [{}]. \
                     Each entry under `evaluations:` is a distinct \
                     post-execution evaluation kind — typos and \
                     misplaced wrappers are rejected here so silent \
                     skipped recall / verify can't happen.",
                    evaluations_vocab().join(", "),
                ));
            }
            // Top-level shorthand wins on collision so users
            // who already have `relevancy: {...}` at the op
            // level don't see their config replaced if they
            // also added `evaluations: { relevancy: {...} }`.
            // Warn so the duplicate is visible.
            if op_params.contains_key(k.as_str()) {
                eprintln!(
                    "warning: op '{name}' has '{k}' both at top level \
                     and under `evaluations:` — top-level wins. Pick \
                     one form.",
                );
                continue;
            }
            op_params.insert(k.clone(), v.clone());
        }
    }

    // SRD-108 Part B (+ SRD-109 Part 3) — `abstract:` declares
    // this op as a typed slot: `{ needs: {name: type,…}, yields:
    // {name: type,…}, results: {name: type,…} }`. Unknown keys
    // are rejected; types are polydat DSL type names, verified
    // against the compiled op-template program at pre-map
    // synthesis.
    let abstract_interface: Option<crate::model::OpInterface> = match map.get("abstract") {
        None => None,
        Some(v) => {
            let obj = v.as_object().ok_or_else(|| format!(
                    "op '{name}': `abstract:` must be a mapping with                      `needs:` / `yields:` / `results:` maps of wire-name -> type"))?;
            let mut iface = crate::model::OpInterface::default();
            for (k, section) in obj {
                let target = match k.as_str() {
                    "needs" => &mut iface.needs,
                    "yields" => &mut iface.yields,
                    "results" => &mut iface.results,
                    other => {
                        return Err(format!(
                            "op '{name}': unknown key '{other}' under                              `abstract:` (allowed: needs, yields, results)"
                        ));
                    }
                };
                let entries = section.as_object().ok_or_else(|| format!(
                        "op '{name}': `abstract.{k}:` must be a mapping                          of wire-name -> type"))?;
                for (wire, typ) in entries {
                    let type_name = typ.as_str().ok_or_else(|| format!(
                            "op '{name}': `abstract.{k}.{wire}:` type                              must be a string, got {typ}"))?;
                    target.insert(wire.clone(), type_name.to_string());
                }
            }
            Some(iface)
        }
    };

    let condition = map
        .get("if")
        .and_then(|v| v.as_str())
        .map(normalize_condition_clause);

    let delay = match map.get("delay") {
        None => None,
        Some(v) => {
            Some(crate::model::parse_delay_spec_value(v).map_err(|e| format!("op '{name}': {e}"))?)
        }
    };

    // Daemon-op declaration. Parses bool / int / "on"/"off" /
    // "true"/"false" via `parse_daemon_spec_value`. When set
    // to MaxFibers(N), the cycle-pool dispatch spawns the op
    // on a daemon fiber instead of awaiting inline; the cap
    // is enforced at spawn time.
    let daemon = match map.get("daemon") {
        Some(v) => crate::model::parse_daemon_spec_value(v)
            .map_err(|e| format!("op '{name}' (block '{block_name}'): {e}",))?,
        None => crate::model::DaemonSpec::Disabled,
    };
    let daemon_cancel_grace_ms = map.get("daemon_cancel_grace_ms").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
    });
    let daemon_enabled = !daemon.is_disabled();
    if daemon_enabled {
        // `cycles:` and `ratio:` on a daemon op are workload-shape
        // errors — the daemon's dispatch cadence is governed by
        // the cycle-pool walks + per-op `rate:`, not by cycles or
        // ratios. Silently accepting them would hide a mis-shaped
        // workload; reject at parse so the operator sees the
        // contradiction immediately.
        if let Some(c) = op_params.get("cycles") {
            return Err(format!(
                "op '{name}' (block '{block_name}'): `daemon:` and \
                 `cycles: {c}` are mutually exclusive. A daemon op's \
                 dispatch cadence is governed by the cycle-pool's \
                 stanza walk + per-op `rate:`, not by `cycles:`."
            ));
        }
        if let Some(r) = op_params.get("ratio") {
            return Err(format!(
                "op '{name}' (block '{block_name}'): `daemon:` and \
                 `ratio: {r}` are mutually exclusive — a daemon op \
                 does not participate in cycle-pool ratio scheduling."
            ));
        }
    }
    if !daemon_enabled && daemon_cancel_grace_ms.is_some() {
        return Err(format!(
            "op '{name}' (block '{block_name}'): \
             `daemon_cancel_grace_ms` is only meaningful when \
             `daemon:` is enabled (true / N). Either enable `daemon:` \
             or drop the grace field."
        ));
    }

    // Loop / rate primitives (apply to both cycle-pool and
    // daemon ops, though the typical use case is daemon+while+rate).
    let while_cond = map
        .get("while")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let rate = map.get("rate").and_then(|v| {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
            .or_else(|| v.as_f64().map(|f| f.to_string()))
    });
    // Rate without while- or per-cycle dispatch is meaningless —
    // an op that fires once-per-cycle can't be rate-limited in
    // any observable way. Warn at parse rather than silently
    // accepting a no-op field.
    if rate.is_some() && while_cond.is_none() && daemon.is_disabled() {
        // Soft warn via stderr; this is a workload-shape smell
        // but not always an error (a workload-author may be
        // configuring forward-compatible op templates).
        eprintln!(
            "warning: op '{name}' (block '{block_name}'): `rate:` is set \
             but the op has neither `while:` nor `daemon:`. The rate \
             limit will only fire on each cycle-pool dispatch — likely \
             not the intended behavior. Add `while:` for a loop or \
             `daemon:` to enable fiber-kind dispatch.",
        );
    }

    let metrics = parse_metrics_field(map.get("metrics"), &name, &mut op_bindings)
        .map_err(|e| format!("op '{name}' metrics: {e}"))?;
    let traverse = parse_traverse_field(map.get("traverse"), &name)?;
    let result = parse_result_field(map.get("result"), &name)
        .map_err(|e| format!("op '{name}' result: {e}"))?;

    // Extract capture-point specs from every string-valued entry in
    // `op` and replace each value with the bracket-stripped form.
    // Adapters consume the cleaned text; `TraversingDispenser` and
    // other capture-aware wrappers read the harvested specs from
    // `ParsedOp.captures` directly — no re-parse at wrap-time.
    //
    // De-duplicates by `as_name` across multiple op-fields: the
    // same wire name appearing in two fields means the same
    // capture, not two separate writes.
    let mut captures: Vec<crate::bindpoints::CapturePoint> = Vec::new();
    // Declarative `capture:` map block — pulls JSON-Pointer-keyed
    // values out of structured response bodies (e.g. Jolokia
    // bulk-POST arrays). Each entry is a path string addressed
    // by [`serde_json::Value::pointer`]; a trailing `:count`
    // collapses the addressed sub-tree to a u64 count instead
    // of capturing it as-is. This complements the legacy
    // bracket form `[name]` embedded in op text — that form
    // still works for adapters whose statements have column
    // references; the declarative form is for adapters whose
    // responses are JSON and need positional / nested access.
    if let Some(cap_val) = map.get("capture") {
        let cap_obj = cap_val.as_object().ok_or_else(|| {
            format!(
                "op '{name}' (block '{block_name}'): `capture:` must be a \
             mapping of <wire-name> → <json-pointer-path>. Got {kind}.",
                kind = eval_value_kind(cap_val),
            )
        })?;
        for (wire_name, spec_val) in cap_obj.iter() {
            let raw = spec_val.as_str().ok_or_else(|| {
                format!(
                    "op '{name}' (block '{block_name}'): `capture.{wire_name}` \
                 must be a string (JSON-Pointer path, optionally with a \
                 `:count` suffix). Got {kind}.",
                    kind = eval_value_kind(spec_val),
                )
            })?;
            let (path, count, agg, row_filter) = match raw.strip_suffix(":count") {
                Some(p) => (p.to_string(), true, None, None),
                None => match parse_capture_agg_suffix(raw) {
                    Some((p, a, f)) => (p, false, Some(a), f),
                    None => (raw.to_string(), false, None, None),
                },
            };
            // Two accepted spellings, one meaning — but never a guess in
            // between:
            //
            //   `/value`, `/0/name`  RFC 6901 JSON-Pointer, used verbatim.
            //   `value`              a bare top-level member name, which is
            //                        exactly `/value` and normalized to it
            //                        here, so runtime has one form to resolve.
            //   ``                   the root document.
            //
            // A bare name may contain neither `/` nor `~`, and that is the
            // whole anti-ambiguity rule: `a/b` could be a pointer someone
            // forgot to lead with '/', or a member literally named "a/b"
            // (RFC 6901 spells that `/a~1b`), and `~0`/`~1` are pointer
            // escapes. Rather than pick a reading, name both and let the
            // author say which. `verify:`'s `field:` is a bare name by
            // definition, so an author moving between the two blocks lands
            // on the bare form naturally — it now works rather than erroring.
            let path = if path.is_empty() || path.starts_with('/') {
                path
            } else if !path.contains('/') && !path.contains('~') {
                format!("/{path}")
            } else {
                return Err(format!(
                    "op '{name}' (block '{block_name}'): \
                     `capture.{wire_name}` path '{path}' is ambiguous. Write \
                     a JSON-Pointer starting with '/' (e.g. '/{first}/...', \
                     escaping '~' as '~0' and '/' as '~1'), or a bare \
                     top-level member name containing neither '/' nor '~'. \
                     An empty path addresses the root document.",
                    first = path.split('/').next().unwrap_or(&path),
                ));
            };
            captures.push(crate::bindpoints::CapturePoint {
                row_filter,
                source_name: wire_name.clone(),
                as_name: wire_name.clone(),
                cast_type: None,
                slurp: false,
                path: Some(path),
                count,
                agg,
            });
        }
    }
    for value in op_fields.values_mut() {
        if let serde_json::Value::String(s) = value {
            let parsed = crate::bindpoints::parse_capture_points(s);
            if parsed.captures.is_empty() {
                continue;
            }
            for cap in parsed.captures {
                if !captures
                    .iter()
                    .any(|existing| existing.as_name == cap.as_name)
                {
                    captures.push(cap);
                }
            }
            *s = parsed.raw_template;
        }
    }

    Ok(ParsedOp {
        traverse,
        name,
        description,
        op: op_fields,
        bindings: op_bindings,
        params: op_params,
        tags: op_tags,
        condition,
        delay,
        metrics,
        result,
        wrappers: None,
        captures,
        abstract_interface,
        interface_bound: false,
        daemon,
        daemon_cancel_grace_ms,
        while_cond,
        rate,
    })
}

/// SRD-40b §1 + §2: parse the `metrics:` field on an op
/// template. Three YAML shapes accepted, dispatched on the
/// value's type:
///
/// - **Scalar** (bare string, §2.1): one metric with the
///   string as both family and `value:`.
/// - **Sequence** (list, §2.2): each entry is a bare-name
///   string OR a `name := <Polydat expression>` wire-expression.
///   Wire expressions are auto-injected into the op's
///   `bindings:` block; the metric is then a bare-name
///   reference to the new wire.
/// - **Mapping** (object, §2.3): canonical full-shape form
///   keyed by metric name. Each value is either a string
///   (treated as `value:`) or a full `MetricSpec` mapping.
/// Parses a `:min(field)` / `:max(field)` / `:sum(field)` suffix on a
/// declarative capture path, returning `(path_prefix, agg)`. The path
/// prefix may be empty (root = the whole rows array). Returns `None`
/// when the spec carries no recognized aggregation suffix.
/// Split an optional `where <field>='<value>'` predicate off the tail of an
/// aggregate's argument, returning `(field_expr, predicate)`.
///
/// `progress where kind='secondary index build'` →
/// `("progress", Some(("kind", "secondary index build")))`.
///
/// Single or double quotes; the value may contain spaces, which is why it is
/// quoted rather than whitespace-delimited.
fn split_capture_row_filter(arg: &str) -> (String, Option<(String, String)>) {
    let lower = arg.to_ascii_lowercase();
    let Some(idx) = lower.find(" where ") else {
        return (arg.trim().to_string(), None);
    };
    let field = arg[..idx].trim().to_string();
    let pred = arg[idx + " where ".len()..].trim();
    let Some((k, v)) = pred.split_once('=') else {
        return (arg.trim().to_string(), None);
    };
    let k = k.trim();
    let v = v.trim();
    let unquoted = v
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .or_else(|| v.strip_prefix('"').and_then(|r| r.strip_suffix('"')));
    match unquoted {
        Some(val) if !k.is_empty() => (field, Some((k.to_string(), val.to_string()))),
        // An unquoted or malformed predicate is left alone rather than
        // silently half-applied — the caller then treats the whole string as a
        // field name and the unknown-field error names it.
        _ => (arg.trim().to_string(), None),
    }
}

fn parse_capture_agg_suffix(
    raw: &str,
) -> Option<(
    String,
    crate::bindpoints::CaptureAgg,
    Option<(String, String)>,
)> {
    use crate::bindpoints::CaptureAgg;
    if !raw.ends_with(')') {
        return None;
    }
    for (tag, make) in [
        (":min(", CaptureAgg::Min as fn(String) -> CaptureAgg),
        (":max(", CaptureAgg::Max as fn(String) -> CaptureAgg),
        (":sum(", CaptureAgg::Sum as fn(String) -> CaptureAgg),
    ] {
        if let Some(idx) = raw.rfind(tag) {
            let arg = &raw[idx + tag.len()..raw.len() - 1];
            let (field, row_filter) = split_capture_row_filter(arg);
            if !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some((raw[..idx].to_string(), make(field), row_filter));
            }
        }
    }
    None
}

fn parse_metrics_field(
    val: Option<&JVal>,
    op_name: &str,
    op_bindings: &mut BindingsDef,
) -> Result<HashMap<String, MetricSpec>, String> {
    use crate::model::MetricSpec;
    let Some(v) = val else {
        return Ok(HashMap::new());
    };
    let mut out: HashMap<String, MetricSpec> = HashMap::new();
    match v {
        JVal::String(s) => {
            let name = s.trim().to_string();
            if name.is_empty() {
                return Err("scalar form requires a metric name".into());
            }
            out.insert(
                name.clone(),
                MetricSpec {
                    value: name,
                    family: None,
                    kind: None,
                    unit: None,
                    format: None,
                    cell: Default::default(),
                },
            );
        }
        JVal::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                let raw = item.as_str().ok_or_else(|| {
                    format!(
                        "metrics list entry {idx}: must be a string \
                     (bare name or `name := <polydat expr>`)"
                    )
                })?;
                let trimmed = raw.trim();
                if let Some((name, expr)) = trimmed.split_once(":=") {
                    // Wire-expression form: declare the binding +
                    // register the metric.
                    let name = name.trim();
                    let expr = expr.trim();
                    if name.is_empty() || expr.is_empty() {
                        return Err(format!(
                            "metrics list entry {idx} '{raw}': wire \
                             expression must be `name := <expression>`"
                        ));
                    }
                    inject_wire_into_bindings(op_bindings, name, expr, op_name)?;
                    if out.contains_key(name) {
                        return Err(format!("duplicate metric wire '{name}' in metrics list"));
                    }
                    out.insert(
                        name.to_string(),
                        MetricSpec {
                            value: name.to_string(),
                            family: None,
                            kind: None,
                            unit: None,
                            format: None,
                            cell: Default::default(),
                        },
                    );
                } else {
                    // Bare-name form.
                    if trimmed.is_empty() {
                        return Err(format!("metrics list entry {idx}: empty name"));
                    }
                    if out.contains_key(trimmed) {
                        return Err(format!("duplicate metric '{trimmed}' in metrics list"));
                    }
                    out.insert(
                        trimmed.to_string(),
                        MetricSpec {
                            value: trimmed.to_string(),
                            family: None,
                            kind: None,
                            unit: None,
                            format: None,
                            cell: Default::default(),
                        },
                    );
                }
            }
        }
        JVal::Object(map) => {
            for (key, val) in map {
                if out.contains_key(key) {
                    return Err(format!("duplicate metric key '{key}' in metrics map"));
                }
                let mut spec = parse_metric_spec_value(val, key)?;
                // SRD-13d Phase 9 mapping-form auto-inject: if
                // `value:` isn't a bare name, promote it to an
                // op-template binding `<key> := <value>` and
                // replace the spec's `value:` with the bare key.
                // Mirrors the list-form `name := expr` flow.
                let value_trimmed = spec.value.trim();
                let bare = !value_trimmed.is_empty()
                    && value_trimmed
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_');
                if !bare {
                    if !is_valid_ident(key) {
                        return Err(format!(
                            "metric '{key}' value '{value}' is a non-bare \
                             expression so the metric key must itself be a \
                             valid identifier (alphanumerics + underscore, \
                             not starting with a digit) so it can be used \
                             as a binding name. Rename the metric key, or \
                             move the expression into `bindings:` and set \
                             `value:` to the bare name.",
                            value = spec.value
                        ));
                    }
                    inject_wire_into_bindings(op_bindings, key, value_trimmed, op_name)?;
                    spec.value = key.clone();
                }
                out.insert(key.clone(), spec);
            }
        }
        _ => {
            return Err(format!(
                "metrics: expected scalar, sequence, or mapping; got {v:?}"
            ));
        }
    }
    Ok(out)
}

/// Parse a phase-level `metrics:` field. Same three YAML shapes as
/// the op-level [`parse_metrics_field`] (scalar / sequence / mapping)
/// and the same [`MetricSpec`] schema, with one deliberate
/// difference: phase metrics do **not** auto-inject non-bare value
/// expressions into a `bindings:` block. Op metrics inject so the
/// closure-binding-economy walker can allocate magic-extern slots
/// (`body`/`count`/`ok`) for the value expression; phase metrics have
/// no result body and are pulled directly by the executor from
/// `__metric_<name>`, so the phase synthesiser emits
/// `volatile __metric_<name> := <value>` straight from the raw
/// `value:` expression preserved here.
/// Parse a phase-level `dimensions:` block.
///
/// ```yaml
/// dimensions:
///   tier: { type: str }
///   tier: str            # shorthand
/// ```
///
/// Unknown keys are rejected rather than dropped: a silently-ignored
/// dimension declaration would make a metric's `cell:` reference fail its
/// existence check for a reason nothing points at.
/// Check every `cell:` reference in a phase against its declared dimensions.
///
/// Both tiers are covered: the phase's own `metrics:` and each inline op's.
/// The error names the declared set, because the common failure is a name
/// declared one tier away rather than a nonsense name.
fn validate_cell_dimensions(
    phase_name: &str,
    dimensions: &std::collections::BTreeMap<String, crate::model::DimensionSpec>,
    ops: &[ParsedOp],
    phase_metrics: &HashMap<String, MetricSpec>,
) -> Result<(), String> {
    let declared = || {
        if dimensions.is_empty() {
            "none are declared on this phase".to_string()
        } else {
            format!(
                "declared here: {}",
                dimensions.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    };
    let check = |site: &str, metric: &str, spec: &MetricSpec| -> Result<(), String> {
        for dim in spec.cell.keys() {
            if !dimensions.contains_key(dim) {
                return Err(format!(
                    "phase '{phase_name}' {site} metric '{metric}': cell \
                     dimension '{dim}' is not declared. A dimension is a label \
                     name owned by exactly one tier, so declare it on the phase \
                     ({dim}: str) before placing a metric in it — {}",
                    declared()
                ));
            }
        }
        Ok(())
    };
    for (name, spec) in phase_metrics {
        check("phase-level", name, spec)?;
    }
    for op in ops {
        for (name, spec) in &op.metrics {
            check(&format!("op '{}'", op.name), name, spec)?;
        }
    }
    Ok(())
}

fn parse_dimensions_field(
    val: Option<&JVal>,
    phase_name: &str,
) -> Result<std::collections::BTreeMap<String, crate::model::DimensionSpec>, String> {
    use crate::model::{DimensionSpec, DimensionType};
    let mut out = std::collections::BTreeMap::new();
    let Some(v) = val else { return Ok(out) };
    let JVal::Object(map) = v else {
        return Err(format!(
            "phase '{phase_name}' dimensions: expected a mapping of \
             <name>: <declaration>, got {v:?}"
        ));
    };
    for (name, decl) in map {
        if !is_valid_ident(name) {
            return Err(format!(
                "phase '{phase_name}' dimension '{name}': a dimension name \
                 becomes a metric label, so it must be a valid identifier \
                 (alphanumerics + underscore, not starting with a digit)"
            ));
        }
        let value_type = match decl {
            // Shorthand: `tier: str`
            JVal::String(s) => parse_dimension_type(s, phase_name, name)?,
            JVal::Object(obj) => {
                for k in obj.keys() {
                    if k != "type" {
                        return Err(format!(
                            "phase '{phase_name}' dimension '{name}': unknown \
                             field `{k}`. Recognised fields: type"
                        ));
                    }
                }
                match obj.get("type") {
                    None => DimensionType::default(),
                    Some(JVal::String(s)) => parse_dimension_type(s, phase_name, name)?,
                    Some(other) => {
                        return Err(format!(
                            "phase '{phase_name}' dimension '{name}' type: \
                         expected a string, got {other:?}"
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "phase '{phase_name}' dimension '{name}': expected a type \
                 name or a mapping, got {other:?}"
                ));
            }
        };
        out.insert(name.clone(), DimensionSpec { value_type });
    }
    Ok(out)
}

fn parse_dimension_type(
    s: &str,
    phase_name: &str,
    dim: &str,
) -> Result<crate::model::DimensionType, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "str" | "string" => Ok(crate::model::DimensionType::Str),
        other => Err(format!(
            "phase '{phase_name}' dimension '{dim}' type '{other}': label \
             values are strings; `str` is the only supported type"
        )),
    }
}

fn parse_phase_metrics_field(
    val: Option<&JVal>,
    phase_name: &str,
) -> Result<HashMap<String, MetricSpec>, String> {
    use crate::model::MetricSpec;
    let Some(v) = val else {
        return Ok(HashMap::new());
    };
    let mut out: HashMap<String, MetricSpec> = HashMap::new();
    match v {
        JVal::String(s) => {
            // Scalar: a bare wire name used as both family and value.
            let name = s.trim().to_string();
            if name.is_empty() {
                return Err("scalar form requires a metric name".into());
            }
            out.insert(
                name.clone(),
                MetricSpec {
                    value: name,
                    family: None,
                    kind: None,
                    unit: None,
                    format: None,
                    cell: Default::default(),
                },
            );
        }
        JVal::Array(items) => {
            // Sequence: bare wire names only (no `name := expr` form —
            // phase metrics don't inject bindings).
            for (idx, item) in items.iter().enumerate() {
                let raw = item
                    .as_str()
                    .ok_or_else(|| format!("metrics list entry {idx}: must be a bare wire name"))?;
                let name = raw.trim();
                if name.is_empty() {
                    return Err(format!("metrics list entry {idx}: empty name"));
                }
                if name.contains(":=") {
                    return Err(format!(
                        "metrics list entry {idx} '{raw}': the `name := expr` \
                         form is op-only; for a phase, declare the wire in the \
                         phase `bindings:` block and list its bare name here, \
                         or use the mapping form `{{ {name}: {{ value: <expr> }} }}`"
                    ));
                }
                if out.contains_key(name) {
                    return Err(format!("duplicate metric '{name}' in metrics list"));
                }
                out.insert(
                    name.to_string(),
                    MetricSpec {
                        value: name.to_string(),
                        family: None,
                        kind: None,
                        unit: None,
                        format: None,
                        cell: Default::default(),
                    },
                );
            }
        }
        JVal::Object(map) => {
            // Mapping: canonical full-shape form. Raw `value:` kept.
            for (key, val) in map {
                if out.contains_key(key) {
                    return Err(format!("duplicate metric key '{key}' in metrics map"));
                }
                out.insert(key.clone(), parse_metric_spec_value(val, key)?);
            }
        }
        _ => {
            return Err(format!(
                "phase '{phase_name}' metrics: expected scalar, sequence, or \
             mapping; got {v:?}"
            ));
        }
    }
    Ok(out)
}

/// Parse one entry under the mapping form of `metrics:`.
/// Accepts a bare string (treated as `value:`) or a full
/// `MetricSpec` object.
fn parse_metric_spec_value(v: &JVal, key: &str) -> Result<crate::model::MetricSpec, String> {
    use crate::model::MetricSpec;
    match v {
        JVal::String(s) => Ok(MetricSpec {
            value: s.clone(),
            family: None,
            kind: None,
            unit: None,
            format: None,
            cell: Default::default(),
        }),
        JVal::Object(map) => {
            // SRD-30 unknown-field hygiene: reject any key outside the
            // MetricSpec surface rather than silently dropping it (a
            // dropped `kind:` would let a counter/histogram silently
            // default to gauge). The instrument-type discriminator is
            // `kind`, not `type` — `type` is the word OpenMetrics /
            // Prometheus use, so it's the predictable mistake; give it
            // a targeted hint.
            const KNOWN: &[&str] = &["value", "family", "kind", "unit", "format", "cell"];
            for k in map.keys() {
                if KNOWN.contains(&k.as_str()) {
                    continue;
                }
                let hint = if k == "type" {
                    " — the instrument-type discriminator is `kind` \
                     (gauge | histogram | counter)"
                } else {
                    ""
                };
                return Err(format!(
                    "metric '{key}': unknown field `{k}`{hint}. Recognised \
                     fields: value, family, kind, unit, format, cell"
                ));
            }
            let value = map
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    format!(
                        "metric '{key}': required field `value:` missing or \
                     not a string"
                    )
                })?
                .to_string();
            let family = map.get("family").and_then(|v| v.as_str()).map(String::from);
            let unit = map.get("unit").and_then(|v| v.as_str()).map(String::from);
            let format = map.get("format").and_then(|v| v.as_str()).map(String::from);
            // Validate format syntax at parse time so the user
            // hears about a bad `#.##` pattern at workload
            // load, not first-cycle. SRD-40b §1.
            if let Some(f) = format.as_deref() {
                crate::metric_format::parse_format_spec(f)
                    .map_err(|e| format!("metric '{key}' format '{f}': {e}"))?;
            }
            let kind = match map.get("kind") {
                None => None,
                Some(JVal::String(s)) => Some(parse_metric_kind(s, key)?),
                Some(other) => {
                    return Err(format!(
                        "metric '{key}' kind: expected string, got {other:?}"
                    ));
                }
            };
            // `cell:` — dimensional placement. Each entry is
            // `<dimension>: <polydat expression>`; the expression is
            // reified by scope synthesis as a typed kernel binding, so a
            // coordinate is compiled and checkable rather than a string
            // assembled at runtime.
            let mut cell = std::collections::BTreeMap::new();
            match map.get("cell") {
                None => {}
                Some(JVal::Object(dims)) => {
                    if dims.is_empty() {
                        return Err(format!(
                            "metric '{key}' cell: empty. Omit `cell:` entirely, \
                             or name at least one dimension."
                        ));
                    }
                    for (dim, expr) in dims {
                        let expr = expr.as_str().ok_or_else(|| {
                            format!(
                                "metric '{key}' cell '{dim}': expected a polydat \
                             expression string, got {expr:?}"
                            )
                        })?;
                        if expr.trim().is_empty() {
                            return Err(format!("metric '{key}' cell '{dim}': empty expression"));
                        }
                        if !is_valid_ident(dim) {
                            return Err(format!(
                                "metric '{key}' cell '{dim}': a dimension name \
                                 becomes a metric label, so it must be a valid \
                                 identifier (alphanumerics + underscore, not \
                                 starting with a digit)"
                            ));
                        }
                        cell.insert(dim.clone(), expr.trim().to_string());
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "metric '{key}' cell: expected a mapping of \
                     <dimension>: <expression>, got {other:?}"
                    ));
                }
            }
            Ok(MetricSpec {
                value,
                family,
                kind,
                unit,
                format,
                cell,
            })
        }
        _ => Err(format!(
            "metric '{key}': expected string or mapping, got {v:?}"
        )),
    }
}

fn parse_metric_kind(s: &str, key: &str) -> Result<crate::model::MetricKind, String> {
    use crate::model::MetricKind;
    match s.to_ascii_lowercase().as_str() {
        "gauge" => Ok(MetricKind::Gauge),
        "histogram" => Ok(MetricKind::Histogram),
        "counter" => Ok(MetricKind::Counter),
        other => Err(format!(
            "metric '{key}' kind '{other}': expected one of \
             gauge / histogram / counter"
        )),
    }
}

/// Auto-inject `name := expr` into the op template's
/// `bindings:` block (per SRD-40b §2.2). Conflicts with an
/// existing declaration of the same name are a strict
/// workload parse error per §2.2.
fn inject_wire_into_bindings(
    bindings: &mut BindingsDef,
    name: &str,
    expr: &str,
    op_name: &str,
) -> Result<(), String> {
    // Look for an existing same-name declaration to refuse
    // shadowing. The check is textual: a line beginning with
    // `<name>` followed by whitespace + `:=`. Prefix matching
    // would surface false positives for `foo` vs `foobar`,
    // hence the boundary check.
    let line_to_inject = format!("{name} := {expr}\n");
    // BindingsDef has no `Empty` variant — `Map(empty)` is the
    // default. Detect emptiness via the existing helper, then
    // promote to PolydatSource for injection (we're adding a real
    // Polydat statement, not a name→expr pair the legacy Map form
    // can't carry alone).
    if bindings.is_empty() {
        *bindings = BindingsDef::PolydatSource(line_to_inject);
        return Ok(());
    }
    match bindings {
        BindingsDef::PolydatSource(src) => {
            if has_binding_named(src, name) {
                return Err(format!(
                    "metric wire '{name}' (op '{op_name}') collides \
                     with existing `bindings:` declaration of the \
                     same name"
                ));
            }
            if !src.ends_with('\n') {
                src.push('\n');
            }
            src.push_str(&line_to_inject);
        }
        BindingsDef::Map(map) => {
            if map.contains_key(name) {
                return Err(format!(
                    "metric wire '{name}' (op '{op_name}') collides \
                     with existing `bindings:` declaration of the \
                     same name"
                ));
            }
            map.insert(name.to_string(), expr.to_string());
        }
    }
    Ok(())
}

/// True when `s` is a valid Polydat identifier: non-empty, first
/// char is a letter or underscore, remaining chars are
/// alphanumerics or underscore. Used by the mapping-form
/// metric auto-inject to confirm the metric key can stand in
/// as a binding name.
fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// True when the Polydat source contains a binding line
/// `<name> := …` at the start of any (whitespace-trimmed)
/// line. Used by the wire-expression injection to detect
/// shadowing without parsing the Polydat grammar.
fn has_binding_named(src: &str, name: &str) -> bool {
    for raw in src.lines() {
        let line = raw.trim_start();
        if let Some(rest) = line.strip_prefix(name) {
            // Boundary: next char must be whitespace or `:=`.
            let rest = rest.trim_start();
            if rest.starts_with(":=") {
                return true;
            }
        }
    }
    false
}

/// SRD-66 §"Surface 1 §Schema": parse the vari-structured
/// `result:` field on an op template. Three shapes:
///
/// - **String** scalar — Polydat source block (multi-line or
///   single-line). Each `<name> := <expr>` declares one
///   wire.
/// - **List** sequence — each element is itself a
///   `ResultSpec` (recursively); fragments concatenate.
/// - **Mapping** — named-key short-forms; each value is a
///   string parsed as `count` / `ok` / path-expr / Polydat expr.
/// Parse an op's `traverse:` block.
///
/// Customises the always-installed traversal layer; it does not select it, so
/// absence means "defaults", not "no traversal". Unknown keys are rejected
/// rather than dropped — a silently ignored `on_missing:` would leave the
/// author believing absent captures are being policed when they are not.
fn parse_traverse_field(
    val: Option<&JVal>,
    op_name: &str,
) -> Result<Option<crate::model::TraverseSpec>, String> {
    use crate::model::{OnMissing, TraverseSpec};
    let Some(v) = val else { return Ok(None) };
    let JVal::Object(map) = v else {
        return Err(format!(
            "op '{op_name}' traverse: expected a mapping, got {v:?}"
        ));
    };
    const KNOWN: &[&str] = &["path", "on_missing"];
    for k in map.keys() {
        if !KNOWN.contains(&k.as_str()) {
            return Err(format!(
                "op '{op_name}' traverse: unknown field `{k}`. Recognised \
                 fields: path, on_missing"
            ));
        }
    }
    let path = match map.get("path") {
        None => None,
        Some(JVal::String(p)) => {
            let p = p.trim();
            if !p.is_empty() && !p.starts_with('/') {
                return Err(format!(
                    "op '{op_name}' traverse: path '{p}' must start with '/' \
                     (RFC 6901 JSON-Pointer); an empty path addresses the root"
                ));
            }
            Some(p.to_string())
        }
        Some(other) => {
            return Err(format!(
                "op '{op_name}' traverse: path expected a string, got {other:?}"
            ));
        }
    };
    let on_missing = match map.get("on_missing") {
        None => OnMissing::default(),
        Some(JVal::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "ignore" => OnMissing::Ignore,
            "warn" => OnMissing::Warn,
            "error" | "fail" => OnMissing::Error,
            other => {
                return Err(format!(
                    "op '{op_name}' traverse: on_missing '{other}': expected one of \
                 ignore / warn / error"
                ));
            }
        },
        Some(other) => {
            return Err(format!(
                "op '{op_name}' traverse: on_missing expected a string, got {other:?}"
            ));
        }
    };
    Ok(Some(TraverseSpec { path, on_missing }))
}

fn parse_result_field(
    val: Option<&JVal>,
    op_name: &str,
) -> Result<Option<crate::model::ResultSpec>, String> {
    let Some(v) = val else {
        return Ok(None);
    };
    let spec = parse_result_spec(v, op_name)?;
    if spec.is_empty() {
        Ok(None)
    } else {
        Ok(Some(spec))
    }
}

fn parse_result_spec(v: &JVal, op_name: &str) -> Result<crate::model::ResultSpec, String> {
    use crate::model::ResultSpec;
    match v {
        JVal::Null => Ok(ResultSpec::String(String::new())),
        JVal::String(s) => Ok(ResultSpec::String(s.clone())),
        JVal::Array(items) => {
            let mut out: Vec<ResultSpec> = Vec::with_capacity(items.len());
            for item in items {
                out.push(parse_result_spec(item, op_name)?);
            }
            Ok(ResultSpec::List(out))
        }
        JVal::Object(map) => {
            let mut out: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for (key, val) in map {
                let source = match val {
                    JVal::String(s) => s.clone(),
                    JVal::Null => String::new(),
                    other => {
                        return Err(format!(
                            "op '{op_name}' result.{key}: expected string \
                         (short-form keyword `count`/`ok`, path \
                         expression, or Polydat expression); got {other}"
                        ));
                    }
                };
                if out.insert(key.clone(), source).is_some() {
                    return Err(format!("op '{op_name}' result: duplicate key '{key}'"));
                }
            }
            Ok(ResultSpec::Map(out))
        }
        _ => Err(format!(
            "op '{op_name}' result: expected string (GK source), \
             list (sequence of fragments), or mapping (named \
             short-forms); got {v}"
        )),
    }
}

// -----------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------

/// Extract bindings from a YAML value.
///
/// If the value is a string, it's native Polydat grammar source.
/// If it's a mapping, it's legacy name→expression pairs.
fn extract_bindings(val: Option<&JVal>) -> BindingsDef {
    match val {
        Some(JVal::String(s)) => BindingsDef::PolydatSource(s.clone()),
        Some(JVal::Object(obj)) => {
            let mut map = HashMap::new();
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    map.insert(k.clone(), s.to_string());
                } else {
                    map.insert(k.clone(), v.to_string());
                }
            }
            BindingsDef::Map(map)
        }
        _ => BindingsDef::default(),
    }
}

/// SRD-13f Push D: inline a block's YAML-level `bindings:`
/// sugar into one of its enclosed ops.
///
/// Blocks are not a Polydat scope — they're YAML authoring sugar
/// (named groups for tag-filtering + shared defaults). A
/// block-level `bindings:` field is *syntactic sugar* meaning
/// "every op underneath has these bindings as part of its own
/// op-level bindings." This helper does that expansion at
/// parse time.
///
/// Semantics (preserves prior `merge_bindings` shape for the
/// only call site that still uses it):
/// - The op's own PolydatSource fully shadows the block's sugar
///   (the op declares its full binding set explicitly).
/// - Op Map merges with block Map (op keys override block).
/// - Empty op inherits the block sugar verbatim.
///
/// No cross-scope semantics: workload-level and phase-level
/// `bindings:` no longer flow through this helper. They reach
/// ops via the Polydat Kernel chain.
fn inline_block_sugar_into_op(block_sugar: &BindingsDef, op_own: &BindingsDef) -> BindingsDef {
    match (block_sugar, op_own) {
        (_, BindingsDef::PolydatSource(s)) if !s.trim().is_empty() => {
            BindingsDef::PolydatSource(s.clone())
        }
        (BindingsDef::Map(p), BindingsDef::Map(c)) => {
            let mut merged = p.clone();
            for (k, v) in c {
                merged.insert(k.clone(), v.clone());
            }
            BindingsDef::Map(merged)
        }
        (_, BindingsDef::Map(c)) if c.is_empty() => block_sugar.clone(),
        (_, child) => child.clone(),
    }
}

/// Render a YAML/JSON param value as the text form
/// `add_param_binding` expects.
///
/// Scalar shapes pass through RAW (no extra quoting): a YAML
/// `iter_count: "3"` and a YAML `iter_count: 3` BOTH come out as
/// the string `"3"` here — the downstream classifier sees
/// numeric-shape and emits a bare U64 binding either way. YAML's
/// quotes are presentation, not semantic, for scalars.
///
/// Array shape gets the polydat array literal form
/// (`[v1, v2, v3]`) — that's the new convention the workload
/// surface needs to support. Array ELEMENTS are formatted with
/// polydat literal grammar (strings explicitly quoted) since
/// polydat's parser requires quotes inside array literals;
/// `format_jval_in_array_context` handles the recursion.
///
/// Object shape (rare for params) falls back to JSON
/// serialization — there's no polydat literal form for objects,
/// so the value passes through whatever-it-is for callers
/// downstream to handle.
pub(crate) fn format_jval_as_polydat_literal(v: &JVal) -> String {
    match v {
        JVal::Null => String::new(),
        JVal::Bool(b) => b.to_string(),
        JVal::Number(n) => n.to_string(),
        // Scalar strings pass through unquoted to preserve the
        // legacy "YAML quotes are presentation" behavior. The
        // downstream classifier (`add_param_binding`) figures
        // out the actual type from the content — numeric-shape
        // becomes U64/F64, identifier-shape becomes a reference,
        // string-shape becomes a polydat-quoted Str.
        JVal::String(s) => s.clone(),
        JVal::Array(items) => {
            let elts: Vec<String> = items.iter().map(format_jval_in_array_context).collect();
            format!("[{}]", elts.join(", "))
        }
        JVal::Object(_) => v.to_string(),
    }
}

/// Element-context formatter: polydat array literals require
/// explicit quotes around string elements (`["a", "b"]`), unlike
/// the scalar-context formatter which leaves strings unquoted.
fn format_jval_in_array_context(v: &JVal) -> String {
    match v {
        JVal::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
        JVal::Array(items) => {
            let elts: Vec<String> = items.iter().map(format_jval_in_array_context).collect();
            format!("[{}]", elts.join(", "))
        }
        _ => format_jval_as_polydat_literal(v),
    }
}

fn extract_string_map(val: Option<&JVal>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(JVal::Object(obj)) = val {
        for (k, v) in obj {
            // Format every YAML value as polydat-native source.
            // Strings come out quote-wrapped, arrays as
            // `[a, b, c]`, numbers / bools bare. The downstream
            // `add_param_binding` classifier reads this and emits
            // the const binding without re-quoting.
            map.insert(k.clone(), format_jval_as_polydat_literal(v));
        }
    }
    map
}

// Shared classifier helpers — the `set:` block parser routes
// every value through the same shape detection so the numeric /
// array-literal / quoted-string surface is consistent.

fn is_polydat_quoted_string(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    if !s.starts_with('"') || !s.ends_with('"') {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    let last = bytes.len() - 1;
    while i < last {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return false;
        }
        i += 1;
    }
    true
}

fn extract_value_map(val: Option<&JVal>) -> HashMap<String, JVal> {
    let mut map = HashMap::new();
    if let Some(JVal::Object(obj)) = val {
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }
    }
    map
}

fn merge_string_maps(
    parent: &HashMap<String, String>,
    child: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = parent.clone();
    for (k, v) in child {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

/// Activity/phase-scope param keys that must never be blast-merged onto ops.
/// They are consumed at phase/activity scope; leaking them into op `params`
/// makes an inherited `rate` collide with the `rate` wrapper's
/// field-ownership guard (SRD-32a). The op-level `rate:` field reaches ops via
/// the typed [`ParsedOp::rate`] path, not params, so this exclusion is safe.
const ACTIVITY_PARAM_KEYS: &[&str] = &["cycles", "concurrency", "rate", "errors", "error_rate_max"];

/// Clone `params` minus the activity/phase-scope keys ([`ACTIVITY_PARAM_KEYS`]).
fn exclude_activity_keys(params: &HashMap<String, JVal>) -> HashMap<String, JVal> {
    params
        .iter()
        .filter(|(k, _)| !ACTIVITY_PARAM_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn merge_value_maps(
    parent: &HashMap<String, JVal>,
    child: &HashMap<String, JVal>,
) -> HashMap<String, JVal> {
    let mut merged = parent.clone();
    for (k, v) in child {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_metrics_mapping_form_preserves_raw_value() {
        // Phase-level `metrics:` keeps the raw `value:` expression
        // (no bare-key injection into bindings — the phase synthesiser
        // emits `volatile __metric_<name> := <value>` directly).
        let yaml = r#"
phases:
  build_index:
    metrics:
      time_to_index: { value: "current_epoch_millis() - phase_start", kind: gauge }
    ops:
      work: { stmt: "op" }
scenarios:
  default: [build_index]
"#;
        let wl = parse_workload(yaml, &HashMap::new()).expect("parse");
        let phase = wl.phases.get("build_index").expect("phase build_index");
        let m = phase
            .metrics
            .get("time_to_index")
            .expect("time_to_index metric");
        assert_eq!(
            m.value, "current_epoch_millis() - phase_start",
            "raw value must be preserved verbatim"
        );
        assert_eq!(m.kind, Some(crate::model::MetricKind::Gauge));
        assert!(
            phase.bindings.is_empty(),
            "phase metrics must NOT auto-inject into bindings: {:?}",
            phase.bindings
        );
    }

    #[test]
    fn tries_accepts_sugared_number_and_map_form() {
        // Sugared: a bare number sets the count, no backoff overrides.
        let sugar = r#"
phases:
  load:
    tries: 20
    ops: { work: { stmt: "op" } }
scenarios: { default: [load] }
"#;
        let wl = parse_workload(sugar, &HashMap::new()).expect("parse sugar");
        let p = wl.phases.get("load").unwrap();
        assert_eq!(p.tries, Some(20));
        assert_eq!(p.tries_backoff, None);

        // Map form: `count` + nested `backoff` (durations kept as strings).
        let map = r#"
phases:
  load:
    tries:
      count: 20
      backoff:
        ratio: 2.0
        min: 100ms
        max: 10s
    ops: { work: { stmt: "op" } }
scenarios: { default: [load] }
"#;
        let wl = parse_workload(map, &HashMap::new()).expect("parse map");
        let p = wl.phases.get("load").unwrap();
        assert_eq!(p.tries, Some(20));
        let bo = p.tries_backoff.as_ref().expect("backoff parsed");
        assert_eq!(bo.ratio, Some(2.0));
        assert_eq!(bo.min.as_deref(), Some("100ms"));
        assert_eq!(bo.max.as_deref(), Some("10s"));

        // A bad shape (string) is a loud parse error, not a silent drop.
        let bad = r#"
phases: { load: { tries: "lots", ops: { w: { stmt: "op" } } } }
scenarios: { default: [load] }
"#;
        assert!(
            parse_workload(bad, &HashMap::new()).is_err(),
            "non-number, non-map tries must fail to parse"
        );
    }

    #[test]
    fn metric_spec_rejects_unknown_field_type_with_hint() {
        // `type:` is the OpenMetrics word; ours is `kind`. Reject it
        // loudly with a hint rather than silently dropping it (which
        // would default the metric to gauge).
        let yaml = r#"
phases:
  p:
    metrics:
      m: { type: counter, value: "x" }
    ops:
      work: { stmt: "op" }
scenarios:
  default: [p]
"#;
        let err = parse_workload(yaml, &HashMap::new())
            .expect_err("unknown metric field `type` must be rejected");
        assert!(
            err.contains("unknown field `type`"),
            "must name the offending field; got: {err}"
        );
        assert!(
            err.contains("kind"),
            "must hint at the canonical `kind` field; got: {err}"
        );
    }

    #[test]
    fn metric_spec_rejects_arbitrary_unknown_field() {
        let yaml = r#"
phases:
  p:
    metrics:
      m: { value: "x", flavour: gauge }
    ops:
      work: { stmt: "op" }
scenarios:
  default: [p]
"#;
        let err = parse_workload(yaml, &HashMap::new())
            .expect_err("arbitrary unknown metric field must be rejected");
        assert!(
            err.contains("unknown field `flavour`"),
            "must name the offending field; got: {err}"
        );
    }

    #[test]
    fn metric_spec_accepts_all_known_fields() {
        // Regression guard: the unknown-field check must not reject any
        // legitimate field.
        let yaml = r#"
phases:
  p:
    metrics:
      m: { value: "x", family: fam, kind: counter, unit: bytes, format: "0.00" }
    ops:
      work: { stmt: "op" }
scenarios:
  default: [p]
"#;
        let wl = parse_workload(yaml, &HashMap::new()).expect("all known fields accepted");
        let m = wl.phases.get("p").unwrap().metrics.get("m").unwrap();
        assert_eq!(m.kind, Some(crate::model::MetricKind::Counter));
        assert_eq!(m.unit.as_deref(), Some("bytes"));
        assert_eq!(m.family.as_deref(), Some("fam"));
    }

    #[test]
    fn phase_metrics_list_form_rejects_wire_expression() {
        // The `name := expr` list form is op-only; for a phase the
        // author must use the phase `bindings:` block + a bare name,
        // or the mapping form. Reject loudly rather than silently.
        let yaml = r#"
phases:
  p:
    metrics:
      - "te := current_epoch_millis() - phase_start"
    ops:
      work: { stmt: "op" }
scenarios:
  default: [p]
"#;
        let err = parse_workload(yaml, &HashMap::new())
            .expect_err("list `name := expr` form must be rejected for phases");
        assert!(
            err.contains("op-only") || err.contains("mapping form"),
            "diagnostic should point at the op-only form; got: {err}"
        );
    }

    #[test]
    fn readouts_block_form_a_scalar_binds_on_update() {
        let workload: serde_yaml::Value =
            serde_yaml::from_str(r#"readouts: phase_status"#).unwrap();
        let json = serde_json::to_value(&workload).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        assert_eq!(r.on_update, vec!["phase_status".to_string()]);
        assert!(r.on_phase_end.is_empty());
    }

    #[test]
    fn readouts_block_form_b_mapping_binds_explicit_slots() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  on_phase_end: phase_outcome
  on_update: "phase_status lod=compact"
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        assert_eq!(r.on_phase_end, vec!["phase_outcome".to_string()]);
        assert_eq!(r.on_update, vec!["phase_status lod=compact".to_string()]);
    }

    #[test]
    fn readouts_block_form_c_list_composes() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  on_phase_end:
    - phase_outcome
    - phase_failure_hint
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        assert_eq!(
            r.on_phase_end,
            vec![
                "phase_outcome".to_string(),
                "phase_failure_hint".to_string(),
            ]
        );
    }

    #[test]
    fn readouts_block_each_wildcard_expands() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  each_*: scope_bracket
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        assert_eq!(r.on_each_start, vec!["scope_bracket".to_string()]);
        assert_eq!(r.on_each_end, vec!["scope_bracket".to_string()]);
        // Other slots untouched.
        assert!(r.on_phase_end.is_empty());
        assert!(r.on_update.is_empty());
    }

    #[test]
    fn readouts_block_phase_wildcard_expands() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  phase_*: trace
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        assert_eq!(r.on_phase_start, vec!["trace".to_string()]);
        assert_eq!(r.on_phase_end, vec!["trace".to_string()]);
        assert!(r.on_each_start.is_empty());
    }

    #[test]
    fn readouts_block_universal_wildcard_expands_to_all() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  "*": trace
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let r = parse_readouts_block(json.get("readouts")).unwrap();
        for slot in [
            &r.on_session_start,
            &r.on_session_end,
            &r.on_phase_start,
            &r.on_phase_end,
            &r.on_each_start,
            &r.on_each_end,
            &r.on_scope_start,
            &r.on_scope_end,
            &r.on_update,
        ] {
            assert_eq!(slot, &vec!["trace".to_string()]);
        }
    }

    #[test]
    fn readouts_block_unknown_slot_is_error() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
readouts:
  on_unknown: phase_outcome
"#,
        )
        .unwrap();
        let json = serde_json::to_value(&yaml).unwrap();
        let err = parse_readouts_block(json.get("readouts")).unwrap_err();
        assert!(
            err.contains("unknown slot 'on_unknown'"),
            "wrong message: {err}"
        );
    }

    #[test]
    fn parse_single_string_op() {
        let ops = parse_ops("op: select * from bar.table;").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].name, "stmt1");
        assert_eq!(ops[0].op["stmt"], "select * from bar.table;");
    }

    #[test]
    fn parse_ops_list_of_strings() {
        let yaml = r#"
ops:
  - select * from t1;
  - select * from t2;
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].op["stmt"], "select * from t1;");
        assert_eq!(ops[1].op["stmt"], "select * from t2;");
    }

    #[test]
    fn parse_ops_map_of_strings() {
        let yaml = r#"
ops:
  read: select * from t1;
  write: insert into t1 values (1);
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops.len(), 2);
        let read = ops.iter().find(|o| o.name == "read").unwrap();
        assert_eq!(read.op["stmt"], "select * from t1;");
    }

    #[test]
    fn parse_named_blocks() {
        let yaml = r#"
blocks:
  schema:
    ops:
      create: "CREATE TABLE t (id int PRIMARY KEY);"
  main:
    ops:
      read: "SELECT * FROM t WHERE id={id};"
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops.len(), 2);
        let create = ops.iter().find(|o| o.name == "create").unwrap();
        assert_eq!(create.tags["block"], "schema");
        let read = ops.iter().find(|o| o.name == "read").unwrap();
        assert_eq!(read.tags["block"], "main");
    }

    #[test]
    fn parse_property_inheritance() {
        let yaml = r#"
bindings:
  id: Identity()
params:
  prepared: true
tags:
  workload: test
blocks:
  main:
    bindings:
      id: Hash()
    ops:
      op1: "SELECT * FROM t;"
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops.len(), 1);
        // Block-level binding overrides doc-level
        assert_eq!(ops[0].bindings.as_map()["id"], "Hash()");
        // Doc-level param inherited
        assert_eq!(ops[0].params["prepared"], true);
        // Doc-level tag inherited
        assert_eq!(ops[0].tags["workload"], "test");
        // Auto-tag
        assert_eq!(ops[0].tags["block"], "main");
    }

    #[test]
    fn parse_auto_naming() {
        let yaml = r#"
ops:
  - "first op"
  - "second op"
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops[0].name, "stmt1");
        assert_eq!(ops[1].name, "stmt2");
    }

    #[test]
    fn parse_auto_tagging() {
        let yaml = r#"
ops:
  myop: "SELECT 1;"
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops[0].tags["name"], "myop");
        assert_eq!(ops[0].tags["op"], "myop");
        assert_eq!(ops[0].tags["block"], "block0");
    }

    #[test]
    fn condition_clause_passthrough_for_identifier() {
        // Bare identifier — legacy "name a binding" form.
        assert_eq!(normalize_condition_clause("my_flag"), "my_flag");
        assert_eq!(normalize_condition_clause(" my_flag "), "my_flag");
    }

    #[test]
    fn condition_clause_passthrough_for_braced_forms() {
        assert_eq!(normalize_condition_clause("{my_flag}"), "{my_flag}");
        assert_eq!(normalize_condition_clause("{{x == 1}}"), "{{x == 1}}");
        assert_eq!(normalize_condition_clause("{:=x == 1:=}"), "{:=x == 1:=}");
    }

    #[test]
    fn condition_clause_wraps_bare_expressions() {
        assert_eq!(
            normalize_condition_clause("cql_dialect == 'cass'"),
            "{{cql_dialect == 'cass'}}",
        );
        assert_eq!(
            normalize_condition_clause("a > 0 && b < 10"),
            "{{a > 0 && b < 10}}",
        );
        assert_eq!(normalize_condition_clause("foo(bar)"), "{{foo(bar)}}",);
    }

    #[test]
    fn condition_clause_empty_passthrough() {
        assert_eq!(normalize_condition_clause(""), "");
        assert_eq!(normalize_condition_clause("   "), "");
    }

    #[test]
    fn parse_op_with_fields() {
        let yaml = r#"
ops:
  op1:
    field1: value1
    field2: value2
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops[0].op["field1"], "value1");
        assert_eq!(ops[0].op["field2"], "value2");
    }

    #[test]
    fn parse_explicit_op_field() {
        let yaml = r#"
ops:
  op1:
    op:
      stmt: "SELECT * FROM t;"
      type: query
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops[0].op["stmt"], "SELECT * FROM t;");
        assert_eq!(ops[0].op["type"], "query");
    }

    #[test]
    fn parse_scenarios() {
        let yaml = r#"
scenarios:
  default:
    schema: run driver=cql tags==block:schema threads==1
    main: run driver=cql tags==block:main cycles=1M
ops:
  op1: "test"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        let default = &workload.scenarios["default"];
        assert_eq!(default.len(), 2);
        // Legacy command-string format: names are preserved as Phase nodes
        assert!(matches!(&default[0], ScenarioNode::Phase(n) if n == "schema"));
        assert!(matches!(&default[1], ScenarioNode::Phase(n) if n == "main"));
    }

    #[test]
    fn parse_template_expansion() {
        let yaml = r#"
ops:
  op1: "SELECT * FROM t LIMIT TEMPLATE(limit, 100);"
"#;
        let ops = parse_ops(yaml).unwrap();
        assert_eq!(ops[0].op["stmt"], "SELECT * FROM t LIMIT 100;");
    }

    #[test]
    fn parse_description() {
        let yaml = r#"
description: |
  This is a test workload.
ops:
  op1: "test"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert!(workload.description.unwrap().contains("test workload"));
    }

    // ── Scenario malformed-node rejection (Never Ignore
    // Silently). The parser used to silently drop an
    // unrecognised scenario-node key like `iterate:` →
    // `{phases: [...]}`, leaving the scenario empty and
    // surfacing as a confusing downstream "phase not found"
    // error. These tests pin the loud-rejection behavior so a
    // refactor can't regress the silent-drop. ──

    #[test]
    fn scenario_node_with_unknown_map_key_collects_parse_error() {
        let yaml = r#"
scenarios:
  bogus:
    - iterate:
        phases:
          - phase_x
phases:
  phase_x:
    ops:
      noop: "x"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert!(
            !workload.scenario_parse_errors.is_empty(),
            "malformed `iterate:` node MUST surface a scenario_parse_error \
             — silent drop is the safety bug being prevented"
        );
        let msg = workload.scenario_parse_errors[0].as_str();
        assert!(
            msg.contains("bogus"),
            "error must name the offending scenario: {msg}"
        );
        assert!(
            msg.contains("iterate"),
            "error must name the bad key: {msg}"
        );
        // Bogus scenario must NOT have absorbed phase_x as a Phase
        // node via the legacy catch-all.
        assert_eq!(
            workload.scenarios.get("bogus").map(|v| v.len()),
            Some(0),
            "malformed node must NOT silently produce ScenarioNode::Phase"
        );
    }

    #[test]
    fn scenario_node_with_legacy_command_string_form_still_works() {
        // Pre-existing nosqlbench-style scenario shape: each
        // map key is a step name, value is a `run ...` CLI
        // string. The malformed-rejection refactor MUST NOT
        // break this — the discriminator is "map value is
        // a string" → legacy, "map value is a map/array" →
        // malformed.
        let yaml = r#"
scenarios:
  default:
    schema: run tags==block:schema
    main: run tags==block:main
ops:
  op1: "test"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert!(
            workload.scenario_parse_errors.is_empty(),
            "legacy command-string form must NOT be flagged as malformed: {:?}",
            workload.scenario_parse_errors
        );
        let default = &workload.scenarios["default"];
        assert_eq!(default.len(), 2);
        assert!(matches!(&default[0], ScenarioNode::Phase(n) if n == "schema"));
        assert!(matches!(&default[1], ScenarioNode::Phase(n) if n == "main"));
    }

    #[test]
    fn scenario_node_malformed_error_lists_recognised_keys() {
        // The error message must point the operator at the
        // recognised key vocabulary so they can self-correct
        // without grepping the source.
        let yaml = r#"
scenarios:
  s1:
    - typoed_for_each:
        phases:
          - phase_x
phases:
  phase_x:
    ops:
      noop: "x"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert_eq!(workload.scenario_parse_errors.len(), 1);
        let msg = &workload.scenario_parse_errors[0];
        // Must mention at least the load-bearing alternatives.
        for expected in ["for_each", "scenarios", "do_while", "bindings"] {
            assert!(
                msg.contains(expected),
                "error message must mention `{expected}` as a valid \
                 alternative; got: {msg}"
            );
        }
    }

    #[test]
    fn parse_polydat_source_bindings() {
        // SRD-13f Push D: workload-level `bindings:` live on
        // `Workload.bindings` and reach ops via the Polydat Kernel
        // chain at runtime — they are NOT folded into per-op
        // bindings at parse time.
        let yaml = r#"
bindings: |
  // Explicit wiring — every intermediate is named
  input cycle: u64
  h := hash(cycle)
  user_id := mod(h, 1000000)
  code_hash := hash(user_id)
  code := combinations(code_hash, '0-9A-Z')

  // Equivalent concise form (nested composition):
  // user_id := mod(hash(cycle), 1000000)
  // code := combinations(hash(user_id), '0-9A-Z')
ops:
  insert: "INSERT INTO users (id, code) VALUES ({user_id}, '{code}');"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        match &workload.bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(src.contains("input cycle: u64"));
                assert!(src.contains("user_id := mod(h, 1000000)"));
            }
            BindingsDef::Map(_) => panic!("expected PolydatSource at workload level, got Map"),
        }
        assert_eq!(workload.ops.len(), 1);
        // Op carries no workload bindings — they reach it via
        // the Polydat Kernel chain, not via parse-time merge.
        assert!(workload.ops[0].bindings.is_empty());
    }

    #[test]
    fn parse_map_bindings_still_works() {
        // SRD-13f Push D: Map-form workload bindings live on
        // `Workload.bindings`, not on per-op bindings.
        let yaml = r#"
bindings:
  id: "Hash(); Mod(100)"
ops:
  op1: "SELECT * FROM t WHERE id={id};"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert_eq!(workload.bindings.as_map()["id"], "Hash(); Mod(100)");
        // The op itself carries no merged-in workload bindings.
        assert!(workload.ops[0].bindings.is_empty());
    }

    #[test]
    fn parse_phased_workload() {
        let yaml = r#"
scenarios:
  default:
    - schema
    - main

phases:
  schema:
    cycles: 1
    concurrency: 1
    ops:
      create_table:
        stmt: "CREATE TABLE t (id int PRIMARY KEY);"
  main:
    cycles: 1000
    concurrency: 10
    rate: 500.0
    ops:
      read:
        stmt: "SELECT * FROM t WHERE id={id};"
      write:
        stmt: "INSERT INTO t (id) VALUES ({id});"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();

        // Phases parsed
        assert_eq!(workload.phases.len(), 2);
        assert!(workload.phases.contains_key("schema"));
        assert!(workload.phases.contains_key("main"));

        // Phase order preserved
        assert_eq!(workload.phase_order, vec!["schema", "main"]);

        // Schema phase config
        let schema = &workload.phases["schema"];
        assert_eq!(schema.cycles.as_deref(), Some("1"));
        assert_eq!(schema.concurrency.as_deref(), Some("1"));
        assert_eq!(schema.rate, None);
        assert_eq!(schema.ops.len(), 1);
        assert_eq!(schema.ops[0].name, "create_table");

        // Main phase config
        let main = &workload.phases["main"];
        assert_eq!(main.cycles.as_deref(), Some("1000"));
        assert_eq!(main.concurrency.as_deref(), Some("10"));
        assert_eq!(main.rate.as_deref(), Some("500.0"));
        assert_eq!(main.ops.len(), 2);

        // Scenario parsed as phase name list
        let default = &workload.scenarios["default"];
        assert_eq!(default.len(), 2);
        assert!(matches!(&default[0], ScenarioNode::Phase(n) if n == "schema"));
        assert!(matches!(&default[1], ScenarioNode::Phase(n) if n == "main"));
    }

    #[test]
    fn parse_phased_workload_with_tags() {
        let yaml = r#"
blocks:
  schema:
    ops:
      create: "CREATE TABLE t (id int PRIMARY KEY);"
  main:
    ops:
      read: "SELECT * FROM t;"

phases:
  setup:
    tags: "block:schema"
    cycles: 1
  run:
    tags: "block:main"
    cycles: 1000
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert_eq!(workload.phases.len(), 2);

        // SRD-108 Part A: the selector resolves at parse time —
        // the phase carries the selected block ops as if inline.
        let setup = &workload.phases["setup"];
        assert_eq!(setup.tags.as_deref(), Some("block:schema"));
        assert_eq!(setup.ops.len(), 1);
        assert_eq!(setup.ops[0].name, "create");
        assert_eq!(
            setup.ops[0].tags.get("phase").map(String::as_str),
            Some("setup")
        );

        let run = &workload.phases["run"];
        assert_eq!(run.tags.as_deref(), Some("block:main"));
        assert_eq!(run.ops.len(), 1);
        assert_eq!(run.ops[0].name, "read");
    }

    /// SRD-108 Part A — a selector matching nothing is a load
    /// error naming the phase, never a dispatch-time panic.
    #[test]
    fn phase_tag_selector_matching_nothing_is_a_load_error() {
        let yaml = r#"
blocks:
  schema:
    ops:
      create: "CREATE TABLE t (id int PRIMARY KEY);"
phases:
  setup:
    tags: "block:nonexistent"
    cycles: 1
"#;
        let err = parse_workload(yaml, &HashMap::new()).unwrap_err();
        assert!(
            err.contains("setup") && err.contains("matched no ops"),
            "error must name the phase and the failure: {err}"
        );
    }

    /// SRD-108 Part A — inline ops and a selector together are
    /// rejected: one source of ops per phase.
    #[test]
    fn phase_with_inline_ops_and_selector_is_rejected() {
        let yaml = r#"
blocks:
  schema:
    ops:
      create: "CREATE TABLE t (id int PRIMARY KEY);"
phases:
  setup:
    tags: "block:schema"
    cycles: 1
    ops:
      also: "SELECT 1;"
"#;
        let err = parse_workload(yaml, &HashMap::new()).unwrap_err();
        assert!(
            err.contains("both inline ops and a"),
            "error must explain the conflict: {err}"
        );
    }

    #[test]
    fn parse_phased_workload_polydat_cycles() {
        let yaml = r#"
phases:
  rampup:
    cycles: "{train_count}"
    concurrency: 100
    ops:
      insert:
        stmt: "INSERT INTO t (id) VALUES ({id});"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        let rampup = &workload.phases["rampup"];
        assert_eq!(rampup.cycles.as_deref(), Some("{train_count}"));
    }

    // ── SRD-40b §1 + §2: `metrics:` discriminant on op template ──

    #[test]
    fn parse_metrics_full_mapping_form() {
        let yaml = r##"
phases:
  predict:
    bindings: |
      example_factor := 1.0 + 2.5
    ops:
      synth:
        stmt: "noop"
        metrics:
          example_factor:
            value: example_factor
            kind: gauge
            unit: ratio
            format: "#.##"
"##;
        let wl = parse_workload(yaml, &HashMap::new()).unwrap();
        let op = &wl.phases["predict"].ops[0];
        assert_eq!(op.name, "synth");
        let m = &op.metrics["example_factor"];
        assert_eq!(m.value, "example_factor");
        assert_eq!(m.kind, Some(crate::model::MetricKind::Gauge));
        assert_eq!(m.unit.as_deref(), Some("ratio"));
        assert_eq!(m.format.as_deref(), Some("#.##"));
    }

    #[test]
    fn parse_metrics_bare_string_sugar() {
        let yaml = r#"
phases:
  p:
    bindings: |
      overscan := 1.0
    ops:
      o:
        stmt: "noop"
        metrics: overscan
"#;
        let wl = parse_workload(yaml, &HashMap::new()).unwrap();
        let op = &wl.phases["p"].ops[0];
        let m = &op.metrics["overscan"];
        // Bare-string form: family + value both = "overscan"
        // (defaults), kind unset (defaults to gauge at runtime).
        assert_eq!(m.value, "overscan");
        assert_eq!(m.family, None);
        assert_eq!(m.kind, None);
    }

    #[test]
    fn parse_metrics_list_with_wire_expression() {
        let yaml = r#"
phases:
  p:
    ops:
      o:
        stmt: "noop"
        metrics:
          - latency_pred := 0.5 + 1.5 * pow(limit, -0.4)
          - already_bound
"#;
        let wl = parse_workload(yaml, &HashMap::new()).unwrap();
        let op = &wl.phases["p"].ops[0];
        // Both metric entries registered.
        assert!(op.metrics.contains_key("latency_pred"));
        assert!(op.metrics.contains_key("already_bound"));
        // Wire expression auto-injected into op bindings.
        match &op.bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(
                    src.contains("latency_pred := 0.5 + 1.5 * pow(limit, -0.4)"),
                    "wire not injected; bindings: {src:?}"
                );
            }
            other => panic!("expected PolydatSource bindings, got {other:?}"),
        }
    }

    #[test]
    fn parse_metrics_mapping_form_with_wire_expression() {
        let yaml = r#"
phases:
  p:
    bindings: |
      base := 10
    ops:
      o:
        stmt: "noop"
        metrics:
          scaled:
            value: base * 2
            kind: gauge
"#;
        let wl = parse_workload(yaml, &HashMap::new()).unwrap();
        let op = &wl.phases["p"].ops[0];
        let m = &op.metrics["scaled"];
        // After auto-inject the spec's `value:` is the bare key.
        assert_eq!(m.value, "scaled");
        // The non-bare expression landed in op-template bindings.
        match &op.bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(
                    src.contains("scaled := base * 2"),
                    "expression not injected; bindings: {src:?}"
                );
            }
            other => panic!("expected PolydatSource bindings, got {other:?}"),
        }
    }

    #[test]
    fn parse_metrics_mapping_form_invalid_key_for_non_bare_value() {
        // Non-bare value + key that can't be a binding name → reject.
        let yaml = r#"
phases:
  p:
    ops:
      o:
        stmt: "noop"
        metrics:
          "1bad":
            value: foo + 1
            kind: gauge
"#;
        let err = parse_workload(yaml, &HashMap::new()).unwrap_err();
        assert!(
            err.contains("must itself be a valid identifier"),
            "expected identifier diagnostic, got: {err}"
        );
    }

    #[test]
    fn parse_metrics_format_validation_runs_at_load() {
        let yaml = r##"
phases:
  p:
    ops:
      o:
        stmt: "noop"
        metrics:
          x:
            value: x
            format: "%3.2f"
"##;
        let err = parse_workload(yaml, &HashMap::new()).unwrap_err();
        assert!(
            err.contains("printf-style"),
            "format error not surfaced at parse time: {err}"
        );
    }

    #[test]
    fn parse_metrics_wire_expression_collision_errors() {
        let yaml = r#"
phases:
  p:
    ops:
      o:
        stmt: "noop"
        bindings: |
          foo := 1.0
        metrics:
          - foo := 2.0
"#;
        let err = parse_workload(yaml, &HashMap::new()).unwrap_err();
        assert!(err.contains("collides"), "collision not detected: {err}");
    }

    #[test]
    fn parse_phase_bindings_round_trip() {
        // SRD-13d Phase 1: phase-level `bindings:` block must
        // land on WorkloadPhase.bindings (not just merged into
        // ops) so HasGkMatter can classify it.
        let yaml = r#"
phases:
  p:
    bindings: |
      phase_factor := 7
    ops:
      o:
        stmt: "noop"
"#;
        let wl = parse_workload(yaml, &HashMap::new()).unwrap();
        match &wl.phases["p"].bindings {
            BindingsDef::PolydatSource(s) => assert!(s.contains("phase_factor := 7")),
            other => panic!("expected PolydatSource, got {other:?}"),
        }
    }

    #[test]
    fn parse_phased_workload_default_scenario_from_order() {
        // No scenarios section — phases should run in definition order
        let yaml = r#"
phases:
  alpha:
    cycles: 1
    ops:
      op1:
        stmt: "a"
  beta:
    cycles: 2
    ops:
      op2:
        stmt: "b"
  gamma:
    cycles: 3
    ops:
      op3:
        stmt: "c"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert_eq!(workload.phase_order, vec!["alpha", "beta", "gamma"]);
        assert!(workload.scenarios.is_empty());
    }

    #[test]
    fn parse_backward_compat_no_phases() {
        // Workload without phases should work exactly as before
        let yaml = r#"
ops:
  op1: "SELECT 1;"
  op2: "SELECT 2;"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        assert!(workload.phases.is_empty());
        assert!(workload.phase_order.is_empty());
        assert_eq!(workload.ops.len(), 2);
    }

    #[test]
    fn block_level_params_override_workload_default() {
        // SRD 21 §"Parameter Resolution": closest-wins. The DDL block declares
        // `consistency=1`, overriding the workload-level default `100` for ops
        // in that block. Uses an op-scope param (`consistency`); activity-scope
        // keys (`concurrency`/`rate`/`cycles`/`errors`) are deliberately NOT
        // merged onto ops (SRD-32a — see `ACTIVITY_PARAM_KEYS`), so this tests
        // op-param precedence with a key that actually reaches ops.
        let yaml = r#"
params:
  consistency: "100"
blocks:
  ddl:
    params:
      consistency: "1"
    ops:
      schema_create: "CREATE TABLE foo (id int PRIMARY KEY);"
  bulk:
    ops:
      insert: "INSERT INTO foo (id) VALUES (?);"
"#;
        let ops = parse_ops(yaml).unwrap();
        let ddl = ops.iter().find(|o| o.name == "schema_create").unwrap();
        let bulk = ops.iter().find(|o| o.name == "insert").unwrap();
        assert_eq!(
            ddl.params.get("consistency").and_then(|v| v.as_str()),
            Some("1"),
            "block-level override should win for ddl op",
        );
        assert_eq!(
            bulk.params.get("consistency").and_then(|v| v.as_str()),
            Some("100"),
            "non-overriding block inherits workload-level default",
        );
    }

    #[test]
    fn cli_overrides_block_level_params() {
        // CLI is the outermost layer per SRD 21 — it wins even over block-level
        // explicit overrides. Uses an op-scope param (`consistency`); see
        // `block_level_params_override_workload_default` for why not an
        // activity-scope key like `concurrency`.
        let yaml = r#"
params:
  consistency: "100"
blocks:
  ddl:
    params:
      consistency: "1"
    ops:
      schema_create: "CREATE TABLE foo (id int PRIMARY KEY);"
"#;
        let mut cli = HashMap::new();
        cli.insert("consistency".to_string(), "200".to_string());
        let workload = parse_workload(yaml, &cli).unwrap();
        let ddl = workload
            .ops
            .iter()
            .find(|o| o.name == "schema_create")
            .unwrap();
        assert_eq!(
            ddl.params.get("consistency").and_then(|v| v.as_str()),
            Some("200"),
            "CLI override should beat block-level",
        );
        // Workload-level params likewise reflect CLI.
        assert_eq!(
            workload.params.get("consistency").map(|s| s.as_str()),
            Some("200"),
        );
    }

    #[test]
    fn parse_polydat_source_overrides_parent_map() {
        // Block-level Polydat source completely replaces doc-level map bindings
        let yaml = r#"
bindings:
  id: "Hash()"
blocks:
  main:
    bindings: |
      input cycle: u64
      h := hash(cycle)
      id := mod(h, 1000)
      // Concise equivalent:
      // id := mod(hash(cycle), 1000)
    ops:
      op1: "SELECT * FROM t WHERE id={id};"
"#;
        let ops = parse_ops(yaml).unwrap();
        match &ops[0].bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(src.contains("input cycle: u64"));
                assert!(src.contains("id := mod(h, 1000)"));
            }
            BindingsDef::Map(_) => panic!("expected PolydatSource, got Map"),
        }
    }

    #[test]
    fn parse_scenarios_plural_list_form() {
        // The plural `scenarios: [a, b, c]` form composes
        // several named scenarios at one node. Each list
        // entry expands to an `IncludedScenario` and resolves
        // post-parse. Equivalent to `[- scenario: a, -
        // scenario: b, ...]` but reads more naturally for the
        // "just compose these" case.
        let yaml = r#"
scenarios:
  rampup:
    - prep
  query:
    - run

  composed:
    - scenarios:
        - rampup
        - query

phases:
  prep:
    ops:
      create:
        raw: "select 1"
  run:
    ops:
      sel:
        raw: "select {cycle}"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        let composed = workload
            .scenarios
            .get("composed")
            .expect("composed scenario must parse");
        // After resolution the IncludedScenario wrappers carry
        // their resolved children. The scenario tree shape is:
        //   composed
        //     └── IncludedScenario("rampup") [Phase("prep")]
        //     └── IncludedScenario("query")  [Phase("run")]
        // Walk one level deep into each include to assert.
        assert_eq!(
            composed.len(),
            2,
            "scenarios: [a, b] should produce two top-level nodes"
        );
        let names: Vec<&str> = composed
            .iter()
            .filter_map(|n| match n {
                ScenarioNode::IncludedScenario { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["rampup", "query"]);
        // First include resolves to its sole `Phase("prep")`.
        let first_children = match &composed[0] {
            ScenarioNode::IncludedScenario { children, .. } => children,
            _ => panic!("expected IncludedScenario at index 0"),
        };
        let first_phase = first_children.iter().find_map(|n| match n {
            ScenarioNode::Phase(p) => Some(p.as_str()),
            _ => None,
        });
        assert_eq!(first_phase, Some("prep"));
    }

    #[test]
    fn parse_scenarios_plural_mixes_with_other_node_shapes() {
        // List entries can be a mix of bare strings and other
        // scenario-node shapes (objects with `scenario:`,
        // `for_each:`, etc.). This matches the heterogeneous
        // shape `parse_scenario_nodes` already accepts at the
        // top level, so the plural form composes naturally
        // with everything else.
        let yaml = r#"
scenarios:
  rampup:
    - prep
  composed:
    - scenarios:
        - rampup
        - { scenario: rampup }

phases:
  prep:
    ops:
      create:
        raw: "select 1"
"#;
        let workload = parse_workload(yaml, &HashMap::new()).unwrap();
        let composed = workload.scenarios.get("composed").unwrap();
        // Both list entries should resolve to an IncludedScenario
        // wrapping the same `prep` phase.
        assert_eq!(composed.len(), 2);
        for node in composed {
            match node {
                ScenarioNode::IncludedScenario { name, children } => {
                    assert_eq!(name, "rampup");
                    assert!(
                        children
                            .iter()
                            .any(|c| matches!(c, ScenarioNode::Phase(p) if p == "prep"))
                    );
                }
                other => panic!("expected IncludedScenario, got {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------
    // Checkpoint declaration parsing — SRD-44 §"Forms"
    // -----------------------------------------------------------------

    fn parse_checkpoint_field(yaml: &str) -> Option<crate::model::Checkpoint> {
        let yaml = format!(
            "phases:\n  p:\n{}\n    ops:\n      - select 1;\n",
            yaml.lines()
                .map(|l| format!("    {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("yaml parse");
        let json: serde_json::Value = serde_json::to_value(&v).expect("json convert");
        let phases_obj = json
            .get("phases")
            .and_then(|p| p.as_object())
            .expect("phases");
        let phase = phases_obj
            .get("p")
            .and_then(|p| p.as_object())
            .expect("phase p");
        phase.get("checkpoint").map(|v| {
            serde_json::from_value::<crate::model::Checkpoint>(v.clone()).expect("checkpoint parse")
        })
    }

    #[test]
    fn checkpoint_short_form_idempotent() {
        let cp = parse_checkpoint_field("checkpoint: idempotent").expect("present");
        assert!(cp.idempotent);
        assert!(cp.hashed);
        assert!(cp.verify.is_none());
    }

    #[test]
    fn checkpoint_short_form_none_disables_skip() {
        let cp = parse_checkpoint_field("checkpoint: none").expect("present");
        assert!(!cp.idempotent);
        // hashed default-true is preserved even when disabled —
        // the disabled state is about skip eligibility, not
        // about the hash field.
        assert!(cp.hashed);
        assert!(cp.verify.is_none());
    }

    #[test]
    fn checkpoint_short_form_no_and_false_and_off_all_disable() {
        for word in &["no", "false", "off"] {
            let cp = parse_checkpoint_field(&format!("checkpoint: {word}")).expect("present");
            assert!(!cp.idempotent, "expected disabled for '{word}'");
        }
    }

    #[test]
    fn checkpoint_bool_false_disables() {
        // YAML's bare `false` should map to disabled.
        let cp = parse_checkpoint_field("checkpoint: false").expect("present");
        assert!(!cp.idempotent);
    }

    #[test]
    fn checkpoint_full_form_all_explicit() {
        let cp = parse_checkpoint_field("checkpoint:\n  idempotent: true\n  hashed: false")
            .expect("present");
        assert!(cp.idempotent);
        assert!(!cp.hashed);
        assert!(cp.verify.is_none());
    }

    #[test]
    fn checkpoint_full_form_with_verify() {
        let cp = parse_checkpoint_field(
            "checkpoint:\n  idempotent: true\n  verify:\n    raw: 'SELECT 1'\n    poll: assert_one",
        )
        .expect("present");
        assert!(cp.idempotent);
        assert!(cp.hashed); // default
        let v = cp.verify.expect("verify body");
        assert_eq!(v.get("raw").and_then(|x| x.as_str()), Some("SELECT 1"));
        assert_eq!(v.get("poll").and_then(|x| x.as_str()), Some("assert_one"));
    }

    #[test]
    fn checkpoint_full_form_idempotent_false_equivalent_to_none() {
        let cp = parse_checkpoint_field("checkpoint:\n  idempotent: false\n  hashed: true")
            .expect("present");
        assert!(!cp.idempotent);
        assert!(cp.hashed);
    }

    #[test]
    fn checkpoint_unknown_short_form_errors() {
        // Should fail to parse — an unknown short string is a
        // workload bug, not silently treated as `none`.
        let yaml = "phases:\n  p:\n    checkpoint: maybe\n    ops:\n      - select 1;\n";
        let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let json: serde_json::Value = serde_json::to_value(&v).unwrap();
        let phases_obj = json.get("phases").and_then(|p| p.as_object()).unwrap();
        let phase = phases_obj.get("p").and_then(|p| p.as_object()).unwrap();
        let cp_val = phase.get("checkpoint").unwrap().clone();
        let err = serde_json::from_value::<crate::model::Checkpoint>(cp_val).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown short form"),
            "expected unknown-short-form error, got: {msg}"
        );
        assert!(
            msg.contains("'maybe'"),
            "expected the bad token in error, got: {msg}"
        );
    }

    #[test]
    fn checkpoint_unknown_key_errors() {
        let yaml = "phases:\n  p:\n    checkpoint:\n      idempotent: true\n      bogus: yes\n    ops:\n      - select 1;\n";
        let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let json: serde_json::Value = serde_json::to_value(&v).unwrap();
        let cp_val = json.pointer("/phases/p/checkpoint").unwrap().clone();
        let err = serde_json::from_value::<crate::model::Checkpoint>(cp_val).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown key 'bogus'"),
            "expected unknown-key error, got: {msg}"
        );
    }

    #[test]
    fn checkpoint_field_absent_yields_none() {
        let cp = parse_checkpoint_field("# no checkpoint declared\n");
        assert!(
            cp.is_none(),
            "absent declaration should yield None, not Default"
        );
    }

    /// SRD-44 validation: a phase declared `checkpoint:
    /// idempotent` inside a do_while loop must be rejected at
    /// workload-load time. The do-loop iterates the same phase
    /// identity multiple times, contradicting the per-execution
    /// unit checkpointing assumes.
    #[test]
    fn rejects_idempotent_phase_inside_do_while() {
        let yaml = r#"
scenarios:
  default:
    - do_while: "true"
      phases:
        - probe
phases:
  probe:
    checkpoint: idempotent
    cycles: 1
    ops:
      step:
        stmt: "probe"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("expected validation rejection");
        assert!(
            err.contains("checkpoint: idempotent"),
            "error should explain the rejection: {err}"
        );
        assert!(
            err.contains("'probe'"),
            "error should name the offending phase: {err}"
        );
        assert!(
            err.contains("do_while") || err.contains("do_until"),
            "error should mention the do-loop ancestor: {err}"
        );
    }

    /// `do_until` triggers the same rejection.
    #[test]
    fn rejects_idempotent_phase_inside_do_until() {
        let yaml = r#"
scenarios:
  default:
    - do_until: "false"
      phases:
        - probe
phases:
  probe:
    checkpoint: idempotent
    cycles: 1
    ops:
      step:
        stmt: "probe"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("expected validation rejection");
        assert!(err.contains("'probe'"));
    }

    /// A do_while'd phase WITHOUT checkpoint declaration is
    /// fine — only the combination with `checkpoint:
    /// idempotent` is rejected.
    #[test]
    fn allows_do_while_phase_without_checkpoint_declaration() {
        let yaml = r#"
scenarios:
  default:
    - do_while: "true"
      phases:
        - probe
phases:
  probe:
    cycles: 1
    ops:
      step:
        stmt: "probe"
"#;
        super::parse_workload(yaml, &HashMap::new()).expect("plain do_while phase should parse");
    }

    /// Idempotent phases NOT inside a do-loop are fine.
    #[test]
    fn allows_idempotent_phase_outside_do_loop() {
        let yaml = r#"
scenarios:
  default:
    - probe
phases:
  probe:
    checkpoint: idempotent
    cycles: 1
    ops:
      step:
        stmt: "probe"
"#;
        super::parse_workload(yaml, &HashMap::new())
            .expect("idempotent phase outside loop should parse");
    }

    /// Declarative `capture:` map block on an op produces
    /// CapturePoint entries with [`CapturePoint::path`] set.
    /// JSON-Pointer paths are not validated against the
    /// response shape (it's a runtime read), so the parser's
    /// only job is to forbid obviously-malformed inputs
    /// (non-string values, paths missing the leading `/`).
    #[test]
    fn parses_declarative_capture_block_with_json_pointer_paths() {
        let yaml = r#"
scenarios:
  default:
    - probe
phases:
  probe:
    cycles: 1
    ops:
      read_state:
        adapter: http
        method: POST
        uri: "http://h:8778/jolokia/"
        body: "[]"
        capture:
          sstables: "/0/value"
          active_count: "/1/value:count"
          pending_for_cf: "/2/value"
"#;
        let wl = super::parse_workload(yaml, &HashMap::new())
            .expect("workload with declarative captures should parse");
        let phase = wl.phases.get("probe").expect("probe phase");
        let op = phase
            .ops
            .iter()
            .find(|o| o.name == "read_state")
            .expect("read_state op");
        assert_eq!(
            op.captures.len(),
            3,
            "expected 3 declarative captures, got {:?}",
            op.captures
        );
        let by_name = |n: &str| {
            op.captures
                .iter()
                .find(|c| c.as_name == n)
                .unwrap_or_else(|| panic!("capture {n} missing"))
        };
        let sstables = by_name("sstables");
        assert_eq!(sstables.path.as_deref(), Some("/0/value"));
        assert!(!sstables.count);
        let active = by_name("active_count");
        assert_eq!(
            active.path.as_deref(),
            Some("/1/value"),
            ":count suffix should be stripped from stored path"
        );
        assert!(active.count, ":count suffix should set CapturePoint.count");
        let pending = by_name("pending_for_cf");
        assert_eq!(pending.path.as_deref(), Some("/2/value"));
        assert!(!pending.count);
    }

    /// SRD-75: phase-level `poll:` block parses into
    /// `WorkloadPhase.poll`. Distinct from the op-level
    /// `poll:` field which lives on a single op and routes
    /// through the `PollingDispenser` wrapper.
    #[test]
    fn parses_phase_level_poll_block() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      until: "sstables == 1 && active_for_cf == 0"
      interval_ms: 5000
      timeout_ms: 14400000
      max_error_retries: 3
      metric_name: ensure_wait_s
    ops:
      read_state:
        stmt: "noop"
"#;
        let wl =
            super::parse_workload(yaml, &HashMap::new()).expect("phase-poll block should parse");
        let phase = wl.phases.get("ensure").expect("ensure phase");
        let poll = phase.poll.as_ref().expect("phase.poll Some");
        assert_eq!(poll.until, "sstables == 1 && active_for_cf == 0");
        assert_eq!(poll.interval_ms.as_deref(), Some("5000"));
        assert_eq!(poll.timeout_ms.as_deref(), Some("14400000"));
        assert_eq!(poll.max_error_retries.as_deref(), Some("3"));
        assert_eq!(poll.metric_name.as_deref(), Some("ensure_wait_s"));
    }

    #[test]
    fn optimize_string_is_sugar_for_objective_block() {
        // SRD-86 — `optimize: <string>` is shorthand for
        // `optimize: { objective: <string> }` with every other field defaulted.
        let yaml = r#"
scenarios:
  default:
    - search
phases:
  search:
    cycles: 1
    for_each: "ef in 1.0 .. 5.0"
    optimize: |
      0 - (ef - 4) * (ef - 4)
    ops:
      probe:
        stmt: "probe ef={ef}"
"#;
        let wl = super::parse_workload(yaml, &HashMap::new())
            .expect("string-form optimize should parse");
        let opt = wl
            .phases
            .get("search")
            .and_then(|p| p.optimize.as_ref())
            .expect("optimize block present");
        // The whole string became the objective; the rest defaulted.
        assert_eq!(opt.objective.trim(), "0 - (ef - 4) * (ef - 4)");
        assert_eq!(opt.method, "sweep");
        assert!(opt.servo.is_empty());

        // ...and it is equivalent to the explicit map form.
        let map_yaml = yaml.replace(
            "    optimize: |\n      0 - (ef - 4) * (ef - 4)\n",
            "    optimize: { objective: \"0 - (ef - 4) * (ef - 4)\" }\n",
        );
        let wl2 = super::parse_workload(&map_yaml, &HashMap::new())
            .expect("map-form optimize should parse");
        let opt2 = wl2
            .phases
            .get("search")
            .and_then(|p| p.optimize.as_ref())
            .unwrap();
        assert_eq!(opt.objective.trim(), opt2.objective.trim());
        assert_eq!(opt.method, opt2.method);
    }

    /// SRD-75 §"Workload-load validation": phase-poll +
    /// concurrency > 1 is rejected at parse time. The
    /// predicate's evaluation depends on a serial sequence
    /// of capture writes; concurrent cycle execution has no
    /// well-defined semantic here.
    #[test]
    fn rejects_phase_poll_with_concurrency_gt_one() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    concurrency: 4
    poll:
      until: "done == 1"
    ops:
      op1:
        stmt: "noop"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("poll: + concurrency > 1 must error");
        assert!(
            err.contains("poll:") && err.contains("concurrency"),
            "expected error to name both poll: and concurrency; got: {err}"
        );
    }

    /// SRD-75 §"Workload-load validation": phase-poll with
    /// no ops is rejected — captures are produced by op
    /// execution; a poll-phase with no ops has nothing to
    /// drive the predicate.
    #[test]
    fn rejects_phase_poll_with_no_ops() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      until: "done == 1"
"#;
        let err =
            super::parse_workload(yaml, &HashMap::new()).expect_err("poll: without ops must error");
        assert!(
            err.contains("poll:") && err.contains("ops"),
            "expected error to name poll: and ops:; got: {err}"
        );
    }

    /// Unknown keys under `poll:` are typos waiting to
    /// silently default. Surface them at parse time.
    #[test]
    fn rejects_unknown_keys_under_phase_poll() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      until: "done == 1"
      tinerval_ms: 5000
    ops:
      op1:
        stmt: "noop"
"#;
        let err =
            super::parse_workload(yaml, &HashMap::new()).expect_err("typo under poll: must error");
        assert!(
            err.contains("tinerval_ms"),
            "expected error to name the offending key; got: {err}"
        );
    }

    /// SRD-75 §"on_timeout" — accepts the documented
    /// closed-vocabulary values and persists them on the
    /// parsed phase model. Both `error` and `abort` parse
    /// cleanly; case is normalised at parse time.
    #[test]
    fn parses_phase_poll_on_timeout_accepts_error_and_abort() {
        for (input, expected) in [
            ("error", "error"),
            ("abort", "abort"),
            ("ABORT", "abort"),
            ("Error", "error"),
        ] {
            let yaml = format!(
                r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      until: "done == 1"
      on_timeout: {input}
    ops:
      op1:
        stmt: "noop"
"#
            );
            let wl = super::parse_workload(&yaml, &HashMap::new())
                .expect("on_timeout value should parse");
            let phase = wl.phases.get("ensure").expect("ensure phase");
            let poll = phase.poll.as_ref().expect("phase.poll Some");
            assert_eq!(
                poll.on_timeout.as_deref(),
                Some(expected),
                "on_timeout '{input}' should normalise to '{expected}'"
            );
        }
    }

    /// SRD-75 §"on_timeout" — anything outside the
    /// closed vocabulary is rejected at parse time.
    /// Typos like `aborts:` or `fail:` should fail loudly,
    /// not silently default to `error` and obscure the
    /// workload-author's actual intent.
    #[test]
    fn rejects_phase_poll_unknown_on_timeout_value() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      until: "done == 1"
      on_timeout: fail
    ops:
      op1:
        stmt: "noop"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("unknown on_timeout value must error");
        assert!(
            err.contains("on_timeout") && err.contains("'fail'"),
            "expected error to name the offending value; got: {err}"
        );
    }

    /// `poll:` requires `until:` — the predicate is the
    /// loop's whole purpose.
    #[test]
    fn rejects_phase_poll_without_until() {
        let yaml = r#"
scenarios:
  default:
    - ensure
phases:
  ensure:
    cycles: 1
    poll:
      interval_ms: 5000
    ops:
      op1:
        stmt: "noop"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("poll: without until: must error");
        assert!(
            err.contains("until"),
            "expected error to require until:; got: {err}"
        );
    }

    /// SRD-83 Part 5 — the `effect:`/`action:` verb vocabulary is
    /// closed. An unknown verb used to fall silently to the shell
    /// default at the trip site; it is a load error now.
    #[test]
    fn rejects_unknown_stop_condition_effect() {
        let yaml = r#"
scenarios:
  default:
    - work
phases:
  work:
    cycles: 10
    stop_when:
      - when: "cycles_total > 5"
        effect: sotp
    ops:
      op1:
        stmt: "noop"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("unknown effect verb must be a load error");
        assert!(
            err.contains("sotp") && err.contains("stop|fail|abort"),
            "expected error to name the verb and the vocabulary; got: {err}"
        );
    }

    /// The three declared verbs — and the `action:` alias — all load.
    #[test]
    fn accepts_declared_stop_condition_effects() {
        for (key, verb) in [("effect", "stop"), ("effect", "fail"), ("action", "abort")] {
            let yaml = format!(
                r#"
scenarios:
  default:
    - work
phases:
  work:
    cycles: 10
    stop_when:
      - when: "cycles_total > 5"
        {key}: {verb}
    ops:
      op1:
        stmt: "noop"
"#
            );
            super::parse_workload(&yaml, &HashMap::new())
                .unwrap_or_else(|e| panic!("{key}: {verb} must load: {e}"));
        }
    }

    /// A path that contains `/` but doesn't begin with one is the
    /// ambiguous case: `0/value` is either a JSON-Pointer missing its
    /// leading slash (the classic transcription bug) or a member
    /// literally named "0/value" (which RFC 6901 spells `/0~1value`).
    /// Both readings are defensible, so the parser refuses rather than
    /// picking one — surfaced at parse time, not as silent
    /// capture-misses at runtime. Bare names WITHOUT a `/` are
    /// unambiguous and accepted; see the tests below.
    #[test]
    fn rejects_capture_path_without_leading_slash() {
        let yaml = r#"
scenarios:
  default:
    - probe
phases:
  probe:
    cycles: 1
    ops:
      read_state:
        adapter: http
        method: POST
        uri: "http://h:8778/"
        body: "[]"
        capture:
          bad: "0/value"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("path without leading / must error");
        assert!(
            err.contains("`capture.bad`") && err.contains("'/'"),
            "expected parse error to name the offending capture and require '/'; got: {err}"
        );
    }

    /// A bare top-level member name means exactly `/name`, and is
    /// normalized to it at parse time so runtime resolves one form. This
    /// is the spelling `verify:`'s `field:` uses, so an author moving
    /// between the two blocks on one op writes the same thing twice.
    #[test]
    fn accepts_bare_member_name_as_capture_path() {
        let yaml = r#"
scenarios:
  default:
    - probe
phases:
  probe:
    cycles: 1
    ops:
      read_state:
        adapter: http
        method: GET
        uri: "http://h:8778/"
        capture:
          bare: "value"
          pointer: "/value"
          rooted: ""
"#;
        let wl = super::parse_workload(yaml, &HashMap::new()).expect("both spellings must parse");
        let op = wl
            .phases
            .get("probe")
            .expect("phase")
            .ops
            .iter()
            .find(|o| o.name == "read_state")
            .expect("op");
        let path_of = |wire: &str| {
            op.captures
                .iter()
                .find(|c| c.as_name == wire)
                .unwrap_or_else(|| panic!("capture {wire} missing"))
                .path
                .clone()
                .expect("declarative capture carries a path")
        };
        assert_eq!(
            path_of("bare"),
            "/value",
            "a bare name normalizes to the equivalent pointer"
        );
        assert_eq!(
            path_of("pointer"),
            "/value",
            "and is indistinguishable from the pointer spelling downstream"
        );
        assert_eq!(
            path_of("rooted"),
            "",
            "empty still addresses the root document"
        );
    }

    /// `~` is a JSON-Pointer escape introducer (`~0` = `~`, `~1` = `/`), so
    /// a bare name containing one is ambiguous for the same reason `/` is.
    #[test]
    fn rejects_bare_name_containing_pointer_escape_char() {
        let yaml = r#"
scenarios:
  default:
    - probe
phases:
  probe:
    cycles: 1
    ops:
      read_state:
        adapter: http
        method: GET
        uri: "http://h:8778/"
        capture:
          bad: "a~1b"
"#;
        let err = super::parse_workload(yaml, &HashMap::new())
            .expect_err("a bare name with '~' must error");
        assert!(
            err.contains("`capture.bad`") && err.contains("ambiguous"),
            "expected an ambiguity error naming the capture; got: {err}"
        );
    }

    #[test]
    fn set_value_bare_is_reference() {
        // A bare `set:` value is a wire REFERENCE (consistent with
        // comprehension r-values); a string literal is the explicit
        // polydat-quoted form; a number/bool is a literal; a sequence is a list.
        fn set_source(pair: &str) -> String {
            let yaml = format!(
                "scenarios:\n  s:\n    - set: {{ {pair} }}\n      \
                 phases:\n        - p\nops:\n  p: \"test\"\n"
            );
            let wl = super::parse_workload(&yaml, &HashMap::new()).expect("parse");
            for n in &wl.scenarios["s"] {
                if let ScenarioNode::Bindings { source, .. } = n {
                    return source.trim().to_string();
                }
            }
            panic!("no Bindings node for `set: {{ {pair} }}`");
        }
        // bare identifier → wire reference (unquoted)
        assert_eq!(set_source("x: mnc"), "const x := mnc");
        assert_eq!(set_source("x: verbose"), "const x := verbose");
        // explicit polydat-quoted form → a string literal
        assert_eq!(set_source(r#"x: '"verbose"'"#), r#"const x := "verbose""#);
        // a YAML sequence → a list literal (of references)
        assert_eq!(set_source("x: [a, b]"), "const x := [a, b]");
        // numbers/bools are literals (serde distinguishes 8 from "8")
        assert_eq!(set_source("x: 8"), "const x := 8");
        assert_eq!(set_source(r#"x: "8""#), r#"const x := "8""#);
        assert_eq!(set_source("x: true"), "const x := true");
    }
}
