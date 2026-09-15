// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Adapter traits: the tiered interface that database/protocol drivers implement (SRD 38).
//!
//! The tiered interface separates init-time template analysis from cycle-time execution:
//! - `DriverAdapter::map_op()` — called once per template at activity startup
//! - `OpDispenser::execute()` — called per-cycle to bind values and execute

use std::any::Any;
use std::fmt;
use std::sync::OnceLock;

// Re-export so adapter crates can write `use nmbrs_runtime::adapter::ExecCtx`
// alongside `use nmbrs_runtime::adapter::{OpDispenser, ResolvedFields, ...}`.
pub use crate::fixture::ExecCtx;

// Re-export PolydatKernel so adapter `map_op` impls can name the
// `parent: &PolydatKernel` parameter type without each adapter crate
// taking a direct polydat dependency. SRD-68 §"Adapter API
// surface" pins this as the canonical import path for adapters.
pub use polydat::kernel::PolydatKernel;

// Re-export the binder API so `binders_for` impls in adapter
// crates can name `Binder` / `BinderSlot` / `PortType` through
// the existing `nmbrs_runtime::adapter` import path, no direct
// polydat dependency required. Same reason as the PolydatKernel
// re-export above.
pub use polydat::ast::PortType;
pub use polydat::binder::{Binder, BinderSlot};

/// Boxed, `Send` future returned by [`DriverAdapter::map_op`] — yields a
/// boxed [`OpDispenser`] (or an error message). The lifetime ties the
/// future to the borrowed `&self` / template references.
pub type MapOpFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
>;

/// Boxed, `Send` future produced by an adapter/driver `create` factory —
/// yields a connected [`DriverAdapter`] (or an error message).
pub type CreateAdapterFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<std::sync::Arc<dyn DriverAdapter>, String>> + Send>,
>;

/// Trait for adapter-specific result bodies.
///
/// The adapter defines its own concrete result type and implements
/// this trait. Internal adapter code can downcast via `as_any()` to
/// access native types (e.g., CQL rows, HTTP response structs).
/// External consumers call `to_json()` for a universal representation.
///
/// The `element_count()` and `byte_count()` methods support result
/// traversal — the framework uses these to verify that result data
/// was fully received and to record consumption metrics.
pub trait ResultBody: Send + Sync + fmt::Debug {
    /// Serialize this result to JSON for logging, capture, verification.
    fn to_json(&self) -> serde_json::Value;

    /// Downcast support for adapter-internal use.
    fn as_any(&self) -> &dyn Any;

    /// Count of logical elements in this result (rows, records, items).
    /// Used by the result traverser for metrics. Default 1 (one result).
    fn element_count(&self) -> u64 {
        1
    }

    /// Size in bytes of the result payload, if known.
    /// Used by the result traverser for throughput metrics.
    fn byte_count(&self) -> Option<u64> {
        None
    }

    /// SRD-66 §"Surface 1" body projection — return the body's
    /// canonical text representation for use by result-binding
    /// expressions (the magic `body: Str` extern). Default
    /// implementation falls back to `serde_json::to_string`
    /// over `to_json()`; adapters with native text bodies (e.g.
    /// `TextBody`, the CQL describe row) override to return
    /// the underlying text directly.
    fn to_text(&self) -> String {
        serde_json::to_string(&self.to_json()).unwrap_or_default()
    }
}

/// Simple text result body.
#[derive(Debug, Clone)]
pub struct TextBody(pub String);

impl ResultBody for TextBody {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::String(self.0.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn to_text(&self) -> String {
        self.0.clone()
    }
}

/// JSON result body. Adapters that natively produce JSON
/// (HTTP/REST endpoints, the Jolokia JMX bridge, JSON-RPC) wrap
/// the parsed value here so `verify:` field assertions and
/// result-binding extractors can address nested keys (`status`,
/// `value`, etc.) without a re-parse step.
///
/// `element_count` follows the JSON-array convention: array
/// bodies report `array.len()`, scalar/object bodies report `1`.
/// That makes `await_empty` polling work naturally against
/// endpoints whose "still running" state is a non-empty array
/// and "done" state is `[]`.
#[derive(Debug, Clone)]
pub struct JsonBody(pub serde_json::Value);

impl ResultBody for JsonBody {
    fn to_json(&self) -> serde_json::Value {
        self.0.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn element_count(&self) -> u64 {
        match &self.0 {
            serde_json::Value::Array(arr) => arr.len() as u64,
            _ => 1,
        }
    }
    fn to_text(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_default()
    }
}

/// The result of a successful operation.
///
/// If you have an `OpResult`, the operation succeeded. Failure is
/// represented by `ExecutionError`, not by a flag on the result.
/// Protocol-specific status codes (HTTP, CQL) live inside the
/// adapter's `ResultBody` implementation, not on the generic result.
#[derive(Default)]
pub struct OpResult {
    /// Adapter-specific response body. The adapter owns the native
    /// type; consumers call `.to_json()` for a universal view.
    /// Adapter-internal code can downcast via `.as_any()`.
    /// `None` for operations with no meaningful result (e.g., DDL).
    pub body: Option<Box<dyn ResultBody>>,
    /// If true, this op was conditionally skipped (via `if:` field).
    /// The activity loop counts this as a skip, not a success or error.
    pub skipped: bool,
}

impl OpResult {
    /// Create a skipped result (no execution).
    pub fn skipped() -> Self {
        Self {
            body: None,
            skipped: true,
        }
    }
}

impl fmt::Debug for OpResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpResult")
            .field("body", &self.body.as_ref().map(|b| b.to_json()))
            .finish()
    }
}

/// Execution error with scope delamination.
///
/// Distinguishes between per-op errors (template-specific, retryable)
/// and adapter-level errors (connection-wide, affect all ops).
#[derive(Debug)]
pub enum ExecutionError {
    /// Per-op failure: this specific operation failed. Template-specific,
    /// may be retried with the same resolved fields.
    Op(AdapterError),
    /// Adapter-level failure: the driver connection or session is
    /// degraded. Affects all ops. The activity may need to pause or stop.
    Adapter(AdapterError),
}

impl ExecutionError {
    /// Access the inner AdapterError regardless of scope.
    pub fn error(&self) -> &AdapterError {
        match self {
            ExecutionError::Op(e) | ExecutionError::Adapter(e) => e,
        }
    }

    /// Whether this is an adapter-level (connection-wide) error.
    pub fn is_adapter_level(&self) -> bool {
        matches!(self, ExecutionError::Adapter(_))
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::Op(e) => write!(f, "[op] {e}"),
            ExecutionError::Adapter(e) => write!(f, "[adapter] {e}"),
        }
    }
}

impl std::error::Error for ExecutionError {}

/// Error from an adapter operation.
#[derive(Debug)]
pub struct AdapterError {
    /// Error classification name (for error handler routing).
    pub error_name: String,
    /// Human-readable error message.
    pub message: String,
    /// Hint to the executor: is this error worth retrying?
    pub retryable: bool,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.error_name, self.message)
    }
}

impl std::error::Error for AdapterError {}

/// A protocol-specific driver adapter. Constructed once per activity,
/// shared across fibers via Arc.
///
/// The adapter owns the driver connection (session, client, pool) and
/// provides OpDispensers that pre-process each op template at init time.
pub trait DriverAdapter: Send + Sync + 'static {
    /// Human-readable adapter name (e.g., "cql", "http", "stdout").
    fn name(&self) -> &str;

    /// Map an op template into a dispenser. Called once per unique op
    /// template at activity startup — before any cycles execute.
    ///
    /// Init-time work: parse the op, prepare statements, pre-compute
    /// bind-point resolution, validate field names, attach metrics.
    ///
    /// `parent` is the phase scope kernel — the Polydat context the op
    /// template's matter (phase `bindings:`, `result:` block, etc.)
    /// should attach to. Adapters that need their own Polydat context
    /// for op-template-scope name resolution clone the Arc and
    /// either retain it directly (no op-level matter) or use it as
    /// the parent in a SRD-67 `build_subscope` call to materialise
    /// their canonical kernel (op-level matter present); adapters
    /// with no Polydat needs ignore the parameter. The Arc lets the
    /// dispenser own a long-lived reference to the canonical
    /// kernel without re-cloning state. See SRD-68 §"Adapter API
    /// surface".
    /// Construct an [`OpDispenser`] for one op template — the
    /// per-op dispenser-initialization stack frame.
    ///
    /// This is where the currying stack completes: prepare any
    /// protocol-level handles (CQL: prepare statement, read
    /// parameter metadata), build the per-cycle data pullers,
    /// fold everything into the dispenser the runtime will then
    /// call repeatedly with `execute(cycle, ctx)`. Nothing about
    /// init-time state needs to outlive this call — once `map_op`
    /// returns, the dispenser holds whatever it needs and the
    /// rest is dropped.
    ///
    /// ## Per-op binder verification (the typed-lvalue contract)
    ///
    /// Per-op compulsion: for any op-template field this
    /// dispenser will bind through a typed-parameter API
    /// (anything beyond pure text concatenation into a `Str`
    /// lvalue), the implementor MUST construct the appropriate
    /// [`Binder`] shape from its protocol-side metadata
    /// (positional / named / single — see [`polydat::binder`])
    /// and verify it against `parent` via
    /// [`polydat::binder::verify_against_kernel`] before
    /// returning the dispenser. A verification failure surfaces
    /// as a `map_op` `Err`; construction stops before any cycle
    /// runs and the operator sees the rvalue→lvalue mismatch
    /// (one violation per slot, no silent passthrough).
    ///
    /// This is stack-local work: the binder is constructed,
    /// verified, used to wire up the typed binding path in the
    /// dispenser, and dropped. It is NOT cached on the adapter,
    /// returned as a sidecar, or otherwise persisted past the
    /// `map_op` return. Per-op-template, not adapter-wide.
    ///
    /// Adapters whose every op-template field is a text template
    /// (stdout, http body templates, testkit captures) can skip
    /// the verify step — every wire reference's lvalue is `Str`
    /// and the rvalue→lvalue rule permits any rvalue into a `Str`
    /// lvalue, so verification is always a no-op for them. The
    /// runtime guard in `wires::substitute_via_wires` remains the
    /// catch-all safety net for that path.
    fn map_op<'a>(
        &'a self,
        template: &'a nmbrs_workload::model::ParsedOp,
        parent: std::sync::Arc<PolydatKernel>,
    ) -> MapOpFuture<'a>;

    /// Default metric names to display on the status line for this adapter.
    ///
    /// Each entry is a metric name (matching `adapter_metrics()` labels)
    /// and a display label. Workloads can override this via a `status:`
    /// field on phases or ops. Default: empty (no adapter-specific status).
    fn default_status_metrics(&self) -> Vec<StatusMetric> {
        Vec::new()
    }

    /// Preferred display mode for this adapter.
    ///
    /// When multiple adapters are involved in a workload, the runtime
    /// uses the most restrictive (lowest) mode. Adapters that use
    /// raw terminal output (plotter) return `Off` to prevent the TUI
    /// from entering alternate screen.
    ///
    /// - `Auto`: adapter is compatible with TUI (default)
    /// - `Off`: adapter requires raw stderr/stdout, TUI must not activate
    fn display_preference(&self) -> DisplayPreference {
        DisplayPreference::Auto
    }

    /// Declare the set of op-field names this adapter knows how to
    /// interpret. SRD 30 §"Core-first field processing" requires
    /// that after the core runtime strips its own fields, every
    /// remaining key in `ParsedOp.op` must be in this list — an
    /// unknown field is a hard error, not a silent pass-through.
    ///
    /// Return `None` (the default) to opt out of strict validation
    /// during the transition; existing adapters that haven't been
    /// audited remain permissive. Adapters that return `Some(...)`
    /// get the "unknown field" guard automatically — core rejects
    /// templates with fields the adapter doesn't claim.
    ///
    /// Returning an empty slice `Some(&[])` is a valid declaration
    /// for an adapter that consumes no op fields (e.g. `stdout`
    /// rendering the raw bindings only).
    fn known_op_fields(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// Adapter-specific params keys allowed under an op's
    /// top-level `params` (not `op`) section. Returned keys
    /// extend the core's [`crate::validation::CORE_OP_PARAMS`]
    /// allow-list at op-validation time. Default: empty —
    /// adapters that don't need extras simply rely on the
    /// core vocab. Override to declare adapter-only params
    /// (e.g. CQL's `cl:` consistency-level overrides if those
    /// were lifted from `op` to `params` someday).
    fn known_op_params(&self) -> &'static [&'static str] {
        &[]
    }

    /// Declare adapter-specific dynamic controls (SRD 23) on a
    /// subcomponent attached to the activity's component. Called
    /// once per activity, *after* the activity declares its own
    /// `concurrency` / `rate` controls. The default no-op fits
    /// adapters that have no adapter-level dynamic knobs.
    ///
    /// Convention: each adapter that overrides this attaches a
    /// single subcomponent named after itself (e.g. `cql`,
    /// `http`) under `parent`, and declares all of its dynamic
    /// controls there. That keeps controls reachable from every
    /// descendant scope via the standard SRD-16 walk-up while
    /// giving each adapter a stable component path label.
    ///
    /// The trait method takes an `&dyn DriverAdapter` (i.e.
    /// `&self`), so adapters that hold per-instance state
    /// (handles, atomics) can wire up control appliers that
    /// write into that state. Multiple ops in one activity see
    /// the same adapter instance and the same controls — there
    /// is exactly one subcomponent per adapter per activity.
    fn declare_controls(
        &self,
        _parent: &std::sync::Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
    ) {
    }

    /// Async teardown hook fired by the resource pool when a
    /// shared adapter's last reference detaches (or at session
    /// shutdown). Default: no-op — drop is enough for adapters
    /// whose underlying driver closes synchronously in its
    /// destructor.
    ///
    /// Override to await a driver-specific close handshake.
    /// The CQL adapter's override calls
    /// `Session::close().await` (which wraps
    /// `cass_session_close`) inside a 5-second timeout so a
    /// hung node doesn't pin the runtime.
    ///
    /// The future returned here borrows `&self` for the
    /// duration of the await; callers keep the adapter Arc
    /// alive across the await so the borrow stays valid.
    fn shutdown<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    /// SRD-104 — the adapter's **accessor payload**: a type-erased handle
    /// (`Arc<dyn Any + Send + Sync>`) a kernel node can obtain by fingerprint
    /// via [`polydat::resource_lookup`]. The resource pool surfaces this
    /// through [`crate::resource_pool::SharedResource::accessor_payload`]
    /// (the pool-shared wrapper delegates here) and stores it on the entry at
    /// init. Default `None` — an adapter opts in only when it wants kernels
    /// to reach a live handle (the first consumer is the CQL session handle,
    /// SRD-103). Built over the adapter's own connected session, so the
    /// payload and the op-execution path share one resource.
    fn accessor_payload(&self) -> Option<std::sync::Arc<dyn Any + Send + Sync>> {
        None
    }
}

/// Adapter display preference for TUI activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DisplayPreference {
    /// TUI must not activate — adapter uses raw terminal output.
    Off = 0,
    /// Adapter is compatible with TUI (default for most adapters).
    Auto = 1,
}

/// A metric to display on the activity status line.
pub struct StatusMetric {
    /// Metric name matching the `name` label in `adapter_metrics()` samples.
    pub metric_name: String,
    /// Short display label for the status line (e.g., "rows/s").
    pub display: String,
    /// How to render the value: "rate" (count/elapsed), "count", "latency".
    pub render: StatusRender,
}

/// How to render a status metric value.
pub enum StatusRender {
    /// Show as rate: count / elapsed seconds (e.g., "1.2K/s").
    Rate,
    /// Show as raw count.
    Count,
    /// Show as latency with auto-scaled units.
    Latency,
}

/// A per-template op factory. Created at init time by the adapter's
/// `map_op()`, called per-cycle to bind values and execute operations.
///
/// The dispenser captures template-specific state (prepared statement,
/// field names, bind-point indices, metrics) so the per-cycle path is
/// minimal: bind resolved values and execute.
///
/// Dispensers are shared across fibers and must be thread-safe.
pub trait OpDispenser: Send + Sync {
    /// Execute an operation for the given cycle.
    ///
    /// The `ctx` bundle carries:
    /// - `ctx.fields` — op-field substitution view for the inner adapter
    ///   (positional / by-name access matching the prepared statement).
    /// - `ctx.pulls`  — wrapper-facing handle-indexed view of Polydat values
    ///   (used by validation / conditional / throttle wrappers; adapters
    ///   ignore this).
    ///
    /// See SRD 32 §"`ExecCtx` — cycle-time bundle" for the design.
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    >;

    /// One-line description of the op this dispenser
    /// represents — typically the statement text /
    /// request shape with bind-point placeholders left
    /// unresolved (`{table}`, `{key}`, `?`, …).
    ///
    /// Used by the runtime's error-capture path to attach
    /// the actual op identity to a phase-stop diagnostic.
    /// Without this, an error like `[validation_failed]
    /// op 'indexes_present_cass' …` only names the op
    /// template; the operator can't see what statement
    /// the dispenser is actually firing without grepping
    /// the workload yaml. With this hooked, the captured
    /// reason carries the rendered statement so the
    /// operator can immediately tell whether (a) it's the
    /// wrong dialect's branch firing, (b) the bindpoints
    /// resolved to something unexpected, (c) the
    /// statement is malformed in the workload, etc.
    ///
    /// Returns `None` for dispensers whose op shape isn't
    /// usefully one-lineable (composed wrappers that
    /// delegate, dispensers whose body is multi-paragraph
    /// HTTP, etc.). The runtime falls through to the
    /// inner-dispenser chain when this returns None.
    /// Default: delegate to inner dispenser if any, else
    /// None.
    fn describe(&self) -> Option<String> {
        self.inner_dispenser().and_then(|inner| inner.describe())
    }

    /// Render the actual op the dispenser would fire for the given
    /// cycle — the dryrun-equivalent view of the statement after
    /// every bind point is interpolated.
    ///
    /// Pairs with [`Self::describe`]: `describe()` returns the
    /// op-template (placeholders intact) so the operator can match
    /// the failure to the workload yaml; `describe_resolved(wires)`
    /// returns what was *actually* sent for this cycle, so the
    /// operator can immediately tell whether bindpoints resolved to
    /// expected values, whether the wrong dialect's branch fired,
    /// whether quoting / escaping is broken, etc.
    ///
    /// SRD-68 Push 5: takes the dispenser's bound `WireSource`
    /// (same surface adapters use at cycle time) so the rendered
    /// view comes from the canonical resolution path — no
    /// synthesis-layer ResolvedFields detour.
    ///
    /// Returns `None` when the dispenser can't usefully render the
    /// resolved form (e.g. opaque request bodies, dispensers without
    /// per-cycle interpolation). The default delegates to the inner
    /// dispenser; leaf dispensers should override when they have a
    /// useful per-cycle rendering.
    fn describe_resolved(&self, wires: &dyn crate::wires::WireSource) -> Option<String> {
        self.inner_dispenser()
            .and_then(|inner| inner.describe_resolved(wires))
    }

    /// SRD-68 invariant I-3 — the dispenser's canonical Polydat Kernel,
    /// established at construction by `map_op` from its parent
    /// (with optional matter assembly via `build_subscope`).
    /// Returns `None` for dispensers that don't own a kernel
    /// (adapters with no Polydat needs, or wrappers that delegate to
    /// an inner dispenser).
    ///
    /// The executor walks `Some` returns at fiber spawn to
    /// materialise per-fiber subscope kernels — one per dispenser,
    /// indexed parallel to the dispenser registry. At cycle time
    /// the firing fiber's slot for this dispenser is handed in
    /// via `ExecCtx::wires` so cycle-time reads stay on the SRD-68
    /// I-1 single resolution surface.
    ///
    /// Default: delegate to inner dispenser if any, else `None`.
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<PolydatKernel>> {
        self.inner_dispenser()
            .and_then(|inner| inner.canonical_kernel())
    }

    /// Snapshot adapter-specific metrics for inclusion in the capture
    /// snapshot.
    ///
    /// Called by the metrics scheduler alongside the standard activity
    /// metrics. Adapters return additional `(family_name, labels,
    /// MetricValue)` triples that represent adapter-internal state
    /// (e.g., rows/s for batched CQL). These appear in the summary
    /// report.
    ///
    /// The OpenMetrics-shaped runtime model lives under `nmbrs_metrics::snapshot`
    /// — adapters typically build `MetricValue::Counter` /
    /// `MetricValue::Histogram` / `MetricValue::Gauge` directly.
    /// Default: no additional metrics.
    fn adapter_metrics(
        &self,
    ) -> Vec<(
        String,
        nmbrs_metrics::labels::Labels,
        nmbrs_metrics::snapshot::MetricValue,
    )> {
        if let Some(inner) = self.inner_dispenser() {
            inner.adapter_metrics()
        } else {
            Vec::new()
        }
    }

    /// Adapter-specific status line entries (cumulative, non-destructive read).
    ///
    /// Unlike `adapter_metrics()` which snapshots delta timers, this method
    /// returns cumulative counters safe to read from the progress thread
    /// without interfering with the metrics pipeline.
    /// Returns `(display_name, cumulative_count)` pairs.
    ///
    /// A name with a **leading underscore** (`_batch_writes`) is an INTERNAL
    /// counter: published only to back a derived display metric (the
    /// `rows/batch` average divides `rows_inserted` by `_batch_writes`), it is
    /// looked up by name for that computation but never rendered as its own
    /// `<name>/s` throughput chip. See
    /// [`crate::readout_context::is_internal_counter`] — the single predicate
    /// every chip-rendering surface filters on.
    /// Default: delegates to inner dispenser (for wrapper chains).
    fn status_counters(&self) -> Vec<(&str, u64)> {
        if let Some(inner) = self.inner_dispenser() {
            inner.status_counters()
        } else {
            Vec::new()
        }
    }

    /// The op's uniform per-invocation cursor consumption — how many
    /// consecutive wire ordinals one `execute` call reads and covers.
    ///
    /// The executor drives the phase cursor with `Σ rows_per_op` over
    /// the stanza's ops (instead of the raw stanza length) and hands
    /// each op a contiguous sub-run of exactly this size, so a batch
    /// op that reads `N` rows per call also advances the cursor by
    /// `N` — consecutive stanzas then cover disjoint ordinal runs
    /// (SRD-22 "phase extent = cursor exhaustion", cover-once). A
    /// batch dispenser overrides this to return its fixed stride `N`;
    /// every ordinary op keeps the default `1` (identical to the
    /// pre-batching model).
    ///
    /// The default **delegates to the inner dispenser** so a wrapped
    /// batch op (retry / result / metrics layers) still reports its
    /// leaf stride — the executor sees the outermost wrapper, and the
    /// stride must reach it through the chain (mirrors `describe` /
    /// `adapter_metrics`). Leaves with no inner fall through to `1`.
    ///
    /// This is the NOMINAL stride (what sets `reserve(N)`); at the
    /// cursor tail the reservation may be shorter, and the executor
    /// passes the actual (possibly-short) run length via
    /// [`crate::fixture::ExecCtx::run_len`] so the op inserts exactly
    /// the ordinals reserved — never over-reading, never dropping the
    /// remainder.
    fn rows_per_op(&self) -> usize {
        self.inner_dispenser()
            .map(|inner| inner.rows_per_op())
            .unwrap_or(1)
    }

    /// Returns the wrapped dispenser when this is a
    /// wrapper, `None` when this is a leaf (the adapter's
    /// base dispenser — e.g. CQL raw / prepared / batch,
    /// HTTP, stdout, plotter, …).
    ///
    /// Default returns `None`, which is correct for every
    /// leaf. Adapter-base dispensers should rely on the
    /// default — they have no inner to expose, and asking
    /// them to write `fn inner_dispenser(&self) -> None`
    /// is pure boilerplate.
    ///
    /// **Wrappers MUST override** to return
    /// `Some(self.inner.as_ref())`. Several pieces of
    /// cross-cutting machinery walk this chain:
    /// - `adapter_metrics` and `status_counters` delegate
    ///   through wrapper layers via the default
    ///   implementations on this trait, which call
    ///   `inner_dispenser()` to find the wrapped layer.
    /// - `describe()` walks inward to surface the runtime
    ///   op shape (CQL statement text) for error-context
    ///   dumps; missing this on a wrapper silently breaks
    ///   the walk and the error loses its op-shape line.
    ///
    /// The [`WrappingDispenser`] marker trait below is
    /// the type-system signal that flags "this is a
    /// wrapper"; wrappers should implement both. Future
    /// composition machinery (SRD-32a) will use the
    /// `WrappingDispenser` bound to require the override
    /// at the registration boundary.
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        None
    }
}

/// Marker trait for dispensers that wrap another
/// `OpDispenser`. Implementing it is the type-level
/// commitment that this layer has overridden
/// [`OpDispenser::inner_dispenser`] to expose its inner
/// dispenser. Without that override, cross-cutting
/// machinery (`describe()`, `adapter_metrics`,
/// `status_counters`) silently stops walking at the
/// wrapper, which has been a real source of bugs.
///
/// Pure marker — no methods. The chain-walking code
/// already takes `&dyn OpDispenser` and calls
/// `inner_dispenser()`; this trait exists to make the
/// wrapper-vs-leaf split visible at the type level and
/// to give SRD-32a's wrapper registry a bound to require
/// at composition time.
///
/// Leaves do not implement this trait — their
/// `inner_dispenser` falls through to the default
/// `None`-returning impl on `OpDispenser`, no boilerplate.
pub trait WrappingDispenser: OpDispenser {}

/// Resolved field values for a single cycle. Produced by the GK
/// synthesis pipeline, consumed by the OpDispenser.
///
/// Fields are indexed by name. Typed values are always available;
/// string rendering is deferred until first access to avoid wasted
/// work for adapters that bind typed values natively (e.g., CQL).
///
/// SRD-68 Push 5: this struct survives as an in-adapter rendering
/// container (stdout/testkit/plotter build one locally from
/// [`crate::wires::resolve_op_fields_via_wires`] when they need
/// name-keyed value access). Adapters that bind typed values
/// natively (CQL prepared params, vector arguments) read directly
/// through `wires.get(name)` and don't construct this type.
pub struct ResolvedFields {
    /// Field names in op template declaration order.
    pub names: Vec<String>,
    /// Typed values, parallel to `names`.
    pub values: Vec<polydat::ast::Value>,
    /// Lazily rendered string representations, parallel to `names`.
    strings: OnceLock<Vec<String>>,
}

impl fmt::Debug for ResolvedFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedFields")
            .field("names", &self.names)
            .field("values", &self.values)
            .finish()
    }
}

impl Clone for ResolvedFields {
    fn clone(&self) -> Self {
        Self {
            names: self.names.clone(),
            values: self.values.clone(),
            strings: self.strings.clone(),
        }
    }
}

impl ResolvedFields {
    /// Create with names and typed values. Strings are lazily rendered.
    pub fn new(names: Vec<String>, values: Vec<polydat::ast::Value>) -> Self {
        Self {
            names,
            values,
            strings: OnceLock::new(),
        }
    }

    /// Access the lazily-rendered string representations.
    /// Computed once on first call, then cached.
    pub fn strings(&self) -> &[String] {
        self.strings
            .get_or_init(|| self.values.iter().map(|v| v.to_display_string()).collect())
    }

    /// Get a field value by name as a string.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| self.strings()[i].as_str())
    }

    /// Get a field value by name as a typed Value.
    pub fn get_value(&self, name: &str) -> Option<&polydat::ast::Value> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| &self.values[i])
    }

    /// Get a string by index (triggers lazy rendering if needed).
    pub fn str_at(&self, index: usize) -> &str {
        &self.strings()[index]
    }

    /// Return a copy with the named field removed.
    /// Used by `ConditionalDispenser` to strip internal fields
    /// before the adapter sees them.
    pub fn without(&self, name: &str) -> Self {
        let mut names = Vec::new();
        let mut values = Vec::new();
        for (i, n) in self.names.iter().enumerate() {
            if n != name {
                names.push(n.clone());
                values.push(self.values[i].clone());
            }
        }
        Self::new(names, values)
    }

    /// Serialize all fields to JSON for diagnostic/logging use.
    pub fn to_json(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .names
            .iter()
            .zip(self.values.iter())
            .map(|(name, value)| {
                let json_val = match value {
                    polydat::ast::Value::U64(v) => serde_json::Value::Number((*v).into()),
                    polydat::ast::Value::F64(v) => serde_json::Number::from_f64(*v)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                    polydat::ast::Value::Bool(v) => serde_json::Value::Bool(*v),
                    _ => serde_json::Value::String(value.to_display_string()),
                };
                (name.clone(), json_val)
            })
            .collect();
        serde_json::Value::Object(map)
    }
}

/// Capture point declaration in an op template.
///
/// Parsed from `[name]`, `[source as alias]`, or `[(Type)name]` syntax.
#[derive(Debug, Clone)]
pub struct CaptureDecl {
    /// The field name in the operation result.
    pub source_name: String,
    /// The name under which the value is stored in the capture context.
    pub as_name: String,
    /// Optional type qualifier for validation.
    pub type_qualifier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_fields_lazy_strings() {
        let fields = ResolvedFields::new(
            vec!["a".into(), "b".into()],
            vec![polydat::ast::Value::U64(42), polydat::ast::Value::F64(3.5)],
        );
        // Strings not computed yet
        assert!(fields.strings.get().is_none());
        // First access triggers rendering
        assert_eq!(fields.get_str("a"), Some("42"));
        assert!(fields.strings.get().is_some());
        assert_eq!(fields.get_str("b"), Some("3.5"));
    }

    #[test]
    fn resolved_fields_get_value() {
        let fields = ResolvedFields::new(vec!["x".into()], vec![polydat::ast::Value::F64(3.5)]);
        match fields.get_value("x") {
            Some(polydat::ast::Value::F64(v)) => assert!((v - 3.5).abs() < 1e-10),
            other => panic!("expected F64(3.5), got {other:?}"),
        }
        // get_value doesn't trigger string rendering
        assert!(fields.strings.get().is_none());
    }

    #[test]
    fn execution_error_display() {
        let op_err = ExecutionError::Op(AdapterError {
            error_name: "Timeout".into(),
            message: "timed out".into(),
            retryable: true,
        });
        assert!(format!("{op_err}").contains("[op]"));
        assert!(!op_err.is_adapter_level());

        let adapter_err = ExecutionError::Adapter(AdapterError {
            error_name: "ConnectionRefused".into(),
            message: "refused".into(),
            retryable: false,
        });
        assert!(format!("{adapter_err}").contains("[adapter]"));
        assert!(adapter_err.is_adapter_level());
    }
}

// =========================================================================
// Adapter Registration (inventory-based, link-time collection)
// =========================================================================

/// An adapter module's registration, submitted at link time via `inventory`.
///
/// Each adapter crate (nmbrs-adapter-stdout, nmbrs-adapter-cql, etc.)
/// submits one of these. The shared runner collects all submissions to
/// build the adapter dispatch table without any explicit adapter list.
pub struct AdapterRegistration {
    /// Driver names this adapter responds to (e.g., `&["stdout"]` or `&["cql", "cassandra"]`).
    pub names: fn() -> &'static [&'static str],
    /// Extra param names this adapter accepts (for CLI validation).
    pub known_params: fn() -> &'static [&'static str],
    /// Display preference for TUI activation. Checked at startup before
    /// any adapter is constructed — no connection overhead. Takes the
    /// run params so an adapter whose terminal use depends on config can
    /// decide (e.g. `stdout` wants the TUI off when it writes op output
    /// to the console, but is TUI-compatible when `filename=` redirects
    /// to a file).
    pub display_preference: fn(&std::collections::HashMap<String, String>) -> DisplayPreference,
    /// SRD-23 — the dynamic controls this adapter can declare, as static
    /// [`ControlDesc`](crate::control_catalog::ControlDesc) capability
    /// descriptors. This is the *discovery* surface (`nmbrs describe controls`
    /// / `describe adapter=<name>`), read without constructing the adapter;
    /// the adapter's [`declare_controls`](DriverAdapter::declare_controls)
    /// *derives* the live control from the same descriptors so the two cannot
    /// drift. Default `|| &[]` for adapters with no dynamic knobs.
    pub supported_controls: fn() -> &'static [crate::control_catalog::ControlDesc],
    /// Async factory: given params, create the adapter.
    /// Returns a boxed future so async connect is supported (e.g., CQL).
    pub create: fn(std::collections::HashMap<String, String>) -> CreateAdapterFuture,
}

inventory::collect!(AdapterRegistration);

/// Look up an adapter by driver name from all link-time registrations.
pub fn find_adapter_registration(driver: &str) -> Option<&'static AdapterRegistration> {
    inventory::iter::<AdapterRegistration>
        .into_iter()
        .find(|&reg| (reg.names)().contains(&driver))
        .map(|v| v as _)
}

/// List all registered driver names.
pub fn registered_driver_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for reg in inventory::iter::<AdapterRegistration> {
        names.extend_from_slice((reg.names)());
    }
    names
}

/// Look up the display preference for a driver name without constructing the adapter.
///
/// Returns `Auto` if the driver is not registered (unknown adapters are
/// assumed TUI-compatible; construction will fail later with a clear error).
pub fn adapter_display_preference(
    driver: &str,
    params: &std::collections::HashMap<String, String>,
) -> DisplayPreference {
    find_adapter_registration(driver)
        .map(|reg| (reg.display_preference)(params))
        .unwrap_or(DisplayPreference::Auto)
}

/// Collect all extra known params from registered adapters,
/// unioned with driver-implementation–specific params
/// contributed via [`DriverImpl`] entries (e.g. CQL's `hosts`,
/// `port`, `keyspace`, ...) so they don't trip the
/// "unrecognized parameter" guard at the CLI layer.
pub fn registered_adapter_params() -> Vec<&'static str> {
    let mut params = Vec::new();
    for reg in inventory::iter::<AdapterRegistration> {
        params.extend_from_slice((reg.known_params)());
    }
    for entry in inventory::iter::<DriverImpl> {
        params.extend_from_slice((entry.known_params)());
    }
    params
}

// =========================================================================
// Driver implementations
// =========================================================================
//
// Some adapters (e.g. `cql`) are backed by more than one
// internal driver implementation — for CQL these are `scylla`
// (pure Rust) and `cassandra-cpp` (Apache Cassandra C++ driver via FFI).
// The adapter is a single user-facing concept; the driver is an
// internal implementation detail surfaced through a per-adapter
// selector parameter (e.g. `cqldriver=scylla`).

/// One driver implementation registered for an adapter.
///
/// Each driver contributes a [`DriverImpl`] to the inventory.
/// At session start the runner picks one — by user override
/// (e.g. `cqldriver=scylla`) or by default ranking — and calls
/// its `create` factory. The resulting [`DriverAdapter`] wears
/// the **adapter's** name (e.g. `"cql"`), not the driver's; the
/// driver name is internal and never appears in op-level
/// `adapter:` fields.
///
/// ```ignore
/// // In a driver module under nmbrs-adapter-cql:
/// inventory::submit! {
///     nmbrs_runtime::adapter::DriverImpl {
///         adapter: "cql",
///         driver: "scylla",
///         default_rank: 200,
///         create: |params| Box::pin(async move { ... }),
///         known_params: || &["hosts", "port", ...],
///     }
/// }
/// ```
pub struct DriverImpl {
    /// The adapter this driver implements (e.g. `"cql"`).
    /// Matches the [`AdapterRegistration::names`] of the
    /// adapter the user selects with `adapter=…`.
    pub adapter: &'static str,
    /// The driver identifier. Surfaces in
    /// `<adapter>driver=<driver>` for user-facing selection
    /// (e.g. `cqldriver=scylla`). Internal — never appears as
    /// a top-level adapter name.
    pub driver: &'static str,
    /// Lower wins when no user override is given. Drivers pick
    /// their own rank — convention is to space ranks by 100s
    /// so future drivers can slot between them.
    pub default_rank: u32,
    /// Async factory for this driver. Same shape as
    /// [`AdapterRegistration::create`] — given workload params,
    /// connect and return a boxed `DriverAdapter` whose
    /// [`DriverAdapter::name`] is the *adapter* name.
    pub create: fn(std::collections::HashMap<String, String>) -> CreateAdapterFuture,
    /// Driver-specific param names — unioned into the
    /// adapter's known-params surface for CLI validation.
    pub known_params: fn() -> &'static [&'static str],
}

inventory::collect!(DriverImpl);

/// SRD-35 Push B: declares that a `(adapter, driver)` pair
/// supports pool-shared instances. Drivers that submit one
/// of these opt into the resource pool's `Shared` policy
/// path: instead of a fresh `Arc<dyn DriverAdapter>` per
/// phase, the pool caches the adapter under
/// [`Self::resource_key`] and reuses it across every phase
/// whose params produce the same key.
///
/// Drivers without a `SharedDriverRegistration` continue
/// to use Push A's `LegacyAdapterResource` shim, which is
/// `PerPhase`-isolated. Migration is opt-in per driver.
pub struct SharedDriverRegistration {
    /// Adapter name this registration applies to (e.g.
    /// `"cql"`). Pairs with [`Self::driver`] for lookup.
    pub adapter: &'static str,
    /// Driver identifier (e.g. `"cassandra-cpp"`,
    /// `"scylla"`). Distinguishes registrations when the
    /// same adapter has multiple driver implementations.
    pub driver: &'static str,
    /// Strongest sharing the driver supports. Default
    /// `Shared` for typical pool-friendly drivers; stricter
    /// only if the driver type can't be safely shared.
    pub share_capability: crate::resource_pool::ShareCapability,
    /// Pure function that derives the resource key from
    /// the adapter's params. Two phases with the same key
    /// share an instance under `Shared` policy. SRD-35
    /// §"Instance-shaping vs shell-shaping params" — only
    /// instance-shaping params (`hosts`, `keyspace`, …)
    /// belong here; per-statement and per-phase knobs MUST
    /// NOT.
    pub resource_key: fn(
        &std::collections::HashMap<String, String>,
    ) -> Result<crate::resource_pool::ResourceKey, String>,
}

inventory::collect!(SharedDriverRegistration);

/// Look up the shared registration for a `(adapter, driver)`
/// pair. Returns `None` when the driver hasn't migrated to
/// the pool-shared shape — the executor falls back to the
/// `LegacyAdapterResource` shim under `PerPhase` policy in
/// that case.
pub fn find_shared_driver(
    adapter: &str,
    driver: &str,
) -> Option<&'static SharedDriverRegistration> {
    inventory::iter::<SharedDriverRegistration>
        .into_iter()
        .find(|e| e.adapter == adapter && e.driver == driver)
}

/// Default order of drivers registered for `adapter`, sorted by
/// ascending [`DriverImpl::default_rank`]. Used as the fallback
/// when the user doesn't set the adapter's driver-selector
/// parameter.
pub fn default_drivers(adapter: &str) -> Vec<&'static str> {
    let mut entries: Vec<&'static DriverImpl> = inventory::iter::<DriverImpl>
        .into_iter()
        .filter(|e| e.adapter == adapter)
        .collect();
    entries.sort_by_key(|e| e.default_rank);
    entries.into_iter().map(|e| e.driver).collect()
}

/// Find a driver implementation by `(adapter, driver)`. Used
/// by the runner to pick the right factory after resolving the
/// driver-selector parameter.
pub fn find_driver(adapter: &str, driver: &str) -> Option<&'static DriverImpl> {
    inventory::iter::<DriverImpl>
        .into_iter()
        .find(|e| e.adapter == adapter && e.driver == driver)
}

/// Union of every driver-specific known-param for `adapter`.
/// Surfaced in CLI validation so unknown-param warnings don't
/// fire for driver-private knobs (e.g. CQL's `hosts`, `port`,
/// `keyspace`).
pub fn adapter_driver_params(adapter: &str) -> Vec<&'static str> {
    let mut params: Vec<&'static str> = Vec::new();
    for e in inventory::iter::<DriverImpl>
        .into_iter()
        .filter(|e| e.adapter == adapter)
    {
        params.extend_from_slice((e.known_params)());
    }
    params
}

/// Pick a driver for `adapter` (user-supplied selector or
/// default ranking) and instantiate it.
///
/// `selector_param` is the workload param a user sets to override
/// the default — e.g. `"cqldriver"` for the `cql` adapter.
/// Single-name semantics by convention; comma-separated lists
/// are accepted (the runner walks them in order and takes the
/// first that's compiled in).
/// Sentinel driver name for adapters that don't have
/// multiple `DriverImpl` registrations (HTTP, stdout,
/// testkit, openapi — anything that registers only via
/// [`AdapterRegistration`] with a direct `create`
/// factory). The shared-pool path keys
/// `SharedDriverRegistration` by `(adapter, driver)`;
/// single-engine adapters submit theirs with this sentinel
/// so the executor's lookup finds them after
/// [`resolve_driver_name`] returns the same sentinel.
pub const DEFAULT_DRIVER_NAME: &str = "default";

/// Resolve the driver name for an `(adapter, params)` pair
/// without instantiating anything. Used by the resource
/// pool's Push B path: the executor needs the resolved
/// driver name to look up a [`SharedDriverRegistration`]
/// before deciding which attach path to use. Mirrors the
/// resolution logic in [`instantiate_with_driver`] —
/// user-supplied selector first (comma-separated, in
/// order), then the rank-sorted default list.
///
/// Returns [`DEFAULT_DRIVER_NAME`] when no `DriverImpl` is
/// registered for the adapter (single-engine adapters that
/// only register via `AdapterRegistration`). Single-engine
/// adapters that opt into pool sharing submit their
/// `SharedDriverRegistration` with the same sentinel.
pub fn resolve_driver_name(
    adapter: &str,
    selector_param: &str,
    params: &std::collections::HashMap<String, String>,
) -> Option<&'static str> {
    let user_order: Option<Vec<&str>> = params.get(selector_param).map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let default_order = default_drivers(adapter);
    let order: Vec<&str> = match &user_order {
        Some(v) => v.clone(),
        None => default_order.to_vec(),
    };
    for driver in &order {
        if let Some(entry) = find_driver(adapter, driver) {
            return Some(entry.driver);
        }
    }
    // Single-engine adapter (no DriverImpl registered, just
    // a direct AdapterRegistration). Fall back to the
    // sentinel so SharedDriverRegistration lookup can find
    // a `(adapter, "default")` entry.
    Some(DEFAULT_DRIVER_NAME)
}

pub async fn instantiate_with_driver(
    adapter: &str,
    selector_param: &str,
    params: std::collections::HashMap<String, String>,
) -> Result<std::sync::Arc<dyn DriverAdapter>, String> {
    let user_order: Option<Vec<&str>> = params.get(selector_param).map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let default_order = default_drivers(adapter);
    let order: Vec<&str> = match &user_order {
        Some(v) => v.clone(),
        None => default_order.clone(),
    };
    if order.is_empty() {
        return Err(format!(
            "adapter '{adapter}': no driver implementations registered. \
             Build the binary with at least one driver feature enabled."
        ));
    }
    for driver in &order {
        if let Some(entry) = find_driver(adapter, driver) {
            return (entry.create)(params).await;
        }
    }
    Err(format!(
        "adapter '{adapter}': no driver in {selector_param}='{}' is compiled in; \
         available drivers: [{}]",
        order.join(","),
        default_order.join(", "),
    ))
}
