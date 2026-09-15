// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-109 — report synthesis: the workload's structure IS the report
//! definition.
//!
//! When a workload carries no explicit `report:` block, this module
//! synthesizes one from the structural fixtures — `key_metrics`
//! designations on phases and `anchor` cues on scenario sweeps — and
//! the result is a `serde_json` mapping fed through the SAME
//! [`crate::report::parse_report`] entry the YAML path uses. That is
//! the affine contract made literal: everything synthesized is
//! expressible by hand in the SRD-46 grammar, and `nmbrs report synth`
//! dumps exactly what would be hand-written.
//!
//! Well-formedness is enforced here, at synthesis (= report) time:
//! a designated family that no op emits, and a designated phase whose
//! activations multiply through a NON-anchored sweep (a silent
//! flatten), are hard errors carrying the fix in the message. There
//! are no implied aggregates anywhere: designations carry their
//! aggregate by grammar (enforced at workload parse), the SRD-91
//! spine columns carry contract-level defaults declared in this
//! module, and everything else is an error.
//!
//! MVP scope decisions (documented, revisitable):
//! - The spine table rows are phases with activation cardinality 1
//!   (not under any sweep). View-attached phases live in their view's
//!   table; un-anchored looped phases WITHOUT designations are listed
//!   in a text note instead of silently aggregated.
//! - View tables carry designation columns only (no auto spine
//!   columns), keyed by the anchor sweep's coordinate label(s).
//! - The rollup window is a fixed generous lookback ([`WINDOW`]); the
//!   execution selection narrows actual sample ranges.

use std::collections::BTreeMap;

use crate::model::{KeyAgg, KeyMetric, ScenarioNode, Workload, WorkloadPhase};

/// Rollup lookback for every synthesized query. Must outlast any
/// session; the metricsql evaluation window (latest-sample span)
/// bounds what it actually reads.
const WINDOW: &str = "30d";

/// SRD-109 §"The spine contract" — the contract-level default
/// designations every phase carries through its SRD-91 outcome
/// instruments. Declared HERE (once) so the spine is not an implied
/// aggregate.
fn spine_contract() -> Vec<KeyMetric> {
    use KeyAgg::*;
    vec![
        KeyMetric {
            column: "count".into(),
            agg: Last,
            family: "result_success".into(),
        },
        KeyMetric {
            column: "failures".into(),
            agg: Last,
            family: "result_failure".into(),
        },
        KeyMetric {
            column: "wall".into(),
            agg: Span,
            family: String::new(),
        },
        KeyMetric {
            column: "p99".into(),
            agg: Max,
            family: "result_success_p99".into(),
        },
    ]
}

/// One view (anchored table) discovered in the scenario walk.
#[derive(Debug, Default)]
struct View {
    /// Coordinate labels of the anchoring sweep(s). All anchors
    /// sharing this view name must agree (SRD-109 §Anchors).
    coords: Vec<String>,
    /// Phases attached beneath this view, in first-seen order.
    phases: Vec<String>,
    /// Human renderings of the anchoring comprehension(s) — the
    /// definitions that generate this view's rows, for the
    /// per-figure explainer. Unique, first-seen order.
    defs: Vec<String>,
}

/// Where a phase's designations land.
#[derive(Debug, Clone, PartialEq)]
enum Attach {
    Spine,
    View(String),
    /// Beneath a non-anchored sweep (relative to its nearest anchor):
    /// designations here are the silent-flatten error; undesignated
    /// phases are merely noted.
    Flattened {
        via: String,
    },
}

struct Walk {
    views: BTreeMap<String, View>,
    /// phase name -> attachments (a phase may appear in several
    /// scenarios / positions; every distinct attachment is kept).
    attachments: BTreeMap<String, Vec<Attach>>,
    errors: Vec<String>,
}

impl Walk {
    fn walk_nodes(
        &mut self,
        nodes: &[ScenarioNode],
        anchor: Option<&str>,
        flattened_via: Option<&str>,
    ) {
        for node in nodes {
            match node {
                ScenarioNode::Phase(name) => {
                    let attach = match (flattened_via, anchor) {
                        (Some(via), _) => Attach::Flattened {
                            via: via.to_string(),
                        },
                        (None, Some(v)) => Attach::View(v.to_string()),
                        (None, None) => Attach::Spine,
                    };
                    let entry = self.attachments.entry(name.clone()).or_default();
                    if !entry.contains(&attach) {
                        entry.push(attach.clone());
                    }
                    if let Attach::View(v) = &attach {
                        let view = self
                            .views
                            .get_mut(v)
                            .expect("view registered before descent");
                        if !view.phases.contains(name) {
                            view.phases.push(name.clone());
                        }
                    }
                }
                ScenarioNode::Comprehension {
                    comprehension,
                    children,
                    anchor: node_anchor,
                    ..
                } => {
                    let coords = comprehension.coordinate_names();
                    match node_anchor {
                        Some(view_name) => {
                            let view = self.views.entry(view_name.clone()).or_default();
                            let def = describe_comprehension(comprehension);
                            if !view.defs.contains(&def) {
                                view.defs.push(def);
                            }
                            if view.coords.is_empty() {
                                view.coords = coords.clone();
                            } else if view.coords != coords {
                                self.errors.push(format!(
                                    "anchor view '{view_name}': coordinate label sets \
                                     disagree across anchors ({:?} vs {:?}) — views \
                                     sharing a name must share coordinates",
                                    view.coords, coords
                                ));
                            }
                            // An anchor RESETS the flatten state: rows exist
                            // at this altitude, so activations below are 1:1
                            // with rows until another sweep intervenes.
                            self.walk_nodes(children, Some(view_name), None);
                        }
                        None => {
                            // Non-anchored sweep: everything beneath
                            // multiplies relative to the current row space.
                            let via = format!("for {}", coords.join(","));
                            self.walk_nodes(children, anchor, Some(&via));
                        }
                    }
                }
                ScenarioNode::IncludedScenario { children, .. }
                | ScenarioNode::Bindings { children, .. }
                | ScenarioNode::DoWhile { children, .. }
                | ScenarioNode::DoUntil { children, .. } => {
                    // Transparent for attachment purposes (do_while /
                    // do_until are unbounded sweeps and SHOULD flatten,
                    // but none of our workloads designate beneath one
                    // yet — revisit with breakouts).
                    self.walk_nodes(children, anchor, flattened_via);
                }
            }
        }
    }
}

/// Families a phase is known to emit: declared phase/op `metrics:`
/// keys, the poll wrapper's `metric_name`, the SRD-91 outcome
/// instruments (with their stat suffixes), and the validation
/// observer's `recall_*` families for ANN ops with ground truth.
fn known_families(phase: &WorkloadPhase) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in &phase.metrics {
        out.push(name.clone());
    }
    for op in &phase.ops {
        for (name, _) in &op.metrics {
            out.push(name.clone());
        }
        // Op-level poll wrappers: `poll` is a RESERVED op key, so the
        // parser routes it into `op.params` (the runtime reads it from
        // there too — see scope.rs template_poll_until). Its
        // `metric_name` is a duration family the wrapper emits.
        for source in [&op.params, &op.op] {
            if let Some(m) = source
                .get("poll")
                .and_then(|v| v.as_object())
                .and_then(|p| p.get("metric_name"))
                .and_then(|v| v.as_str())
            {
                out.push(m.to_string());
            }
        }
    }
    if let Some(poll) = &phase.poll
        && let Some(m) = &poll.metric_name
    {
        out.push(m.clone());
    }
    const INSTRUMENTS: &[&str] = &[
        "result_success",
        "result_failure",
        "result_total",
        "attempt_total",
        "attempt_success",
        "attempt_failure",
        "result_bytes",
        "result_elements",
        "cycles_total",
        "cycles_servicetime",
    ];
    const SUFFIXES: &[&str] = &[
        "", "_p50", "_p75", "_p90", "_p95", "_p99", "_p999", "_mean", "_min", "_max", "_stddev",
        "_rate", "_count",
    ];
    for i in INSTRUMENTS {
        for sfx in SUFFIXES {
            out.push(format!("{i}{sfx}"));
        }
    }
    out
}

fn family_known(phase: &WorkloadPhase, family: &str) -> bool {
    family.starts_with("recall_") || known_families(phase).iter().any(|f| f == family)
}

/// Human form of a designation's aggregate — the legend spelling
/// (`avg(load_weight)`, `rate(result_success)`, `span()`).
fn agg_desc(km: &KeyMetric) -> String {
    use KeyAgg::*;
    match km.agg {
        Span => "span()".to_string(),
        Rate => format!("rate({})", km.family),
        Delta => format!("delta({})", km.family),
        _ => format!("{}({})", format!("{:?}", km.agg).to_lowercase(), km.family),
    }
}

/// Human rendering of a comprehension — the `var in source` form an
/// operator wrote, reconstructed from the AST for figure explainers.
fn describe_comprehension(c: &polydat::iteration::comprehension::Comprehension) -> String {
    use polydat::iteration::comprehension::Comprehension as C;
    match c {
        C::Clause { name, source } => format!("{name} in {}", describe_source(source)),
        C::Cartesian { children } => children
            .iter()
            .map(describe_comprehension)
            .collect::<Vec<_>>()
            .join(", "),
        C::Zip { children, .. } => format!(
            "zip({})",
            children
                .iter()
                .map(describe_comprehension)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        C::Union { children } => children
            .iter()
            .map(describe_comprehension)
            .collect::<Vec<_>>()
            .join(" | "),
        C::Filter { child, predicate } => {
            format!("{} if {predicate}", describe_comprehension(child))
        }
        C::Order {
            child,
            strategy,
            truncation,
        } => match truncation {
            Some(n) => format!(
                "{} ordered {strategy:?} take {n}",
                describe_comprehension(child)
            ),
            None => format!("{} ordered {strategy:?}", describe_comprehension(child)),
        },
    }
}

fn describe_source(s: &polydat::iteration::comprehension::Source) -> String {
    use polydat::iteration::comprehension::Source;
    use polydat::iteration::comprehension::source::LiteralValue;
    match s {
        Source::Literal { values } => values
            .iter()
            .map(|v| match v {
                LiteralValue::Int(i) => i.to_string(),
                LiteralValue::Float(f) => f.to_string(),
                LiteralValue::String(st) => st.clone(),
                LiteralValue::Bool(b) => b.to_string(),
                LiteralValue::Json(j) => j.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Source::IntRange { lo, hi, step } if *step == 1 => format!("{lo}..{hi}"),
        Source::IntRange { lo, hi, step } => format!("{lo}..{hi} step {step}"),
        Source::Generator { expr, .. } => expr.clone(),
        Source::WorkloadParamList { name, .. } => format!("{{{name}}}"),
        Source::ContinuousInterval { .. } => "<continuous interval>".to_string(),
        Source::Distribution { distribution, .. } => format!("{distribution:?}(…)"),
    }
}

/// The metricsql expression for one designation, scoped to `phase`
/// and grouped by `by_labels`. This is the ONE place the aggregate
/// vocabulary maps to query text — every mapping explicit, none
/// implied (SRD-109 §Well-formedness).
fn query_for(metric: &KeyMetric, phase: &str, by_labels: &str) -> String {
    use KeyAgg::*;
    let f = &metric.family;
    let sel = format!("{{phase=\"{phase}\"}}");
    // Time-derived designations ride the `_interval_ns` / `_rate`
    // virtual stats (per-sample active-window bookkeeping) rather
    // than first/last timestamp arithmetic: a phase shorter than
    // the durable write cadence leaves ONE sample behind, where
    // `tlast - tfirst` degenerates to zero but the sample's own
    // interval is exact.
    let span =
        format!("max(sum_over_time(result_success_interval_ns{sel}[{WINDOW}])) by ({by_labels})");
    match metric.agg {
        Min => format!("min(min_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Max => format!("max(max_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Avg => format!("avg(avg_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Last => format!("max(last_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        First => format!("min(first_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Median => format!("avg(median_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Stddev => format!("avg(stddev_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Sum => format!("sum(sum_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Count => format!("sum(count_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Span => span,
        Rate => format!("avg(avg_over_time({f}_rate{sel}[{WINDOW}])) by ({by_labels})"),
        Delta => format!(
            "max(last_over_time({f}{sel}[{WINDOW}])) by ({by_labels}) - \
             min(first_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"
        ),
    }
}

/// Synthesize the report mapping for a workload with no explicit
/// `report:` block. `Ok(None)` when the workload has an explicit
/// block (synthesis suppressed). Errors are the SRD-109
/// well-formedness contract.
pub fn synthesize(workload: &Workload) -> Result<Option<serde_json::Value>, String> {
    if !workload.report.groups.is_empty() {
        return Ok(None);
    }
    synthesize_forced(workload).map(Some)
}

/// [`synthesize`] without the explicit-block suppression: the section
/// synthesized from the structural fixtures IRRESPECTIVE of any
/// `report:` block. This is what `nmbrs report synth` dumps and what
/// `--synthesized` renders — the affine mirror is always inspectable,
/// even (especially) when a hand-tuned block has diverged from it.
pub fn synthesize_forced(workload: &Workload) -> Result<serde_json::Value, String> {
    synthesize_forced_for(workload, None)
}

/// [`synthesize_forced`] scoped to ONE scenario when the caller knows
/// which ran (a session records it): the walk visits only that
/// scenario, so views carry only phases reachable in the execution —
/// no structurally-empty column groups from sibling scenarios.
/// `None`, or a name the workload doesn't declare, falls back to the
/// all-scenarios union.
pub fn synthesize_forced_for(
    workload: &Workload,
    scenario: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut walk = Walk {
        views: BTreeMap::new(),
        attachments: BTreeMap::new(),
        errors: Vec::new(),
    };
    // Union across every scenario: anchors and attachments merge; a
    // phase reachable both anchored and un-anchored keeps both
    // attachments (it renders in both places). Visit order pins the
    // first-seen phase order inside each view — and with it the
    // table's column order — so iterate `default` first (the
    // workload's narrative order) and the rest by name, never the
    // scenarios HashMap's per-process ordering.
    let mut scenario_names: Vec<&String> = workload.scenarios.keys().collect();
    scenario_names.sort_by_key(|n| (*n != "default", (*n).clone()));
    if let Some(sel) = scenario
        && let Some(name) = scenario_names.iter().find(|n| n.as_str() == sel).copied()
    {
        scenario_names = vec![name];
    }
    for name in scenario_names {
        walk.walk_nodes(&workload.scenarios[name], None, None);
    }
    let Walk {
        views,
        attachments,
        mut errors,
        ..
    } = walk;

    // Well-formedness: designated families must be emitted; designated
    // phases must not silently flatten through a non-anchored sweep.
    for (phase_name, attaches) in &attachments {
        let Some(phase) = workload.phases.get(phase_name) else {
            continue;
        };
        for km in &phase.key_metrics {
            if km.agg != KeyAgg::Span && !family_known(phase, &km.family) {
                errors.push(format!(
                    "phase '{phase_name}' key_metrics.{}: family '{}' is not \
                     emitted by this phase (declared metrics, poll timers, \
                     SRD-91 instruments and their stat suffixes, recall_*)",
                    km.column, km.family
                ));
            }
        }
        if !phase.key_metrics.is_empty() {
            for a in attaches {
                if let Attach::Flattened { via } = a {
                    errors.push(format!(
                        "phase '{phase_name}' designates key metrics but its \
                         activations multiply through a non-anchored sweep \
                         ({via}): one table row would silently aggregate many \
                         activations. Anchor that sweep (`anchor: <view>`) or \
                         remove the designations. There are no implied \
                         aggregates."
                    ));
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "report synthesis: {} well-formedness error(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        ));
    }

    let mut groups = serde_json::Map::new();

    // ── Spine ────────────────────────────────────────────────────
    let spine_phases: Vec<&String> = attachments
        .iter()
        .filter(|(_, a)| a.contains(&Attach::Spine))
        .map(|(n, _)| n)
        .collect();
    let looped_unanchored: Vec<&String> = attachments
        .iter()
        .filter(|(_, a)| {
            a.iter().any(|x| matches!(x, Attach::Flattened { .. }))
                && !a
                    .iter()
                    .any(|x| matches!(x, Attach::View(_)) || *x == Attach::Spine)
        })
        .map(|(n, _)| n)
        .collect();
    let mut spine = String::new();
    spine.push_str(
        "text phases_intro as \"Workload phases — outcomes at a glance\":\n \
         One row per workload phase, keyed by the `phase` label. Column \
         headers carry each value's definition (aggregate over the \
         phase's samples). Each anchored sweep in the scenario renders \
         as its own view table below, one row per sweep iteration.\n",
    );
    if !looped_unanchored.is_empty() {
        spine.push_str(&format!(
            "text phases_unanchored as \"Not tabulated\":\n \
             Looped phases with no anchor and no designations — activations \
             would aggregate silently, so no rows are synthesized: {}.\n",
            looped_unanchored
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Restrict to spine phases explicitly so anchored phases don't
    // leak in through the label join. NO spine-eligible phases ⇒ no
    // spine table at all (an empty selector would match everything,
    // silently re-admitting exactly the phases the notes exclude).
    if !spine_phases.is_empty() {
        spine.push_str("table phases:\n  group_by: phase\n");
        spine.push_str("  label \"phases — one row per workload phase\"\n");
        let spine_sel = format!(
            "{{phase=~\"{}\"}}",
            spine_phases
                .iter()
                .map(|s| regex_escape(s))
                .collect::<Vec<_>>()
                .join("|")
        );
        for km in spine_contract() {
            let q = query_for_selector(&km, &spine_sel, "phase");
            spine.push_str(&format!("  query: {}: {}\n", km.column, q));
            spine.push_str(&format!("  header {}: {}\n", km.column, agg_desc(&km)));
        }
    }
    groups.insert("phases".to_string(), serde_json::Value::String(spine));

    // ── One group per view ──────────────────────────────────────
    for (view_name, view) in &views {
        let by = view.coords.join(",");
        // Each column's header carries its definition — the declared
        // aggregate and, when several phases contribute, the emitting
        // phase — so the rendered table explains itself with no
        // separate legend to cross-reference.
        let mut table = String::new();
        let mut seen_cols: Vec<String> = Vec::new();
        let mut phase_names: Vec<&str> = Vec::new();
        let contributing = view
            .phases
            .iter()
            .filter(|p| {
                workload
                    .phases
                    .get(p.as_str())
                    .is_some_and(|ph| !ph.key_metrics.is_empty())
            })
            .count();
        for phase_name in &view.phases {
            let Some(phase) = workload.phases.get(phase_name) else {
                continue;
            };
            if !phase.key_metrics.is_empty() {
                phase_names.push(phase_name);
            }
            for km in &phase.key_metrics {
                let col = if seen_cols.contains(&km.column) {
                    format!("{phase_name}_{}", km.column)
                } else {
                    km.column.clone()
                };
                seen_cols.push(col.clone());
                let q = query_for(km, phase_name, &by);
                table.push_str(&format!("  query: {col}: {q}\n"));
                let note = if contributing > 1 {
                    format!("{} @{phase_name}", agg_desc(km))
                } else {
                    agg_desc(km)
                };
                table.push_str(&format!("  header {col}: {note}\n"));
            }
        }
        // The table's own label carries the row semantics into the
        // rendered heading and the listing.
        let label = format!(
            "{view_name} — one row per {by} ({})",
            phase_names.join(", ")
        );
        // Direct explainer: where this figure's rows COME FROM — the
        // comprehension definition(s) that generate them — which
        // labels identify a row, and which phases feed the columns.
        let defs = view
            .defs
            .iter()
            .map(|d| format!("`for {d}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        let about = format!(
            "text {view_name}_about as \"{view_name} — one row per {by}\":\n \
             Rows are the iterations of {defs}; the {by} label(s) \
             identify each row within the workload. Columns aggregate \
             each iteration's activations of: {}. Headers carry each \
             column's definition{}.\n",
            phase_names.join(", "),
            if contributing > 1 {
                " and its @phase provenance"
            } else {
                ""
            }
        );
        let body =
            format!("{about}table {view_name}:\n  group_by: {by}\n  label \"{label}\"\n{table}");
        groups.insert(format!("view_{view_name}"), serde_json::Value::String(body));
    }

    Ok(serde_json::Value::Object(groups))
}

/// [`query_for`] variant taking a pre-built selector (the spine's
/// phase-set restriction) instead of a single phase.
fn query_for_selector(metric: &KeyMetric, sel: &str, by_labels: &str) -> String {
    use KeyAgg::*;
    let f = &metric.family;
    match metric.agg {
        Span => format!(
            "max(sum_over_time(result_success_interval_ns{sel}[{WINDOW}])) by ({by_labels})"
        ),
        Last => format!("max(last_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        Max => format!("max(max_over_time({f}{sel}[{WINDOW}])) by ({by_labels})"),
        _ => {
            // The spine contract only uses last/span/max today; route
            // anything else through the single-phase mapper's shapes.
            let km = KeyMetric {
                column: metric.column.clone(),
                agg: metric.agg,
                family: f.clone(),
            };
            let q = query_for(&km, "__sel__", by_labels);
            q.replace("{phase=\"__sel__\"}", sel)
        }
    }
}

/// Dump the synthesized section as a `report:` YAML block — the
/// affine round-trip surface (`nmbrs report synth`): copy, paste into
/// the workload, edit, and synthesis is suppressed in favor of the
/// hand-tuned version.
pub fn synthesize_yaml(workload: &Workload) -> Result<String, String> {
    let value = synthesize_forced(workload)?;
    let map = value.as_object().expect("synthesize emits a mapping");
    let mut out = String::from("report:\n");
    for (group, body) in map {
        out.push_str(&format!("  {group}: |\n"));
        for line in body.as_str().unwrap_or_default().lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    Ok(out)
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(yaml: &str) -> Workload {
        crate::parse::parse_workload(yaml, &std::collections::HashMap::new())
            .expect("workload parses")
    }

    const BASE: &str = r#"
params: { adapter: testkit }
phases:
  tick:
    key_metrics:
      rows: last(result_success)
      spd: rate(result_success)
    ops: { t: { stmt: "X" } }
  plain:
    ops: { t: { stmt: "Y" } }
"#;

    #[test]
    fn synthesizes_spine_and_anchored_view() {
        let y = format!(
            "{BASE}
scenarios:
  default:
    - plain
    - for: \"k in 1,2\"
      anchor: sweep
      phases: [tick]
"
        );
        let v = synthesize(&wl(&y))
            .expect("well-formed")
            .expect("synthesized");
        let m = v.as_object().unwrap();
        assert!(m.contains_key("phases"));
        let sweep = m.get("view_sweep").unwrap().as_str().unwrap();
        assert!(
            sweep.contains("group_by: k"),
            "anchor coordinate keys the view"
        );
        assert!(sweep.contains("last_over_time(result_success{phase=\"tick\"}"));
        assert!(
            sweep.contains("query: spd:"),
            "rate designation synthesized"
        );
        let spine = m.get("phases").unwrap().as_str().unwrap();
        assert!(spine.contains("plain"), "un-looped phase rows the spine");
        assert!(
            !spine.contains("|tick") && !spine.contains("\"tick"),
            "anchored phase stays out of the spine selector"
        );
        // Affinity: the synthesized mapping parses through the SAME
        // grammar entry the YAML path uses.
        crate::report::parse_report(&v).expect("synthesized section parses");
    }

    #[test]
    fn designated_phase_under_unanchored_sweep_is_an_error() {
        let y = format!(
            "{BASE}
scenarios:
  default:
    - for: \"k in 1,2\"
      phases: [tick]
"
        );
        let e = synthesize(&wl(&y)).unwrap_err();
        assert!(e.contains("non-anchored sweep"), "{e}");
        assert!(
            e.contains("no implied") || e.contains("Anchor that sweep"),
            "{e}"
        );
    }

    #[test]
    fn unknown_family_is_an_error_with_provenance() {
        let y = "
params: { adapter: testkit }
phases:
  tick:
    key_metrics: { bogus: avg(no_such_family) }
    ops: { t: { stmt: \"X\" } }
scenarios:
  default: [tick]
";
        let e = synthesize(&wl(y)).unwrap_err();
        assert!(e.contains("no_such_family") && e.contains("tick"), "{e}");
    }

    #[test]
    fn explicit_report_block_suppresses_synthesis() {
        let y = "
params: { adapter: testkit }
report:
  g: |
    text t as \"T\":
     body
phases:
  tick: { ops: { t: { stmt: \"X\" } } }
scenarios:
  default: [tick]
";
        assert!(synthesize(&wl(y)).expect("ok").is_none());
    }

    #[test]
    fn unqualified_designation_is_a_parse_error_with_vocabulary() {
        let y = "
params: { adapter: testkit }
phases:
  tick:
    key_metrics: { rows: result_success }
    ops: { t: { stmt: \"X\" } }
scenarios:
  default: [tick]
";
        let e = crate::parse::parse_workload(y, &std::collections::HashMap::new())
            .expect_err("must reject unqualified aggregate");
        assert!(
            e.contains("aggregate qualification required") && e.contains("min, max, avg"),
            "{e}"
        );
    }
}
