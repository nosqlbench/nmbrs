// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Dynamic controls — see SRD 23.
//!
//! A [`Control<T>`] is a named, typed, observable cell that
//! coordinates a **confirmed-apply** write protocol across a
//! set of registered [`ControlApplier<T>`] consumers. A caller
//! that [`Control::set`]s a new value does not proceed until
//! every applier has acknowledged the change is in effect; if
//! any applier fails or times out, the committed value is not
//! advanced and the writer sees the aggregated error.
//!
//! Controls are structural runtime properties of a component
//! (SRD 24): they are declared when the owning component /
//! wrapper / dispenser is instantiated, enumerable through the
//! component tree, and looked up — never implicitly created —
//! by writers.
//!
//! Follow-up integrations:
//! - **Gauge reification.** Each control's current value is
//!   also published as a numeric gauge under the metric name
//!   `control.<name>`, visible through every metric sink that
//!   reads the component tree (SRD 23 §"Reification as
//!   metrics"). Opt in via [`ControlBuilder::reify_as_gauge`].
//! - **Adapter-side integrations** (rate limiter, fiber
//!   executor): the control primitive works end-to-end with
//!   its tests; wiring it into `nmbrs-rate` and the fiber pool
//!   lands in a follow-up pass.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::future::join_all;
use tokio::sync::Mutex as AsyncMutex;

use crate::instruments::gauge::ValueGauge;
use crate::labels::Labels;
use crate::snapshot::MetricSet;

// =========================================================================
// Rev allocator (session-wide monotonic revision counter)
// =========================================================================

/// A session-wide revision counter. Every successful control
/// write allocates a fresh `rev` via [`Self::next`].
///
/// Callers who build a session share one `RevAllocator` across
/// every `Control` in that session; the counter is strictly
/// increasing and usable for log ordering or reverse lookups.
///
/// When no explicit allocator is wired (e.g. tests, single-
/// purpose uses), a process-global allocator is available via
/// [`RevAllocator::global`].
#[derive(Debug, Default)]
pub struct RevAllocator {
    counter: AtomicU64,
}

impl RevAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    /// Shared allocator used when no session-scoped one is
    /// explicitly supplied. Useful for tests and for short-lived
    /// single-session processes — production runs should give
    /// each session its own allocator.
    pub fn global() -> &'static Arc<RevAllocator> {
        static GLOBAL: OnceLock<Arc<RevAllocator>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(RevAllocator::new()))
    }
}

// =========================================================================
// Origin, Versioned<T>, error types
// =========================================================================

/// Attribution for a control write. Every committed `Versioned`
/// remembers who caused the change so log / replay / summary
/// tools can explain "where did this come from?" after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlOrigin {
    /// Initial seed from `params:` at scenario start.
    Launch,
    /// Future `nmbrs ctl` CLI mutation.
    Cli,
    /// Keybind / input in the TUI.
    Tui,
    /// Scripted feedback loop (GK `control_set(...)` node).
    Polydat { binding: String },
    /// Runtime governor (e.g. the phase `throttle:` backpressure
    /// loop) — `source` names the governing phase.
    Governor { source: String },
    /// External API — `source` identifies the caller (endpoint, auth id, etc.)
    Api { source: String },
    /// Test harness — never seen in production.
    Test,
}

/// A committed control value bundled with its metadata.
#[derive(Clone, Debug)]
pub struct Versioned<T: Clone> {
    pub value: T,
    /// Session-wide monotonic revision. Strictly increasing
    /// across every successful write to every control sharing
    /// a [`RevAllocator`].
    pub rev: u64,
    /// Wall-clock time at which the commit happened.
    pub updated_at: Instant,
    pub origin: ControlOrigin,
}

/// Error outcomes returned by [`Control::set`]. A write can
/// reject at validation (before fan-out), at apply (during
/// fan-out), or up front because the control is `final` at its
/// declaring scope. In every case the committed value is
/// unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetError {
    /// The pre-fanout validator rejected the value. No applier
    /// was called. Message explains why.
    ValidationFailed(String),
    /// One or more appliers returned `Err` (or timed out). The
    /// list carries the index + error message for each failure.
    ApplyFailed(Vec<ApplyFailure>),
    /// The control was declared `final` at `scope` and is
    /// pinned to its Launch value. Runtime writes (any origin
    /// other than `Launch`) are rejected.
    FinalViolation { scope: String },
}

/// Declared visibility scope of a [`Control`]. Governs how
/// descendant components resolve the control and how `set`
/// writes propagate to newly-instantiated components below the
/// declaring node. See SRD 23 §"Branch-scoped and final
/// controls".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchScope {
    /// Control applies only to the component that declares it.
    /// Descendants resolve the same name only if they declare
    /// it themselves (or if an ancestor declares it with
    /// [`BranchScope::Subtree`]).
    Local,
    /// Control applies to the declaring component and every
    /// descendant in the subtree. Descendants resolving the
    /// control name walk up and find this declaration;
    /// descendant components constructed after a successful
    /// write read the new committed value at construction.
    Subtree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyFailure {
    /// Registration index of the failing applier (0-based, in
    /// insertion order). The applier itself is opaque to the
    /// writer — the index plus the component the control lives
    /// on is enough to locate the offending subscriber.
    pub applier_index: usize,
    /// Why the apply failed — either the `Err` the applier
    /// returned or a timeout notice.
    pub message: String,
}

impl fmt::Display for SetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValidationFailed(msg) => write!(f, "validation failed: {msg}"),
            Self::ApplyFailed(failures) => {
                write!(f, "{} applier(s) failed:", failures.len())?;
                for fail in failures {
                    write!(f, " [#{}: {}]", fail.applier_index, fail.message)?;
                }
                Ok(())
            }
            Self::FinalViolation { scope } => {
                write!(
                    f,
                    "control is declared final at scope '{scope}'; runtime writes are rejected"
                )
            }
        }
    }
}

impl std::error::Error for SetError {}

// =========================================================================
// ControlApplier trait
// =========================================================================

/// Async callback registered on a [`Control`]. Each call attempts
/// to apply a new value and returns `Ok(())` once the new value
/// is in effect for this subscriber, or `Err(msg)` if the apply
/// could not be completed.
///
/// Implementors should keep their own state consistent with
/// either the old or the new value; partial / corrupted apply
/// states are outside the protocol.
pub trait ControlApplier<T>: Send + Sync + 'static
where
    T: Clone + Send + Sync + 'static,
{
    fn apply(&self, value: T) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

/// Convenience wrapper for synchronous appliers. Most appliers
/// just set an atomic or reconfigure a rate limiter and have no
/// genuine async work to do — this wrapper lifts a plain
/// `Fn(T) -> Result<(), String>` into a [`ControlApplier<T>`].
pub struct SyncApplier<T, F>
where
    T: Clone + Send + Sync + 'static,
    F: Fn(T) -> Result<(), String> + Send + Sync + 'static,
{
    f: F,
    _marker: std::marker::PhantomData<fn(T)>,
}

impl<T, F> SyncApplier<T, F>
where
    T: Clone + Send + Sync + 'static,
    F: Fn(T) -> Result<(), String> + Send + Sync + 'static,
{
    pub fn new(f: F) -> Self {
        Self {
            f,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, F> ControlApplier<T> for SyncApplier<T, F>
where
    T: Clone + Send + Sync + 'static,
    F: Fn(T) -> Result<(), String> + Send + Sync + 'static,
{
    fn apply(&self, value: T) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let out = (self.f)(value);
        Box::pin(async move { out })
    }
}

// =========================================================================
// Control<T>
// =========================================================================

type Validator<T> = Box<dyn Fn(&T) -> Result<(), String> + Send + Sync>;
type ToF64<T> = Box<dyn Fn(&T) -> Option<f64> + Send + Sync>;
type FromF64<T> = Box<dyn Fn(f64) -> Result<T, String> + Send + Sync>;

pub struct Control<T: Clone + Send + Sync + 'static> {
    inner: Arc<ControlInner<T>>,
}

impl<T: Clone + Send + Sync + 'static> Clone for Control<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct ControlInner<T: Clone + Send + Sync + 'static> {
    name: String,
    /// Serializes writers across validate → fan-out → ack →
    /// commit. Held by `set()` for its entire duration so two
    /// writers can't overlap each other's fan-outs.
    write_lock: AsyncMutex<()>,
    /// Current committed value. Readers take a snapshot under
    /// this RwLock; writers update it only on a fully-successful
    /// apply.
    committed: std::sync::RwLock<Versioned<T>>,
    appliers: Mutex<Vec<Arc<dyn ControlApplier<T>>>>,
    validator: OnceLock<Validator<T>>,
    apply_timeout: Duration,
    rev_allocator: Arc<RevAllocator>,
    /// Optional gauge reification — when declared via
    /// [`ControlBuilder::reify_as_gauge`], the control publishes
    /// its current value as a numeric gauge. The gauge is
    /// `Arc`-shared so readers that want to sample without going
    /// through the control machinery can do so directly.
    gauge: Option<GaugeReification<T>>,
    /// When `Some(scope)`, the control is pinned: only the
    /// Launch-origin seeding writes succeed; every subsequent
    /// runtime write returns [`SetError::FinalViolation`] with
    /// this scope name so the operator knows where the pin is
    /// declared.
    final_at_scope: Option<String>,
    /// Declared visibility of this control within the component
    /// tree.
    branch_scope: BranchScope,
    /// Optional converter from `f64` to `T`. Enables type-erased
    /// writes from Polydat `control_set(name, value)` nodes and the
    /// web API's JSON-number body. Controls that don't declare a
    /// converter reject `f64` writes with `ValidationFailed`.
    from_f64: Option<FromF64<T>>,
}

struct GaugeReification<T> {
    gauge: Arc<ValueGauge>,
    to_f64: ToF64<T>,
}

impl<T: Clone + Send + Sync + 'static> Control<T> {
    /// Name of this control within its owning component's
    /// registry.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Borrow the current committed value.
    pub fn get(&self) -> Versioned<T> {
        self.inner
            .committed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Convenience shortcut for `self.get().value` when the
    /// caller doesn't need the metadata.
    pub fn value(&self) -> T {
        self.get().value
    }

    /// Register an applier. Every subsequent [`Self::set`] call
    /// will include this applier in its fanout. There is no
    /// unregister — appliers live for the control's lifetime.
    /// Returns the insertion index so callers can correlate
    /// later failure reports.
    pub fn register_applier<A>(&self, applier: A) -> usize
    where
        A: ControlApplier<T>,
    {
        let arc: Arc<dyn ControlApplier<T>> = Arc::new(applier);
        let mut g = self
            .inner
            .appliers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        g.push(arc);
        g.len() - 1
    }

    /// Number of currently-registered appliers.
    pub fn applier_count(&self) -> usize {
        self.inner
            .appliers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Atomic write. Returns `Ok(rev)` with the new revision
    /// only if every registered applier acknowledged the new
    /// value within the control's apply timeout. On any failure
    /// the committed value is not advanced.
    ///
    /// A control declared `final` via
    /// [`ControlBuilder::final_at_scope`] rejects every write
    /// whose origin is not [`ControlOrigin::Launch`] with
    /// [`SetError::FinalViolation`]. The Launch-seed write
    /// still goes through so the initial value can be
    /// installed; runtime writers (CLI, TUI, GK, API) are
    /// rejected against the pin.
    pub async fn set(&self, value: T, origin: ControlOrigin) -> Result<u64, SetError> {
        let _guard = self.inner.write_lock.lock().await;

        // 0. Final-declaration pin. Checked before validation
        //    so operators see the pin message rather than a
        //    validation error that happens to also reject the
        //    value.
        if let Some(ref scope) = self.inner.final_at_scope
            && origin != ControlOrigin::Launch
        {
            return Err(SetError::FinalViolation {
                scope: scope.clone(),
            });
        }

        // 1. Validation runs before any applier is called.
        if let Some(validator) = self.inner.validator.get() {
            validator(&value).map_err(SetError::ValidationFailed)?;
        }

        // 2. Fan out to all appliers concurrently, each with the
        //    per-control timeout. A missing apply (timeout) is
        //    treated as a failure just like an explicit Err.
        let appliers: Vec<Arc<dyn ControlApplier<T>>> = {
            let g = self
                .inner
                .appliers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            g.clone()
        };

        let timeout = self.inner.apply_timeout;
        let futures = appliers.iter().enumerate().map(|(idx, applier)| {
            let v = value.clone();
            let applier = applier.clone();
            async move {
                let fut = applier.apply(v);
                match tokio::time::timeout(timeout, fut).await {
                    Ok(Ok(())) => Ok(idx),
                    Ok(Err(msg)) => Err(ApplyFailure {
                        applier_index: idx,
                        message: msg,
                    }),
                    Err(_) => Err(ApplyFailure {
                        applier_index: idx,
                        message: format!("apply timed out after {:?}", timeout),
                    }),
                }
            }
        });

        let results = join_all(futures).await;
        let failures: Vec<ApplyFailure> = results.into_iter().filter_map(|r| r.err()).collect();

        if !failures.is_empty() {
            return Err(SetError::ApplyFailed(failures));
        }

        // 3. All appliers succeeded — allocate a rev and commit.
        let rev = self.inner.rev_allocator.next();
        let versioned = Versioned {
            value: value.clone(),
            rev,
            updated_at: Instant::now(),
            origin,
        };
        *self
            .inner
            .committed
            .write()
            .unwrap_or_else(|e| e.into_inner()) = versioned;
        // 4. Publish the new value to the reified gauge (if any).
        //    This lands after commit so a reader that samples the
        //    gauge concurrently always sees a value at least as
        //    new as the committed `Versioned` — never newer.
        self.publish_gauge(&value);

        Ok(rev)
    }

    /// Current reified-gauge value, if the control opted in via
    /// [`ControlBuilder::reify_as_gauge`] and the conversion
    /// returns `Some`. The registry snapshots this into a
    /// `MetricSet` so control values flow through the normal
    /// metric sinks.
    pub fn gauge_f64(&self) -> Option<f64> {
        let g = self.inner.gauge.as_ref()?;
        let v = self.get().value;
        (g.to_f64)(&v)
    }

    /// Raw handle to the reified [`ValueGauge`] for callers that
    /// want to sample the gauge directly without going through
    /// the control machinery (e.g. registering it on a
    /// [`crate::component::Component`] via
    /// `register_instrument`). `None` if the control wasn't
    /// declared with gauge reification.
    pub fn reified_gauge(&self) -> Option<Arc<ValueGauge>> {
        self.inner.gauge.as_ref().map(|g| g.gauge.clone())
    }

    /// `true` if the control was declared `final` via
    /// [`ControlBuilder::final_at_scope`]. Final controls
    /// reject every non-Launch write.
    pub fn is_final(&self) -> bool {
        self.inner.final_at_scope.is_some()
    }

    /// Name of the scope at which the control was declared
    /// `final`, if any. `None` for non-final controls.
    pub fn final_scope(&self) -> Option<&str> {
        self.inner.final_at_scope.as_deref()
    }

    /// Declared branch scope of this control.
    pub fn branch_scope(&self) -> BranchScope {
        self.inner.branch_scope
    }

    fn publish_gauge(&self, value: &T) {
        if let Some(ref g) = self.inner.gauge
            && let Some(f) = (g.to_f64)(value)
        {
            g.gauge.set(f);
        }
    }
}

// =========================================================================
// ControlBuilder
// =========================================================================

/// Construct a [`Control<T>`]. Every control needs an initial
/// value, a name, and a rev allocator. Apply timeout and
/// validator are optional.
pub struct ControlBuilder<T: Clone + Send + Sync + 'static> {
    name: String,
    initial: T,
    rev_allocator: Arc<RevAllocator>,
    apply_timeout: Duration,
    validator: Option<Validator<T>>,
    reify: Option<ToF64<T>>,
    final_at_scope: Option<String>,
    branch_scope: BranchScope,
    from_f64: Option<FromF64<T>>,
}

impl<T: Clone + Send + Sync + 'static> ControlBuilder<T> {
    pub fn new(name: &str, initial: T) -> Self {
        Self {
            name: name.to_string(),
            initial,
            rev_allocator: RevAllocator::global().clone(),
            // Five seconds is long enough for a sluggish reconfigure
            // (e.g. restarting a rate limiter) but short enough that
            // a stuck subscriber surfaces as an error well within a
            // human operator's attention span.
            apply_timeout: Duration::from_secs(5),
            validator: None,
            reify: None,
            final_at_scope: None,
            branch_scope: BranchScope::Local,
            from_f64: None,
        }
    }

    pub fn rev_allocator(mut self, allocator: Arc<RevAllocator>) -> Self {
        self.rev_allocator = allocator;
        self
    }

    pub fn apply_timeout(mut self, d: Duration) -> Self {
        self.apply_timeout = d;
        self
    }

    /// Register a pre-fanout validator. Bad values reject the
    /// write before any applier is called. Typical use: bounds
    /// checking (`0 < concurrency <= 10_000`), format parsing.
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    /// Publish the control's current value as a numeric gauge.
    /// `to_f64` converts each committed value to an `f64` (or
    /// `None` to suppress that sample — useful for enum-valued
    /// controls where some variants don't have a meaningful
    /// numeric projection). The reified gauge is captured
    /// alongside instrument-emitted metrics by the tree walk —
    /// consumers (SQLite, VictoriaMetrics push, summary report,
    /// TUI) pick it up without any extra wiring.
    ///
    /// Metric name: `control.<control_name>`. Labels: the
    /// effective-labels of the component the control lives on.
    pub fn reify_as_gauge<F>(mut self, to_f64: F) -> Self
    where
        F: Fn(&T) -> Option<f64> + Send + Sync + 'static,
    {
        self.reify = Some(Box::new(to_f64));
        self
    }

    /// Pin the control's value at the declaring scope. The
    /// Launch-origin seed write still goes through so the
    /// initial value can be installed; every subsequent runtime
    /// write returns [`SetError::FinalViolation`] with
    /// `scope_name` so the operator can see where the pin lives.
    ///
    /// Use when a workload (or a parent scope) needs to
    /// guarantee that a runtime writer cannot override a value
    /// this scope has chosen — e.g. a DDL phase that
    /// deliberately runs at `concurrency=1` regardless of what
    /// the operator nudges the session-wide concurrency to.
    pub fn final_at_scope(mut self, scope_name: impl Into<String>) -> Self {
        self.final_at_scope = Some(scope_name.into());
        self
    }

    /// Declare the visibility of this control within the
    /// component tree. Defaults to [`BranchScope::Local`].
    ///
    /// [`BranchScope::Subtree`] means every descendant
    /// component resolves the control name through walk-up;
    /// descendants constructed after a successful write read
    /// the new committed value at construction time. See
    /// SRD 23 §"Branch-scoped and final controls".
    pub fn branch_scope(mut self, scope: BranchScope) -> Self {
        self.branch_scope = scope;
        self
    }

    /// Register a converter that lets type-erased writers
    /// (GK `control_set`, web API JSON bodies) push `f64`
    /// values into this control. Without a converter,
    /// f64 writes return [`SetError::ValidationFailed`] with
    /// a "no f64 setter" message.
    ///
    /// For `Control<f64>` a trivial converter is `|v| Ok(v)`;
    /// for `Control<u32>` a reasonable converter validates the
    /// range (`if v < 0 || v > u32::MAX as f64 { Err(...) }`)
    /// and casts. More complex types (e.g. a rate spec) can
    /// interpret `f64` as ops/sec and synthesize a fresh
    /// domain value.
    pub fn from_f64<F>(mut self, f: F) -> Self
    where
        F: Fn(f64) -> Result<T, String> + Send + Sync + 'static,
    {
        self.from_f64 = Some(Box::new(f));
        self
    }

    pub fn build(self) -> Control<T> {
        let versioned = Versioned {
            value: self.initial.clone(),
            rev: 0,
            updated_at: Instant::now(),
            origin: ControlOrigin::Launch,
        };
        let validator_slot: OnceLock<Validator<T>> = OnceLock::new();
        if let Some(v) = self.validator {
            let _ = validator_slot.set(v);
        }
        // If the caller opted into gauge reification, seed the
        // ValueGauge with the initial value (skipping when the
        // conversion returns None).
        let gauge = self.reify.map(|to_f64| {
            let gauge = Arc::new(ValueGauge::new(Labels::of("control", &self.name)));
            if let Some(f) = to_f64(&self.initial) {
                gauge.set(f);
            }
            GaugeReification { gauge, to_f64 }
        });
        Control {
            inner: Arc::new(ControlInner {
                name: self.name,
                write_lock: AsyncMutex::new(()),
                committed: std::sync::RwLock::new(versioned),
                appliers: Mutex::new(Vec::new()),
                validator: validator_slot,
                apply_timeout: self.apply_timeout,
                rev_allocator: self.rev_allocator,
                gauge,
                final_at_scope: self.final_at_scope,
                branch_scope: self.branch_scope,
                from_f64: self.from_f64,
            }),
        }
    }
}

// =========================================================================
// Type-erased control handle
// =========================================================================

/// Erased view of a [`Control<T>`] for the registry. Lets the
/// registry hold controls of different value types in one
/// homogeneous map, and lets enumeration / discovery ask
/// questions that don't need the value's static type.
pub trait ErasedControl: Send + Sync {
    fn name(&self) -> &str;
    fn rev(&self) -> u64;
    fn origin(&self) -> ControlOrigin;
    fn applier_count(&self) -> usize;
    /// Human-readable rendering of the current value, for
    /// diagnostics / `dryrun=controls` output.
    fn value_string(&self) -> String;
    /// Static type name of `T` — stable enough to hint the
    /// caller what concrete type they'd need to downcast to.
    fn value_type_name(&self) -> &'static str;
    /// The reified gauge value (if the control opted in), for
    /// the registry's MetricSet snapshot. `None` means either
    /// no reification or the current value has no numeric
    /// projection right now.
    fn gauge_f64(&self) -> Option<f64>;
    /// True if the control was declared with gauge reification.
    fn has_reified_gauge(&self) -> bool;
    /// True if the control was declared `final`. Final
    /// controls reject every non-Launch write.
    fn is_final(&self) -> bool;
    /// Scope name carried by a final declaration, if any.
    fn final_scope(&self) -> Option<String>;
    /// Declared branch scope of this control.
    fn branch_scope(&self) -> BranchScope;
    /// `true` if the control opts into `f64`-routed writes via
    /// [`ControlBuilder::from_f64`].
    fn accepts_f64_writes(&self) -> bool;
    /// Type-erased write through the `f64` converter. Returns
    /// the boxed async future so the caller can await from any
    /// runtime. If the control didn't register a converter the
    /// future resolves to [`SetError::ValidationFailed`] with a
    /// "no f64 setter registered" message.
    fn set_f64(
        &self,
        value: f64,
        origin: ControlOrigin,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetError>> + Send>>;
}

impl<T> ErasedControl for Control<T>
where
    T: Clone + Send + Sync + fmt::Debug + 'static,
{
    fn name(&self) -> &str {
        Control::name(self)
    }

    fn rev(&self) -> u64 {
        self.get().rev
    }

    fn origin(&self) -> ControlOrigin {
        self.get().origin
    }

    fn applier_count(&self) -> usize {
        Control::applier_count(self)
    }

    fn value_string(&self) -> String {
        format!("{:?}", self.get().value)
    }

    fn value_type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }

    fn gauge_f64(&self) -> Option<f64> {
        Control::gauge_f64(self)
    }

    fn has_reified_gauge(&self) -> bool {
        self.inner.gauge.is_some()
    }

    fn is_final(&self) -> bool {
        Control::is_final(self)
    }

    fn final_scope(&self) -> Option<String> {
        Control::final_scope(self).map(|s| s.to_string())
    }

    fn branch_scope(&self) -> BranchScope {
        Control::branch_scope(self)
    }

    fn accepts_f64_writes(&self) -> bool {
        self.inner.from_f64.is_some()
    }

    fn set_f64(
        &self,
        value: f64,
        origin: ControlOrigin,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetError>> + Send>> {
        let inner = self.inner.clone();
        let self_clone = Control { inner };
        Box::pin(async move {
            let converter = match self_clone.inner.from_f64.as_ref() {
                Some(c) => c,
                None => {
                    return Err(SetError::ValidationFailed(format!(
                        "control '{}' has no f64 setter registered — \
                         declare one via ControlBuilder::from_f64",
                        self_clone.inner.name,
                    )));
                }
            };
            let typed = match converter(value) {
                Ok(t) => t,
                Err(msg) => return Err(SetError::ValidationFailed(msg)),
            };
            self_clone.set(typed, origin).await
        })
    }
}

// =========================================================================
// ControlRegistry — per-component store of declared controls
// =========================================================================

/// Holds every control declared on one component. Lookup by
/// name is typed via [`Self::get::<T>`]; enumeration returns
/// erased handles so callers can render / inspect without
/// knowing `T`.
#[derive(Default)]
pub struct ControlRegistry {
    entries: std::sync::RwLock<HashMap<String, Arc<dyn std::any::Any + Send + Sync>>>,
    erased: std::sync::RwLock<HashMap<String, Arc<dyn ErasedControl>>>,
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a control. Panics if a control by the same name
    /// already exists on this component — declaration is
    /// structural and name collisions are bugs.
    pub fn declare<T>(&self, control: Control<T>)
    where
        T: Clone + Send + Sync + fmt::Debug + 'static,
    {
        let name = control.name().to_string();
        let erased: Arc<dyn ErasedControl> = Arc::new(control.clone());
        let typed: Arc<dyn std::any::Any + Send + Sync> = Arc::new(control);

        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let mut erased_map = self.erased.write().unwrap_or_else(|e| e.into_inner());
        assert!(
            !entries.contains_key(&name),
            "ControlRegistry: duplicate control declaration for '{name}'",
        );
        entries.insert(name.clone(), typed);
        erased_map.insert(name, erased);
    }

    /// Look up a control by name with a specific value type.
    /// Returns `None` if the name doesn't exist OR if the
    /// caller's `T` doesn't match the registered type.
    pub fn get<T>(&self, name: &str) -> Option<Control<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let any = entries.get(name)?.clone();
        drop(entries);
        any.downcast::<Control<T>>().ok().map(|arc| (*arc).clone())
    }

    /// Erased access — returns the control's metadata without
    /// needing to know `T`. Used by discovery / enumeration.
    pub fn get_erased(&self, name: &str) -> Option<Arc<dyn ErasedControl>> {
        self.erased
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Enumerate every declared control on this component.
    /// Order is unspecified.
    pub fn list(&self) -> Vec<Arc<dyn ErasedControl>> {
        self.erased
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Produce a `MetricSet` with two families per declared
    /// control:
    ///
    /// 1. **Numeric gauge** `control.<name>` — emitted only when
    ///    the control has a reified-gauge projection *and* the
    ///    current value projects to a numeric `f64`. Missing or
    ///    `None` projections skip this family.
    /// 2. **Info family** `control_info.<name>` — emitted for
    ///    every control regardless of numeric projection.
    ///    Constant value `1.0`; the current value is carried on
    ///    a `value="..."` label so sinks that filter / group by
    ///    dimensional labels can read the symbolic value
    ///    directly. Follows the OpenMetrics "info" pattern (SRD
    ///    23 §"Non-numeric controls: info family").
    ///
    /// Enum / bool / string controls (no numeric projection)
    /// flow through the metrics pipeline via (2). Numeric
    /// controls get both families so reporters can query the
    /// running total as a number AND group by its textual form.
    ///
    /// Called from [`crate::component::capture_tree`] at every
    /// scheduler tick.
    pub fn snapshot_gauges(&self, base_labels: &Labels, captured_at: Instant) -> MetricSet {
        let mut set = MetricSet::at(captured_at, Duration::ZERO);
        let erased = self.erased.read().unwrap_or_else(|e| e.into_inner());
        for ctl in erased.values() {
            // Numeric family: only when a projection exists.
            if let Some(value) = ctl.gauge_f64() {
                let family_name = format!("control_{}", ctl.name());
                let labels = base_labels.with("control", ctl.name());
                set.insert_gauge(&family_name, labels, value, captured_at);
            }
            // Info family: always. Carries the symbolic value
            // via the `value` label; the gauge itself is the
            // OpenMetrics-style constant `1.0`. Downstream sinks
            // filter with `control="name" value="running"`.
            let info_family = format!("control_info_{}", ctl.name());
            let info_labels = base_labels
                .with("control", ctl.name())
                .with("value", ctl.value_string());
            set.insert_gauge(&info_family, info_labels, 1.0, captured_at);
        }
        set
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    // ---- RevAllocator -----------------------------------------

    #[test]
    fn rev_allocator_is_monotonic() {
        let a = RevAllocator::new();
        let r1 = a.next();
        let r2 = a.next();
        let r3 = a.next();
        assert!(r1 < r2 && r2 < r3);
        // No zero — callers use 0 as the "never written" sentinel
        // so allocate strictly > 0.
        assert!(r1 >= 1);
    }

    // ---- Control basics ---------------------------------------

    fn build_u32(name: &str, initial: u32) -> Control<u32> {
        ControlBuilder::new(name, initial)
            .apply_timeout(Duration::from_secs(1))
            .build()
    }

    #[tokio::test]
    async fn initial_value_is_seeded() {
        let c = build_u32("concurrency", 16);
        let got = c.get();
        assert_eq!(got.value, 16);
        assert_eq!(got.rev, 0);
        assert_eq!(got.origin, ControlOrigin::Launch);
    }

    #[tokio::test]
    async fn set_with_no_appliers_commits_and_advances_rev() {
        let c = build_u32("concurrency", 4);
        let rev = c.set(32, ControlOrigin::Test).await.unwrap();
        assert!(rev >= 1);
        let v = c.get();
        assert_eq!(v.value, 32);
        assert_eq!(v.rev, rev);
        assert_eq!(v.origin, ControlOrigin::Test);
    }

    #[tokio::test]
    async fn two_successive_sets_produce_strictly_increasing_revs() {
        let c = build_u32("c", 1);
        let r1 = c.set(2, ControlOrigin::Test).await.unwrap();
        let r2 = c.set(3, ControlOrigin::Test).await.unwrap();
        assert!(r1 < r2);
    }

    // ---- Validator --------------------------------------------

    #[tokio::test]
    async fn validator_rejects_bad_values() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 4)
            .validator(|v| {
                if *v == 0 {
                    Err("must be > 0".into())
                } else if *v > 10_000 {
                    Err("too large".into())
                } else {
                    Ok(())
                }
            })
            .build();

        match c.set(0, ControlOrigin::Test).await {
            Err(SetError::ValidationFailed(msg)) => assert!(msg.contains("must be > 0")),
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
        // Old value untouched.
        assert_eq!(c.value(), 4);

        // Valid value still commits.
        c.set(32, ControlOrigin::Test).await.unwrap();
        assert_eq!(c.value(), 32);
    }

    // ---- Appliers: success path -------------------------------

    #[tokio::test]
    async fn single_applier_sees_new_value() {
        let seen = Arc::new(AtomicU32::new(0));
        let c = build_u32("c", 0);
        let seen_clone = seen.clone();
        c.register_applier(SyncApplier::new(move |v: u32| {
            seen_clone.store(v, Ordering::SeqCst);
            Ok(())
        }));
        c.set(42, ControlOrigin::Test).await.unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 42);
    }

    #[tokio::test]
    async fn multiple_appliers_all_see_same_value_on_success() {
        let c = build_u32("c", 0);
        let counts: Vec<Arc<AtomicU32>> = (0..5).map(|_| Arc::new(AtomicU32::new(0))).collect();
        for counter in &counts {
            let c2 = counter.clone();
            c.register_applier(SyncApplier::new(move |v: u32| {
                c2.store(v, Ordering::SeqCst);
                Ok(())
            }));
        }
        c.set(99, ControlOrigin::Test).await.unwrap();
        for counter in &counts {
            assert_eq!(counter.load(Ordering::SeqCst), 99);
        }
    }

    // ---- Appliers: failure paths ------------------------------

    #[tokio::test]
    async fn any_applier_error_fails_the_set_and_reports_index() {
        let c = build_u32("c", 0);
        // Applier 0 succeeds, 1 fails, 2 succeeds.
        c.register_applier(SyncApplier::new(|_| Ok(())));
        c.register_applier(SyncApplier::new(|_| Err("subsystem X offline".into())));
        c.register_applier(SyncApplier::new(|_| Ok(())));

        match c.set(1, ControlOrigin::Test).await {
            Err(SetError::ApplyFailed(failures)) => {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].applier_index, 1);
                assert!(failures[0].message.contains("subsystem X offline"));
            }
            other => panic!("expected ApplyFailed, got {other:?}"),
        }
        // Committed value unchanged on failure.
        assert_eq!(c.value(), 0);
        assert_eq!(c.get().rev, 0);
    }

    #[tokio::test]
    async fn multiple_applier_failures_all_reported() {
        let c = build_u32("c", 0);
        c.register_applier(SyncApplier::new(|_| Err("first".into())));
        c.register_applier(SyncApplier::new(|_| Ok(())));
        c.register_applier(SyncApplier::new(|_| Err("third".into())));

        match c.set(1, ControlOrigin::Test).await {
            Err(SetError::ApplyFailed(failures)) => {
                assert_eq!(failures.len(), 2);
                let indices: Vec<usize> = failures.iter().map(|f| f.applier_index).collect();
                assert!(indices.contains(&0));
                assert!(indices.contains(&2));
            }
            other => panic!("expected ApplyFailed with 2 failures, got {other:?}"),
        }
        assert_eq!(c.value(), 0);
    }

    #[tokio::test]
    async fn applier_timeout_fails_the_set() {
        let c: Control<u32> = ControlBuilder::new("c", 0)
            .apply_timeout(Duration::from_millis(50))
            .build();

        struct SlowApplier;
        impl ControlApplier<u32> for SlowApplier {
            fn apply(
                &self,
                _v: u32,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(())
                })
            }
        }
        c.register_applier(SlowApplier);

        match c.set(1, ControlOrigin::Test).await {
            Err(SetError::ApplyFailed(failures)) => {
                assert_eq!(failures.len(), 1);
                assert!(
                    failures[0].message.contains("timed out"),
                    "message = {}",
                    failures[0].message,
                );
            }
            other => panic!("expected timeout as ApplyFailed, got {other:?}"),
        }
        assert_eq!(c.value(), 0);
    }

    // ---- Serialization / concurrency --------------------------

    #[tokio::test]
    async fn concurrent_writers_serialize_through_write_lock() {
        // Two writers hit the same control concurrently. The
        // write lock serializes them; both commit (no drops).
        // Their revisions must differ and come from the same
        // allocator's monotonic sequence.
        let c = Arc::new(build_u32("c", 0));
        let apply_count = Arc::new(AtomicUsize::new(0));
        let ac = apply_count.clone();
        c.register_applier(SyncApplier::new(move |_: u32| {
            ac.fetch_add(1, Ordering::SeqCst);
            // Small synthetic delay so the concurrent writers
            // meaningfully contend on the lock.
            std::thread::sleep(Duration::from_millis(5));
            Ok(())
        }));

        let c1 = c.clone();
        let c2 = c.clone();
        let h1 = tokio::spawn(async move { c1.set(10, ControlOrigin::Test).await });
        let h2 = tokio::spawn(async move { c2.set(20, ControlOrigin::Test).await });

        let r1 = h1.await.unwrap().unwrap();
        let r2 = h2.await.unwrap().unwrap();
        assert_ne!(r1, r2, "revs must be distinct across concurrent writes");
        assert_eq!(apply_count.load(Ordering::SeqCst), 2);
        // Final value is one of the two, matching whichever won the race.
        let final_v = c.value();
        assert!(final_v == 10 || final_v == 20);
    }

    // ---- Registry ---------------------------------------------

    #[test]
    fn registry_declare_and_typed_lookup() {
        let reg = ControlRegistry::new();
        let c = build_u32("concurrency", 16);
        reg.declare(c);
        let looked_up: Control<u32> = reg.get("concurrency").unwrap();
        assert_eq!(looked_up.value(), 16);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_get_with_wrong_type_returns_none() {
        let reg = ControlRegistry::new();
        reg.declare(build_u32("concurrency", 8));
        // Asking with a different T should miss cleanly.
        let wrong: Option<Control<u64>> = reg.get("concurrency");
        assert!(wrong.is_none());
    }

    #[test]
    fn registry_missing_control_returns_none() {
        let reg = ControlRegistry::new();
        assert!(reg.get::<u32>("nope").is_none());
        assert!(reg.get_erased("nope").is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate control declaration")]
    fn registry_duplicate_declaration_panics() {
        let reg = ControlRegistry::new();
        reg.declare(build_u32("c", 1));
        reg.declare(build_u32("c", 2));
    }

    #[tokio::test]
    async fn erased_view_surfaces_name_rev_and_value_string() {
        let reg = ControlRegistry::new();
        let c = build_u32("concurrency", 8);
        reg.declare(c.clone());
        c.set(16, ControlOrigin::Test).await.unwrap();

        let erased = reg.get_erased("concurrency").unwrap();
        assert_eq!(erased.name(), "concurrency");
        assert!(erased.rev() >= 1);
        assert_eq!(erased.value_string(), "16");
        assert_eq!(erased.origin(), ControlOrigin::Test);
        assert_eq!(erased.applier_count(), 0);
    }

    #[test]
    fn registry_list_returns_all_controls() {
        let reg = ControlRegistry::new();
        reg.declare(build_u32("a", 1));
        reg.declare(build_u32("b", 2));
        let listed = reg.list();
        assert_eq!(listed.len(), 2);
        let mut names: Vec<&str> = listed.iter().map(|c| c.name()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    // ---- Complex value type ----------------------------------

    // ---- Gauge reification -----------------------------------

    #[tokio::test]
    async fn reified_gauge_seeds_with_initial_value() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 32u32)
            .reify_as_gauge(|v| Some(*v as f64))
            .build();
        // Before any set, the gauge already holds the initial
        // value so a capture-tree tick right after declaration
        // shows the control with a sensible reading.
        assert_eq!(c.gauge_f64(), Some(32.0));
    }

    #[tokio::test]
    async fn reified_gauge_updates_on_commit() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 8u32)
            .reify_as_gauge(|v| Some(*v as f64))
            .build();
        c.set(64, ControlOrigin::Test).await.unwrap();
        assert_eq!(c.gauge_f64(), Some(64.0));
    }

    #[tokio::test]
    async fn reified_gauge_unchanged_on_apply_failure() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 8u32)
            .reify_as_gauge(|v| Some(*v as f64))
            .build();
        c.register_applier(SyncApplier::new(|_: u32| Err("no".into())));
        let _ = c.set(999, ControlOrigin::Test).await;
        // Commit never happened → gauge stays at the initial.
        assert_eq!(c.gauge_f64(), Some(8.0));
    }

    #[tokio::test]
    async fn unreified_control_has_no_gauge() {
        let c = build_u32("c", 1);
        assert!(c.gauge_f64().is_none());
        assert!(c.reified_gauge().is_none());
    }

    #[tokio::test]
    async fn to_f64_can_suppress_samples() {
        // Enum-valued controls may not have a meaningful numeric
        // projection for every variant — `to_f64` returning None
        // skips the sample rather than forcing a sentinel.
        #[derive(Clone, Debug, PartialEq)]
        enum Mode {
            Off,
            On(u32),
        }
        let c: Control<Mode> = ControlBuilder::new("explain", Mode::Off)
            .reify_as_gauge(|m| match m {
                Mode::Off => None,
                Mode::On(n) => Some(*n as f64),
            })
            .build();
        assert!(c.gauge_f64().is_none());
        c.set(Mode::On(7), ControlOrigin::Test).await.unwrap();
        assert_eq!(c.gauge_f64(), Some(7.0));
        c.set(Mode::Off, ControlOrigin::Test).await.unwrap();
        assert!(c.gauge_f64().is_none());
    }

    #[tokio::test]
    async fn registry_snapshot_emits_numeric_gauge_for_reified_controls() {
        let reg = ControlRegistry::new();
        reg.declare(
            ControlBuilder::new("concurrency", 4u32)
                .reify_as_gauge(|v| Some(*v as f64))
                .build(),
        );
        reg.declare(ControlBuilder::new("non_reified", 99u32).build());
        let base = crate::labels::Labels::of("phase", "rampup");
        let now = std::time::Instant::now();
        let snap = reg.snapshot_gauges(&base, now);

        // Numeric gauge for the reified control.
        let family = snap
            .family("control_concurrency")
            .expect("reified control should produce a numeric gauge family");
        let metric = family.metrics().next().unwrap();
        assert_eq!(metric.labels().get("phase"), Some("rampup"));
        assert_eq!(metric.labels().get("control"), Some("concurrency"));

        // The non-reified control has no numeric family —
        // it's carried by the info family instead.
        assert!(snap.family("control_non_reified").is_none());
    }

    #[tokio::test]
    async fn registry_snapshot_emits_info_family_for_every_control() {
        // SRD 23 §"Non-numeric controls: info family": every
        // control emits `control_info.<name>` with the current
        // symbolic value as a `value="..."` label, regardless of
        // whether a numeric projection exists. Numeric controls
        // get the info family in addition to `control.<name>`;
        // non-numeric controls only get the info family.
        let reg = ControlRegistry::new();
        reg.declare(
            ControlBuilder::new("concurrency", 4u32)
                .reify_as_gauge(|v| Some(*v as f64))
                .build(),
        );
        reg.declare(ControlBuilder::new("enabled", true).build());
        reg.declare(ControlBuilder::new("errors_policy", "retry".to_string()).build());

        let base = crate::labels::Labels::of("phase", "bulk");
        let now = std::time::Instant::now();
        let snap = reg.snapshot_gauges(&base, now);

        // Numeric control: both families.
        assert!(snap.family("control_concurrency").is_some());
        let info = snap
            .family("control_info_concurrency")
            .expect("info family should accompany the numeric gauge");
        let m = info.metrics().next().unwrap();
        assert_eq!(m.labels().get("value"), Some("4"));
        assert_eq!(m.labels().get("control"), Some("concurrency"));

        // Bool control: info family only, carrying "true".
        assert!(snap.family("control_enabled").is_none());
        let info = snap
            .family("control_info_enabled")
            .expect("bool control should emit info family");
        let m = info.metrics().next().unwrap();
        assert_eq!(m.labels().get("value"), Some("true"));

        // String control: Debug rendering wraps in quotes — the
        // operator-facing tooling is expected to strip those if
        // needed. What matters is the label survives and the
        // dimension exists.
        let info = snap
            .family("control_info_errors_policy")
            .expect("string control should emit info family");
        let m = info.metrics().next().unwrap();
        assert_eq!(m.labels().get("value"), Some("\"retry\""));
    }

    #[tokio::test]
    async fn works_with_non_copy_value_type() {
        // Exercise the Clone bound with an owned Vec<String>
        // so the fanout/commit path genuinely clones the value.
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Targets {
            hosts: Vec<String>,
        }

        let c: Control<Targets> = ControlBuilder::new(
            "targets",
            Targets {
                hosts: vec!["a".into()],
            },
        )
        .build();

        let seen: Arc<Mutex<Option<Targets>>> = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        c.register_applier(SyncApplier::new(move |t: Targets| {
            *seen_c.lock().unwrap() = Some(t);
            Ok(())
        }));

        let new_val = Targets {
            hosts: vec!["a".into(), "b".into()],
        };
        c.set(new_val.clone(), ControlOrigin::Test).await.unwrap();
        assert_eq!(c.value(), new_val);
        assert_eq!(*seen.lock().unwrap(), Some(new_val));
    }

    // ---- Final declarations -----------------------------------

    #[tokio::test]
    async fn final_control_rejects_non_launch_writes() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 1u32)
            .final_at_scope("ddl_phase")
            .build();
        assert!(c.is_final());
        assert_eq!(c.final_scope(), Some("ddl_phase"));

        // Runtime origins (Test, Tui, Cli, ...) all reject.
        for origin in [ControlOrigin::Test, ControlOrigin::Tui, ControlOrigin::Cli] {
            match c.set(42, origin.clone()).await {
                Err(SetError::FinalViolation { scope }) => {
                    assert_eq!(scope, "ddl_phase");
                }
                other => panic!("expected FinalViolation for {origin:?}, got {other:?}"),
            }
        }
        // Value unchanged across all rejected writes.
        assert_eq!(c.value(), 1u32);
        assert_eq!(c.get().rev, 0);
    }

    #[tokio::test]
    async fn final_control_accepts_launch_seed() {
        // Launch-origin is the one write that goes through so
        // the initial value can be installed. Tests the flow
        // where the control is declared final but the scenario
        // startup still needs to seed a non-default initial.
        let c: Control<u32> = ControlBuilder::new("concurrency", 4u32)
            .final_at_scope("ddl_phase")
            .build();
        let rev = c.set(1u32, ControlOrigin::Launch).await.unwrap();
        assert!(rev >= 1);
        assert_eq!(c.value(), 1u32);
    }

    #[tokio::test]
    async fn final_rejection_runs_before_validation() {
        // A final control with a validator that would reject
        // the attempted value must still surface
        // FinalViolation — the pin takes precedence so operators
        // see the real reason the write was rejected.
        let c: Control<u32> = ControlBuilder::new("concurrency", 1u32)
            .final_at_scope("ddl_phase")
            .validator(|v| {
                if *v == 99 {
                    Err("no 99s".into())
                } else {
                    Ok(())
                }
            })
            .build();
        match c.set(99, ControlOrigin::Test).await {
            Err(SetError::FinalViolation { scope }) => {
                assert_eq!(scope, "ddl_phase");
            }
            other => panic!("expected FinalViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonfinal_control_is_default() {
        let c = build_u32("concurrency", 1);
        assert!(!c.is_final());
        assert!(c.final_scope().is_none());
    }

    // ---- Branch scope ----------------------------------------

    #[test]
    fn default_branch_scope_is_local() {
        let c = build_u32("c", 1);
        assert_eq!(c.branch_scope(), BranchScope::Local);
    }

    #[test]
    fn branch_scope_subtree_is_preserved_in_erased() {
        let c: Control<u32> = ControlBuilder::new("hdr_sigdigs", 3u32)
            .branch_scope(BranchScope::Subtree)
            .build();
        assert_eq!(c.branch_scope(), BranchScope::Subtree);

        let reg = ControlRegistry::new();
        reg.declare(c);
        let erased = reg.get_erased("hdr_sigdigs").unwrap();
        assert_eq!(erased.branch_scope(), BranchScope::Subtree);
    }

    // ---- from_f64 / set_f64 (type-erased write path) ----------

    #[tokio::test]
    async fn set_f64_rejects_without_converter() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 4u32).build();
        let reg = ControlRegistry::new();
        reg.declare(c);
        let erased = reg.get_erased("concurrency").unwrap();
        assert!(!erased.accepts_f64_writes());
        match erased.set_f64(8.0, ControlOrigin::Test).await {
            Err(SetError::ValidationFailed(msg)) => {
                assert!(msg.contains("no f64 setter"), "got: {msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_f64_writes_through_converter_to_typed_control() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 4u32)
            .from_f64(|v| {
                if v < 0.0 || v > u32::MAX as f64 {
                    Err(format!("concurrency out of range: {v}"))
                } else {
                    Ok(v as u32)
                }
            })
            .build();
        let reg = ControlRegistry::new();
        reg.declare(c.clone());
        let erased = reg.get_erased("concurrency").unwrap();
        assert!(erased.accepts_f64_writes());

        let rev = erased.set_f64(64.0, ControlOrigin::Test).await.unwrap();
        assert!(rev >= 1);
        assert_eq!(c.value(), 64u32);
    }

    #[tokio::test]
    async fn set_f64_converter_error_is_surfaced() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 4u32)
            .from_f64(|v| {
                if v < 0.0 {
                    Err(format!("negative: {v}"))
                } else {
                    Ok(v as u32)
                }
            })
            .build();
        let reg = ControlRegistry::new();
        reg.declare(c.clone());
        let erased = reg.get_erased("concurrency").unwrap();
        match erased.set_f64(-1.0, ControlOrigin::Test).await {
            Err(SetError::ValidationFailed(msg)) => {
                assert!(msg.contains("negative"), "got: {msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
        // Committed value unchanged on failed conversion.
        assert_eq!(c.value(), 4u32);
    }

    #[tokio::test]
    async fn set_f64_respects_final_scope() {
        let c: Control<u32> = ControlBuilder::new("concurrency", 4u32)
            .from_f64(|v| Ok(v as u32))
            .final_at_scope("ddl_phase")
            .build();
        let reg = ControlRegistry::new();
        reg.declare(c.clone());
        let erased = reg.get_erased("concurrency").unwrap();
        match erased.set_f64(8.0, ControlOrigin::Test).await {
            Err(SetError::FinalViolation { scope }) => {
                assert_eq!(scope, "ddl_phase");
            }
            other => panic!("expected FinalViolation, got {other:?}"),
        }
    }
}
