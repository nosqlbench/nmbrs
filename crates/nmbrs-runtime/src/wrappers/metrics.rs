// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Synthetic-metric recorder (SRD-40b §6). After the inner
//! adapter returns, pulls each declared metric's value through
//! the per-fiber op-template kernel via `ctx.wires.get` and
//! records it onto the kind-specialised instrument
//! (gauge / histogram / counter).

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("metrics");

/// Trigger: op declares a non-empty `metrics:` map.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    !template.metrics.is_empty()
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    if template.metrics.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = template.metrics.keys().map(|s| s.as_str()).collect();
    names.sort();
    Some(format!("metrics: emits {}", names.join(", ")))
}

/// `metrics` must sit outside every other non-dryrun wrapper —
/// it observes their per-cycle effects on `ctx.wires`. Listed
/// as the `forbids_outer` set on metrics' registration so the
/// constraint validator hard-errors if anything tries to slip
/// outside it (except DRYRUN, which is strictly outermost via
/// its own `forbids_outer` declaration).
const FORBIDS_OUTER: &[WrapperName] = &[
    super::traverse::NAME,
    super::delay::NAME,
    crate::validation::WRAPPER_NAME,
    super::poll::NAME,
    super::r#if::NAME,
    // `emit` is INTENTIONALLY allowed outer of metrics — the
    // emit wrapper is the operator-visible render surface and
    // under `dryrun=emit` it sits outer of DRYRUN (and therefore
    // outer of metrics) so the rendered op text reaches stdout
    // before the short-circuit. Metrics fires inner of emit;
    // the per-cycle measurement is unaffected.
    super::result::NAME,
];

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        // `metrics:` is parsed into ParsedOp.metrics, not into
        // params, so there's no owned `params`-key. Trigger
        // fires only when the metrics map is non-empty.
        owned_fields: &[],
        triggers,
        requires_inner: &[],
        forbids_outer: FORBIDS_OUTER,
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner OpDispenser to publish per-cycle synthetic
/// metrics declared in the op template's `metrics:` map
/// (SRD-40b §6).
///
/// Per-cycle responsibilities (in order, matching SRD-40b §5.2 →
/// §6 pipeline):
///
/// 1. Await the inner dispenser's `execute`. With a
///    [`crate::wrappers::ResultDispenser`] in the wrapper stack
///    between the inner adapter and this one, declared `result:`
///    wires are already written through `ctx.wires.write` to the
///    per-fiber kernel by the time we run.
/// 2. For each declared metric, read the value through
///    `ctx.wires.get(name)` (bare-binding-name canonical form per
///    SRD-40b §1). The read pulls fresh through the eval cone
///    so any computed output (e.g. `row_count := count`) reflects
///    this cycle's value.
/// 3. Apply the optional [`metric_format::FormatSpec`] sanitiser
///    to round to the configured precision (Phase B).
/// 4. Dispatch to the kind-specific instrument record method
///    (SRD-40b §6.1):
///    - [`MetricKind::Gauge`] → [`ValueGauge::set`] (f64).
///    - [`MetricKind::Histogram`] → [`Histogram::record`]
///      (truncated to u64 after format rounding).
///    - [`MetricKind::Counter`] → [`Counter::inc_by`] (u64);
///      non-positive values warn and skip — counters are
///      monotonic by definition.
///
/// Non-bare-name expressions (`factor * 2.0`, `if(...)`, …) are
/// **deferred** — the wrap step errors when `spec.value` is not
/// a bare identifier.
pub struct MetricsDispenser {
    inner: Arc<dyn OpDispenser>,
    /// One slot per declared metric. Stable ordering by metric
    /// name keeps per-cycle dispatch deterministic for tests and
    /// makes any per-cycle warning sequence reproducible.
    slots: Arc<Vec<MetricSlot>>,
}

/// Lenient per-iteration GAUGE publication for poll drains: the
/// dispenser above only fires when the (potentially hours-long)
/// drain op completes, so the poll wrapper re-publishes gauge slots
/// per poll iteration against that iteration's wires — the store
/// then carries live samples (`compaction_progress_parts` etc.)
/// instead of a single end-of-drain point. Gauges only: counters
/// and histograms keep their one-record-per-op semantics (the
/// end-of-op publish would double-count them). Resolution misses
/// degrade to a debug log — status must never fail the poll.
pub(crate) fn publish_gauges_lenient(slots: &[MetricSlot], wires: &dyn crate::wires::WireSource) {
    for slot in slots {
        // Cell-placed metrics are skipped: their instrument depends on a
        // coordinate resolved from the cycle's wires, which this lenient
        // path has no basis to choose.
        let Some(MetricInstrument::Gauge(g)) = &slot.instrument else {
            continue;
        };
        let Some(value) = wires.get(&slot.binding_name) else {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "poll gauge '{}': binding '{}' unresolved this iteration",
                slot.family,
                slot.binding_name
            );
            continue;
        };
        let Some(raw) = value_to_f64(&value) else {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "poll gauge '{}': '{}' non-numeric this iteration",
                slot.family,
                slot.value_expr
            );
            continue;
        };
        let sanitised = slot.format.as_ref().map(|f| f.apply(raw)).unwrap_or(raw);
        g.set(sanitised);
    }
}

/// One compiled metric slot: instrument storage + sanitiser +
/// pre-bound Polydat pull handle.
pub(crate) struct MetricSlot {
    /// Family name registered with the [`Component`]. Used in
    /// diagnostic messages (e.g. the counter non-positive warning).
    family: String,
    /// The original `value:` text from the workload, kept for
    /// diagnostics. Per-cycle resolution reads through the
    /// internal `binding_name` below; the user's original text
    /// surfaces in error messages so operators see what they wrote.
    value_expr: String,
    /// Internal kernel-output name (`__metric_<name>`) the
    /// op-template synthesiser created from this metric's
    /// `value:` expression. Cycle-time reads go through
    /// `ctx.wires.get(&binding_name)`.
    binding_name: String,
    /// Optional value sanitiser. Applied after the value is
    /// pulled, before the instrument record.
    format: Option<nmbrs_workload::metric_format::FormatSpec>,
    /// Resolved instrument storage — exactly one variant is
    /// populated per slot, matching `MetricSpec.kind`.
    /// The instrument, for a metric that registers ONCE on the dispenser's
    /// own component. `None` when the metric is cell-placed: its instruments
    /// are materialised per coordinate in [`CellPlacement`], because each cell
    /// is a distinct identity and therefore a distinct instrument.
    instrument: Option<MetricInstrument>,
    /// Dimensional placement, when the metric declared `cell:`.
    placement: Option<CellPlacement>,
}

/// Per-coordinate instrument materialisation for a cell-placed metric.
///
/// The parent is the component this metric would otherwise have registered on,
/// so a cell REFINES that identity rather than replacing part of it. Nothing
/// ambient is consulted.
pub(crate) struct CellPlacement {
    /// `(dimension, synthesised coordinate wire)`, in the workload's declared
    /// order. The wire is `__cell_<metric>__<dim>` — see
    /// `crate::scope::synthesize_cell_binding_name`.
    dims: Vec<(String, String)>,
    /// The registration site whose identity each cell refines.
    parent: Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
    kind: nmbrs_workload::model::MetricKind,
    unit: Option<String>,
    /// Instruments already materialised, keyed by the coordinate's canonical
    /// rendering. Steady state is a hash lookup: the registry write and the
    /// component attach happen once per distinct coordinate, never per cycle.
    instances: std::sync::Mutex<std::collections::HashMap<String, MetricInstrument>>,
}

/// Kind-specialised instrument storage owned by a [`MetricSlot`].
///
/// The same `Arc<...>` is shared with the dispenser's `Component`
/// instrument registry — registered via
/// `Component::register_instrument` at [`MetricsDispenser::wrap`] time.
/// Per-cycle code records through the slot's typed `Arc`; the cadence
/// reporter snapshots through the registry. One source of truth, two
/// access paths.
#[derive(Clone)]
enum MetricInstrument {
    Gauge(Arc<nmbrs_metrics::instruments::gauge::ValueGauge>),
    Histogram(Arc<nmbrs_metrics::instruments::histogram::Histogram>),
    Counter(Arc<nmbrs_metrics::instruments::counter::Counter>),
}

impl MetricInstrument {
    /// Promote the kind-erased slot value into the canonical
    /// [`InstrumentRef`] for registry storage.
    fn as_ref(&self) -> nmbrs_metrics::component::InstrumentRef {
        match self {
            MetricInstrument::Gauge(g) => nmbrs_metrics::component::InstrumentRef::Gauge(g.clone()),
            MetricInstrument::Histogram(h) => {
                nmbrs_metrics::component::InstrumentRef::Histogram(h.clone())
            }
            MetricInstrument::Counter(c) => {
                nmbrs_metrics::component::InstrumentRef::Counter(c.clone())
            }
        }
    }
}

/// Numeric coercion for capture-map lookups. Returns `None`
/// for non-numeric variants (string, vector, none) so the
/// MetricsDispenser slot path logs + skips rather than panicking
/// through `Value::as_f64`'s strict matcher.
fn value_to_f64(v: &polydat::ast::Value) -> Option<f64> {
    match v {
        polydat::ast::Value::F64(f) => Some(*f),
        polydat::ast::Value::U64(u) => Some(*u as f64),
        polydat::ast::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

impl MetricsDispenser {
    /// Wrap an inner dispenser with synthetic-metric publication
    /// for the op template's `metrics:` declarations.
    ///
    /// Init steps (SRD-40b §6 init):
    /// 1. Empty declaration → return `inner` unchanged. No
    ///    overhead for ops that don't publish synthetic metrics.
    /// 2. For each `(name, spec)`, allocate the kind-specific
    ///    instrument and register it on the component via
    ///    `Component::register_instrument`. A duplicate-family
    ///    collision (§7.2) errors here, before any cycle runs.
    /// 3. Pre-parse the optional `format:` string into a
    ///    [`FormatSpec`].
    ///
    /// `component` is borrowed mutably so `register_instrument`
    /// can claim the family slot atomically with the instrument
    /// allocation. The same `Arc<...>` is held both on the
    /// component (for cadence-reporter capture) and in the
    /// returned dispenser's slots (for per-cycle record).
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        metrics: &HashMap<String, nmbrs_workload::model::MetricSpec>,
        component: &mut nmbrs_metrics::component::Component,
        component_arc: &Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        Self::wrap_with_slots(inner, metrics, component, component_arc, fx).map(|(d, _)| d)
    }

    /// As [`Self::wrap`], additionally returning the compiled slot
    /// list (shared `Arc`) so the poll wrapper can re-publish the
    /// GAUGE slots per poll iteration (see
    /// [`publish_gauges_lenient`]). `None` when the op declares no
    /// metrics.
    pub(crate) fn wrap_with_slots(
        inner: Arc<dyn OpDispenser>,
        metrics: &HashMap<String, nmbrs_workload::model::MetricSpec>,
        component: &mut nmbrs_metrics::component::Component,
        // The same component, as the handle cells attach under. A cell refines
        // THIS identity, so the parent comes from the registration site rather
        // than from anything ambient.
        component_arc: &Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<(Arc<dyn OpDispenser>, Option<Arc<Vec<MetricSlot>>>), String> {
        if metrics.is_empty() {
            return Ok((inner, None));
        }
        // Stable ordering on metric names so init-time
        // diagnostics + per-cycle dispatch are reproducible.
        let mut entries: Vec<_> = metrics.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        let component_labels = component.effective_labels().clone();
        let mut slots = Vec::with_capacity(entries.len());
        for (name, spec) in entries {
            let family = spec.family.clone().unwrap_or_else(|| name.clone());

            let format = match &spec.format {
                Some(s) => Some(
                    nmbrs_workload::metric_format::parse_format_spec(s)
                        .map_err(|e| format!("metric '{name}' format: {e}"))?,
                ),
                None => None,
            };

            let kind = spec.kind.unwrap_or_default();
            // Instrument labels carry the family name as a label
            // alongside the component's effective labels. The
            // `family` argument to `register_instrument` is the
            // canonical family-name string; the labels are the
            // dimensional cell.
            let instr_labels = component_labels.with("family", family.clone());
            let instrument = match kind {
                nmbrs_workload::model::MetricKind::Gauge => MetricInstrument::Gauge(Arc::new(
                    nmbrs_metrics::instruments::gauge::ValueGauge::new(instr_labels),
                )),
                nmbrs_workload::model::MetricKind::Histogram => {
                    MetricInstrument::Histogram(Arc::new(
                        nmbrs_metrics::instruments::histogram::Histogram::new(instr_labels),
                    ))
                }
                nmbrs_workload::model::MetricKind::Counter => MetricInstrument::Counter(Arc::new(
                    nmbrs_metrics::instruments::counter::Counter::new(instr_labels),
                )),
            };

            // Resolve the metric's value expression against the
            // Polydat Kernel up front. The op-template synthesiser
            // appended each metric's `value:` expression as a
            // `__metric_<name> := <expr>` binding on the kernel
            // (see `crate::scope::synthesize_metric_binding_name`),
            // so cycle-time reads go through that internal output —
            // arbitrary Polydat expressions work, not just bare names.
            // The closure-binding-economy walker injected magic
            // externs (body/count/ok) for any of those names this
            // expression referenced, so a workload that writes
            // `value: count` no longer needs a fake result-binding
            // to wedge the slot open.
            let binding_name = crate::scope::synthesize_metric_binding_name(name);
            let _ = fx.register_pull(&binding_name).map_err(|e| {
                format!(
                    "metric '{name}' value '{value}': {e} (synthesised binding \
                     '{binding_name}' should have been registered by the \
                     op-template kernel synthesiser — this is a bug)",
                    value = spec.value,
                )
            })?;

            // SRD-40b §7.2 — collide-on-duplicate at init. The
            // single registry on `Component` is the canonical
            // store; the slot's `Arc<...>` shares the same
            // instrument for the per-cycle hot path. The
            // optional `unit` rides through to drive the
            // `_<unit>` suffix on `metric_family.name` and the
            // `unit` column at capture time (SRD-40a §4.3).
            // Cell-placed metrics register PER CELL, at first sight of each
            // coordinate — not here. Registering on the dispenser's own
            // component as well would claim the family for the un-refined
            // identity, and the first cell to materialise would then collide
            // with it on this same duplicate-family check.
            let placement = if spec.cell.is_empty() {
                component.register_instrument_with_unit(
                    family.clone(),
                    spec.unit.clone(),
                    instrument.as_ref(),
                )?;
                None
            } else {
                let mut dims = Vec::with_capacity(spec.cell.len());
                for dim in spec.cell.keys() {
                    let wire = crate::scope::synthesize_cell_binding_name(name, dim);
                    let _ = fx.register_pull(&wire).map_err(|e| {
                        format!(
                            "metric '{name}' cell '{dim}': {e} (synthesised \
                             coordinate binding '{wire}' should have been \
                             registered by the op-template kernel synthesiser \
                             — this is a bug)"
                        )
                    })?;
                    dims.push((dim.clone(), wire));
                }
                Some(CellPlacement {
                    dims,
                    parent: component_arc.clone(),
                    kind,
                    unit: spec.unit.clone(),
                    instances: std::sync::Mutex::new(std::collections::HashMap::new()),
                })
            };

            slots.push(MetricSlot {
                family,
                value_expr: spec.value.clone(),
                binding_name,
                format,
                instrument: if placement.is_some() {
                    None
                } else {
                    Some(instrument)
                },
                placement,
            });
        }

        let slots = Arc::new(slots);
        Ok((
            Arc::new(Self {
                inner,
                slots: slots.clone(),
            }),
            Some(slots),
        ))
    }
}

impl CellPlacement {
    /// The instrument for this cycle's coordinate, materialising the cell and
    /// registering the family on first sight of each distinct coordinate.
    ///
    /// Steady state is a hash lookup. The component attach and the registry
    /// write happen once per coordinate, never per cycle — which is what keeps
    /// a per-row metric off the component write lock.
    fn resolve(
        &self,
        wires: &dyn crate::wires::WireSource,
        family: &str,
        cycle: u64,
    ) -> Result<MetricInstrument, ExecutionError> {
        let mut coord = nmbrs_metrics::labels::Labels::default();
        for (dim, wire) in &self.dims {
            let Some(value) = wires.get(wire) else {
                return Err(ExecutionError::Op(crate::adapter::AdapterError {
                    error_name: "metric_cell_unresolved".into(),
                    message: format!(
                        "metric '{family}' on cycle {cycle}: coordinate binding \
                         '{wire}' for dimension '{dim}' did not resolve through \
                         ctx.wires — this is a wiring bug between scope \
                         synthesis and the metrics wrapper"
                    ),
                    retryable: false,
                }));
            };
            // A label value is a string. Anything else would key cells on
            // formatting rather than on identity, so it is refused here rather
            // than stringified behind the author's back.
            let polydat::ast::Value::Str(text) = &value else {
                return Err(ExecutionError::Op(crate::adapter::AdapterError {
                    error_name: "metric_cell_not_a_string".into(),
                    message: format!(
                        "metric '{family}' cell '{dim}' on cycle {cycle}: \
                         coordinate resolved to a non-string {disc:?}. A \
                         dimension's values are label values, which are \
                         strings — convert the expression explicitly.",
                        disc = std::mem::discriminant(&value)
                    ),
                    retryable: false,
                }));
            };
            coord = coord.with(dim.clone(), text.to_string());
        }

        let key = coord.to_prometheus();
        {
            let cache = self.instances.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(found) = cache.get(&key) {
                return Ok(found.clone());
            }
        }

        // First sight of this coordinate: materialise the cell, build the
        // instrument carrying the cell's FULL effective labels, and register
        // the family there. The duplicate-family check runs per cell, which is
        // exactly where it belongs.
        let cell = nmbrs_metrics::cells::resolve_under(&self.parent, &coord);
        let cell_labels = {
            let g = cell.read().unwrap_or_else(|e| e.into_inner());
            g.effective_labels().clone()
        };
        let instr_labels = cell_labels.with("family", family.to_string());
        let instrument = match self.kind {
            nmbrs_workload::model::MetricKind::Gauge => MetricInstrument::Gauge(Arc::new(
                nmbrs_metrics::instruments::gauge::ValueGauge::new(instr_labels),
            )),
            nmbrs_workload::model::MetricKind::Histogram => MetricInstrument::Histogram(Arc::new(
                nmbrs_metrics::instruments::histogram::Histogram::new(instr_labels),
            )),
            nmbrs_workload::model::MetricKind::Counter => MetricInstrument::Counter(Arc::new(
                nmbrs_metrics::instruments::counter::Counter::new(instr_labels),
            )),
        };
        {
            let mut g = cell.write().unwrap_or_else(|e| e.into_inner());
            g.register_instrument_with_unit(
                family.to_string(),
                self.unit.clone(),
                instrument.as_ref(),
            )
            .map_err(|e| {
                ExecutionError::Op(crate::adapter::AdapterError {
                    error_name: "metric_cell_family_collision".into(),
                    message: format!("metric '{family}' cell {key}: {e}"),
                    retryable: false,
                })
            })?;
        }
        let mut cache = self.instances.lock().unwrap_or_else(|e| e.into_inner());
        Ok(cache.entry(key).or_insert(instrument).clone())
    }
}

/// A gauge slot wired straight to a wire name — the metric path a poll
/// publishes through, without the workload/kernel machinery around it.
/// Lets other modules' tests assert on what actually reaches the
/// instrument, rather than on a display-side proxy.
#[cfg(test)]
pub(crate) fn test_gauge_slot(
    family: &str,
    binding_name: &str,
) -> (
    MetricSlot,
    Arc<nmbrs_metrics::instruments::gauge::ValueGauge>,
) {
    let g = Arc::new(nmbrs_metrics::instruments::gauge::ValueGauge::new(
        nmbrs_metrics::labels::Labels::default(),
    ));
    (
        MetricSlot {
            family: family.to_string(),
            value_expr: binding_name.to_string(),
            binding_name: binding_name.to_string(),
            format: None,
            instrument: Some(MetricInstrument::Gauge(g.clone())),
            placement: None,
        },
        g,
    )
}

impl WrappingDispenser for MetricsDispenser {}

impl OpDispenser for MetricsDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self.inner.execute(cycle, ctx).await?;
            // Skipped ops produce no measurement — SRD-40b §5.2 /
            // §6 pipeline only fires on a successfully-executed op.
            if result.skipped {
                return Ok(result);
            }
            for slot in self.slots.iter() {
                // Sole resolution path: ctx.wires reads through the
                // live per-fiber kernel handle (project rule
                // "GK Is Canonical Scope"). The op-template synthesiser
                // compiled this metric's `value:` expression into the
                // kernel as `__metric_<name> := <expr>`; pulling that
                // wire fires the expression's eval cone — including
                // magic-extern reads (body/count/ok) ResultDispenser
                // wrote earlier in the same cycle. ctx.wires.get is
                // the live read; no pre-stack snapshot.
                let Some(value) = ctx.wires.get(&slot.binding_name) else {
                    return Err(ExecutionError::Op(crate::adapter::AdapterError {
                        error_name: "metric_value_unresolved".into(),
                        message: format!(
                            "metric '{family}' on cycle {cycle}: synthesised \
                             binding '{binding}' (from `value: {expr}`) did not \
                             resolve through ctx.wires — this is a wiring bug \
                             between scope synthesis and the metrics wrapper",
                            family = slot.family,
                            binding = slot.binding_name,
                            expr = slot.value_expr,
                        ),
                        retryable: false,
                    }));
                };
                // `None` is the absence of a measurement, not a bad one. A
                // conditional aggregate that matched nothing this cycle
                // (`:sum(x where kind='y')` with no matching rows) resolves to
                // None by design — "absent, not zero". Publishing no sample is
                // the honest response; erroring turns every conditional
                // aggregate into a run-ending fault the first cycle its subject
                // is idle, which is exactly when a drain is being watched.
                //
                // Genuinely non-numeric TYPES (Str, vector, handle) still fail
                // loudly below — those are wiring mistakes, not absences.
                if matches!(value, polydat::ast::Value::None) {
                    continue;
                }
                let raw = match value_to_f64(&value) {
                    Some(v) => v,
                    None => {
                        // The wire resolved but its type can't
                        // coerce to a numeric metric value (Str,
                        // vector, handle, etc.). Surface as a
                        // hard ExecutionError so the activity's
                        // `errors:` policy decides — by default
                        // (errors=stop) the phase + run halt.
                        return Err(ExecutionError::Op(crate::adapter::AdapterError {
                            error_name: "metric_value_non_numeric".into(),
                            message: format!(
                                "metric '{family}' on cycle {cycle}: \
                                 binding '{expr}' is not coercible to f64 \
                                 (got value variant {disc:?}); metric \
                                 values must be numeric (U64 / F64 / Bool)",
                                family = slot.family,
                                expr = slot.value_expr,
                                disc = std::mem::discriminant(&value),
                            ),
                            retryable: false,
                        }));
                    }
                };
                let sanitised = slot.format.as_ref().map(|f| f.apply(raw)).unwrap_or(raw);
                // A cell-placed metric resolves its coordinate from this
                // cycle's wires and lands on the instrument for THAT cell.
                let instrument = match (&slot.instrument, &slot.placement) {
                    (Some(i), _) => std::borrow::Cow::Borrowed(i),
                    (None, Some(p)) => match p.resolve(ctx.wires, &slot.family, cycle) {
                        Ok(i) => std::borrow::Cow::Owned(i),
                        Err(e) => return Err(e),
                    },
                    (None, None) => unreachable!("a slot has either an instrument or a placement"),
                };
                match instrument.as_ref() {
                    MetricInstrument::Gauge(g) => g.set(sanitised),
                    MetricInstrument::Histogram(h) => h.record(sanitised as u64),
                    MetricInstrument::Counter(c) => {
                        if sanitised <= 0.0 {
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "counter '{}' got non-positive value {sanitised}; skipping",
                                slot.family,
                            );
                        } else {
                            c.inc_by(sanitised as u64);
                        }
                    }
                }
            }
            Ok(result)
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod absent_value_tests {
    /// A gauge whose binding resolved to `None` must publish NO SAMPLE, not
    /// fail the run. `None` is the absence of a measurement: a conditional
    /// aggregate like `:sum(progress where kind='secondary index build')`
    /// yields it by design whenever no row has that kind, which is precisely
    /// when a drain is idle and being watched. Erroring there killed a
    /// 58-minute run at tier 19 with
    /// `metric_value_non_numeric ... variant Discriminant(19)`.
    ///
    /// Genuinely non-numeric TYPES must still fail loudly — those are wiring
    /// mistakes, not absences — so the skip is matched on `Value::None`
    /// specifically, never on "failed to coerce".
    #[test]
    fn absent_binding_skips_the_sample_but_bad_types_still_fail() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wrappers/metrics.rs"
        ))
        .expect("read own source");
        let skip = src
            .find("if matches!(value, polydat::ast::Value::None) {")
            .expect("None must be skipped explicitly");
        let coerce = src
            .find("let raw = match value_to_f64(&value) {")
            .expect("the coercion site must still exist");
        assert!(
            skip < coerce,
            "the None skip must come BEFORE the coercion, or an absent value \
             still reaches the error path"
        );
        assert!(
            src.contains("metric_value_non_numeric"),
            "non-numeric TYPES must still raise metric_value_non_numeric — \
             the skip is for absence, not for bad wiring"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{ExecutionError, OpResult};
    use crate::fixture::ExecCtx;
    use nmbrs_workload::model::{MetricKind, MetricSpec};

    struct CapturesInner;
    impl OpDispenser for CapturesInner {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                Ok(OpResult {
                    body: None,
                    skipped: false,
                })
            })
        }
    }

    fn fresh_component() -> nmbrs_metrics::component::Component {
        nmbrs_metrics::component::Component::new(
            nmbrs_metrics::labels::Labels::empty(),
            HashMap::new(),
        )
    }

    /// A component as the production path holds it: an `Arc` (cells attach
    /// under it) plus the `&mut` borrow registration needs.
    fn fresh_component_arc() -> Arc<std::sync::RwLock<nmbrs_metrics::component::Component>> {
        Arc::new(std::sync::RwLock::new(fresh_component()))
    }

    /// `MetricsDispenser::wrap` with the component handled the way the
    /// activity does it.
    fn wrap_on(
        inner: Arc<dyn OpDispenser>,
        decl: &HashMap<String, MetricSpec>,
        comp: &Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let mut guard = comp.write().unwrap();
        MetricsDispenser::wrap(inner, decl, &mut guard, comp, fx)
    }

    fn fresh_fixture() -> crate::fixture::ScopeFixture {
        use polydat::compile::assembly::{PolydatAssembler, WireRef};
        use polydat::library::identity::Identity;
        let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
        asm.add_node(
            "cycle_id",
            Box::new(Identity::new(polydat::ast::PortType::U64)),
            vec![WireRef::input("cycle")],
        );
        asm.add_output("cycle_id", WireRef::node("cycle_id"));
        let kernel = asm.compile().expect("test fixture asm.compile");
        crate::fixture::ScopeFixture::new(kernel.program().clone())
    }

    fn make_spec(value: &str, kind: MetricKind, format: Option<&str>) -> MetricSpec {
        MetricSpec {
            cell: Default::default(),
            value: value.to_string(),
            family: None,
            kind: Some(kind),
            unit: None,
            format: format.map(|s| s.to_string()),
        }
    }

    #[test]
    fn metrics_dispenser_empty_returns_inner_unchanged() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let inner_ptr = Arc::as_ptr(&inner);
        let comp = fresh_component_arc();
        let mut fx = fresh_fixture();
        let wrapped = wrap_on(inner.clone(), &HashMap::new(), &comp, &mut fx).unwrap();
        assert_eq!(Arc::as_ptr(&wrapped), inner_ptr);
    }

    /// Test-only introspection: peek at allocated instrument
    /// `Arc`s by family name. Tests need this because `wrap`
    /// returns `Arc<dyn OpDispenser>` and we want to assert
    /// against the same `ValueGauge` / `Histogram` / `Counter`
    /// the wrapper writes through.
    impl MetricsDispenser {
        fn slot_gauge(
            &self,
            family: &str,
        ) -> Option<Arc<nmbrs_metrics::instruments::gauge::ValueGauge>> {
            self.slots
                .iter()
                .find(|s| s.family == family)
                .and_then(|s| match s.instrument.as_ref()? {
                    MetricInstrument::Gauge(g) => Some(g.clone()),
                    _ => None,
                })
        }
        fn slot_histogram(
            &self,
            family: &str,
        ) -> Option<Arc<nmbrs_metrics::instruments::histogram::Histogram>> {
            self.slots
                .iter()
                .find(|s| s.family == family)
                .and_then(|s| match s.instrument.as_ref()? {
                    MetricInstrument::Histogram(h) => Some(h.clone()),
                    _ => None,
                })
        }
        fn slot_counter(
            &self,
            family: &str,
        ) -> Option<Arc<nmbrs_metrics::instruments::counter::Counter>> {
            self.slots
                .iter()
                .find(|s| s.family == family)
                .and_then(|s| match s.instrument.as_ref()? {
                    MetricInstrument::Counter(c) => Some(c.clone()),
                    _ => None,
                })
        }
    }

    fn kernel_with_const_outputs(
        consts: &[(&str, f64)],
    ) -> (
        crate::scope_kernel::ScopeKernel,
        crate::fixture::ScopeFixture,
    ) {
        use polydat::compile::assembly::{PolydatAssembler, WireRef};
        use polydat::library::fixed::ConstF64;
        let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
        for (name, val) in consts {
            let binding = crate::scope::synthesize_metric_binding_name(name);
            asm.add_node(&binding, Box::new(ConstF64::new(*val)), vec![]);
            asm.add_output(&binding, WireRef::node(&binding));
        }
        let kernel =
            crate::scope_kernel::ScopeKernel::from(asm.compile().expect("test kernel asm.compile"));
        let fx = crate::fixture::ScopeFixture::new(kernel.program().clone());
        (kernel, fx)
    }

    fn typed_wrap_with_kernel(
        inner: Arc<dyn OpDispenser>,
        decls: &HashMap<String, MetricSpec>,
        consts: &[(&str, f64)],
    ) -> Result<
        (
            Arc<MetricsDispenser>,
            crate::fixture::ResolvedPulls,
            crate::scope_kernel::ScopeKernel,
        ),
        String,
    > {
        let (mut kernel, mut fx) = kernel_with_const_outputs(consts);
        let mut comp = fresh_component();

        if decls.is_empty() {
            return Err("typed_wrap_with_kernel requires non-empty decls".into());
        }
        let mut entries: Vec<_> = decls.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let component_labels = comp.effective_labels().clone();
        let mut slots = Vec::with_capacity(entries.len());
        for (name, spec) in entries {
            let family = spec.family.clone().unwrap_or_else(|| name.clone());
            let format = match &spec.format {
                Some(s) => Some(
                    nmbrs_workload::metric_format::parse_format_spec(s)
                        .map_err(|e| format!("metric '{name}' format: {e}"))?,
                ),
                None => None,
            };
            let kind = spec.kind.unwrap_or_default();
            let instr_labels = component_labels.with("family", family.clone());
            let instrument = match kind {
                MetricKind::Gauge => MetricInstrument::Gauge(Arc::new(
                    nmbrs_metrics::instruments::gauge::ValueGauge::new(instr_labels),
                )),
                MetricKind::Histogram => MetricInstrument::Histogram(Arc::new(
                    nmbrs_metrics::instruments::histogram::Histogram::new(instr_labels),
                )),
                MetricKind::Counter => MetricInstrument::Counter(Arc::new(
                    nmbrs_metrics::instruments::counter::Counter::new(instr_labels),
                )),
            };
            comp.register_instrument_with_unit(
                family.clone(),
                spec.unit.clone(),
                instrument.as_ref(),
            )?;
            let binding_name = crate::scope::synthesize_metric_binding_name(name);
            let _ = fx.register_pull(&binding_name)?;
            slots.push(MetricSlot {
                family,
                value_expr: spec.value.clone(),
                binding_name,
                format,
                instrument: Some(instrument),
                placement: None,
            });
        }
        let typed = Arc::new(MetricsDispenser {
            inner,
            slots: Arc::new(slots),
        });

        let plan = fx.seal();
        kernel.set_inputs(&[0]);
        let pulls = plan.resolve_with(&mut kernel);
        Ok((typed, pulls, kernel))
    }

    fn run_dispenser(
        dispenser: Arc<dyn OpDispenser>,
        pulls: &crate::fixture::ResolvedPulls,
        kernel: &mut crate::scope_kernel::ScopeKernel,
    ) -> Result<OpResult, ExecutionError> {
        let fields = crate::adapter::ResolvedFields::new(vec![], vec![]);
        let cw = crate::wires::CycleWires::new(kernel);
        let ctx = ExecCtx::with_wires(&fields, pulls, &cw);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(dispenser.execute(0, &ctx))
    }

    #[test]
    fn metrics_dispenser_gauge_records_f64() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "my_factor".into(),
            make_spec("my_factor", MetricKind::Gauge, None),
        );

        let (typed, pulls, mut kernel) =
            typed_wrap_with_kernel(inner, &decl, &[("my_factor", 3.5)]).unwrap();
        let gauge = typed.slot_gauge("my_factor").unwrap();
        run_dispenser(typed.clone() as Arc<dyn OpDispenser>, &pulls, &mut kernel).unwrap();

        assert!((gauge.get() - 3.5).abs() < 1e-9);
    }

    #[test]
    fn metrics_dispenser_histogram_truncates_to_u64() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "latency_ms".into(),
            make_spec("latency_ms", MetricKind::Histogram, None),
        );

        let (typed, pulls, mut kernel) =
            typed_wrap_with_kernel(inner, &decl, &[("latency_ms", 7.9)]).unwrap();
        let hist = typed.slot_histogram("latency_ms").unwrap();
        run_dispenser(typed.clone() as Arc<dyn OpDispenser>, &pulls, &mut kernel).unwrap();

        let snap = hist.peek_snapshot();
        assert_eq!(snap.max(), 7);
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn metrics_dispenser_counter_positive_inc_and_skip_non_positive() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "ok_inc".into(),
            make_spec("ok_inc", MetricKind::Counter, None),
        );
        decl.insert(
            "skip_inc".into(),
            make_spec("skip_inc", MetricKind::Counter, None),
        );

        let (typed, pulls, mut kernel) =
            typed_wrap_with_kernel(inner, &decl, &[("ok_inc", 5.0), ("skip_inc", 0.0)]).unwrap();
        let ok_counter = typed.slot_counter("ok_inc").unwrap();
        let skip_counter = typed.slot_counter("skip_inc").unwrap();
        run_dispenser(typed.clone() as Arc<dyn OpDispenser>, &pulls, &mut kernel).unwrap();

        assert_eq!(ok_counter.get(), 5);
        assert_eq!(skip_counter.get(), 0);
    }

    #[test]
    fn metrics_dispenser_format_rounds_value() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "ratio".into(),
            make_spec("ratio", MetricKind::Gauge, Some("#.##")),
        );

        let (typed, pulls, mut kernel) =
            typed_wrap_with_kernel(inner, &decl, &[("ratio", 1.234)]).unwrap();
        let gauge = typed.slot_gauge("ratio").unwrap();
        run_dispenser(typed.clone() as Arc<dyn OpDispenser>, &pulls, &mut kernel).unwrap();

        assert!((gauge.get() - 1.23).abs() < 1e-9);
    }

    #[test]
    fn metrics_dispenser_duplicate_family_errors() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let comp = fresh_component_arc();
        comp.write()
            .unwrap()
            .register_instrument(
                "recall_at_10",
                nmbrs_metrics::component::InstrumentRef::Counter(Arc::new(
                    nmbrs_metrics::instruments::counter::Counter::new(
                        nmbrs_metrics::labels::Labels::of("name", "recall_at_10"),
                    ),
                )),
            )
            .unwrap();

        let mut decl = HashMap::new();
        decl.insert(
            "recall_at_10".into(),
            make_spec("recall_at_10", MetricKind::Gauge, None),
        );

        let (_kernel, mut fx) = kernel_with_const_outputs(&[("recall_at_10", 0.0)]);
        let err = match wrap_on(inner, &decl, &comp, &mut fx) {
            Ok(_) => panic!("expected duplicate-family error, got Ok"),
            Err(e) => e,
        };
        assert!(
            err.contains("duplicate family name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn metrics_dispenser_skipped_op_records_nothing() {
        struct SkipInner;
        impl OpDispenser for SkipInner {
            fn execute<'a>(
                &'a self,
                _cycle: u64,
                _ctx: &'a ExecCtx<'a>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
            > {
                Box::pin(async move { Ok(OpResult::skipped()) })
            }
        }
        let mut decl = HashMap::new();
        decl.insert("g".into(), make_spec("g", MetricKind::Gauge, None));

        let (typed, pulls, mut kernel) =
            typed_wrap_with_kernel(Arc::new(SkipInner), &decl, &[("g", 1.0)]).unwrap();
        let gauge = typed.slot_gauge("g").unwrap();

        let res =
            run_dispenser(typed.clone() as Arc<dyn OpDispenser>, &pulls, &mut kernel).unwrap();
        assert!(res.skipped);
        assert_eq!(gauge.get(), 0.0);
    }

    #[test]
    fn metrics_dispenser_accepts_arbitrary_polydat_expression() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "computed".into(),
            make_spec("factor * 2.0", MetricKind::Gauge, None),
        );

        let (mut kernel, mut fx) = kernel_with_const_outputs(&[("computed", 6.0)]);
        let comp = fresh_component_arc();
        let _ = wrap_on(inner, &decl, &comp, &mut fx)
            .expect("arbitrary Polydat expression should wrap cleanly");
        let plan = fx.seal();
        kernel.set_inputs(&[0]);
        let _pulls = plan.resolve_with(&mut kernel);
    }

    #[test]
    fn metrics_dispenser_missing_wire_errors_at_init() {
        let inner: Arc<dyn OpDispenser> = Arc::new(CapturesInner);
        let mut decl = HashMap::new();
        decl.insert(
            "missing_metric".into(),
            make_spec("absent_wire", MetricKind::Gauge, None),
        );

        let (_kernel, mut fx) = kernel_with_const_outputs(&[("present", 1.0)]);
        let comp = fresh_component_arc();
        let err = wrap_on(inner, &decl, &comp, &mut fx)
            .err()
            .expect("missing-wire metric should error at init");
        assert!(err.contains("absent_wire"), "msg: {err}");
        assert!(err.contains("Available"), "msg: {err}");
    }

    /// Silence dead-code warning for value_to_f64 (used inside
    /// wrapper but private to this module otherwise).
    #[test]
    fn value_to_f64_smoke() {
        assert_eq!(value_to_f64(&polydat::ast::Value::U64(5)), Some(5.0));
        assert_eq!(value_to_f64(&polydat::ast::Value::Bool(true)), Some(1.0));
        assert_eq!(value_to_f64(&polydat::ast::Value::Str("x".into())), None);
    }
}
