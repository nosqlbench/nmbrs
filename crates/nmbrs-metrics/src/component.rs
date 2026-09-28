// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Runtime component tree for metrics ownership and dimensional labels.
//!
//! Every Polydat context layer (session, scenario, phase, dispenser) is a
//! [`Component`] in a parent-child tree. Labels inherit downward —
//! a phase component's effective labels include all ancestor labels.
//! Properties walk upward — a child can query a prop set on any ancestor.
//!
//! ## Instrument ownership (consolidated 2026-05)
//!
//! Each component carries a single `Vec<RegisteredInstrument>` — the
//! canonical store for every instrument hung on the node. Per-cycle
//! callers (op-dispenser wrappers, the activity executor) hold typed
//! `Arc<...>` references captured at registration time and never
//! look up by family name on the hot path. The [`Component::find_instrument`]
//! linear scan exists for diagnostics / introspection only.
//!
//! Dynamic instruments whose existence isn't known at init —
//! per-error-type counters allocated on first sighting — register
//! through the [`DynamicCapture`] hook installed via
//! [`Component::set_dynamic_capture`]. Capture walks the registry
//! first, then invokes the dynamic hook (if any).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::instruments::counter::Counter;
use crate::instruments::gauge::ValueGauge;
use crate::instruments::histogram::Histogram;
use crate::instruments::timer::Timer;
use crate::labels::Labels;
use crate::snapshot::MetricSet;

/// Lifecycle state of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentState {
    /// Component is being initialized.
    Starting,
    /// Component is actively running. Instruments are captured.
    Running,
    /// Component is shutting down. Final flush pending.
    Stopping,
    /// Component is done. Instruments no longer captured.
    /// Cumulative view remains queryable in the store until detach.
    Stopped,
}

/// A typed instrument reference owned by a [`Component`].
///
/// One variant per kind matches the [`crate::snapshot::MetricType`]
/// axis (counter / gauge / histogram / timer). Capture dispatches
/// on the variant to call the right kind-specific snapshot method.
#[derive(Clone)]
pub enum InstrumentRef {
    Counter(Arc<Counter>),
    Gauge(Arc<ValueGauge>),
    Histogram(Arc<Histogram>),
    Timer(Arc<Timer>),
}

impl InstrumentRef {
    /// Labels recorded on the underlying instrument. The `name=...`
    /// pair (used by [`split_name_label`]) is preserved here so
    /// existing snapshots keep their shape.
    pub fn labels(&self) -> &Labels {
        match self {
            Self::Counter(c) => c.labels(),
            Self::Gauge(g) => g.labels(),
            Self::Histogram(h) => h.labels(),
            Self::Timer(t) => t.labels(),
        }
    }
}

/// One registry entry: the bare family name, optional OpenMetrics
/// unit, and the typed instrument.
///
/// `family` is the bare name as given to
/// [`Component::register_instrument`]. The `_<unit>` suffix per
/// SRD-40a §4.3 is applied at capture time (in
/// [`crate::snapshot::MetricSet::insert_metric_with_unit`]) so the
/// `metric_family.name` ends up suffixed and `metric_family.unit`
/// holds the unit. Unit `None` means the family is published as-is.
pub struct RegisteredInstrument {
    pub family: String,
    pub unit: Option<String>,
    pub instrument: InstrumentRef,
}

/// Hook for components that own a dynamically-extending set of
/// instruments — e.g. per-error-type counters allocated lazily.
///
/// The registry-side `Vec<RegisteredInstrument>` is the canonical
/// store for instruments known at init. Anything that needs to
/// register more instruments after `register_on` has run installs
/// a `DynamicCapture` via [`Component::set_dynamic_capture`]; the
/// component's capture path invokes it after walking the registry
/// so the dynamic samples ride the same cadence pipeline.
pub trait DynamicCapture: Send + Sync {
    /// Append the dynamic instruments' current samples into `out`.
    /// `drain` mirrors the registry walk: `true` for the cadence
    /// reporter's per-tick path (drain histograms, etc.); `false`
    /// for the non-mutating "current" path.
    fn capture_into(&self, out: &mut MetricSet, now: Instant, drain: bool);
}

/// A node in the runtime component tree.
///
/// Components form a hierarchy: Session → Scenario → Phase → Dispenser.
/// Each component carries its own labels, inheritable properties, and
/// its own instrument registry.
pub struct Component {
    /// This component's own labels (e.g., `phase="rampup"`).
    labels: Labels,
    /// Effective labels = all ancestor labels merged with own labels.
    /// Computed on [`attach`] and cached.
    effective_labels: Labels,
    /// Inheritable properties. Queried via walk-up to first ancestor
    /// that has the key set. Used for `hdr_digits`, `base_interval`, etc.
    props: HashMap<String, String>,
    /// Weak reference to parent for prop walk-up.
    parent: Option<Weak<RwLock<Component>>>,
    /// Child components. Populated at runtime as phases start.
    children: Vec<Arc<RwLock<Component>>>,
    /// Lifecycle state. Only RUNNING components are captured.
    state: ComponentState,
    /// Canonical instrument store for this component.
    ///
    /// Hot-path callers hold typed `Arc<...>` references obtained
    /// at registration time and never look up by name per cycle.
    /// Family-name lookup ([`find_instrument`]) is a linear scan and
    /// is reserved for diagnostics / introspection — see the
    /// [`find_instrument`] doc-comment.
    instruments: Vec<RegisteredInstrument>,
    /// Optional hook for instruments whose existence isn't known at
    /// init (per-error-type counters, etc.). Capture walks
    /// `instruments` first, then invokes this if present. See
    /// [`DynamicCapture`].
    dynamic_capture: Option<Arc<dyn DynamicCapture>>,
    /// Wall-clock instant of the most recent `capture_delta` /
    /// `capture_delta_auto` call. Used by `capture_delta_auto` to
    /// compute the true elapsed-time interval for a phase-end
    /// flush — eliminates the 1-second quantization that comes
    /// from stamping the partial with the scheduler's nominal
    /// `base_interval`. `None` until the first capture; the auto
    /// path treats that as "use caller-supplied fallback".
    last_capture_instant: Mutex<Option<Instant>>,
    /// Dynamic-controls declared on this component (SRD 23).
    /// Empty unless the code that instantiates the component
    /// explicitly declares a control via
    /// `component.controls().declare(...)`.
    controls: crate::controls::ControlRegistry,
    /// Data-materialised child cells, keyed by coordinate.
    ///
    /// Held behind an `Arc` and handed out BY VALUE, not as a borrow through
    /// the component's guard: resolving a cell attaches a child, which takes
    /// this component's WRITE lock. A caller that reached the map through a
    /// read guard would still be holding it — a self-deadlock on the same
    /// `RwLock`. Returning the `Arc` lets the guard drop before resolution.
    ///
    /// A cell's lifetime is this component's: a phase's cells go when the
    /// phase subtree does.
    cells: std::sync::Arc<crate::cells::CellMap>,
    /// Liveness token. `Some` from construction until this component reaches
    /// [`ComponentState::Stopped`], then dropped.
    ///
    /// A parent holds `Weak` clones of its children's tokens to detect
    /// CONCURRENT siblings sharing a label set (see [`attach`]). Weak, so the
    /// check needs no lock on any child and no cleanup hook anywhere: the token
    /// dies when the component stops, and again if the component is simply
    /// dropped without stopping. There is no counter to decrement and therefore
    /// no way to leak a phantom sibling that blocks a legitimate re-attach.
    live: Option<std::sync::Arc<()>>,
    /// Live children indexed by own-label rendering, for the
    /// concurrent-sibling check in [`attach`]. Holds `Weak` tokens and is
    /// pruned lazily on the only path that reads a bucket, so it never keeps a
    /// dead component alive and never needs an explicit teardown pass.
    live_children: std::collections::HashMap<String, Vec<std::sync::Weak<()>>>,
}

impl Component {
    /// Create a new detached component with the given labels and props.
    ///
    /// The component starts in [`ComponentState::Starting`]. Call
    /// [`attach`] to wire it into the tree and compute effective labels.
    pub fn new(labels: Labels, props: HashMap<String, String>) -> Self {
        Self {
            effective_labels: labels.clone(),
            labels,
            props,
            parent: None,
            children: Vec::new(),
            state: ComponentState::Starting,
            instruments: Vec::new(),
            dynamic_capture: None,
            last_capture_instant: Mutex::new(None),
            controls: crate::controls::ControlRegistry::new(),
            cells: std::sync::Arc::new(crate::cells::CellMap::new()),
            live: Some(std::sync::Arc::new(())),
            live_children: std::collections::HashMap::new(),
        }
    }

    /// Create a root component (session level). No parent.
    pub fn root(labels: Labels, props: HashMap<String, String>) -> Arc<RwLock<Self>> {
        let mut component = Self::new(labels, props);
        component.state = ComponentState::Running;
        Arc::new(RwLock::new(component))
    }

    /// This component's own labels (not including ancestors).
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Effective labels: all ancestor labels merged with own labels.
    pub fn effective_labels(&self) -> &Labels {
        &self.effective_labels
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ComponentState {
        self.state
    }

    /// Transition to a new lifecycle state.
    ///
    /// Reaching [`ComponentState::Stopped`] drops the liveness token, which is
    /// what releases this component's claim on its label set among its
    /// siblings. Every stop path goes through here, so there is exactly one
    /// place that can forget to do it.
    pub fn set_state(&mut self, state: ComponentState) {
        self.state = state;
        if state == ComponentState::Stopped {
            self.live = None;
        }
    }

    /// A `Weak` handle to this component's liveness token, for a parent's
    /// concurrent-sibling index. Dead once the component stops or is dropped.
    fn live_handle(&self) -> std::sync::Weak<()> {
        match &self.live {
            Some(t) => std::sync::Arc::downgrade(t),
            None => std::sync::Weak::new(),
        }
    }

    /// Register an instrument under `family` on this component.
    ///
    /// Returns `Err` when `family` is already registered on this
    /// component — duplicate-family declarations on the same
    /// dimensional cell surface as a workload error here, before
    /// any cycle runs (SRD-40b §7.2). The component's
    /// `effective_labels` define the dimensional cell; the same
    /// family on a different component is a different cell and
    /// produces no collision.
    ///
    /// The collision check is a linear scan over the registry
    /// Vec — see the storage-shape note on [`Self::instruments`].
    pub fn register_instrument(
        &mut self,
        family: impl Into<String>,
        instrument: InstrumentRef,
    ) -> Result<(), String> {
        self.register_instrument_with_unit(family, None, instrument)
    }

    /// Variant of [`Self::register_instrument`] that records an
    /// OpenMetrics unit (`ms`, `bytes`, …).
    ///
    /// At capture time the unit drives the `_<unit>` suffix on
    /// `metric_family.name` and populates the `unit` column per
    /// SRD-40a §4.3 / SRD-40b §1. `None` is identical to the
    /// no-unit `register_instrument` path.
    pub fn register_instrument_with_unit(
        &mut self,
        family: impl Into<String>,
        unit: Option<String>,
        instrument: InstrumentRef,
    ) -> Result<(), String> {
        let family = family.into();
        if self.instruments.iter().any(|ri| ri.family == family) {
            return Err(format!(
                "duplicate family name on dimensionally-same metric \
                 context: {family}{}",
                self.effective_labels.to_prometheus(),
            ));
        }
        self.instruments.push(RegisteredInstrument {
            family,
            unit,
            instrument,
        });
        Ok(())
    }

    /// Read-only view of every registered instrument on this
    /// component, in insertion order. Walked by the cadence
    /// reporter on every tick.
    pub fn instruments(&self) -> &[RegisteredInstrument] {
        &self.instruments
    }

    /// Linear scan by family name — diagnostic / rare-path only.
    ///
    /// Hot-path callers must use the typed `Arc<...>` they
    /// captured at registration time. The Vec storage and linear
    /// scan are deliberate: registration is once-at-init,
    /// per-cycle access is pre-bound, and a HashMap probe would
    /// add API + Hash bound for ~40 ns saved once per workload load.
    ///
    /// If you find yourself reaching for this on a per-cycle code
    /// path, that's a design bug in the caller — pre-bind the
    /// `Arc<...>` you got from [`register_instrument`] instead.
    pub fn find_instrument(&self, family: &str) -> Option<&InstrumentRef> {
        self.instruments
            .iter()
            .find(|ri| ri.family == family)
            .map(|ri| &ri.instrument)
    }

    /// Install a [`DynamicCapture`] hook for instruments whose
    /// existence isn't known at init time. Replaces any prior
    /// installation. See the trait doc.
    pub fn set_dynamic_capture(&mut self, hook: Arc<dyn DynamicCapture>) {
        self.dynamic_capture = Some(hook);
    }

    /// Capture a delta snapshot covering `interval`.
    ///
    /// Drains histogram/timer reservoirs; counters report their
    /// absolute running total (no draining). Called by the scheduler on
    /// every tick — the result feeds the cadence reporter's
    /// smallest-cadence accumulator. The caller-supplied
    /// `interval` is recorded on the snapshot verbatim; the
    /// scheduler passes its nominal `base_interval` so the
    /// "canonical scheduler cadence" property is preserved
    /// in storage even when wall-clock between ticks drifts
    /// (drift surfaces via the scheduler's tick warning,
    /// not by mutating the snapshot's interval).
    ///
    /// Also stamps `last_capture_instant` so the phase-end
    /// flush path (`capture_delta_auto`) can compute true
    /// elapsed time since the previous capture.
    pub fn capture_delta(&self, interval: Duration) -> MetricSet {
        let now = Instant::now();
        let mut out = MetricSet::at(now, interval);
        self.capture_registry_into(&mut out, now, true);
        if let Some(hook) = &self.dynamic_capture {
            hook.capture_into(&mut out, now, true);
        }
        *self
            .last_capture_instant
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(now);
        out
    }

    /// Phase-end variant of [`capture_delta`] that stamps the
    /// snapshot with **real elapsed wall time** since the
    /// previous capture, rather than a caller-supplied
    /// nominal interval.
    ///
    /// Eliminates the 1-second quantization that surfaced as
    /// the spurious `cycles_total_rate = 10000/8 = 1250.0`
    /// cluster for short phases — a 7.84-second phase will now
    /// carry a 7843-ms interval (subject to storage precision)
    /// instead of being padded to the scheduler's nominal 1s
    /// final-flush stamp.
    ///
    /// `fallback` covers the edge case where no prior capture
    /// has happened (a phase that ended before the first
    /// scheduler tick). The phase-end caller passes the
    /// scheduler's nominal `base_interval` here — a phase
    /// shorter than one tick still gets stamped with that
    /// nominal duration rather than zero.
    pub fn capture_delta_auto(&self, fallback: Duration) -> MetricSet {
        let now = Instant::now();
        let interval = {
            let mut prev = self
                .last_capture_instant
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let elapsed = prev.map(|t| now.duration_since(t));
            *prev = Some(now);
            elapsed.unwrap_or(fallback)
        };
        let mut out = MetricSet::at(now, interval);
        self.capture_registry_into(&mut out, now, true);
        if let Some(hook) = &self.dynamic_capture {
            hook.capture_into(&mut out, now, true);
        }
        out
    }

    /// Capture a non-mutating snapshot of current state.
    ///
    /// - Counters: absolute totals (atomic load).
    /// - Gauges: current value.
    /// - Histograms / Timers: non-draining clone (`peek_snapshot`).
    ///
    /// Never touches internal accumulators — callers may invoke
    /// this arbitrarily often without perturbing the scheduler's
    /// per-tick cascade.
    pub fn capture_current(&self) -> MetricSet {
        let now = Instant::now();
        let mut out = MetricSet::at(now, Duration::ZERO);
        self.capture_registry_into(&mut out, now, false);
        if let Some(hook) = &self.dynamic_capture {
            hook.capture_into(&mut out, now, false);
        }
        out
    }

    /// Walk the registered instruments and emit their samples into
    /// `out`. `drain=true` drains histogram/timer reservoirs;
    /// `drain=false` peeks without disturbing them. Counters always
    /// report their absolute running total — `drain` does not apply.
    fn capture_registry_into(&self, out: &mut MetricSet, now: Instant, drain: bool) {
        for ri in &self.instruments {
            let family = ri.family.clone();
            let unit = ri.unit.as_deref();
            match &ri.instrument {
                InstrumentRef::Counter(c) => {
                    let lbl = strip_name_label(c.labels());
                    // A counter is its absolute running total — captured the
                    // same on every path (the `drain` flag only governs
                    // histogram/timer reservoirs). Per-interval deltas are
                    // derived downstream by differencing samples. See the
                    // cumulative-counter note.
                    out.insert_counter_with_unit(family, unit, lbl, c.get(), now);
                }
                InstrumentRef::Gauge(g) => {
                    let lbl = strip_name_label(g.labels());
                    out.insert_gauge_with_unit(family, unit, lbl, g.get(), now);
                }
                InstrumentRef::Histogram(h) => {
                    let lbl = strip_name_label(h.labels());
                    let reservoir = if drain {
                        h.snapshot()
                    } else {
                        h.peek_snapshot()
                    };
                    // `cumulative_count` is the instrument's lifetime total
                    // (monotonic, never drained); the reservoir carries the
                    // per-window distribution. See the cumulative-counter note.
                    out.insert_histogram_with_unit_cumulative(
                        family,
                        unit,
                        lbl,
                        reservoir,
                        h.total(),
                        now,
                    );
                }
                InstrumentRef::Timer(t) => {
                    let lbl = strip_name_label(t.labels());
                    let snap = if drain {
                        t.snapshot()
                    } else {
                        t.peek_snapshot()
                    };
                    // `snap.count` is the Timer's absolute lifetime count.
                    out.insert_histogram_with_unit_cumulative(
                        family,
                        unit,
                        lbl,
                        snap.histogram,
                        snap.count,
                        now,
                    );
                }
            }
        }
    }

    /// Get a property by name, walking up the tree.
    ///
    /// Checks this component's props first, then each ancestor in
    /// order until found. Returns `None` if no ancestor has the key.
    pub fn get_prop(&self, name: &str) -> Option<String> {
        if let Some(value) = self.props.get(name) {
            return Some(value.clone());
        }
        if let Some(ref parent_weak) = self.parent
            && let Some(parent_arc) = parent_weak.upgrade()
            && let Ok(parent) = parent_arc.read()
        {
            return parent.get_prop(name);
        }
        None
    }

    /// Set a property on this component.
    pub fn set_prop(&mut self, name: &str, value: &str) {
        self.props.insert(name.to_string(), value.to_string());
    }

    /// Number of child components.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Iterator over this component's direct children.
    pub fn children(&self) -> impl Iterator<Item = &Arc<RwLock<Component>>> {
        self.children.iter()
    }

    /// Borrow this component's dynamic-controls registry. Every
    /// component carries one; empty until something declares a
    /// control on it. See SRD 23.
    pub fn controls(&self) -> &crate::controls::ControlRegistry {
        &self.controls
    }

    /// This component's data-materialised cells. See [`crate::cells::CellMap`]:
    /// one series per dimension instance is one CHILD per instance, not a label
    /// bag on an instrument.
    ///
    /// Returns the `Arc` by value ON PURPOSE — see the field docs. Resolving
    /// takes this component's write lock, so the caller must be able to drop
    /// its read guard first:
    ///
    /// ```ignore
    /// let cells = parent.read().unwrap().cells();  // guard dropped here
    /// let cell  = cells.resolve(&parent, &coord);  // takes the write lock
    /// ```
    pub fn cells(&self) -> std::sync::Arc<crate::cells::CellMap> {
        self.cells.clone()
    }

    /// Resolve a typed control by name, walking up the parent
    /// chain. This component's registry is checked first; then
    /// each ancestor in order. An ancestor declaration is only
    /// honored if its [`BranchScope`] is `Subtree` —
    /// [`BranchScope::Local`] declarations do not propagate to
    /// descendants. Returns `None` if no in-scope declaration
    /// matches the `<name, T>` pair.
    ///
    /// Mirrors [`Self::get_prop`] but for typed controls (SRD 23
    /// §"Branch-scoped and final controls").
    pub fn find_control_up<T>(&self, name: &str) -> Option<crate::controls::Control<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        if let Some(c) = self.controls.get::<T>(name) {
            return Some(c);
        }
        if let Some(ref parent_weak) = self.parent
            && let Some(parent_arc) = parent_weak.upgrade()
            && let Ok(parent) = parent_arc.read()
        {
            return parent.find_control_up_subtree::<T>(name);
        }
        None
    }

    /// Ancestor-side recursion. Honors [`BranchScope::Subtree`]
    /// on an ancestor's declaration; otherwise keeps walking.
    fn find_control_up_subtree<T>(&self, name: &str) -> Option<crate::controls::Control<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        if let Some(erased) = self.controls.get_erased(name)
            && erased.branch_scope() == crate::controls::BranchScope::Subtree
            && let Some(c) = self.controls.get::<T>(name)
        {
            return Some(c);
        }
        if let Some(ref parent_weak) = self.parent
            && let Some(parent_arc) = parent_weak.upgrade()
            && let Ok(parent) = parent_arc.read()
        {
            return parent.find_control_up_subtree::<T>(name);
        }
        None
    }

    /// Erased variant of [`Self::find_control_up`] — returns
    /// just the enumeration handle, useful for diagnostics
    /// (`dryrun=controls`, TUI surfaces) that don't need the
    /// typed value.
    pub fn find_control_erased_up(
        &self,
        name: &str,
    ) -> Option<std::sync::Arc<dyn crate::controls::ErasedControl>> {
        if let Some(erased) = self.controls.get_erased(name) {
            return Some(erased);
        }
        if let Some(ref parent_weak) = self.parent
            && let Some(parent_arc) = parent_weak.upgrade()
            && let Ok(parent) = parent_arc.read()
        {
            return parent.find_control_erased_up_subtree(name);
        }
        None
    }

    fn find_control_erased_up_subtree(
        &self,
        name: &str,
    ) -> Option<std::sync::Arc<dyn crate::controls::ErasedControl>> {
        if let Some(erased) = self.controls.get_erased(name)
            && erased.branch_scope() == crate::controls::BranchScope::Subtree
        {
            return Some(erased);
        }
        if let Some(ref parent_weak) = self.parent
            && let Some(parent_arc) = parent_weak.upgrade()
            && let Ok(parent) = parent_arc.read()
        {
            return parent.find_control_erased_up_subtree(name);
        }
        None
    }

    /// SRD-89 — flatten the up-walk control resolution starting at `start`
    /// into a name → erased-handle map, computed **without nested locks**.
    ///
    /// This has the same visibility as calling
    /// [`Self::find_control_erased_up`] for every name — the start
    /// component's own controls plus any `BranchScope::Subtree` control on an
    /// ancestor, nearest-wins — but it acquires and releases **one** tier's
    /// lock at a time (never holding a child's guard while reading a parent),
    /// so it is immune to the writer-preferring `RwLock` starvation that a
    /// nested up-walk hits under concurrent in-process execution (a hot-path
    /// per-cycle `find_control_erased_up` deadlocks against the cadence
    /// path's instrument-registration writes; this is built once per phase
    /// and read lock-free thereafter — see SRD-89 §3c-i).
    pub fn control_snapshot(
        start: &std::sync::Arc<std::sync::RwLock<Component>>,
    ) -> std::collections::HashMap<String, std::sync::Arc<dyn crate::controls::ErasedControl>> {
        let mut map: std::collections::HashMap<
            String,
            std::sync::Arc<dyn crate::controls::ErasedControl>,
        > = std::collections::HashMap::new();
        let mut next = Some(start.clone());
        let mut is_start = true;
        while let Some(arc) = next {
            let parent_next;
            {
                let g = arc.read().unwrap_or_else(|e| e.into_inner());
                for handle in g.controls.list() {
                    // The start component's own controls are always visible;
                    // an ancestor's only if subtree-scoped. Nearest wins.
                    if is_start || handle.branch_scope() == crate::controls::BranchScope::Subtree {
                        map.entry(handle.name().to_string()).or_insert(handle);
                    }
                }
                parent_next = g.parent.as_ref().and_then(|w| w.upgrade());
            }
            next = parent_next;
            is_start = false;
        }
        map
    }

    /// Count of `Running`-state descendants (this component's
    /// children, grandchildren, …). Used by callers that want a
    /// structural "how many phases are in flight?" query against
    /// the live component tree — e.g. the TUI's Focus-LOD
    /// placeholder decision (SRD 62 §"Scenario done?").
    ///
    /// The component itself is NOT counted — the query is meant
    /// to traverse from an activity root into its phases.
    pub fn running_descendant_count(&self) -> usize {
        let mut count = 0;
        for child in &self.children {
            if let Ok(c) = child.read() {
                if c.state == ComponentState::Running {
                    count += 1;
                }
                count += c.running_descendant_count();
            }
        }
        count
    }
}

/// Strip the legacy `name=...` label from an instrument's `Labels`,
/// returning the dimensional residual that goes onto the captured
/// `MetricFamily` row. The family name itself is provided
/// separately by [`RegisteredInstrument::family`] — historical
/// instruments embedded the family name as a `name=...` label, but
/// that pair must NOT appear on the metric's `LabelSet` (label-set
/// uniqueness within a family per OpenMetrics §4.5.1 would
/// otherwise be polluted).
fn strip_name_label(labels: &Labels) -> Labels {
    let mut out = Labels::default();
    for (k, v) in labels.iter() {
        if k != "name" {
            out = out.with(k, v);
        }
    }
    out
}

/// SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close flush" —
/// fused teardown helper. Captures a final delta from this component's
/// instruments, fires the cadence reporter's `scope_close` (which
/// marks the partial, ingests, and closes the path), and transitions
/// the component to [`ComponentState::Stopped`].
///
/// **Only acts on `Running` components.** Components that are
/// `Starting`, `Stopping`, or already `Stopped` return without
/// touching the reporter — calling `scope_close` twice on the same
/// component is a no-op on the second call, matching SRD-40 §
/// component lifecycle.
///
/// Components with no registered instruments still close the path so
/// any in-flight prebuffer at that label set (e.g. ingests routed via
/// a sibling layer) flushes through the cascade.
pub fn scope_close(
    component: &Arc<RwLock<Component>>,
    cadence_reporter: &crate::cadence_reporter::CadenceReporter,
    interval: Duration,
) {
    // Read-capture the delta first, then take a write guard to
    // transition state. Read guard is released between the two so
    // the write doesn't deadlock.
    let (labels, delta) = {
        let g = component.read().unwrap_or_else(|e| e.into_inner());
        if g.state != ComponentState::Running {
            return;
        }
        let delta = g.capture_delta(interval);
        (g.effective_labels.clone(), delta)
    };

    cadence_reporter.scope_close(&labels, delta);

    // Transition to Stopped so a subsequent capture pass skips this
    // component (capture_tree only walks Running) and a second
    // scope_close call is a no-op.
    let mut g = component.write().unwrap_or_else(|e| e.into_inner());
    g.state = ComponentState::Stopped;
}

// =========================================================================
// Selector-based lookup (SRD 24)
// =========================================================================

/// Collect every component in a subtree whose `effective_labels`
/// match the selector. Order is pre-order DFS: root first, then
/// each child's subtree in insertion order.
///
/// Selector-based lookup takes an `Arc<RwLock<Component>>` root
/// (rather than `&Component`) because the results must also be
/// `Arc<RwLock<Component>>` — `find`'s Vec is a list of live
/// handles callers can then mutate, not a snapshot. Scoping a
/// query to a subtree is expressed by passing that subtree's
/// `Arc` as the root.
///
/// The query is read-only against every visited component: a
/// failed `read()` (e.g. poisoned lock) is treated as "this
/// subtree is opaque for this query" and silently skipped.
/// Collection order is stable across calls as long as no
/// components are attached or detached mid-traversal.
pub fn find(
    root: &Arc<RwLock<Component>>,
    sel: &crate::selector::Selector,
) -> Vec<Arc<RwLock<Component>>> {
    let mut out = Vec::new();
    find_into(root, sel, &mut out);
    out
}

fn find_into(
    root: &Arc<RwLock<Component>>,
    sel: &crate::selector::Selector,
    out: &mut Vec<Arc<RwLock<Component>>>,
) {
    let Ok(guard) = root.read() else { return };
    if sel.matches(&guard.effective_labels) {
        out.push(root.clone());
    }
    let children = guard.children.clone();
    drop(guard);
    for child in &children {
        find_into(child, sel, out);
    }
}

/// Expect exactly one match. Returns
/// [`crate::selector::LookupError::NotFound`] or
/// [`crate::selector::LookupError::Ambiguous`] otherwise.
///
/// Short-circuits on the second hit — the Vec-returning [`find`]
/// is preferable when all matches are wanted.
pub fn find_one(
    root: &Arc<RwLock<Component>>,
    sel: &crate::selector::Selector,
) -> Result<Arc<RwLock<Component>>, crate::selector::LookupError> {
    let mut first: Option<Arc<RwLock<Component>>> = None;
    let mut count = 0usize;
    find_one_walk(root, sel, &mut first, &mut count);
    match first {
        None => Err(crate::selector::LookupError::NotFound),
        Some(c) if count == 1 => Ok(c),
        Some(_) => Err(crate::selector::LookupError::Ambiguous { count }),
    }
}

fn find_one_walk(
    root: &Arc<RwLock<Component>>,
    sel: &crate::selector::Selector,
    first: &mut Option<Arc<RwLock<Component>>>,
    count: &mut usize,
) {
    let Ok(guard) = root.read() else { return };
    if sel.matches(&guard.effective_labels) {
        *count += 1;
        if first.is_none() {
            *first = Some(root.clone());
        }
        // Keep walking so `count` reflects the total — callers
        // rely on the Ambiguous count.
    }
    let children = guard.children.clone();
    drop(guard);
    for child in &children {
        find_one_walk(child, sel, first, count);
    }
}

/// True if any component in the subtree matches. Short-circuits
/// on the first hit.
pub fn any(root: &Arc<RwLock<Component>>, sel: &crate::selector::Selector) -> bool {
    any_walk(root, sel)
}

fn any_walk(root: &Arc<RwLock<Component>>, sel: &crate::selector::Selector) -> bool {
    let Ok(guard) = root.read() else { return false };
    if sel.matches(&guard.effective_labels) {
        return true;
    }
    let children = guard.children.clone();
    drop(guard);
    children.iter().any(|c| any_walk(c, sel))
}

/// Count every matching component in the subtree.
pub fn count(root: &Arc<RwLock<Component>>, sel: &crate::selector::Selector) -> usize {
    let mut n = 0usize;
    count_walk(root, sel, &mut n);
    n
}

fn count_walk(root: &Arc<RwLock<Component>>, sel: &crate::selector::Selector, n: &mut usize) {
    let Ok(guard) = root.read() else { return };
    if sel.matches(&guard.effective_labels) {
        *n += 1;
    }
    let children = guard.children.clone();
    drop(guard);
    for child in &children {
        count_walk(child, sel, n);
    }
}

/// Attach a child component to a parent.
///
/// Computes the child's effective labels by composing the parent's
/// effective labels with the child's own labels. Adds the child to
/// the parent's children list and sets the child's parent reference.
///
/// **Label-ownership invariant.** A dimensional label name is owned
/// by exactly one component in any ancestor chain: once a name is
/// set on a component at initialization, no descendant may redeclare
/// it — neither with a differing value (which would silently corrupt
/// the dimensional cell) nor with the same value (which makes
/// ownership ambiguous). The session tier owns `session`, the
/// execution tier owns `{exec_id, workload}`, the phase tier owns
/// `{phase, …for_each}`, and so on down — each component declares
/// ONLY the labels it introduces. This check enforces that at
/// attach time (init, not per-cycle): a collision is a construction
/// bug and panics with both label sets named, rather than letting
/// the composition silently pick a winner.
///
/// **Concurrent-sibling invariant.** The rule above is *vertical* — it
/// constrains a child against its ANCESTORS. Two SIBLINGS declaring the
/// same own-labels compose byte-identical `effective_labels`, and the same
/// family registered on each then yields two instruments sharing one metric
/// identity, which the per-component duplicate-family check cannot see
/// because they are different components.
///
/// That is rejected only when the siblings are alive AT THE SAME TIME.
/// Sequential reuse is legitimate: an iteration whose values repeat (the fib
/// comprehension yields `n=1` twice) re-materialises the same identity,
/// which is one identity sampled again over time, not a second identity. An
/// unconditional check was implemented and rejected for exactly that reason.
///
/// Liveness is tracked by a token each component holds until it reaches
/// [`ComponentState::Stopped`]; the parent indexes `Weak` clones per
/// own-label set. So the check needs no lock on any child, costs a lookup in
/// one bucket, and cleans up with no teardown pass — a stopped or dropped
/// component's claim simply expires.
pub fn attach(parent: &Arc<RwLock<Component>>, child: &Arc<RwLock<Component>>) {
    let parent_effective = {
        let p = parent.read().unwrap_or_else(|e| e.into_inner());
        p.effective_labels.clone()
    };
    let mut c = child.write().unwrap_or_else(|e| e.into_inner());
    if let Some((k, _)) = c
        .labels
        .iter()
        .find(|(k, _)| parent_effective.get(k).is_some())
    {
        panic!(
            "component label-ownership violation: child re-declares label `{k}` \
             already owned by an ancestor. Each label name must be set on exactly \
             one tier and inherited downward (child {}, ancestors {}).",
            c.labels.to_prometheus(),
            parent_effective.to_prometheus(),
        );
    }
    c.effective_labels = parent_effective.extend(&c.labels);
    c.parent = Some(Arc::downgrade(parent));
    let child_own = c.labels.to_prometheus();
    let child_live = c.live_handle();
    drop(c);

    let mut p = parent.write().unwrap_or_else(|e| e.into_inner());
    // Concurrent-sibling check — the horizontal half of the ownership rule,
    // scoped to components that are alive AT THE SAME TIME.
    //
    // Sequential reuse of a label set is legitimate and must stay legal: an
    // iteration whose values repeat (the fib comprehension yields `n=1` twice)
    // re-materialises the same identity, which is one identity sampled again
    // over time. Two components alive at once is a different thing entirely —
    // each can register the same family, and the two instruments then share one
    // metric identity with the per-component duplicate check unable to see it.
    //
    // Only this key's bucket is touched, and pruning happens on the same visit,
    // so the check is O(live siblings sharing this exact label set) — normally
    // zero — and the index self-cleans without a teardown pass.
    let bucket = p.live_children.entry(child_own.clone()).or_default();
    bucket.retain(|w| w.strong_count() > 0);
    if !bucket.is_empty() {
        let parent_labels = parent_effective.to_prometheus();
        panic!(
            "component sibling-identity violation: a LIVE sibling already \
             declares the own-label set {child_own} under {parent_labels}. Both \
             would compose byte-identical effective labels, so the same family \
             registered on each yields two instruments sharing one metric \
             identity — which the per-component duplicate-family check cannot \
             see, because they are different components. (Re-using a label set \
             AFTER the previous component stops is fine and is not this.)"
        );
    }
    bucket.push(child_live);
    p.children.push(child.clone());
}

/// Detach a child component from its parent.
///
/// Removes the child from the parent's children list and clears
/// the child's parent reference.
pub fn detach(parent: &Arc<RwLock<Component>>, child: &Arc<RwLock<Component>>) {
    let mut p = parent.write().unwrap_or_else(|e| e.into_inner());
    p.children.retain(|c| !Arc::ptr_eq(c, child));
    let mut c = child.write().unwrap_or_else(|e| e.into_inner());
    c.parent = None;
}

/// Walk the component tree and capture delta snapshots from all
/// RUNNING components.
///
/// Returns one `(effective_labels, snapshot)` pair per captured
/// component. Draining semantics — used by the scheduler tick.
pub fn capture_tree(root: &Arc<RwLock<Component>>, interval: Duration) -> Vec<(Labels, MetricSet)> {
    let mut results = Vec::new();
    capture_recursive(root, interval, &mut results);
    results
}

fn capture_recursive(
    node: &Arc<RwLock<Component>>,
    interval: Duration,
    results: &mut Vec<(Labels, MetricSet)>,
) {
    // Take a read guard, snapshot the values we need, drop the
    // guard before recursing so child locks don't nest on ours.
    let Ok(guard) = node.read() else { return };
    let state = guard.state;
    let effective_labels = guard.effective_labels.clone();
    let children = guard.children.clone();

    if state == ComponentState::Running {
        let snapshot = guard.capture_delta(interval);
        if !snapshot.is_empty() {
            results.push((effective_labels.clone(), snapshot));
        }
        // Reified control gauges — one per declared control that
        // has a numeric projection. Published at every tick so
        // they flow through the same sinks as regular metrics.
        let control_gauges = guard
            .controls
            .snapshot_gauges(&effective_labels, Instant::now());
        if !control_gauges.is_empty() {
            results.push((effective_labels, control_gauges));
        }
    }

    drop(guard);
    for child in &children {
        capture_recursive(child, interval, results);
    }
}

/// Non-mutating counterpart of [`capture_tree`]. Walks every RUNNING
/// component and returns absolute/peeked snapshots via
/// [`Component::capture_current`]. Safe to call arbitrarily often —
/// doesn't drain histogram/timer reservoirs.
pub fn capture_tree_current(root: &Arc<RwLock<Component>>) -> Vec<(Labels, MetricSet)> {
    let mut results = Vec::new();
    capture_current_recursive(root, &mut results);
    results
}

fn capture_current_recursive(
    node: &Arc<RwLock<Component>>,
    results: &mut Vec<(Labels, MetricSet)>,
) {
    let Ok(guard) = node.read() else { return };
    let state = guard.state;
    let effective_labels = guard.effective_labels.clone();
    let children = guard.children.clone();

    if state == ComponentState::Running {
        let snapshot = guard.capture_current();
        if !snapshot.is_empty() {
            results.push((effective_labels.clone(), snapshot));
        }
        let control_gauges = guard
            .controls
            .snapshot_gauges(&effective_labels, Instant::now());
        if !control_gauges.is_empty() {
            results.push((effective_labels, control_gauges));
        }
    }

    drop(guard);
    for child in &children {
        capture_current_recursive(child, results);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn new_counter(family: &str) -> Arc<Counter> {
        Arc::new(Counter::new(Labels::of("name", family)))
    }

    // ── SRD-40b §7.2: register_instrument duplicate detection ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_instrument_first_time_succeeds() {
        let mut c = Component::new(Labels::empty(), HashMap::new());
        assert!(
            c.register_instrument(
                "recall_at_10",
                InstrumentRef::Counter(new_counter("recall_at_10")),
            )
            .is_ok()
        );
        assert!(c.find_instrument("recall_at_10").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_instrument_duplicate_errors() {
        let mut c = Component::new(Labels::empty(), HashMap::new());
        c.register_instrument(
            "recall_at_10",
            InstrumentRef::Counter(new_counter("recall_at_10")),
        )
        .unwrap();
        let err = c
            .register_instrument(
                "recall_at_10",
                InstrumentRef::Counter(new_counter("recall_at_10")),
            )
            .unwrap_err();
        assert!(err.contains("duplicate family"), "wrong message: {err}");
        assert!(
            err.contains("recall_at_10"),
            "family name not in error: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_instrument_distinct_names_succeed() {
        let mut c = Component::new(Labels::empty(), HashMap::new());
        c.register_instrument("a", InstrumentRef::Counter(new_counter("a")))
            .unwrap();
        c.register_instrument("b", InstrumentRef::Counter(new_counter("b")))
            .unwrap();
        c.register_instrument("c", InstrumentRef::Counter(new_counter("c")))
            .unwrap();
        assert_eq!(c.instruments().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_instrument_error_carries_label_context() {
        // SRD-40b §7's contract: the error message names the
        // dimensional cell so the workload author can see WHICH
        // op-template's label set produced the collision.
        let labels = Labels::of("phase", "pvs_query").with("op", "select_ann");
        let mut c = Component::new(labels, HashMap::new());
        c.register_instrument("overscan", InstrumentRef::Counter(new_counter("overscan")))
            .unwrap();
        let err = c
            .register_instrument("overscan", InstrumentRef::Counter(new_counter("overscan")))
            .unwrap_err();
        assert!(err.contains("phase"), "missing phase label: {err}");
        assert!(err.contains("pvs_query"), "missing phase value: {err}");
        assert!(err.contains("op"), "missing op label: {err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_instrument_isolated_per_component() {
        // Two components — registering the same family on each
        // is OK; dimensional uniqueness comes from the
        // component-tree structure, not a global registry.
        let mut a = Component::new(Labels::of("op", "foo"), HashMap::new());
        let mut b = Component::new(Labels::of("op", "bar"), HashMap::new());
        assert!(
            a.register_instrument("overscan", InstrumentRef::Counter(new_counter("overscan")),)
                .is_ok()
        );
        assert!(
            b.register_instrument("overscan", InstrumentRef::Counter(new_counter("overscan")),)
                .is_ok()
        );
    }

    // Test helper: register a counter that records a fixed value.
    fn install_counter(c: &mut Component, family: &str, value: u64) -> Arc<Counter> {
        let counter = new_counter(family);
        counter.inc_by(value);
        c.register_instrument(family, InstrumentRef::Counter(counter.clone()))
            .unwrap();
        counter
    }

    // ── DynamicCapture hook ──

    struct DynamicCounter {
        inner: AtomicU64,
    }
    impl DynamicCapture for DynamicCounter {
        fn capture_into(&self, out: &mut MetricSet, now: Instant, _drain: bool) {
            let v = self.inner.load(Ordering::Relaxed);
            out.insert_counter("dynamic_counter", Labels::default(), v, now);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dynamic_capture_runs_after_registry() {
        let mut c = Component::new(Labels::empty(), HashMap::new());
        install_counter(&mut c, "static_counter", 5);
        c.set_dynamic_capture(Arc::new(DynamicCounter {
            inner: AtomicU64::new(7),
        }));
        let snap = c.capture_current();
        assert!(snap.family("static_counter").is_some());
        assert!(snap.family("dynamic_counter").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn component_attach_computes_effective_labels() {
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let child = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &child);

        let c = child.read().unwrap();
        let eff = c.effective_labels();
        assert_eq!(eff.get("session"), Some("s1"));
        assert_eq!(eff.get("phase"), Some("rampup"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prop_walk_up_inheritance() {
        let mut root_props = HashMap::new();
        root_props.insert("hdr_digits".to_string(), "4".to_string());
        let root = Component::root(Labels::of("session", "s1"), root_props);

        let child = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &child);

        let c = child.read().unwrap();
        assert_eq!(c.get_prop("hdr_digits").as_deref(), Some("4"));
        assert_eq!(c.get_prop("nonexistent").as_deref(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prop_child_overrides_parent() {
        let mut root_props = HashMap::new();
        root_props.insert("hdr_digits".to_string(), "3".to_string());
        let root = Component::root(Labels::of("session", "s1"), root_props);

        let mut child_props = HashMap::new();
        child_props.insert("hdr_digits".to_string(), "4".to_string());
        let child = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "rampup"),
            child_props,
        )));
        attach(&root, &child);

        let c = child.read().unwrap();
        assert_eq!(c.get_prop("hdr_digits").as_deref(), Some("4"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detach_removes_child() {
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let child = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &child);
        assert_eq!(root.read().unwrap().child_count(), 1);

        detach(&root, &child);
        assert_eq!(root.read().unwrap().child_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_tree_collects_running_components() {
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());

        // Running child with a registered counter.
        let child1 = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "load"),
            HashMap::new(),
        )));
        attach(&root, &child1);
        {
            let mut c = child1.write().unwrap();
            c.set_state(ComponentState::Running);
            install_counter(&mut c, "test_counter", 42);
        }

        // Stopped child with a registered counter — must NOT be captured.
        let child2 = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "done"),
            HashMap::new(),
        )));
        attach(&root, &child2);
        {
            let mut c = child2.write().unwrap();
            c.set_state(ComponentState::Stopped);
            install_counter(&mut c, "test_counter", 99);
        }

        let captured = capture_tree(&root, Duration::from_secs(1));
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0.get("phase"), Some("load"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_tree_walks_nested_children() {
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());

        let scenario = Arc::new(RwLock::new(Component::new(
            Labels::of("scenario", "default"),
            HashMap::new(),
        )));
        attach(&root, &scenario);
        scenario.write().unwrap().set_state(ComponentState::Running);

        let phase = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "search"),
            HashMap::new(),
        )));
        attach(&scenario, &phase);
        {
            let mut p = phase.write().unwrap();
            p.set_state(ComponentState::Running);
            install_counter(&mut p, "test_counter", 10);
        }

        let captured = capture_tree(&root, Duration::from_secs(1));
        assert_eq!(captured.len(), 1);
        let eff = &captured[0].0;
        assert_eq!(eff.get("session"), Some("s1"));
        assert_eq!(eff.get("scenario"), Some("default"));
        assert_eq!(eff.get("phase"), Some("search"));
    }

    // =====================================================================
    // Selector-based lookup (SRD 24)
    // =====================================================================

    /// Fixture: a small session tree with two activity subtrees,
    /// each holding a handful of phases with distinct label shapes.
    ///
    /// Models the production label convention: each label is a
    /// condensed `(semantic, instance)` pair, so the KEY is the
    /// tier's kind (`session` / `activity` / `phase`) and the VALUE
    /// is its instance. No tier redeclares a name an ancestor owns
    /// (the label-ownership invariant `attach` enforces).
    fn sample_tree() -> Arc<RwLock<Component>> {
        let root = Component::root(
            Labels::empty().with("session", "test-session"),
            HashMap::new(),
        );
        // Activity A: rampup + two ann_query phases at different k.
        let activity_a = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("activity", "a"),
            HashMap::new(),
        )));
        attach(&root, &activity_a);
        let rampup = Arc::new(RwLock::new(Component::new(
            Labels::empty()
                .with("phase", "rampup")
                .with("profile", "label_00"),
            HashMap::new(),
        )));
        attach(&activity_a, &rampup);
        for k in ["10", "100"] {
            let aq = Arc::new(RwLock::new(Component::new(
                Labels::empty()
                    .with("phase", "ann_query")
                    .with("profile", "label_00")
                    .with("k", k),
                HashMap::new(),
            )));
            attach(&activity_a, &aq);
        }
        // Activity B: one phase with a different profile shape.
        let activity_b = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("activity", "b"),
            HashMap::new(),
        )));
        attach(&root, &activity_b);
        let teardown = Arc::new(RwLock::new(Component::new(
            Labels::empty()
                .with("phase", "teardown")
                .with("profile", "label_99"),
            HashMap::new(),
        )));
        attach(&activity_b, &teardown);
        root
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_returns_every_match_in_preorder() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().present("phase");
        let hits = find(&root, &sel);
        assert_eq!(hits.len(), 4);
        let names: Vec<String> = hits
            .iter()
            .filter_map(|c| {
                c.read()
                    .ok()
                    .and_then(|g| g.effective_labels().get("phase").map(|s| s.to_string()))
            })
            .collect();
        assert_eq!(names, vec!["rampup", "ann_query", "ann_query", "teardown"],);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_with_empty_selector_returns_everything() {
        let root = sample_tree();
        let all = find(&root, &crate::selector::Selector::new());
        // session root + 2 activities + (rampup + 2 ann_query + teardown) = 7
        assert_eq!(all.len(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_with_glob_and_eq_conjunction() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().glob("phase", "ann_*");
        let hits = find(&root, &sel);
        assert_eq!(hits.len(), 2);
        for h in &hits {
            let g = h.read().unwrap();
            assert_eq!(g.effective_labels().get("phase"), Some("ann_query"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_with_present_and_absent_clauses() {
        let root = sample_tree();
        let with_k = find(
            &root,
            &crate::selector::Selector::new()
                .present("phase")
                .present("k"),
        );
        assert_eq!(with_k.len(), 2);

        let without_k = find(
            &root,
            &crate::selector::Selector::new()
                .present("phase")
                .absent("k"),
        );
        assert_eq!(without_k.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_one_exact_match() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().eq("phase", "rampup");
        let c = find_one(&root, &sel).unwrap();
        assert_eq!(
            c.read().unwrap().effective_labels().get("phase"),
            Some("rampup"),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_one_not_found() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().eq("phase", "nowhere");
        match find_one(&root, &sel) {
            Err(crate::selector::LookupError::NotFound) => {}
            Err(other) => panic!("expected NotFound, got {other:?}"),
            Ok(_) => panic!("expected NotFound, got a match"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_one_ambiguous_reports_count() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().eq("phase", "ann_query");
        match find_one(&root, &sel) {
            Err(crate::selector::LookupError::Ambiguous { count }) => {
                assert_eq!(count, 2);
            }
            Err(other) => panic!("expected Ambiguous, got {other:?}"),
            Ok(_) => panic!("expected Ambiguous, got a single match"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn any_short_circuits_on_first_hit() {
        let root = sample_tree();
        assert!(any(
            &root,
            &crate::selector::Selector::new().eq("phase", "rampup")
        ));
        assert!(!any(
            &root,
            &crate::selector::Selector::new().eq("phase", "zzz")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn count_matches_len_of_find() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new().present("phase");
        assert_eq!(count(&root, &sel), find(&root, &sel).len());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_from_subtree_is_scoped() {
        let root = sample_tree();
        let activity_a = root.read().unwrap().children.first().unwrap().clone();
        let hits = find(
            &activity_a,
            &crate::selector::Selector::new().present("phase"),
        );
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn effective_labels_include_inherited_session_label() {
        let root = sample_tree();
        let sel = crate::selector::Selector::new()
            .eq("session", "test-session")
            .present("phase");
        assert_eq!(count(&root, &sel), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selector_macro_drives_find() {
        let root = sample_tree();
        let hits = find(&root, &crate::selector!(phase = "teardown"));
        assert_eq!(hits.len(), 1);
    }

    // =====================================================================
    // Controls on components (SRD 23)
    // =====================================================================

    #[tokio::test]
    async fn controls_declare_and_lookup_through_component() {
        let root = Component::root(Labels::of("session", "s"), HashMap::new());
        {
            let guard = root.read().unwrap();
            guard
                .controls()
                .declare(crate::controls::ControlBuilder::new("concurrency", 16u32).build());
        }
        let c: crate::controls::Control<u32> = {
            let guard = root.read().unwrap();
            guard.controls().get::<u32>("concurrency").unwrap()
        };
        c.set(32, crate::controls::ControlOrigin::Test)
            .await
            .unwrap();
        let reread: crate::controls::Control<u32> = {
            let guard = root.read().unwrap();
            guard.controls().get::<u32>("concurrency").unwrap()
        };
        assert_eq!(reread.value(), 32);
    }

    #[tokio::test]
    async fn reified_control_gauges_flow_through_capture_tree() {
        let root = Component::root(Labels::empty().with("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &phase);
        {
            let p = phase.write().unwrap();
            p.controls().declare(
                crate::controls::ControlBuilder::new("concurrency", 8u32)
                    .reify_as_gauge(|v| Some(*v as f64))
                    .build(),
            );
        }
        phase.write().unwrap().set_state(ComponentState::Running);

        let c: crate::controls::Control<u32> = phase
            .read()
            .unwrap()
            .controls()
            .get::<u32>("concurrency")
            .unwrap();
        c.set(64, crate::controls::ControlOrigin::Test)
            .await
            .unwrap();

        let captured = capture_tree(&root, Duration::from_secs(1));
        let mut found_value: Option<f64> = None;
        for (labels, set) in &captured {
            if labels.get("phase") != Some("rampup") {
                continue;
            }
            if let Some(fam) = set.family("control_concurrency")
                && let Some(m) = fam.metrics().next()
            {
                if let Some(p) = m.point()
                    && let crate::snapshot::MetricValue::Gauge(g) = p.value()
                {
                    found_value = Some(g.value);
                }
                assert_eq!(m.labels().get("phase"), Some("rampup"));
                assert_eq!(m.labels().get("control"), Some("concurrency"));
            }
        }
        assert_eq!(found_value, Some(64.0));

        let current = capture_tree_current(&root);
        let mut saw_via_current = false;
        for (_, set) in &current {
            if set.family("control_concurrency").is_some() {
                saw_via_current = true;
            }
        }
        assert!(saw_via_current);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dryrun_controls_enumeration_over_tree() {
        let root = sample_tree();
        let phase_hits = find(&root, &crate::selector::Selector::new().present("phase"));
        for (idx, phase) in phase_hits.iter().enumerate() {
            let guard = phase.read().unwrap();
            guard.controls().declare(
                crate::controls::ControlBuilder::new("concurrency", (10 * (idx + 1)) as u32)
                    .build(),
            );
        }

        let mut entries: Vec<(String, String)> = Vec::new();
        for c in find(&root, &crate::selector::Selector::new()) {
            let guard = c.read().unwrap();
            let labels = guard.effective_labels().clone();
            for ctl in guard.controls().list() {
                entries.push((
                    format!("{}/{}", labels.get("phase").unwrap_or("-"), ctl.name(),),
                    ctl.value_string(),
                ));
            }
        }

        assert_eq!(entries.len(), phase_hits.len());
        for (key, value) in &entries {
            assert!(key.ends_with("/concurrency"), "key = {key}");
            assert!(value.parse::<u32>().is_ok(), "value = {value}");
        }
    }

    // ---- Branch-scoped control walk-up (SRD 23) ------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn branch_scope_subtree_resolves_from_descendant() {
        use crate::controls::{BranchScope, ControlBuilder};
        let root = Component::root(Labels::empty().with("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &phase);

        root.read().unwrap().controls().declare(
            ControlBuilder::new("hdr_sigdigs", 3u32)
                .branch_scope(BranchScope::Subtree)
                .build(),
        );

        let resolved = phase.read().unwrap().find_control_up::<u32>("hdr_sigdigs");
        assert!(
            resolved.is_some(),
            "Subtree-scoped control should be visible to descendant"
        );
        assert_eq!(resolved.unwrap().value(), 3u32);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn branch_scope_local_does_not_leak_to_descendants() {
        use crate::controls::{BranchScope, ControlBuilder};
        let root = Component::root(Labels::empty().with("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &phase);

        root.read().unwrap().controls().declare(
            ControlBuilder::new("private", 99u32)
                .branch_scope(BranchScope::Local)
                .build(),
        );

        let leaked = phase.read().unwrap().find_control_up::<u32>("private");
        assert!(
            leaked.is_none(),
            "Local-scoped control must not be visible to descendants"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nearest_declaration_wins_during_walk_up() {
        use crate::controls::{BranchScope, ControlBuilder};
        let root = Component::root(Labels::empty().with("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::empty().with("phase", "rampup"),
            HashMap::new(),
        )));
        attach(&root, &phase);

        root.read().unwrap().controls().declare(
            ControlBuilder::new("hdr_sigdigs", 3u32)
                .branch_scope(BranchScope::Subtree)
                .build(),
        );
        phase
            .read()
            .unwrap()
            .controls()
            .declare(ControlBuilder::new("hdr_sigdigs", 5u32).build());

        let v = phase
            .read()
            .unwrap()
            .find_control_up::<u32>("hdr_sigdigs")
            .unwrap()
            .value();
        assert_eq!(v, 5u32, "phase override should win over session default");
    }

    // =====================================================================
    // SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close flush"
    // =====================================================================

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn component_scope_close_flushes_running_component_marks_partial_and_stops() {
        use crate::cadence::{CadenceTree, Cadences};
        use crate::cadence_reporter::CadenceReporter;

        let tree = CadenceTree::plan_default(Cadences::new(&[Duration::from_secs(1)]).unwrap());
        let reporter = CadenceReporter::new(tree);

        // Build a phase component with a registered counter holding N=42.
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "short"),
            HashMap::new(),
        )));
        attach(&root, &phase);
        {
            let mut p = phase.write().unwrap();
            p.set_state(ComponentState::Running);
            install_counter(&mut p, "test_counter", 42);
        }

        scope_close(&phase, &reporter, Duration::from_millis(150));
        reporter.flush_for_tests();

        // Component is now Stopped — second call must be a no-op.
        assert_eq!(phase.read().unwrap().state(), ComponentState::Stopped);
        scope_close(&phase, &reporter, Duration::from_millis(150));
        reporter.flush_for_tests();

        let labels = phase.read().unwrap().effective_labels().clone();
        let latest = reporter
            .latest(&labels, Duration::from_secs(1))
            .expect("scope_close must publish the partial");
        assert!(latest.is_partial(), "snapshot must be marked partial");
        let f = latest
            .family("test_counter")
            .expect("test_counter family present");
        let m = f.metrics().next().unwrap();
        match m.point().unwrap().value() {
            crate::snapshot::MetricValue::Counter(c) => assert_eq!(c.cumulative, 42),
            v => panic!("expected counter, got {v:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn component_scope_close_skips_non_running_states() {
        use crate::cadence::{CadenceTree, Cadences};
        use crate::cadence_reporter::CadenceReporter;

        let tree = CadenceTree::plan_default(Cadences::new(&[Duration::from_secs(1)]).unwrap());
        let reporter = CadenceReporter::new(tree);

        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "starting"),
            HashMap::new(),
        )));
        attach(&root, &phase);
        assert_eq!(phase.read().unwrap().state(), ComponentState::Starting);
        {
            let mut p = phase.write().unwrap();
            install_counter(&mut p, "test_counter", 99);
        }

        scope_close(&phase, &reporter, Duration::from_millis(150));
        reporter.flush_for_tests();

        assert_eq!(phase.read().unwrap().state(), ComponentState::Starting);
        let labels = phase.read().unwrap().effective_labels().clone();
        assert!(
            reporter.latest(&labels, Duration::from_secs(1)).is_none(),
            "scope_close on a non-Running component must not publish"
        );
    }

    // ── capture_delta_auto: real-elapsed interval ────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_delta_auto_uses_fallback_on_first_call() {
        // No prior capture → fallback is the recorded interval.
        // Same shape the executor's phase-end flush sees on the
        // edge case where a phase ends before any scheduler tick.
        let c = Component::new(Labels::empty(), HashMap::new());
        let s = c.capture_delta_auto(Duration::from_millis(500));
        assert_eq!(s.interval(), Duration::from_millis(500));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_delta_auto_measures_real_elapsed_after_prior_capture() {
        // After a `capture_delta` records the watermark,
        // `capture_delta_auto` reports the actual wall-clock
        // delta between the two — NOT the prior `interval`
        // argument. This is what kills the 1s quantization in
        // the phase-end flush path: a phase-end auto-capture
        // ~80ms after the last scheduler tick stamps ~80ms,
        // not the nominal 1000ms fallback.
        let c = Component::new(Labels::empty(), HashMap::new());
        let _ = c.capture_delta(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(80));
        let s = c.capture_delta_auto(Duration::from_secs(1));
        // Allow generous lower / upper bounds — CI machines
        // can be loaded — but anything within (60ms, 500ms) is
        // unambiguously distinct from the 1000ms fallback.
        assert!(
            s.interval() > Duration::from_millis(60),
            "interval should reflect real ~80ms elapsed, got {:?}",
            s.interval()
        );
        assert!(
            s.interval() < Duration::from_millis(500),
            "interval should be the real elapsed, not the 1s fallback: {:?}",
            s.interval()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_delta_auto_chained_uses_inter_capture_elapsed() {
        // Two consecutive auto-captures: the second sees the
        // elapsed between auto calls, not the cumulative
        // since component creation.
        let c = Component::new(Labels::empty(), HashMap::new());
        let _first = c.capture_delta_auto(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(50));
        let second = c.capture_delta_auto(Duration::from_secs(1));
        assert!(
            second.interval() < Duration::from_millis(500),
            "second auto-capture should measure inter-capture \
             elapsed, not cumulative: {:?}",
            second.interval()
        );
    }
}
